//! Opening an anchor's screens to add money from, or cash out to, the player's own currency, with a fake anchor and a real
//! Postgres (`make dev`, then `make test-integration`). The anchor's address for a screen is returned once and never kept.

mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::http::StatusCode;
use common::*;
use eschess_api::config::{FiatConfig, StellarConfig};
use eschess_api::Config;
use eschess_domain::stellar::AccountKey;
use eschess_stellar::custody::{KeyVault, LocalVault};
use eschess_stellar::sep::{Anchor, AnchorToken, AnchorTx, Interactive, Kind, SepError};
use eschess_stellar::signer::Signer;
use eschess_stellar::tx::Network;
use serde_json::json;
use sqlx::PgPool;

const ISSUER: &str = "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5";

#[derive(Default)]
struct FakeAnchor {
    started: Mutex<Vec<(String, String, AccountKey)>>,
    signed_for: Mutex<Vec<AccountKey>>,
    next: Mutex<u32>,
    down: Mutex<bool>,
}

#[async_trait]
impl Anchor for FakeAnchor {
    fn home_domain(&self) -> &str {
        "anchor.example"
    }
    async fn accounts(&self) -> Result<Vec<AccountKey>, SepError> {
        Ok(vec![])
    }
    async fn authenticate(&self, user: &dyn Signer) -> Result<AnchorToken, SepError> {
        if *self.down.lock().unwrap() {
            return Err(SepError::Unavailable("down".into()));
        }
        self.signed_for.lock().unwrap().push(user.public_key());
        Ok(AnchorToken::new("jwt".into()))
    }
    async fn start(
        &self,
        _: &AnchorToken,
        kind: Kind,
        asset: &str,
        account: &AccountKey,
    ) -> Result<Interactive, SepError> {
        let mut n = self.next.lock().unwrap();
        *n += 1;
        self.started
            .lock()
            .unwrap()
            .push((kind.as_str().into(), asset.into(), *account));
        Ok(Interactive {
            id: format!("anchor-tx-{n}"),
            url: format!("https://anchor.example/screen/{n}?token=SECRET{n}"),
        })
    }
    async fn transaction(&self, _: &AnchorToken, _: &str) -> Result<AnchorTx, SepError> {
        unreachable!("the routes never read a transaction")
    }
}

fn config() -> Config {
    Config {
        stellar: Some(StellarConfig {
            network: "testnet".into(),
            horizon_url: "https://horizon-testnet.stellar.org".into(),
            hot_account: AccountKey::parse(
                "GBL4BGHUU6TAX7WXPSXAES7XBKEPJWLOO3HBIOYHNJH4KMLQZIQVZIF5",
            )
            .unwrap(),
            usdc: eschess_domain::money::AssetId::credit("USDC", ISSUER).unwrap(),
            poll_ms: 5_000,
            withdraw: Default::default(),
        }),
        fiat: Some(FiatConfig {
            domain: "anchor.example".into(),
            toml_base: "https://anchor.example".into(),
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

fn api(pool: PgPool, anchor: Arc<FakeAnchor>) -> Api {
    let vault: Arc<dyn KeyVault> =
        Arc::new(LocalVault::from_bytes([9; 32], 1, Network::Testnet).unwrap());
    Api::with_anchor(pool, vault, config(), anchor)
}

async fn ready(api: &Api, name: &str) {
    sqlx::query("UPDATE wallet_setup SET ready_at = now() WHERE user_id = (SELECT id FROM users WHERE username = $1)").bind(name).execute(&api.pool).await.unwrap();
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_player_opens_the_screens_for_a_deposit_and_a_cash_out_with_their_own_account(
    pool: PgPool,
) {
    let anchor = Arc::new(FakeAnchor::default());
    let api = api(pool, anchor.clone());
    let ada = verified(&api, "ada").await;

    // The player's own account must be ready first; asking sets it up.
    let (status, b) = api.post("/v1/wallet/fiat/deposit", &ada, json!({})).await;
    assert_eq!(
        (status, code(&b)),
        (StatusCode::CONFLICT, "wallet_not_ready")
    );
    ready(&api, "ada").await;

    let (status, dep) = api.post("/v1/wallet/fiat/deposit", &ada, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{dep}");
    assert_eq!(dep["kind"], "deposit");
    assert_eq!(dep["status"], "in_progress");
    assert!(dep["url"]
        .as_str()
        .unwrap()
        .starts_with("https://anchor.example/screen/"));
    let (status, wd) = api.post("/v1/wallet/fiat/withdraw", &ada, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{wd}");
    assert_eq!(wd["kind"], "withdraw");

    // Both were for USDC and for the player's own address, and the anchor was logged in to with the player's key.
    let address: String = sqlx::query_scalar("SELECT address FROM user_wallet_keys")
        .fetch_one(&api.pool)
        .await
        .unwrap();
    let started = anchor.started.lock().unwrap().clone();
    assert_eq!(started.len(), 2);
    assert!(started
        .iter()
        .all(|(_, asset, acct)| asset == "USDC" && acct.to_g() == address));
    assert_eq!(
        anchor
            .signed_for
            .lock()
            .unwrap()
            .iter()
            .map(|k| k.to_g())
            .collect::<Vec<_>>(),
        vec![address.clone(), address]
    );

    // The address of the screen (it carries a login token) is not kept anywhere: not in the list, not in the database.
    let (_, list) = api.get("/v1/wallet/fiat", &ada).await;
    assert_eq!(list["available"], true);
    assert_eq!(list["sessions"].as_array().unwrap().len(), 2);
    assert!(!list.to_string().contains("SECRET"));
    let (_, one) = api
        .get(
            &format!("/v1/wallet/fiat/{}", dep["id"].as_str().unwrap()),
            &ada,
        )
        .await;
    assert!(one.get("url").is_none() && !one.to_string().contains("SECRET"));
    let stored: String = sqlx::query_scalar("SELECT string_agg(f::text, ' ') FROM fiat_sessions f")
        .fetch_one(&api.pool)
        .await
        .unwrap();
    assert!(!stored.contains("SECRET"));
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn sessions_are_private_limited_and_need_a_verified_email(pool: PgPool) {
    let anchor = Arc::new(FakeAnchor::default());
    let api = api(pool, anchor.clone());
    let (ada, grace) = (verified(&api, "ada").await, verified(&api, "grace").await);
    api.post("/v1/wallet/fiat/deposit", &ada, json!({})).await;
    ready(&api, "ada").await;
    let (_, first) = api.post("/v1/wallet/fiat/deposit", &ada, json!({})).await;

    // Someone else's session is not found.
    let (status, _) = api
        .get(
            &format!("/v1/wallet/fiat/{}", first["id"].as_str().unwrap()),
            &grace,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // At most three open at once.
    for _ in 0..2 {
        let (status, _) = api.post("/v1/wallet/fiat/withdraw", &ada, json!({})).await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, b) = api.post("/v1/wallet/fiat/withdraw", &ada, json!({})).await;
    assert_eq!(
        (status, code(&b)),
        (StatusCode::CONFLICT, "too_many_open_sessions")
    );
    assert_eq!(
        anchor.started.lock().unwrap().len(),
        3,
        "the anchor was not asked a fourth time"
    );

    // An unverified email may not.
    let unverified = api.player("heidi").await;
    let (status, _) = api
        .post("/v1/wallet/fiat/deposit", &unverified, json!({}))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn an_anchor_that_is_down_says_so_and_it_is_off_unless_configured(pool: PgPool) {
    let anchor = Arc::new(FakeAnchor::default());
    let api = api(pool.clone(), anchor.clone());
    let ada = verified(&api, "ada").await;
    api.post("/v1/wallet/fiat/deposit", &ada, json!({})).await;
    ready(&api, "ada").await;
    *anchor.down.lock().unwrap() = true;
    let (status, b) = api.post("/v1/wallet/fiat/deposit", &ada, json!({})).await;
    assert_eq!(
        (status, code(&b)),
        (StatusCode::SERVICE_UNAVAILABLE, "anchor_unavailable")
    );
    assert_eq!(
        api.count("SELECT count(*) FROM fiat_sessions").await,
        0,
        "nothing is recorded for a screen that never opened"
    );

    let off = Api::new(pool);
    let grace = verified(&off, "grace").await;
    let (status, b) = off.post("/v1/wallet/fiat/deposit", &grace, json!({})).await;
    assert_eq!(
        (status, code(&b)),
        (StatusCode::NOT_FOUND, "fiat_unavailable")
    );
    let (_, list) = off.get("/v1/wallet/fiat", &grace).await;
    assert_eq!(list["available"], false);
}
