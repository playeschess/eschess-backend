//! Signing up, email confirmation, password reset, login, sessions and the wallet, through the real API against
//! a real Postgres (`make dev`, then `make test-integration`). Each test gets its own fresh database.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use eschess_api::{app, AppState, Config};
use eschess_domain::money::{Amount, AssetId};
use eschess_ledger::{AccountId, Entry, IdempotencyKey, Posting, Side};
use eschess_notification::{CapturingMailer, Message};
use eschess_storage::ledger::{create_account, post, NewAccount};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery";

struct Api {
    app: Router,
    pool: PgPool,
    mailer: Arc<CapturingMailer>,
}

impl Api {
    fn new(pool: PgPool) -> Api {
        Api::with_config(pool, Config::default())
    }
    fn with_config(pool: PgPool, config: Config) -> Api {
        let mailer = Arc::new(CapturingMailer::default());
        Api {
            app: app(AppState {
                pool: pool.clone(),
                config: Arc::new(config),
                mailer: mailer.clone(),
                hub: eschess_api::hub::Hub::new(),
                chain: None,
                vault: None,
                lichess: None,
            }),
            pool,
            mailer,
        }
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(path);
        if let Some(t) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let req = match body {
            Some(b) => req
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(b.to_string()))
                .unwrap(),
            None => req.body(Body::empty()).unwrap(),
        };
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
    async fn get(&self, path: &str, token: &str) -> (StatusCode, Value) {
        self.call(Method::GET, path, Some(token), None).await
    }
    async fn post(&self, path: &str, token: Option<&str>, body: Value) -> (StatusCode, Value) {
        self.call(Method::POST, path, token, Some(body)).await
    }
    async fn signup(&self, email: &str, username: &str) -> (StatusCode, Value) {
        self.post(
            "/v1/auth/signup",
            None,
            json!({ "email": email, "username": username, "password": PASSWORD }),
        )
        .await
    }
    async fn login(&self, email: &str, password: &str) -> (StatusCode, Value) {
        self.post(
            "/v1/auth/login",
            None,
            json!({ "email": email, "password": password }),
        )
        .await
    }
    /// Signs up and returns (access token, refresh token, user id).
    async fn player(&self, email: &str, username: &str) -> (String, String, Uuid) {
        let (status, body) = self.signup(email, username).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        (
            body["access_token"].as_str().unwrap().into(),
            body["refresh_token"].as_str().unwrap().into(),
            body["user"]["id"].as_str().unwrap().parse().unwrap(),
        )
    }
    async fn count(&self, table: &str) -> i64 {
        sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    /// The token in the most recent confirmation (or reset) email sent to `email`, waiting for it to arrive.
    async fn emailed_token(&self, email: &str, reset: bool) -> String {
        for _ in 0..40 {
            let found = self
                .mailer
                .all()
                .iter()
                .rev()
                .find_map(|m| match (m, reset) {
                    (Message::VerifyEmail { to, token, .. }, false)
                    | (Message::PasswordReset { to, token, .. }, true)
                        if to == email =>
                    {
                        Some(token.clone())
                    }
                    _ => None,
                });
            if let Some(token) = found {
                return token;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!(
            "no {} email for {email}",
            if reset { "reset" } else { "confirmation" }
        );
    }
    /// Lets the one-email-per-minute limit pass, as if a minute had gone by.
    async fn minute_passes(&self) {
        sqlx::query("UPDATE email_log SET created_at = now() - interval '2 minutes'")
            .execute(&self.pool)
            .await
            .unwrap();
    }
    async fn forgot(&self, email: &str) -> (StatusCode, Value) {
        self.post("/v1/auth/password/forgot", None, json!({ "email": email }))
            .await
    }
    async fn reset(&self, token: &str, password: &str) -> (StatusCode, Value) {
        self.post(
            "/v1/auth/password/reset",
            None,
            json!({ "token": token, "new_password": password }),
        )
        .await
    }
    async fn confirm(&self, token: &str) -> (StatusCode, Value) {
        self.post("/v1/auth/email/verify", None, json!({ "token": token }))
            .await
    }
}

fn code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("")
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn signing_up_creates_a_player_a_wallet_and_a_session(pool: PgPool) {
    let api = Api::new(pool);
    let (status, body) = api.signup("Ada@Example.com", "Ada_L").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        (
            body["user"]["email"].as_str(),
            body["user"]["username"].as_str()
        ),
        (Some("ada@example.com"), Some("ada_l")),
        "stored normalised"
    );
    assert_eq!(
        (body["token_type"].as_str(), body["expires_in"].as_i64()),
        (Some("Bearer"), Some(900))
    );
    assert!(!body.to_string().contains(PASSWORD) && body.get("password_hash").is_none());

    let token = body["access_token"].as_str().unwrap();
    let (status, me) = api.get("/v1/me", token).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["username"], "ada_l");
    assert_eq!(
        me["wallet"].as_array().unwrap().len(),
        1,
        "one custodial wallet per supported asset"
    );
    assert_eq!(
        (
            me["wallet"][0]["asset_id"].as_str(),
            me["wallet"][0]["amount_minor"].as_str()
        ),
        (Some("XLM"), Some("0")),
        "amounts are strings"
    );

    // The wallet is a ledger account owned by the player, and secrets are stored only as hashes.
    let owner: Option<Uuid> =
        sqlx::query_scalar("SELECT owner_id FROM ledger_accounts WHERE id = $1")
            .bind(
                me["wallet"][0]["account_id"]
                    .as_str()
                    .unwrap()
                    .parse::<Uuid>()
                    .unwrap(),
            )
            .fetch_one(&api.pool)
            .await
            .unwrap();
    assert_eq!(
        owner,
        Some(body["user"]["id"].as_str().unwrap().parse().unwrap())
    );
    let hash: String = sqlx::query_scalar("SELECT password_hash FROM users")
        .fetch_one(&api.pool)
        .await
        .unwrap();
    assert!(hash.starts_with("$argon2id$") && !hash.contains(PASSWORD));
    let (access_hash, refresh_hash): (String, String) = sqlx::query_as("SELECT s.access_hash, r.token_hash FROM sessions s JOIN refresh_tokens r ON r.session_id = s.id").fetch_one(&api.pool).await.unwrap();
    assert!(
        access_hash != token
            && refresh_hash != body["refresh_token"].as_str().unwrap()
            && access_hash.len() == 64
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn an_email_or_username_that_is_taken_is_refused_with_a_clear_error(pool: PgPool) {
    let api = Api::new(pool);
    let (_, _, ada_id) = api.player("ada@example.com", "ada").await;

    // The same email, whatever its capitalisation, is refused and the error says why.
    let (status, body) = api.signup("ADA@example.com", "someone_else").await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, "email_taken"));
    assert_eq!(body["error"]["message"], "that email is already registered");
    // The same goes for a username, again ignoring capitalisation.
    let (status, body) = api.signup("other@example.com", "ADA").await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::CONFLICT, "username_taken")
    );
    assert_eq!(body["error"]["message"], "that username is taken");
    // Both at once: the player is told about the email.
    let (status, body) = api.signup("ada@example.com", "ada").await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, "email_taken"));

    // Nothing was created or changed by the refused attempts, and the original account still works.
    assert_eq!(
        (
            api.count("users").await,
            api.count("ledger_accounts").await,
            api.count("sessions").await
        ),
        (1, 1, 1)
    );
    let owner: Uuid = sqlx::query_scalar("SELECT id FROM users")
        .fetch_one(&api.pool)
        .await
        .unwrap();
    assert_eq!(owner, ada_id);
    assert_eq!(
        api.login("ada@example.com", PASSWORD).await.0,
        StatusCode::OK
    );
    assert_eq!(
        api.signup("fresh@example.com", "fresh_one").await.0,
        StatusCode::CREATED,
        "and a different email and username is fine"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn bad_signups_are_refused_and_leave_nothing_behind(pool: PgPool) {
    let api = Api::new(pool);
    for (email, username, password, why) in [
        ("not-an-email", "ada", PASSWORD, "email"),
        ("ada@example.com", "a", PASSWORD, "short username"),
        ("ada@example.com", "has space", PASSWORD, "bad username"),
        ("ada@example.com", "ada", "short", "short password"),
        (
            "ada@example.com",
            "ada",
            "ada@example.com",
            "password is the email",
        ),
    ] {
        let (status, body) = api
            .post(
                "/v1/auth/signup",
                None,
                json!({ "email": email, "username": username, "password": password }),
            )
            .await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::BAD_REQUEST, "validation_failed"),
            "{why}"
        );
    }
    let (status, _) = api
        .post(
            "/v1/auth/signup",
            None,
            json!({ "email": "ada@example.com" }),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "missing fields");
    assert_eq!(
        (
            api.count("users").await,
            api.count("ledger_accounts").await,
            api.count("sessions").await
        ),
        (0, 0, 0)
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn simultaneous_signups_for_one_email_create_one_player(pool: PgPool) {
    let api = Api::new(pool);
    let names: Vec<String> = (0..6).map(|i| format!("racer_{i}")).collect();
    let results = futures::future::join_all(
        names
            .iter()
            .map(|name| api.signup("race@example.com", name)),
    )
    .await;
    assert_eq!(
        results
            .iter()
            .filter(|(s, _)| *s == StatusCode::CREATED)
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|(s, b)| *s == StatusCode::CONFLICT && code(b) == "email_taken")
            .count(),
        5
    );
    assert_eq!(
        (api.count("users").await, api.count("ledger_accounts").await),
        (1, 1),
        "no orphan wallets from the losers"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn wallets_follow_the_configured_assets(pool: PgPool) {
    let usdc = AssetId::credit(
        "USDC",
        "GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZVN",
    )
    .unwrap();
    let api = Api::with_config(
        pool,
        Config {
            wallet_assets: vec![AssetId::native(), usdc.clone()],
            ..Config::default()
        },
    );
    let (token, _, _) = api.player("ada@example.com", "ada").await;
    let (_, me) = api.get("/v1/me", &token).await;
    let assets: Vec<&str> = me["wallet"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["asset_id"].as_str().unwrap())
        .collect();
    assert_eq!(assets.len(), 2);
    assert!(assets.contains(&"XLM") && assets.contains(&usdc.to_string().as_str()));
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn login_does_not_reveal_which_emails_exist_and_locks_after_repeated_failures(pool: PgPool) {
    let api = Api::new(pool);
    api.player("ada@example.com", "ada").await;

    let (ok_status, ok) = api.login("ADA@example.com", PASSWORD).await;
    assert_eq!(ok_status, StatusCode::OK);
    assert!(ok["access_token"].is_string());
    let wrong = api.login("ada@example.com", "wrong password here").await;
    let unknown = api.login("nobody@example.com", PASSWORD).await;
    let garbled = api.login("not an email", PASSWORD).await;
    assert_eq!(
        wrong, unknown,
        "a wrong password and an unknown email look identical"
    );
    assert_eq!(wrong, garbled);
    assert_eq!(
        (wrong.0, code(&wrong.1)),
        (StatusCode::UNAUTHORIZED, "invalid_credentials")
    );

    // One wrong password so far (above); three more makes four. Five in a row lock the account, and a success resets the count.
    for _ in 0..3 {
        api.login("ada@example.com", "wrong password here").await;
    }
    assert_eq!(
        api.login("ada@example.com", PASSWORD).await.0,
        StatusCode::OK,
        "four failures in a row do not lock"
    );
    for _ in 0..5 {
        api.login("ada@example.com", "wrong password here").await;
    }
    let locked = api.login("ada@example.com", PASSWORD).await;
    assert_eq!(
        locked, wrong,
        "locked out, even with the right password, and it says nothing more"
    );
    sqlx::query("UPDATE users SET locked_until = now() - interval '1 minute'")
        .execute(&api.pool)
        .await
        .unwrap();
    assert_eq!(
        api.login("ada@example.com", PASSWORD).await.0,
        StatusCode::OK,
        "the lock ends by itself"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn requests_without_a_valid_token_are_refused(pool: PgPool) {
    let api = Api::new(pool);
    let (status, body) = api.call(Method::GET, "/v1/me", None, None).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNAUTHORIZED, "unauthorized")
    );
    for token in ["junk", "", &"a".repeat(64)] {
        assert_eq!(
            api.get("/v1/me", token).await.0,
            StatusCode::UNAUTHORIZED,
            "{token:?}"
        );
    }
    // A refresh token is not an access token, and the reverse.
    let (access, refresh, _) = api.player("ada@example.com", "ada").await;
    assert_eq!(
        api.get("/v1/me", &refresh).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        api.post("/v1/auth/refresh", None, json!({ "refresh_token": access }))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn refreshing_rotates_tokens_and_a_replayed_refresh_token_ends_the_session(pool: PgPool) {
    let api = Api::new(pool);
    let (access1, refresh1, _) = api.player("ada@example.com", "ada").await;

    let (status, second) = api
        .post(
            "/v1/auth/refresh",
            None,
            json!({ "refresh_token": refresh1 }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    let (access2, refresh2) = (
        second["access_token"].as_str().unwrap().to_string(),
        second["refresh_token"].as_str().unwrap().to_string(),
    );
    assert!(access2 != access1 && refresh2 != refresh1);
    assert_eq!(
        api.get("/v1/me", &access2).await.0,
        StatusCode::OK,
        "the new access token works"
    );
    assert_eq!(
        api.get("/v1/me", &access1).await.0,
        StatusCode::UNAUTHORIZED,
        "the old one stops working"
    );

    // Someone presents the already-spent refresh token again: the session is revoked, tokens and all.
    let (status, body) = api
        .post(
            "/v1/auth/refresh",
            None,
            json!({ "refresh_token": refresh1 }),
        )
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNAUTHORIZED, "unauthorized")
    );
    assert_eq!(
        api.get("/v1/me", &access2).await.0,
        StatusCode::UNAUTHORIZED,
        "the copied token cost the whole session"
    );
    assert_eq!(
        api.post(
            "/v1/auth/refresh",
            None,
            json!({ "refresh_token": refresh2 })
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let reason: String = sqlx::query_scalar("SELECT revoked_reason FROM sessions")
        .fetch_one(&api.pool)
        .await
        .unwrap();
    assert_eq!(reason, "refresh_token_reuse");
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn two_refreshes_with_one_token_cannot_both_succeed(pool: PgPool) {
    let api = Api::new(pool);
    let (_, refresh, _) = api.player("ada@example.com", "ada").await;
    let results = futures::future::join_all((0..6).map(|_| {
        api.post(
            "/v1/auth/refresh",
            None,
            json!({ "refresh_token": refresh }),
        )
    }))
    .await;
    assert_eq!(
        results.iter().filter(|(s, _)| *s == StatusCode::OK).count(),
        1,
        "a token is spent exactly once"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn tokens_expire(pool: PgPool) {
    let api = Api::new(pool);
    let (access, refresh, _) = api.player("ada@example.com", "ada").await;

    sqlx::query("UPDATE sessions SET access_expires_at = now() - interval '1 second'")
        .execute(&api.pool)
        .await
        .unwrap();
    assert_eq!(
        api.get("/v1/me", &access).await.0,
        StatusCode::UNAUTHORIZED,
        "access tokens are short-lived"
    );
    let (status, fresh) = api
        .post(
            "/v1/auth/refresh",
            None,
            json!({ "refresh_token": refresh }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "but a refresh token gets a new one");
    assert_eq!(
        api.get("/v1/me", fresh["access_token"].as_str().unwrap())
            .await
            .0,
        StatusCode::OK
    );

    sqlx::query("UPDATE refresh_tokens SET expires_at = now() - interval '1 second'")
        .execute(&api.pool)
        .await
        .unwrap();
    assert_eq!(
        api.post(
            "/v1/auth/refresh",
            None,
            json!({ "refresh_token": fresh["refresh_token"] })
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED,
        "refresh tokens expire too"
    );

    let (access, refresh, _) = api.player("grace@example.com", "grace").await;
    sqlx::query("UPDATE sessions SET absolute_expires_at = now() - interval '1 second' WHERE access_hash = $1").bind(eschess_auth::token::hash_token(&access)).execute(&api.pool).await.unwrap();
    assert_eq!(
        api.get("/v1/me", &access).await.0,
        StatusCode::UNAUTHORIZED,
        "a session has a maximum age"
    );
    assert_eq!(
        api.post(
            "/v1/auth/refresh",
            None,
            json!({ "refresh_token": refresh })
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn logging_out_ends_the_session_and_its_refresh_token(pool: PgPool) {
    let api = Api::new(pool);
    let (access, refresh, _) = api.player("ada@example.com", "ada").await;
    assert_eq!(
        api.call(Method::POST, "/v1/auth/logout", Some(&access), None)
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(api.get("/v1/me", &access).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(
        api.post(
            "/v1/auth/refresh",
            None,
            json!({ "refresh_token": refresh })
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn players_can_list_and_end_their_own_sessions_only(pool: PgPool) {
    let api = Api::new(pool);
    let (phone, _, _) = api.player("ada@example.com", "ada").await;
    let (_, laptop) = api.post("/v1/auth/login", None, json!({ "email": "ada@example.com", "password": PASSWORD, "device_label": "Ada's laptop" })).await;
    let laptop_token = laptop["access_token"].as_str().unwrap();
    let (grace_token, _, _) = api.player("grace@example.com", "grace").await;

    let (_, list) = api.get("/v1/auth/sessions", &phone).await;
    let sessions = list["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2, "only Ada's own");
    assert_eq!(sessions.iter().filter(|s| s["current"] == true).count(), 1);
    let laptop_id = sessions
        .iter()
        .find(|s| s["device_label"] == "Ada's laptop")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Grace cannot see or end Ada's sessions, and the answer is the same as for a session that does not exist.
    let theirs = api
        .call(
            Method::DELETE,
            &format!("/v1/auth/sessions/{laptop_id}"),
            Some(&grace_token),
            None,
        )
        .await;
    let nothing = api
        .call(
            Method::DELETE,
            &format!("/v1/auth/sessions/{}", Uuid::now_v7()),
            Some(&grace_token),
            None,
        )
        .await;
    assert_eq!(
        (theirs.0, code(&theirs.1)),
        (StatusCode::NOT_FOUND, "not_found")
    );
    assert_eq!(theirs, nothing);
    assert_eq!(
        api.get("/v1/me", laptop_token).await.0,
        StatusCode::OK,
        "Ada's laptop is untouched"
    );

    assert_eq!(
        api.call(
            Method::DELETE,
            &format!("/v1/auth/sessions/{laptop_id}"),
            Some(&phone),
            None
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        api.get("/v1/me", laptop_token).await.0,
        StatusCode::UNAUTHORIZED,
        "the laptop is signed out"
    );
    assert_eq!(api.get("/v1/me", &phone).await.0, StatusCode::OK);
    assert_eq!(
        api.get("/v1/auth/sessions", &phone).await.1["sessions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        api.call(
            Method::DELETE,
            &format!("/v1/auth/sessions/{laptop_id}"),
            Some(&phone),
            None
        )
        .await
        .0,
        StatusCode::NOT_FOUND,
        "already ended"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_suspended_player_is_locked_out_everywhere(pool: PgPool) {
    let api = Api::new(pool);
    let (access, refresh, _) = api.player("ada@example.com", "ada").await;
    sqlx::query("UPDATE users SET status = 'suspended'")
        .execute(&api.pool)
        .await
        .unwrap();
    assert_eq!(api.get("/v1/me", &access).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(
        api.post(
            "/v1/auth/refresh",
            None,
            json!({ "refresh_token": refresh })
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        api.login("ada@example.com", PASSWORD).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn the_wallet_shows_what_the_ledger_holds(pool: PgPool) {
    let api = Api::new(pool);
    let (token, _, _) = api.player("ada@example.com", "ada").await;
    let (_, me) = api.get("/v1/me", &token).await;
    let wallet = AccountId(
        me["wallet"][0]["account_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap(),
    );

    // A deposit: debit the platform's treasury, credit the player's wallet.
    let treasury = create_account(
        &api.pool,
        &NewAccount {
            kind: "treasury".into(),
            asset: AssetId::native(),
            normal_side: Side::Debit,
            allow_negative: false,
            owner_id: None,
        },
    )
    .await
    .unwrap();
    let xlm = |minor| Amount::new(minor, AssetId::native()).unwrap();
    let deposit = Posting::new(
        IdempotencyKey::new("deposit-ada-0001").unwrap(),
        "deposit",
        None,
        vec![
            Entry::debit(treasury, xlm(25_000_000)),
            Entry::credit(wallet, xlm(25_000_000)),
        ],
    )
    .unwrap();
    post(&api.pool, &deposit).await.unwrap();

    let (_, me) = api.get("/v1/me", &token).await;
    assert_eq!(
        me["wallet"][0]["amount_minor"], "25000000",
        "2.5 XLM, as whole stroops in a string"
    );

    // The wallet account cannot be overdrawn, whatever anyone tries.
    let overdraw = Posting::new(
        IdempotencyKey::new("overdraw-ada-001").unwrap(),
        "withdraw",
        None,
        vec![
            Entry::debit(wallet, xlm(25_000_001)),
            Entry::credit(treasury, xlm(25_000_001)),
        ],
    )
    .unwrap();
    assert!(post(&api.pool, &overdraw).await.is_err());
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn each_player_sees_only_their_own_wallet(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, _, _) = api.player("ada@example.com", "ada").await;
    let (grace, _, _) = api.player("grace@example.com", "grace").await;
    let (_, a) = api.get("/v1/me", &ada).await;
    let (_, g) = api.get("/v1/me", &grace).await;
    assert_ne!(a["wallet"][0]["account_id"], g["wallet"][0]["account_id"]);
    assert_eq!(
        (a["username"].as_str(), g["username"].as_str()),
        (Some("ada"), Some("grace"))
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn signing_up_sends_a_confirmation_email_and_confirming_marks_the_address_verified(
    pool: PgPool,
) {
    let api = Api::new(pool);
    let (status, body) = api.signup("ada@example.com", "ada").await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["user"]["email_verified"], false);
    let access = body["access_token"].as_str().unwrap();
    assert_eq!(
        api.get("/v1/me", access).await.1["email_verified"],
        false,
        "they can use the app, but are marked unconfirmed"
    );

    let token = api.emailed_token("ada@example.com", false).await;
    assert_eq!(token.len(), 64);
    let stored: String = sqlx::query_scalar("SELECT token_hash FROM email_tokens")
        .fetch_one(&api.pool)
        .await
        .unwrap();
    assert!(
        stored != token && stored.len() == 64,
        "only a hash of the token is stored"
    );

    let (status, body) = api.confirm(&"0".repeat(64)).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::BAD_REQUEST, "invalid_token"),
        "an unknown token"
    );
    assert_eq!(
        api.confirm(&token).await,
        (StatusCode::OK, json!({ "email_verified": true }))
    );
    assert_eq!(api.get("/v1/me", access).await.1["email_verified"], true);
    assert_eq!(
        code(&api.confirm(&token).await.1),
        "invalid_token",
        "a token works once"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_confirmation_link_expires_and_is_replaced_by_a_newer_one(pool: PgPool) {
    let api = Api::new(pool);
    let (access, _, _) = api.player("ada@example.com", "ada").await;
    let first = api.emailed_token("ada@example.com", false).await;

    api.minute_passes().await;
    assert_eq!(
        api.post("/v1/auth/email/resend", Some(&access), json!({}))
            .await
            .0,
        StatusCode::ACCEPTED
    );
    api.mailer.wait_for(2).await;
    let second = api.emailed_token("ada@example.com", false).await;
    assert_ne!(first, second);
    assert_eq!(
        code(&api.confirm(&first).await.1),
        "invalid_token",
        "asking again cancels the earlier link"
    );

    sqlx::query("UPDATE email_tokens SET expires_at = now() - interval '1 second'")
        .execute(&api.pool)
        .await
        .unwrap();
    assert_eq!(
        code(&api.confirm(&second).await.1),
        "invalid_token",
        "links expire"
    );
    let verified: Option<bool> =
        sqlx::query_scalar("SELECT email_verified_at IS NOT NULL FROM users")
            .fetch_one(&api.pool)
            .await
            .unwrap();
    assert_eq!(verified, Some(false));
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn resending_the_confirmation_email_is_limited_and_only_for_unconfirmed_players(
    pool: PgPool,
) {
    let api = Api::new(pool);
    let (access, _, _) = api.player("ada@example.com", "ada").await;
    let resend = || api.post("/v1/auth/email/resend", Some(&access), json!({}));

    let (status, body) = resend().await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
        "signup just sent one"
    );
    assert_eq!(api.mailer.settle().await.len(), 1);
    api.minute_passes().await;
    assert_eq!(resend().await.0, StatusCode::ACCEPTED);
    assert_eq!(api.mailer.wait_for(2).await.len(), 2);

    api.confirm(&api.emailed_token("ada@example.com", false).await)
        .await;
    api.minute_passes().await;
    let (status, body) = resend().await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::CONFLICT, "already_verified")
    );
    assert_eq!(
        api.post("/v1/auth/email/resend", None, json!({})).await.0,
        StatusCode::UNAUTHORIZED,
        "must be signed in"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn forgot_password_gives_the_same_answer_for_any_address_and_only_emails_real_accounts(
    pool: PgPool,
) {
    let api = Api::new(pool);
    api.player("ada@example.com", "ada").await;
    api.player("grace@example.com", "grace").await;
    sqlx::query("UPDATE users SET status = 'suspended' WHERE username = 'grace'")
        .execute(&api.pool)
        .await
        .unwrap();
    let before = api.mailer.all().len();

    let real = api.forgot("ADA@example.com").await;
    let unknown = api.forgot("nobody@example.com").await;
    let suspended = api.forgot("grace@example.com").await;
    assert_eq!(
        real, unknown,
        "nobody can tell a registered address from an unregistered one"
    );
    assert_eq!(real, suspended);
    assert_eq!(
        (real.0, real.1["status"].as_str()),
        (StatusCode::ACCEPTED, Some("check_your_email"))
    );

    api.mailer.wait_for(before + 1).await;
    let sent: Vec<Message> = api.mailer.settle().await[before..].to_vec();
    assert_eq!(
        sent.len(),
        1,
        "only the active, registered account is emailed: {sent:?}"
    );
    assert!(
        matches!(&sent[0], Message::PasswordReset { to, ttl_minutes: 30, .. } if to == "ada@example.com")
    );

    let (status, body) = api.forgot("not-an-email").await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::BAD_REQUEST, "validation_failed")
    );
    // Asking again straight away does not send a second email; after a minute it does.
    api.forgot("ada@example.com").await;
    assert_eq!(api.mailer.settle().await.len(), before + 1);
    api.minute_passes().await;
    api.forgot("ada@example.com").await;
    assert_eq!(api.mailer.wait_for(before + 2).await.len(), before + 2);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_reset_link_sets_a_new_password_signs_every_device_out_and_works_once(pool: PgPool) {
    let api = Api::new(pool);
    let (phone_access, phone_refresh, _) = api.player("ada@example.com", "ada").await;
    let (_, laptop) = api.login("ada@example.com", PASSWORD).await;
    let (laptop_access, laptop_refresh) = (
        laptop["access_token"].as_str().unwrap().to_string(),
        laptop["refresh_token"].as_str().unwrap().to_string(),
    );
    sqlx::query(
        "UPDATE users SET locked_until = now() + interval '10 minutes', failed_login_count = 5",
    )
    .execute(&api.pool)
    .await
    .unwrap();
    assert_eq!(
        api.login("ada@example.com", PASSWORD).await.0,
        StatusCode::UNAUTHORIZED,
        "locked out"
    );

    api.forgot("ada@example.com").await;
    let token = api.emailed_token("ada@example.com", true).await;
    let stored: String =
        sqlx::query_scalar("SELECT token_hash FROM email_tokens WHERE purpose = 'reset_password'")
            .fetch_one(&api.pool)
            .await
            .unwrap();
    assert!(stored != token && stored.len() == 64);

    // A weak password is refused without using up the link.
    let (status, body) = api.reset(&token, "short").await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::BAD_REQUEST, "validation_failed")
    );
    assert_eq!(
        api.reset("0".repeat(64).as_str(), "another long password")
            .await
            .0,
        StatusCode::BAD_REQUEST,
        "an unknown token"
    );

    let new_password = "a brand new passphrase";
    let (status, body) = api.reset(&token, new_password).await;
    assert_eq!(
        (status, body["status"].as_str()),
        (StatusCode::OK, Some("password_changed")),
        "{body}"
    );
    assert_eq!(
        code(&api.reset(&token, "yet another passphrase").await.1),
        "invalid_token",
        "the link works once"
    );

    assert_eq!(
        api.login("ada@example.com", PASSWORD).await.0,
        StatusCode::UNAUTHORIZED,
        "the old password is gone"
    );
    let (status, fresh) = api.login("ada@example.com", new_password).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the new one works, and the lockout was lifted"
    );
    for (name, access, refresh) in [
        ("phone", &phone_access, &phone_refresh),
        ("laptop", &laptop_access, &laptop_refresh),
    ] {
        assert_eq!(
            api.get("/v1/me", access).await.0,
            StatusCode::UNAUTHORIZED,
            "{name} is signed out"
        );
        assert_eq!(
            api.post(
                "/v1/auth/refresh",
                None,
                json!({ "refresh_token": refresh })
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED,
            "{name} cannot refresh"
        );
    }
    let me = api
        .get("/v1/me", fresh["access_token"].as_str().unwrap())
        .await
        .1;
    assert_eq!(
        me["email_verified"], true,
        "reading the reset email proves the mailbox is theirs"
    );
    let reasons: Vec<String> =
        sqlx::query_scalar("SELECT revoked_reason FROM sessions WHERE revoked_at IS NOT NULL")
            .fetch_all(&api.pool)
            .await
            .unwrap();
    assert_eq!(reasons, vec!["password_reset", "password_reset"]);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_new_reset_request_cancels_the_old_link_and_links_expire(pool: PgPool) {
    let api = Api::new(pool);
    api.player("ada@example.com", "ada").await;
    api.forgot("ada@example.com").await;
    let first = api.emailed_token("ada@example.com", true).await;
    api.minute_passes().await;
    api.forgot("ada@example.com").await;
    api.mailer.wait_for(3).await;
    let second = api.emailed_token("ada@example.com", true).await;
    assert_ne!(first, second);
    assert_eq!(
        code(&api.reset(&first, "a long enough password").await.1),
        "invalid_token",
        "only the newest link works"
    );

    sqlx::query("UPDATE email_tokens SET expires_at = now() - interval '1 second'")
        .execute(&api.pool)
        .await
        .unwrap();
    assert_eq!(
        code(&api.reset(&second, "a long enough password").await.1),
        "invalid_token",
        "reset links expire"
    );
    assert_eq!(
        api.login("ada@example.com", PASSWORD).await.0,
        StatusCode::OK,
        "nothing changed"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_confirmation_link_is_not_a_reset_link_and_the_reverse(pool: PgPool) {
    let api = Api::new(pool);
    api.player("ada@example.com", "ada").await;
    api.forgot("ada@example.com").await;
    let confirm = api.emailed_token("ada@example.com", false).await;
    let reset = api.emailed_token("ada@example.com", true).await;

    assert_eq!(
        code(&api.reset(&confirm, "a long enough password").await.1),
        "invalid_token",
        "a confirmation token cannot reset a password"
    );
    assert_eq!(
        code(&api.confirm(&reset).await.1),
        "invalid_token",
        "and a reset token cannot confirm an address"
    );
    assert_eq!(
        api.login("ada@example.com", PASSWORD).await.0,
        StatusCode::OK
    );
    assert_eq!(
        api.confirm(&confirm).await.0,
        StatusCode::OK,
        "each still works for its own purpose"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn one_reset_link_used_at_the_same_moment_works_once(pool: PgPool) {
    let api = Api::new(pool);
    api.player("ada@example.com", "ada").await;
    api.forgot("ada@example.com").await;
    let token = api.emailed_token("ada@example.com", true).await;
    let passwords: Vec<String> = (0..6)
        .map(|i| format!("attempt number {i} password"))
        .collect();
    let results = futures::future::join_all(passwords.iter().map(|p| api.reset(&token, p))).await;
    assert_eq!(
        results.iter().filter(|(s, _)| *s == StatusCode::OK).count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|(s, b)| *s == StatusCode::BAD_REQUEST && code(b) == "invalid_token")
            .count(),
        5
    );
    let winners =
        futures::future::join_all(passwords.iter().map(|p| api.login("ada@example.com", p))).await;
    assert_eq!(
        winners.iter().filter(|(s, _)| *s == StatusCode::OK).count(),
        1,
        "exactly one of the passwords took effect"
    );
}
