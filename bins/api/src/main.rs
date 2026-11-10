use std::sync::Arc;

use eschess_api::{app, AppState, Config};

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

    // Email: with SMTP_URL set, mail is really sent (any provider that offers SMTP). Without it, messages are
    // only logged. DEV_PRINT_EMAIL_TOKENS=true also prints the one-time tokens, for local development only.
    let app_base_url =
        std::env::var("APP_BASE_URL").unwrap_or_else(|_| "http://localhost:3000".into());
    let print_tokens = std::env::var("DEV_PRINT_EMAIL_TOKENS").is_ok_and(|v| v == "true");
    let mailer: Arc<dyn eschess_notification::Mailer> = match std::env::var("SMTP_URL") {
        Ok(url) => {
            assert!(
                !print_tokens,
                "DEV_PRINT_EMAIL_TOKENS must not be used together with a real mail provider"
            );
            let from =
                std::env::var("MAIL_FROM").expect("MAIL_FROM must be set when SMTP_URL is set");
            Arc::new(
                eschess_notification::SmtpMailer::from_url(&url, &from, &app_base_url)
                    .expect("invalid mail configuration"),
            )
        }
        Err(_) => Arc::new(eschess_notification::LogMailer { print_tokens }),
    };

    let bind = std::env::var("BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .expect("cannot bind");
    tracing::info!("eschess-api listening on {bind}");
    // The Stellar network, if the platform has a wallet on it: asked about withdrawal destinations.
    let chain: Option<Arc<dyn eschess_stellar::Chain>> = config.stellar.as_ref().map(|st| {
        Arc::new(
            eschess_stellar::horizon::HorizonChain::new(&st.horizon_url)
                .expect("cannot build the Stellar client"),
        ) as Arc<dyn eschess_stellar::Chain>
    });
    // The master key that seals players' wallet keys, if one is configured (ADR 0006). A key file is for the testnet
    // only; the public network needs a hardware-backed key service and is refused here.
    let vault: Option<Arc<dyn eschess_stellar::custody::KeyVault>> =
        config.wallet_master_key_file.as_ref().map(|path| {
            let network = config
                .stellar
                .as_ref()
                .and_then(|st| eschess_stellar::tx::Network::parse(&st.network))
                .unwrap_or(eschess_stellar::tx::Network::Testnet);
            let vault = eschess_stellar::custody::LocalVault::from_file(
                std::path::Path::new(path),
                1,
                network,
            )
            .unwrap_or_else(|e| panic!("WALLET_MASTER_KEY_FILE: {e}"));
            Arc::new(vault) as Arc<dyn eschess_stellar::custody::KeyVault>
        });
    if vault.is_none() {
        tracing::info!("WALLET_MASTER_KEY_FILE is not set: players get no wallet of their own yet");
    }
    let state = AppState {
        pool,
        config: Arc::new(config),
        mailer,
        hub: eschess_api::hub::Hub::new(),
        chain,
        vault,
    };
    // Listen for game changes made by any server, and push them to the players connected to this one.
    eschess_api::hub::spawn_listener(state.hub.clone(), state.pool.clone());
    // Pair waiting players as their acceptable rating gap widens.
    eschess_api::routes::matchmaking::spawn_matchmaker(
        state.pool.clone(),
        std::time::Duration::from_secs(1),
    );
    // Deposits and withdrawals are handled by the worker (`make worker`), not here: a settlement retry storm must not
    // slow the move loop.
    axum::serve(listener, app(state))
        .with_graceful_shutdown(shutdown())
        .await
        .expect("server error");
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}
