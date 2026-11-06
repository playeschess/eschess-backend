use std::sync::Arc;
use std::time::Duration;

use eschess_api::Config;
use eschess_stellar::signer::{LocalSigner, Signer};
use eschess_stellar::tx::Network;
use eschess_stellar::Chain;
use eschess_worker::sender::{tick, SenderConfig};

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
                    if let Err(e) = tick(&pool, chain.as_ref(), &signer, &sender, now).await {
                        tracing::warn!("withdrawal round failed (will retry): {e}");
                    }
                    tokio::time::sleep(poll).await;
                }
            });
        }
    }
    shutdown().await;
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}
