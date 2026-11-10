//! A real player account set up on the real Stellar testnet by the worker's own code (ignored by default).
//!
//! Needs the network and two testnet key files:
//!   WALLET_FUNDER_SECRET_FILE=...  an account with testnet XLM (the platform's funding account)
//!   WALLET_MASTER_KEY_FILE=...     64 hex characters, chmod 600
//! Run: cargo test -p eschess-worker --test wallet_setup_testnet -- --ignored --nocapture

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use eschess_domain::money::AssetId;
use eschess_stellar::custody::{KeyVault, LocalVault};
use eschess_stellar::horizon::HorizonChain;
use eschess_stellar::signer::LocalSigner;
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
}
