use std::sync::Arc;
use std::time::Duration;

use eschess_api::Config;
use eschess_fairplay::{NoopScorer, Scorer as _};
use eschess_stellar::signer::{LocalSigner, Signer};
use eschess_stellar::tx::Network;
use eschess_stellar::Chain;
use eschess_worker::fairplay::FairplayConfig;
use eschess_worker::sender::{tick_with, OwnAccounts, SenderConfig};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let database_url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set (see .env.example)");
    let config = Config::from_env().expect("invalid configuration");
    let pool = eschess_storage::connect(&database_url)
        .await
        .expect("cannot connect to the database");

    // Fair play runs whatever else is configured. This repository has only the scorer that scores everything 0; the
    // private eschess-fairplay repository builds a worker with the real one.
    spawn_fairplay(pool.clone());
    spawn_settle(pool.clone());
    spawn_expiry(pool.clone());

    let Some(stellar) = config.stellar.clone() else {
        tracing::info!(
            "STELLAR_HOT_ACCOUNT is not set: no deposits to watch and no withdrawals to send"
        );
        shutdown().await;
        return;
    };
    let chain: Arc<dyn Chain> = Arc::new(
        eschess_stellar::horizon::HorizonChain::new(&stellar.horizon_url)
            .expect("cannot build the Stellar client"),
    );

    spawn_wallet_setup(pool.clone(), chain.clone(), &stellar);
    spawn_funding(pool.clone(), chain.clone(), &stellar);
    spawn_reconcile(pool.clone(), chain.clone(), &stellar);

    // Deposits need only the public address.
    eschess_api::stellar_watcher::spawn_watcher(pool.clone(), chain.clone(), stellar.clone());

    // Withdrawals need the hot wallet's signing key. Without one they are accepted and wait, approved, until a worker
    // with the key runs.
    let network = Network::parse(&stellar.network).expect("the network was checked by Config");
    let get = |name: &str| std::env::var(name).ok();
    match std::env::var("STELLAR_HOT_SECRET_FILE")
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        None => tracing::warn!(
            "STELLAR_HOT_SECRET_FILE is not set: withdrawals will wait until a worker with the signing key runs"
        ),
        Some(path) => {
            let signer = LocalSigner::from_file(std::path::Path::new(path.trim()), network)
                .expect("cannot load the hot wallet's signing key");
            assert_eq!(
                signer.public_key(),
                stellar.hot_account,
                "STELLAR_HOT_SECRET_FILE holds the key of {}, not of STELLAR_HOT_ACCOUNT",
                signer.public_key().to_g()
            );
            let sender = SenderConfig::from_lookup(&get, network, stellar.hot_account)
                .expect("invalid withdrawal settings");
            let poll = Duration::from_millis(
                get("WITHDRAW_SEND_POLL_MS")
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(2_000u64)
                    .max(500),
            );
            let (pool, chain) = (pool.clone(), chain.clone());
            // With the master key that seals players' keys, withdrawals are paid from the player's own account when it
            // holds the money (ADR 0006); without it, only from the hot wallet.
            let own = std::env::var("WALLET_MASTER_KEY_FILE")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .map(|path| OwnAccounts {
                    vault: Arc::new(
                        eschess_stellar::custody::LocalVault::from_file(
                            std::path::Path::new(path.trim()),
                            1,
                            network,
                        )
                        .unwrap_or_else(|e| panic!("WALLET_MASTER_KEY_FILE: {e}")),
                    ),
                });
            tracing::info!(
                "paying withdrawals from players' own accounts when they hold the money: {}",
                own.is_some()
            );
            tracing::info!(
                "sending withdrawals from {} on the Stellar {}",
                stellar.hot_account.to_g(),
                stellar.network
            );
            tokio::spawn(async move {
                loop {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .expect("the clock is after 1970")
                        .as_secs() as i64;
                    if let Err(e) =
                        tick_with(&pool, chain.as_ref(), &signer, &sender, own.as_ref(), now).await
                    {
                        tracing::warn!("withdrawal round failed (will retry): {e}");
                    }
                    tokio::time::sleep(poll).await;
                }
            });
        }
    }
    shutdown().await;
}

/// Staked games nobody joins are cancelled, and the stake returned, after STAKED_GAME_WAIT_SECS (default 10 minutes).
fn spawn_expiry(pool: sqlx::PgPool) {
    let get = |name: &str| std::env::var(name).ok();
    let wait = eschess_worker::expire::wait_secs(&get).expect("invalid STAKED_GAME_WAIT_SECS");
    tokio::spawn(async move {
        loop {
            match eschess_worker::expire::tick(&pool, wait).await {
                Ok(r) if r.cancelled > 0 => {
                    tracing::info!("expiry: {} staked games cancelled", r.cancelled)
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("expiry round failed (will retry): {e}"),
            }
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
    });
}

/// Compares the ledger with the network on a schedule (ADR 0006 §6): `RECONCILE_INTERVAL_SECS` (default 300, from 30).
/// A persistent difference halts new withdrawals and staked games until a person clears it.
fn spawn_reconcile(
    pool: sqlx::PgPool,
    chain: Arc<dyn eschess_stellar::Chain>,
    stellar: &eschess_api::config::StellarConfig,
) {
    let get = |name: &str| std::env::var(name).ok();
    let interval = get("RECONCILE_INTERVAL_SECS")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(300)
        .max(30);
    let config = eschess_worker::reconcile::ReconcileConfig {
        hot_account: stellar.hot_account,
        assets: vec![stellar.usdc.clone()],
        mismatches_to_halt: eschess_worker::reconcile::mismatches_to_halt(&get)
            .expect("invalid RECONCILE_MISMATCHES_TO_HALT"),
    };
    tracing::info!(
        "checking the ledger against the network every {interval} seconds; {} mismatches in a row halt new outflows",
        config.mismatches_to_halt
    );
    tokio::spawn(async move {
        loop {
            match eschess_worker::reconcile::tick(&pool, &*chain, &config).await {
                Ok(r) if r.halted => {
                    tracing::error!("money halted by the books-against-chain check")
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("books-against-chain check failed (will retry): {e}"),
            }
            tokio::time::sleep(Duration::from_secs(interval)).await;
        }
    });
}

/// Sets players' own Stellar accounts up on the network (ADR 0006). Off unless both the master key that seals their keys
/// (WALLET_MASTER_KEY_FILE) and the funding account's key (WALLET_FUNDER_SECRET_FILE) are given. Testnet only.
fn spawn_wallet_setup(
    pool: sqlx::PgPool,
    chain: Arc<dyn eschess_stellar::Chain>,
    stellar: &eschess_api::config::StellarConfig,
) {
    use eschess_stellar::custody::{KeyVault, LocalVault};
    let get = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    let (Some(master), Some(funder_file)) = (
        get("WALLET_MASTER_KEY_FILE"),
        get("WALLET_FUNDER_SECRET_FILE"),
    ) else {
        tracing::info!(
            "WALLET_MASTER_KEY_FILE and WALLET_FUNDER_SECRET_FILE are not both set: players' own accounts will not be set up by this worker"
        );
        return;
    };
    let network = Network::parse(&stellar.network).expect("the network was checked by Config");
    let vault: Arc<dyn KeyVault> = Arc::new(
        LocalVault::from_file(std::path::Path::new(master.trim()), 1, network)
            .unwrap_or_else(|e| panic!("WALLET_MASTER_KEY_FILE: {e}")),
    );
    let funder = LocalSigner::from_file(std::path::Path::new(funder_file.trim()), network)
        .expect("cannot load the funding account's key");
    let config = eschess_worker::wallet_setup::SetupConfig {
        network,
        usdc: stellar.usdc.clone(),
        starting_balance_stroops: eschess_worker::wallet_setup::funding_stroops(&|n| {
            std::env::var(n).ok()
        })
        .expect("invalid WALLET_FUNDING_XLM"),
        batch: 20,
    };
    tracing::info!(
        "setting up players' own accounts, funded from {} with {} XLM each",
        funder.public_key().to_g(),
        config.starting_balance_stroops as f64 / 1e7
    );
    tokio::spawn(async move {
        loop {
            match eschess_worker::wallet_setup::tick(
                &pool,
                &*chain,
                vault.clone(),
                &funder,
                &config,
            )
            .await
            {
                Ok(r) if r.ready + r.sent + r.failed > 0 => tracing::info!(
                    "wallet setup: {} ready, {} sent, {} waiting, {} failed",
                    r.ready,
                    r.sent,
                    r.waiting,
                    r.failed
                ),
                Ok(_) => {}
                Err(e) => tracing::warn!("wallet setup round failed (will retry): {e}"),
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    });
}

/// Locks the stakes of staked games in the escrow contract (ADR 0007). Off unless the contract (ESCROW_CONTRACT), its
/// admin's key (ESCROW_ADMIN_SECRET_FILE) and the master key for players' wallets (WALLET_MASTER_KEY_FILE) are all given.
/// STELLAR_HOT_SECRET_FILE, if set, lets the platform wallet top up a player whose own account is short. Testnet only.
/// Games are only made on the contract path when the `stake_path` setting says so.
fn spawn_funding(
    pool: sqlx::PgPool,
    chain: Arc<dyn eschess_stellar::Chain>,
    stellar: &eschess_api::config::StellarConfig,
) {
    use eschess_stellar::custody::{KeyVault, LocalVault};
    use eschess_stellar::soroban::SorobanHttp;
    let get = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
    let (Some(contract), Some(admin_file), Some(master)) = (
        get("ESCROW_CONTRACT"),
        get("ESCROW_ADMIN_SECRET_FILE"),
        get("WALLET_MASTER_KEY_FILE"),
    ) else {
        tracing::info!(
            "ESCROW_CONTRACT, ESCROW_ADMIN_SECRET_FILE and WALLET_MASTER_KEY_FILE are not all set: this worker will not lock stakes in the contract"
        );
        return;
    };
    let network = Network::parse(&stellar.network).expect("the network was checked by Config");
    let escrow = eschess_stellar::escrow::Escrow::new(
        eschess_domain::stellar::ContractId::parse(contract.trim())
            .expect("ESCROW_CONTRACT is not a contract address"),
    );
    let token = eschess_stellar::escrow::asset_contract_id(network, &stellar.usdc)
        .expect("cannot work out the USDC token contract");
    let vault: Arc<dyn KeyVault> = Arc::new(
        LocalVault::from_file(std::path::Path::new(master.trim()), 1, network)
            .unwrap_or_else(|e| panic!("WALLET_MASTER_KEY_FILE: {e}")),
    );
    let admin = LocalSigner::from_file(std::path::Path::new(admin_file.trim()), network)
        .expect("cannot load the escrow admin's key");
    let hot = get("STELLAR_HOT_SECRET_FILE").map(|f| {
        LocalSigner::from_file(std::path::Path::new(f.trim()), network)
            .expect("cannot load the platform wallet's key")
    });
    let rpc_url = get("SOROBAN_RPC_URL").unwrap_or_else(|| match network {
        Network::Testnet => "https://soroban-testnet.stellar.org".to_string(),
        Network::Public => panic!("SOROBAN_RPC_URL must be set for the public network"),
    });
    let rpc = SorobanHttp::new(&rpc_url).expect("cannot build the Soroban RPC client");
    let config = eschess_worker::funding::FundingConfig {
        network,
        escrow,
        token,
        usdc: stellar.usdc.clone(),
        timeout_secs: eschess_worker::funding::DEFAULT_TIMEOUT_SECS,
        match_expiry_secs: eschess_worker::funding::DEFAULT_MATCH_EXPIRY_SECS,
        tx_wait: Duration::from_secs(25),
        batch: 10,
    };
    // Settling contract games needs the fair-play scorer to wait for (the same SETTLE_SCORER as the ledger path) and no fee:
    // the contract pays the winner the whole pot (ADR 0007 §5).
    let settle_scorer = match eschess_worker::settle::SettleConfig::from_lookup(&|n| {
        std::env::var(n).ok()
    })
    .expect("invalid settlement settings")
    {
        Some(c) if c.fee_bps > 0 => {
            tracing::error!(
                "STAKE_FEE_BPS is set, but the contract takes no fee: staked games on the contract will NOT be settled until it is 0"
            );
            None
        }
        Some(c) => Some((c.scorer, c.batch)),
        None => {
            tracing::info!("SETTLE_SCORER is not set: finished games on the contract will not be settled by this worker");
            None
        }
    };
    tracing::info!(
        "locking stakes in the escrow contract {} (admin {})",
        contract.trim(),
        admin.public_key().to_g()
    );
    tokio::spawn(async move {
        loop {
            let parts = eschess_worker::funding::Parts {
                pool: &pool,
                rpc: &rpc,
                chain: &*chain,
                vault: vault.clone(),
                admin: &admin,
                hot: hot
                    .as_ref()
                    .map(|h| h as &dyn eschess_stellar::signer::Signer),
            };
            if let Some((scorer, batch)) = &settle_scorer {
                match eschess_worker::contract_settle::tick(&parts, &config, scorer, *batch).await {
                    Ok(r) if r.settled + r.sent + r.failed > 0 => {
                        tracing::info!(
                        "contract settle: {} settled, {} told the contract, {} waiting, {} failed",
                        r.settled, r.sent, r.waiting, r.failed
                    )
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!("contract settle round failed (will retry): {e}"),
                }
            }
            match eschess_worker::funding::tick(&parts, &config).await {
                Ok(r) if r.activated + r.cancelled + r.sent + r.failed > 0 => tracing::info!(
                    "funding: {} started, {} cancelled, {} transactions, {} waiting, {} failed",
                    r.activated,
                    r.cancelled,
                    r.sent,
                    r.waiting,
                    r.failed
                ),
                Ok(_) => {}
                Err(e) => tracing::warn!("funding round failed (will retry): {e}"),
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}

/// Settling staked games is off unless SETTLE_SCORER names the fair-play scorer to wait for (see `settle`).
fn spawn_settle(pool: sqlx::PgPool) {
    let get = |name: &str| std::env::var(name).ok();
    let Some(config) = eschess_worker::settle::SettleConfig::from_lookup(&get)
        .expect("invalid settlement settings")
    else {
        tracing::info!(
            "SETTLE_SCORER is not set: finished staked games will not be settled by this worker"
        );
        return;
    };
    let poll = Duration::from_millis(
        get("SETTLE_POLL_MS")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(5_000u64)
            .max(1_000),
    );
    tracing::info!(
        "settling staked games once {} has looked at them (fee {} bps)",
        config.scorer,
        config.fee_bps
    );
    tokio::spawn(async move {
        loop {
            match eschess_worker::settle::tick(&pool, &config).await {
                Ok(r) if r.paid + r.returned + r.confiscated + r.failed > 0 => tracing::info!(
                    "settle: {} paid, {} returned, {} confiscated, {} waiting, {} failed",
                    r.paid,
                    r.returned,
                    r.confiscated,
                    r.waiting,
                    r.failed
                ),
                Ok(_) => {}
                Err(e) => tracing::warn!("settle round failed (will retry): {e}"),
            }
            tokio::time::sleep(poll).await;
        }
    });
}

fn spawn_fairplay(pool: sqlx::PgPool) {
    let get = |name: &str| std::env::var(name).ok();
    let config = FairplayConfig::from_lookup(&get).expect("invalid fair-play settings");
    let poll = Duration::from_millis(
        get("FAIRPLAY_POLL_MS")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(10_000u64)
            .max(1_000),
    );
    let scorer = NoopScorer;
    tracing::info!(
        "scoring finished games with {} (cases open at {})",
        scorer.name(),
        config.case_threshold
    );
    tokio::spawn(async move {
        loop {
            match eschess_worker::fairplay::tick(&pool, &scorer, &config).await {
                Ok(r) if r.scored + r.unscorable > 0 => tracing::info!(
                    "fair play: {} scored, {} unscorable, {} case updates",
                    r.scored,
                    r.unscorable,
                    r.cases
                ),
                Ok(_) => {}
                Err(e) => tracing::warn!("fair-play round failed (will retry): {e}"),
            }
            tokio::time::sleep(poll).await;
        }
    });
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}
