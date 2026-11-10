//! A real player account set up on the real Stellar testnet by the worker's own code (ignored by default).
//!
//! Needs the network and two testnet key files:
//!   WALLET_FUNDER_SECRET_FILE=...  an account with testnet XLM (the platform's funding account)
//!   WALLET_MASTER_KEY_FILE=...     64 hex characters, chmod 600
//!   EXTERNAL_SECRET_FILE=...       (optional) an account holding testnet USDC; if set, the test also sends it 1 USDC to the
//!                                  new player's address and checks that the watcher credits it
//!   HOT_SECRET_FILE=...            (optional, with EXTERNAL_SECRET_FILE) the platform's wallet key; if set, the test then
//!                                  withdraws half of it back out and checks it is paid from the player's own account
//! Run: cargo test -p eschess-worker --test wallet_setup_testnet -- --ignored --nocapture

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use eschess_domain::money::AssetId;
use eschess_stellar::custody::{KeyVault, LocalVault};
use eschess_stellar::horizon::HorizonChain;
use eschess_stellar::signer::{LocalSigner, Signer};
use eschess_stellar::tx::Network;
use eschess_stellar::Chain;
use eschess_storage::wallets::{self, SetupState};
use eschess_worker::wallet_setup::{tick, SetupConfig};
use sqlx::PgPool;
use uuid::Uuid;

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs the Stellar testnet and key files; see the file header"]
async fn the_worker_sets_up_a_brand_new_players_account_on_the_real_testnet(pool: PgPool) {
    let vault: Arc<dyn KeyVault> = Arc::new(
        LocalVault::from_file(
            Path::new(
                &std::env::var("WALLET_MASTER_KEY_FILE").expect("set WALLET_MASTER_KEY_FILE"),
            ),
            1,
            Network::Testnet,
        )
        .unwrap(),
    );
    let funder = LocalSigner::from_file(
        Path::new(
            &std::env::var("WALLET_FUNDER_SECRET_FILE").expect("set WALLET_FUNDER_SECRET_FILE"),
        ),
        Network::Testnet,
    )
    .unwrap();
    let chain = HorizonChain::new("https://horizon-testnet.stellar.org").unwrap();
    let usdc = AssetId::credit(
        "USDC",
        "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5",
    )
    .unwrap();

    let user = Uuid::now_v7();
    let name = format!("u{}", &user.simple().to_string()[20..]);
    sqlx::query("INSERT INTO users (id, email, username, password_hash) VALUES ($1, $2, $3, 'x')")
        .bind(user)
        .bind(format!("{name}@example.com"))
        .bind(&name)
        .execute(&pool)
        .await
        .unwrap();
    let address = wallets::ensure_wallet(&pool, &*vault, user).await.unwrap();
    println!("new player's address: {}", address.to_g());
    assert!(
        chain.account(&address.to_g()).await.unwrap().is_none(),
        "it does not exist on the network yet"
    );
    assert!(wallets::request_setup(&pool, user).await.unwrap());

    let config = SetupConfig {
        network: Network::Testnet,
        usdc: usdc.clone(),
        starting_balance_stroops: 20_000_000,
        batch: 10,
    };
    for round in 1..=20 {
        let r = tick(&pool, &chain, vault.clone(), &funder, &config)
            .await
            .unwrap();
        println!("round {round}: {r:?}");
        if wallets::setup_state(&pool, user).await.unwrap() == SetupState::Ready {
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    assert_eq!(
        wallets::setup_state(&pool, user).await.unwrap(),
        SetupState::Ready
    );

    // The network agrees: the account exists and can receive USDC.
    let state = chain
        .account(&address.to_g())
        .await
        .unwrap()
        .expect("the account exists");
    assert!(state.can_receive(&usdc));
    let attempts: i32 = sqlx::query_scalar("SELECT attempts FROM wallet_setup WHERE user_id = $1")
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();
    println!("ready after {attempts} transaction(s)");
    assert_eq!(attempts, 1, "one transaction did it all");

    deposit_phase(&pool, &chain, user, &address, &usdc).await;
    withdraw_phase(&pool, &chain, vault.clone(), user, &address, &usdc).await;
}

/// Sends 1 USDC from the account in EXTERNAL_SECRET_FILE to the player's address with our own payment code, then lets the
/// watcher find and credit it. Skipped if EXTERNAL_SECRET_FILE is not set.
async fn deposit_phase(
    pool: &PgPool,
    chain: &HorizonChain,
    user: Uuid,
    address: &eschess_domain::stellar::AccountKey,
    usdc: &AssetId,
) {
    use eschess_domain::money::Amount;
    use eschess_stellar::tx::{self, Payment, PaymentMemo};
    let Ok(file) = std::env::var("EXTERNAL_SECRET_FILE") else {
        println!("EXTERNAL_SECRET_FILE not set: skipping the deposit phase");
        return;
    };
    let external = LocalSigner::from_file(Path::new(&file), Network::Testnet).unwrap();
    let account = chain
        .account(&external.public_key().to_g())
        .await
        .unwrap()
        .unwrap();
    let now = chain.latest_ledger_close_time().await.unwrap() as u64;
    let payment = Payment {
        source: external.public_key(),
        sequence: account.sequence + 1,
        fee_stroops: 1_000,
        min_time: 0,
        max_time: now + 120,
        destination: eschess_domain::stellar::Address::Account(*address),
        amount: Amount::new(10_000_000, usdc.clone()).unwrap(),
        memo: PaymentMemo::None,
    };
    let signed = tx::sign(&payment, Network::Testnet, &external)
        .await
        .unwrap();
    println!(
        "sending 1 USDC to the player's own address: {}",
        signed.hash_hex()
    );
    chain.submit(&signed.envelope_xdr).await.unwrap();

    let before = eschess_storage::stellar::user_wallet(pool, user, usdc)
        .await
        .unwrap();
    for round in 1..=20 {
        // Looking is scheduled; make it due now, as opening the deposit screen would.
        wallets::touch_watch(pool, user).await.unwrap();
        let stats = eschess_api::stellar_watcher::poll_wallets_once(
            pool,
            chain,
            std::slice::from_ref(usdc),
            wallets::WatchTiming::default(),
        )
        .await
        .unwrap();
        println!("watch round {round}: {stats:?}");
        let have = eschess_storage::ledger::balance(pool, before)
            .await
            .unwrap()
            .minor;
        if have > 0 {
            assert_eq!(have, 10_000_000, "exactly the 1 USDC that was sent");
            return;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    panic!("the deposit was not credited");
}

/// Withdraws 0.5 USDC to the external account and checks the player's own account paid it. Skipped unless both
/// HOT_SECRET_FILE and EXTERNAL_SECRET_FILE are set.
async fn withdraw_phase(
    pool: &PgPool,
    chain: &HorizonChain,
    vault: Arc<dyn KeyVault>,
    user: Uuid,
    address: &eschess_domain::stellar::AccountKey,
    usdc: &AssetId,
) {
    use eschess_domain::money::Amount;
    use eschess_stellar::tx::PaymentMemo;
    use eschess_storage::withdrawals::{self, NewWithdrawal, Policy, Status};
    use eschess_worker::sender::{tick_with, OwnAccounts, SenderConfig};
    let (Ok(hot_file), Ok(ext_file)) = (
        std::env::var("HOT_SECRET_FILE"),
        std::env::var("EXTERNAL_SECRET_FILE"),
    ) else {
        println!("HOT_SECRET_FILE or EXTERNAL_SECRET_FILE not set: skipping the withdrawal phase");
        return;
    };
    let hot = LocalSigner::from_file(Path::new(&hot_file), Network::Testnet).unwrap();
    let external = LocalSigner::from_file(Path::new(&ext_file), Network::Testnet).unwrap();
    let destination = external.public_key().to_g();
    let usdc_before_dest = chain
        .account(&destination)
        .await
        .unwrap()
        .unwrap()
        .balance_of(usdc);
    let own_before = chain
        .account(&address.to_g())
        .await
        .unwrap()
        .unwrap()
        .balance_of(usdc);
    assert_eq!(
        own_before, 10_000_000,
        "the player's account holds the 1 USDC that was deposited"
    );

    let (w, _) = withdrawals::request(
        pool,
        &NewWithdrawal {
            user_id: user,
            client_request_id: Uuid::now_v7(),
            amount: Amount::new(5_000_000, usdc.clone()).unwrap(),
            destination,
            memo: PaymentMemo::None,
        },
        Policy {
            auto_approve_up_to_minor: i64::MAX,
            daily_limit_minor: i64::MAX,
        },
    )
    .await
    .unwrap();
    let config = SenderConfig {
        network: Network::Testnet,
        hot_account: hot.public_key(),
        fee_stroops: 1_000,
        tx_timeout_secs: 120,
        expiry_margin_secs: 60,
    };
    let own = OwnAccounts { vault };
    for round in 1..=20 {
        let now = chain.latest_ledger_close_time().await.unwrap();
        let r = tick_with(pool, chain, &hot, &config, Some(&own), now)
            .await
            .unwrap();
        println!("withdrawal round {round}: {r:?}");
        if withdrawals::get(pool, w.id).await.unwrap().status == Status::Confirmed {
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    assert_eq!(
        withdrawals::get(pool, w.id).await.unwrap().status,
        Status::Confirmed
    );
    let source: String = sqlx::query_scalar(
        "SELECT source_account FROM withdrawal_attempts WHERE withdrawal_id = $1",
    )
    .bind(w.id)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(
        source,
        address.to_g(),
        "paid from the player's own account, not the platform wallet"
    );

    // The network agrees: the player's account lost 0.5 USDC, the destination gained it, and the ledger has 0.5 left.
    let own_after = chain
        .account(&address.to_g())
        .await
        .unwrap()
        .unwrap()
        .balance_of(usdc);
    let dest_after = chain
        .account(&external.public_key().to_g())
        .await
        .unwrap()
        .unwrap()
        .balance_of(usdc);
    assert_eq!(
        (own_after, dest_after - usdc_before_dest),
        (5_000_000, 5_000_000)
    );
    let wallet = eschess_storage::stellar::user_wallet(pool, user, usdc)
        .await
        .unwrap();
    assert_eq!(
        eschess_storage::ledger::balance(pool, wallet)
            .await
            .unwrap()
            .minor,
        5_000_000
    );
    println!(
        "withdrew 0.5 USDC from the player's own account; 0.5 left on chain and in the ledger"
    );
}
