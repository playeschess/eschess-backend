//! Linking a Lichess account (ADR 0005), against a real Postgres and a fake Lichess: the login proves who owns the
//! account, each attempt works once and only for the player who started it, and no Lichess token outlives the request.

mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::http::StatusCode;
use common::*;
use eschess_api::config::LichessConfig;
use eschess_api::Config;
use eschess_lichess::{
    AccessToken, Lichess, LichessAccount, LichessError, LichessGame, NewChallenge, OpenChallenge,
};
use serde_json::json;
use sqlx::PgPool;

/// A Lichess where codes are names: the code `magnus` logs in as the account `magnus`. Records what was revoked.
#[derive(Default)]
struct FakeLichess {
    revoked: Mutex<Vec<String>>,
    down: Mutex<bool>,
}

#[async_trait]
impl Lichess for FakeLichess {
    async fn exchange_code(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
        client_id: &str,
    ) -> Result<AccessToken, LichessError> {
        if *self.down.lock().unwrap() {
            return Err(LichessError::Unavailable("down".into()));
        }
        assert!(verifier.len() >= 43, "the verifier of the attempt is sent");
        assert_eq!(redirect_uri, "https://app.example/lichess");
        assert_eq!(client_id, "eschess");
        if code == "bad" {
            return Err(LichessError::Rejected("code expired".into()));
        }
        Ok(AccessToken::new(code.to_string()))
    }
    async fn account(&self, token: &AccessToken) -> Result<LichessAccount, LichessError> {
        // The token is the code, i.e. the account name; flags are spelled into it.
        let raw = token_text(token);
        Ok(LichessAccount {
            id: raw.trim_start_matches("flagged_").to_string(),
            username: raw.trim_start_matches("flagged_").to_uppercase(),
            disabled: false,
            tos_violation: raw.starts_with("flagged_"),
        })
    }
    async fn revoke(&self, token: &AccessToken) -> Result<(), LichessError> {
        self.revoked.lock().unwrap().push(token_text(token));
        Ok(())
    }
    async fn create_open_challenge(&self, _: &NewChallenge) -> Result<OpenChallenge, LichessError> {
        unreachable!("linking never makes a game")
    }
    async fn game(&self, _: &str) -> Result<Option<LichessGame>, LichessError> {
        unreachable!("linking never reads a game")
    }
}

fn token_text(t: &AccessToken) -> String {
    t.expose().to_string()
}

fn config() -> Config {
    Config {
        lichess: Some(LichessConfig {
            client_id: "eschess".into(),
            redirect_uri: "https://app.example/lichess".into(),
            base_url: "https://lichess.org".into(),
            attempt_ttl_secs: 600,
        }),
        ..Config::default()
    }
}

async fn verified(api: &Api, name: &str) -> String {
    let token = api.player(name).await;
    sqlx::query("UPDATE users SET email_verified_at = now() WHERE username = $1")
        .bind(name)
        .execute(&api.pool)
        .await
        .unwrap();
    token
}

async fn start(api: &Api, token: &str) -> String {
    let (status, b) = api.post("/v1/lichess/link/start", token, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{b}");
    let url = b["authorize_url"].as_str().unwrap();
    assert!(url.starts_with("https://lichess.org/oauth?"), "{url}");
    assert!(url.contains("code_challenge_method=S256") && !url.contains("scope"));
    b["state"].as_str().unwrap().to_string()
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_player_logs_in_at_lichess_and_the_account_is_linked_and_the_token_ended(pool: PgPool) {
    let fake = Arc::new(FakeLichess::default());
    let api = Api::with_lichess(pool, config(), fake.clone());
    let ada = verified(&api, "ada").await;

    let (_, s) = api.get("/v1/lichess", &ada).await;
    assert_eq!(
        (s["available"].as_bool(), s["linked"].is_null()),
        (Some(true), true)
    );

    let state = start(&api, &ada).await;
    let (status, b) = api
        .post(
            "/v1/lichess/link/finish",
            &ada,
            json!({ "code": "magnus", "state": state }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert_eq!(b["linked"]["lichess_id"], "magnus");
    assert_eq!(
        *fake.revoked.lock().unwrap(),
        vec!["magnus".to_string()],
        "the token was ended"
    );

    let (_, s) = api.get("/v1/lichess", &ada).await;
    assert_eq!(s["linked"]["username"], "MAGNUS");
    // Nothing from Lichess but the id and username is kept.
    assert_eq!(api.count("SELECT count(*) FROM lichess_links").await, 1);

    // The same attempt cannot be replayed, and a player who is linked cannot start another login.
    let (status, b) = api
        .post(
            "/v1/lichess/link/finish",
            &ada,
            json!({ "code": "magnus", "state": state }),
        )
        .await;
    assert_eq!(
        (status, code(&b)),
        (StatusCode::BAD_REQUEST, "link_attempt_invalid")
    );
    let (status, b) = api.post("/v1/lichess/link/start", &ada, json!({})).await;
    assert_eq!(
        (status, code(&b)),
        (StatusCode::CONFLICT, "lichess_already_linked")
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn an_attempt_belongs_to_the_player_who_started_it_and_an_account_to_one_player(
    pool: PgPool,
) {
    let fake = Arc::new(FakeLichess::default());
    let api = Api::with_lichess(pool, config(), fake.clone());
    let (ada, grace) = (verified(&api, "ada").await, verified(&api, "grace").await);

    // Grace cannot finish Ada's attempt, and a made-up state does nothing.
    let state = start(&api, &ada).await;
    let (status, b) = api
        .post(
            "/v1/lichess/link/finish",
            &grace,
            json!({ "code": "magnus", "state": state }),
        )
        .await;
    assert_eq!(
        (status, code(&b)),
        (StatusCode::BAD_REQUEST, "link_attempt_invalid")
    );
    let (status, _) = api
        .post(
            "/v1/lichess/link/finish",
            &ada,
            json!({ "code": "magnus", "state": "x".repeat(48) }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        fake.revoked.lock().unwrap().is_empty(),
        "no Lichess call for an invalid attempt"
    );

    // Ada links magnus; Grace cannot use the same Lichess account, and the token is still ended.
    let (status, _) = api
        .post(
            "/v1/lichess/link/finish",
            &ada,
            json!({ "code": "magnus", "state": state }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let state = start(&api, &grace).await;
    let (status, b) = api
        .post(
            "/v1/lichess/link/finish",
            &grace,
            json!({ "code": "magnus", "state": state }),
        )
        .await;
    assert_eq!(
        (status, code(&b)),
        (StatusCode::CONFLICT, "lichess_account_taken")
    );
    assert_eq!(fake.revoked.lock().unwrap().len(), 2);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn flagged_accounts_lichess_refusals_and_outages_link_nothing(pool: PgPool) {
    let fake = Arc::new(FakeLichess::default());
    let api = Api::with_lichess(pool, config(), fake.clone());
    let ada = verified(&api, "ada").await;

    let state = start(&api, &ada).await;
    let (status, b) = api
        .post(
            "/v1/lichess/link/finish",
            &ada,
            json!({ "code": "flagged_cheater", "state": state }),
        )
        .await;
    assert_eq!(
        (status, code(&b)),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "lichess_account_not_eligible"
        )
    );
    assert_eq!(
        fake.revoked.lock().unwrap().len(),
        1,
        "the token is ended even when the account is refused"
    );

    let state = start(&api, &ada).await;
    let (status, b) = api
        .post(
            "/v1/lichess/link/finish",
            &ada,
            json!({ "code": "bad", "state": state }),
        )
        .await;
    assert_eq!(
        (status, code(&b)),
        (StatusCode::BAD_REQUEST, "lichess_refused")
    );

    *fake.down.lock().unwrap() = true;
    let state = start(&api, &ada).await;
    let (status, b) = api
        .post(
            "/v1/lichess/link/finish",
            &ada,
            json!({ "code": "magnus", "state": state }),
        )
        .await;
    assert_eq!(
        (status, code(&b)),
        (StatusCode::BAD_GATEWAY, "lichess_unavailable")
    );
    assert_eq!(api.count("SELECT count(*) FROM lichess_links").await, 0);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn linking_needs_a_verified_email_and_is_off_unless_configured(pool: PgPool) {
    let api = Api::with_lichess(pool.clone(), config(), Arc::new(FakeLichess::default()));
    let unverified = api.player("ada").await;
    let (status, _) = api
        .post("/v1/lichess/link/start", &unverified, json!({}))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let off = Api::new(pool);
    let grace = verified(&off, "grace").await;
    let (status, b) = off.post("/v1/lichess/link/start", &grace, json!({})).await;
    assert_eq!(
        (status, code(&b)),
        (StatusCode::NOT_FOUND, "lichess_unavailable")
    );
    let (_, s) = off.get("/v1/lichess", &grace).await;
    assert_eq!(s["available"], false);
}
