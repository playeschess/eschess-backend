use std::sync::Arc;

use axum::routing::{delete, get, post};
use axum::{Json, Router};
use eschess_notification::Mailer;
use serde_json::{json, Value};
use sqlx::PgPool;

pub mod config;
pub mod error;
pub mod extract;
pub mod hub;
pub mod routes;
pub mod stellar_watcher;
pub mod ws;

pub use config::Config;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Arc<Config>,
    pub mailer: Arc<dyn Mailer>,
    /// Who is connected over WebSocket and watching which game.
    pub hub: hub::Hub,
    /// The Stellar network, when the platform has a wallet on it (`config.stellar`). Withdrawals look destinations up here.
    pub chain: Option<Arc<dyn eschess_stellar::Chain>>,
    /// Seals and opens users' wallet keys (ADR 0006). `None` while no master key is configured: players then have no
    /// wallet of their own yet, and nothing else is affected.
    pub vault: Option<Arc<dyn eschess_stellar::custody::KeyVault>>,
    /// Lichess login, to link a player's Lichess account (ADR 0005). `None` while `config.lichess` is off.
    pub lichess: Option<Arc<dyn eschess_lichess::Lichess>>,
}

/// The API's routes. Kept separate from `main` so tests can call it without opening a network port.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/auth/signup", post(routes::auth::signup))
        .route("/v1/auth/login", post(routes::auth::login))
        .route("/v1/auth/email/verify", post(routes::auth::verify_email))
        .route(
            "/v1/auth/email/resend",
            post(routes::auth::resend_verification),
        )
        .route(
            "/v1/auth/password/forgot",
            post(routes::auth::forgot_password),
        )
        .route(
            "/v1/auth/password/reset",
            post(routes::auth::reset_password),
        )
        .route("/v1/auth/refresh", post(routes::auth::refresh))
        .route("/v1/auth/logout", post(routes::auth::logout))
        .route("/v1/auth/sessions", get(routes::auth::list_sessions))
        .route(
            "/v1/auth/sessions/{id}",
            delete(routes::auth::revoke_session),
        )
        .route("/v1/me", get(routes::me::me))
        .route(
            "/v1/matchmaking",
            post(routes::matchmaking::join)
                .get(routes::matchmaking::status)
                .delete(routes::matchmaking::leave),
        )
        .route("/v1/lichess", get(routes::lichess::status))
        .route("/v1/lichess/link/start", post(routes::lichess::start))
        .route("/v1/lichess/link/finish", post(routes::lichess::finish))
        .route("/v1/wallet/deposit-info", get(routes::wallet::deposit_info))
        .route("/v1/wallet/deposits", get(routes::wallet::deposits))
        .route(
            "/v1/wallet/withdrawals",
            get(routes::withdrawals::list).post(routes::withdrawals::request),
        )
        .route("/v1/wallet/withdrawals/{id}", get(routes::withdrawals::get))
        .route(
            "/v1/wallet/withdrawals/{id}/cancel",
            post(routes::withdrawals::cancel),
        )
        .route(
            "/v1/admin/withdrawals",
            get(routes::withdrawals::admin_list),
        )
        .route(
            "/v1/admin/withdrawals/{id}/approve",
            post(routes::withdrawals::admin_approve),
        )
        .route(
            "/v1/admin/withdrawals/{id}/reject",
            post(routes::withdrawals::admin_reject),
        )
        .route("/v1/admin/fairplay/cases", get(routes::fairplay::list))
        .route("/v1/admin/fairplay/cases/{id}", get(routes::fairplay::get))
        .route(
            "/v1/admin/fairplay/cases/{id}/clear",
            post(routes::fairplay::clear),
        )
        .route(
            "/v1/admin/fairplay/cases/{id}/confirm",
            post(routes::fairplay::confirm),
        )
        .route(
            "/v1/admin/fairplay/closures",
            get(routes::fairplay::pending_closures),
        )
        .route(
            "/v1/admin/fairplay/cases/{id}/closure/approve",
            post(routes::fairplay::approve_closure),
        )
        .route(
            "/v1/admin/fairplay/cases/{id}/closure/decline",
            post(routes::fairplay::decline_closure),
        )
        .route("/v1/me/sanctions", get(routes::fairplay::my_sanctions))
        .route(
            "/v1/me/sanctions/{id}/appeal",
            post(routes::fairplay::file_appeal),
        )
        .route(
            "/v1/admin/fairplay/appeals",
            get(routes::fairplay::pending_appeals),
        )
        .route(
            "/v1/admin/fairplay/appeals/{id}/overturn",
            post(routes::fairplay::overturn_appeal),
        )
        .route(
            "/v1/admin/fairplay/appeals/{id}/uphold",
            post(routes::fairplay::uphold_appeal),
        )
        .route("/v1/admin/money", get(routes::money::status))
        .route("/v1/admin/money/halt/clear", post(routes::money::clear))
        .route("/v1/leaderboard", get(routes::ratings::leaderboard))
        .route("/v1/ws", get(ws::upgrade))
        .route(
            "/v1/games",
            get(routes::games::list).post(routes::games::create),
        )
        .route("/v1/games/{id}", get(routes::games::get))
        .route("/v1/games/{id}/join", post(routes::games::join))
        .route("/v1/games/{id}/cancel", post(routes::games::cancel))
        .route("/v1/games/{id}/moves", post(routes::games::play_move))
        .route("/v1/games/{id}/resign", post(routes::games::resign))
        .route("/v1/games/{id}/claim-draw", post(routes::games::claim_draw))
        .route("/v1/games/{id}/draw-offer", post(routes::games::offer_draw))
        .route(
            "/v1/games/{id}/draw-offer/accept",
            post(routes::games::accept_draw_offer),
        )
        .route(
            "/v1/games/{id}/draw-offer/decline",
            post(routes::games::decline_draw_offer),
        )
        .with_state(state)
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok", "service": "eschess-api", "version": env!("CARGO_PKG_VERSION") }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    /// A state whose database is never contacted: these tests only touch routes that do not need it.
    fn offline() -> AppState {
        AppState {
            pool: sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://nobody@127.0.0.1:1/none")
                .unwrap(),
            config: Arc::new(Config::default()),
            mailer: Arc::new(eschess_notification::LogMailer {
                print_tokens: false,
            }),
            hub: hub::Hub::new(),
            chain: None,
            vault: None,
            lichess: None,
        }
    }

    #[tokio::test]
    async fn health_reports_ok() {
        let res = app(offline())
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(
            (body["status"].as_str(), body["service"].as_str()),
            (Some("ok"), Some("eschess-api"))
        );
    }

    #[tokio::test]
    async fn unknown_routes_are_404_and_protected_routes_refuse_strangers() {
        let res = app(offline())
            .oneshot(Request::get("/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
        let res = app(offline())
            .oneshot(Request::get("/v1/me").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            StatusCode::UNAUTHORIZED,
            "no token: refused before any database work"
        );
    }
}
