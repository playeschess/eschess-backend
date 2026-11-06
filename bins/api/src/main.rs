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
    // The Stellar network, if the platform has a wallet on it: watched for deposits, and asked about withdrawal destinations.
    let chain: Option<Arc<dyn eschess_stellar::Chain>> = config.stellar.as_ref().map(|st| {
        Arc::new(
            eschess_stellar::horizon::HorizonChain::new(&st.horizon_url)
                .expect("cannot build the Stellar client"),
        ) as Arc<dyn eschess_stellar::Chain>
    });
    let state = AppState {
        pool,
        config: Arc::new(config),
        mailer,
        hub: eschess_api::hub::Hub::new(),
        chain,
    };
    // Listen for game changes made by any server, and push them to the players connected to this one.
    eschess_api::hub::spawn_listener(state.hub.clone(), state.pool.clone());
    // Pair waiting players as their acceptable rating gap widens.
    eschess_api::routes::matchmaking::spawn_matchmaker(
        state.pool.clone(),
        std::time::Duration::from_secs(1),
    );
    // Credit players for Stellar deposits, if the platform has a wallet configured.
    if let (Some(stellar), Some(chain)) = (state.config.stellar.clone(), state.chain.clone()) {
        eschess_api::stellar_watcher::spawn_watcher(state.pool.clone(), chain, stellar);
    }
    axum::serve(listener, app(state))
        .with_graceful_shutdown(shutdown())
        .await
        .expect("server error");
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}
