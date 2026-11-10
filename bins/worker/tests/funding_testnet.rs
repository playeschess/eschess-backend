//! A staked game locked into the real escrow contract on the real Stellar testnet by the worker's own code (ignored by
//! default). Two brand-new players get accounts, the platform wallet tops them up, and their stakes are deposited.
//!
//! Needs the network and these testnet key files:
//!   WALLET_MASTER_KEY_FILE    64 hex characters, chmod 600
//!   WALLET_FUNDER_SECRET_FILE an account with testnet XLM (sets the players' accounts up)
//!   ESCROW_CONTRACT           the deployed contract (C...)
//!   ESCROW_ADMIN_SECRET_FILE  the contract's current admin
//!   STELLAR_HOT_SECRET_FILE   the platform wallet, holding at least 1 USDC (tops the players up)
//! Run: cargo test -p eschess-worker --test funding_testnet -- --ignored --nocapture

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use eschess_domain::money::{Amount, AssetId};
use eschess_domain::stellar::ContractId;
use eschess_ledger::{Entry, IdempotencyKey, Posting, Side};
use eschess_stellar::custody::{KeyVault, LocalVault};
use eschess_stellar::escrow::{asset_contract_id, parse_match, Escrow, MatchStatus};
use eschess_stellar::horizon::HorizonChain;
use eschess_stellar::signer::{LocalSigner, Signer};
use eschess_stellar::soroban::{prepare, send_and_wait, simulate, Resolved, SorobanHttp};
use eschess_stellar::tx::Network;
use eschess_stellar::Chain;
use eschess_storage::games;
use eschess_storage::ledger::post;
use eschess_storage::stakes::Stake;
use eschess_storage::stellar::{system_account, user_wallet, CUSTODY_KIND};
use eschess_storage::wallets::{self, SetupState};
use eschess_worker::funding::{self, FundingConfig, Parts};
use eschess_worker::wallet_setup::{self, SetupConfig};
use sqlx::PgPool;
use uuid::Uuid;

const STAKE: i64 = 5_000_000; // 0.5 USDC

fn file(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("set {name}"))
}

fn usdc() -> AssetId {
    AssetId::credit(
        "USDC",
        "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5",
    )
    .unwrap()
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs the Stellar testnet and key files; see the file header"]
async fn two_new_players_stakes_are_locked_in_the_real_contract(pool: PgPool) {
    let network = Network::Testnet;
    let vault: Arc<dyn KeyVault> = Arc::new(
        LocalVault::from_file(Path::new(&file("WALLET_MASTER_KEY_FILE")), 1, network).unwrap(),
    );
    let funder =
        LocalSigner::from_file(Path::new(&file("WALLET_FUNDER_SECRET_FILE")), network).unwrap();
    let admin =
        LocalSigner::from_file(Path::new(&file("ESCROW_ADMIN_SECRET_FILE")), network).unwrap();
    let hot = LocalSigner::from_file(Path::new(&file("STELLAR_HOT_SECRET_FILE")), network).unwrap();
    let chain = HorizonChain::new("https://horizon-testnet.stellar.org").unwrap();
    let rpc = SorobanHttp::new("https://soroban-testnet.stellar.org").unwrap();
    let escrow = Escrow::new(ContractId::parse(&file("ESCROW_CONTRACT")).unwrap());
    let token = asset_contract_id(network, &usdc()).unwrap();

    // Two players, each with a wallet key and an account set up on the network.
    let mut players = Vec::new();
    for _ in 0..2 {
        let id = Uuid::now_v7();
        let name = format!("u{}", &id.simple().to_string()[20..]);
        sqlx::query("INSERT INTO users (id, email, username, password_hash, email_verified_at) VALUES ($1, $2, $3, 'x', now())")
            .bind(id).bind(format!("{name}@example.com")).bind(&name).execute(&pool).await.unwrap();
        let address = wallets::ensure_wallet(&pool, &*vault, id).await.unwrap();
        assert!(wallets::request_setup(&pool, id).await.unwrap());
        // Their ledger balance (as a deposit would have given them).
        let custody = system_account(&pool, CUSTODY_KIND, &usdc(), Side::Debit)
            .await
            .unwrap();
        let wallet = user_wallet(&pool, id, &usdc()).await.unwrap();
        let amount = Amount::new(STAKE, usdc()).unwrap();
        post(
            &pool,
            &Posting::new(
                IdempotencyKey::new(&format!("dep:{id}")).unwrap(),
                "deposit",
                None,
                vec![
                    Entry::debit(custody, amount.clone()),
                    Entry::credit(wallet, amount),
                ],
            )
            .unwrap(),
        )
        .await
        .unwrap();
        players.push((id, address));
    }
    let setup = SetupConfig {
        network,
        usdc: usdc(),
        starting_balance_stroops: 20_000_000,
        batch: 10,
    };
    for _ in 0..20 {
        wallet_setup::tick(&pool, &chain, vault.clone(), &funder, &setup)
            .await
            .unwrap();
        let mut all = true;
        for (id, _) in &players {
            all &= wallets::setup_state(&pool, *id).await.unwrap() == SetupState::Ready;
        }
        if all {
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    for (id, a) in &players {
        assert_eq!(
            wallets::setup_state(&pool, *id).await.unwrap(),
            SetupState::Ready,
            "{}",
            a.to_g()
        );
        assert_eq!(
            chain
                .account(&a.to_g())
                .await
                .unwrap()
                .unwrap()
                .balance_of(&usdc()),
            0,
            "they start with no USDC"
        );
    }
    println!("both players' accounts are ready on the network");

    // A staked game on the contract path: it waits in funding.
    sqlx::query("UPDATE platform_settings SET value = 'contract' WHERE name = 'stake_path'")
        .execute(&pool)
        .await
        .unwrap();
    let stake = Stake {
        asset: usdc(),
        minor: STAKE,
    };
    let game = games::create_game(&pool, players[0].0, 300_000, 0, "white", true, Some(&stake))
        .await
        .unwrap();
    games::join_game(&pool, game, players[1].0, true)
        .await
        .unwrap();
    let status = |pool: PgPool| async move {
        sqlx::query_scalar::<_, String>("SELECT status FROM games WHERE id = $1")
            .bind(game)
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    assert_eq!(status(pool.clone()).await, "funding");

    let config = FundingConfig {
        network,
        escrow,
        token,
        usdc: usdc(),
        timeout_secs: 300,
        match_expiry_secs: 3 * 24 * 3600,
        tx_wait: Duration::from_secs(40),
        batch: 5,
    };
    for round in 1..=12 {
        let parts = Parts {
            pool: &pool,
            rpc: &rpc,
            chain: &chain,
            vault: vault.clone(),
            admin: &admin,
            hot: Some(&hot),
        };
        let r = funding::tick(&parts, &config).await.unwrap();
        println!("round {round}: {r:?}");
        if status(pool.clone()).await == "active" {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    assert_eq!(
        status(pool.clone()).await,
        "active",
        "the stakes were locked and the game started"
    );

    // The network agrees: the contract holds both stakes, the players hold none.
    let sim = simulate(
        &rpc,
        &admin.public_key(),
        chain
            .account(&admin.public_key().to_g())
            .await
            .unwrap()
            .unwrap()
            .sequence
            + 1,
        &escrow.get_match(*game.as_bytes()),
    )
    .await
    .unwrap();
    let m = parse_match(&sim.return_value.unwrap()).unwrap();
    println!("on the network: {m:?}");
    assert_eq!(
        (m.status, m.stake, m.a_funded, m.b_funded),
        (MatchStatus::Open, STAKE as i128, true, true)
    );
    for (_, a) in &players {
        assert_eq!(
            chain
                .account(&a.to_g())
                .await
                .unwrap()
                .unwrap()
                .balance_of(&usdc()),
            0
        );
    }
    let txs: i64 = sqlx::query_scalar("SELECT count(*) FROM contract_txs WHERE game_id = $1")
        .bind(game)
        .fetch_one(&pool)
        .await
        .unwrap();
    println!("{txs} transactions were recorded for the game");

    // Tidy up the testnet: return the stakes with our own client (the game is not going to be played).
    let admin_seq = chain
        .account(&admin.public_key().to_g())
        .await
        .unwrap()
        .unwrap()
        .sequence
        + 1;
    let now = chain.latest_ledger_close_time().await.unwrap() as u64;
    let prepared = prepare(
        &rpc,
        network,
        &admin,
        &escrow.cancel(*game.as_bytes()),
        admin_seq,
        now + 120,
    )
    .await
    .unwrap();
    assert!(matches!(
        send_and_wait(
            &rpc,
            &prepared,
            Duration::from_secs(40),
            Duration::from_secs(2)
        )
        .await
        .unwrap(),
        Resolved::Success { .. }
    ));
    for (_, a) in &players {
        assert_eq!(
            chain
                .account(&a.to_g())
                .await
                .unwrap()
                .unwrap()
                .balance_of(&usdc()),
            STAKE,
            "the stake came back"
        );
    }
}
