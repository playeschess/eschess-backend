//! Withdrawals through the API, with a fake Stellar network (`make dev`, then `make test-integration`).

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use common::*;
use eschess_api::config::{
    StellarConfig, WithdrawLimits, TESTNET_HORIZON_URL, TESTNET_USDC_ISSUER,
};
use eschess_api::Config;
use eschess_domain::money::{Amount, AssetId};
use eschess_domain::stellar::AccountKey;
use eschess_ledger::{Entry, IdempotencyKey, Posting, Side};
use eschess_stellar::{AccountState, Chain, ChainError, Page};
use eschess_storage::stellar::{system_account, user_wallet, CUSTODY_KIND};
use eschess_storage::withdrawals::{grant_role, APPROVER_ROLE};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

const HOT: &str = "GBL4BGHUU6TAX7WXPSXAES7XBKEPJWLOO3HBIOYHNJH4KMLQZIQVZIF5";
/// Exists and trusts our USDC.
const DEST: &str = "GBUUQEXJEVAN2FLTOEBQ2OUJREAQOAEYYNW5R2X7O7RE4LZ2DNMBUEAV";
/// Exists, but has no USDC trustline.
const NO_TRUST: &str = "GDK4GYHCFWWDDFISK266LRQCRKNIOG2Y4PJLVR6FOLCBM4ECA2LXGGHQ";
/// A valid address that was never funded.
const MISSING: &str = "GCLSLSBBGOP2GTENWWKA2DXRTNWMRBE6KGVV4UNZK7LMIGMCOQ6EJT2J";
const UNIT: i64 = 10_000_000; // 1 USDC

fn usdc() -> AssetId {
    AssetId::credit("USDC", TESTNET_USDC_ISSUER).unwrap()
}

#[derive(Default)]
struct FakeChain {
    accounts: Mutex<HashMap<String, AccountState>>,
    down: Mutex<bool>,
    lookups: Mutex<u32>,
}

#[async_trait::async_trait]
impl Chain for FakeChain {
    async fn payments(&self, _: &str, _: Option<&str>, _: u32) -> Result<Page, ChainError> {
        Ok(Page {
            records: vec![],
            next_cursor: None,
        })
    }

    async fn account(&self, account: &str) -> Result<Option<AccountState>, ChainError> {
        *self.lookups.lock().unwrap() += 1;
        if *self.down.lock().unwrap() {
            return Err(ChainError::Unavailable("the network is down".into()));
        }
        Ok(self.accounts.lock().unwrap().get(account).cloned())
    }
}

fn chain() -> Arc<FakeChain> {
    let c = FakeChain::default();
    let mut accounts = c.accounts.lock().unwrap();
    accounts.insert(
        DEST.into(),
        AccountState {
            sequence: 1,
            can_receive: vec![usdc()],
        },
    );
    accounts.insert(
        NO_TRUST.into(),
        AccountState {
            sequence: 1,
            can_receive: vec![],
        },
    );
    drop(accounts);
    Arc::new(c)
}

fn config() -> Config {
    Config {
        stellar: Some(StellarConfig {
            network: "testnet".into(),
            horizon_url: TESTNET_HORIZON_URL.into(),
            hot_account: AccountKey::parse(HOT).unwrap(),
            usdc: usdc(),
            poll_ms: 5_000,
            withdraw: WithdrawLimits {
                min_minor: UNIT,
                auto_approve_minor: 100 * UNIT,
                daily_limit_minor: 1_000 * UNIT,
            },
        }),
        ..Config::default()
    }
}

fn api(pool: PgPool, chain: &Arc<FakeChain>) -> Api {
    Api::with_chain(pool, config(), Some(chain.clone() as Arc<dyn Chain>))
}

/// A signed-up player with a confirmed email and `units` USDC in their wallet, credited as a deposit would be.
async fn player(api: &Api, name: &str, units: i64) -> (String, Uuid) {
    let token = api.player(name).await;
    let id: Uuid = sqlx::query_scalar(
        "UPDATE users SET email_verified_at = now() WHERE username = $1 RETURNING id",
    )
    .bind(name)
    .fetch_one(&api.pool)
    .await
    .unwrap();
    if units > 0 {
        let custody = system_account(&api.pool, CUSTODY_KIND, &usdc(), Side::Debit)
            .await
            .unwrap();
        let wallet = user_wallet(&api.pool, id, &usdc()).await.unwrap();
        let amount = Amount::new(units * UNIT, usdc()).unwrap();
        let posting = Posting::new(
            IdempotencyKey::new(&format!("test-deposit:{id}")).unwrap(),
            "deposit",
            None,
            vec![
                Entry::debit(custody, amount.clone()),
                Entry::credit(wallet, amount),
            ],
        )
        .unwrap();
        eschess_storage::ledger::post(&api.pool, &posting)
            .await
            .unwrap();
    }
    (token, id)
}

async fn balance(api: &Api, token: &str) -> i64 {
    let (_, me) = api.get("/v1/me", token).await;
    me["wallet"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["asset_id"] == usdc().to_string().as_str())
        .map(|w| w["amount_minor"].as_str().unwrap().parse().unwrap())
        .unwrap_or(0)
}

fn ask(amount: &str, destination: &str) -> Value {
    json!({ "client_request_id": Uuid::now_v7(), "amount": amount, "destination": destination })
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_withdrawal_is_held_at_once_listed_and_can_be_cancelled(pool: PgPool) {
    let chain = chain();
    let api = api(pool, &chain);
    let (alice, _) = player(&api, "alice", 50).await;

    let body = json!({ "client_request_id": Uuid::now_v7(), "amount": "12.5", "destination": DEST,
                       "memo": { "type": "id", "value": "123456" } });
    let (status, w) = api
        .post("/v1/wallet/withdrawals", &alice, body.clone())
        .await;
    assert_eq!(status, StatusCode::CREATED, "{w}");
    assert_eq!(w["status"], "approved", "below the auto-approve amount");
    assert_eq!(
        (w["amount"].as_str(), w["amount_minor"].as_str()),
        (Some("12.5"), Some("125000000"))
    );
    assert_eq!(w["memo"], json!({ "type": "id", "value": "123456" }));
    assert_eq!(balance(&api, &alice).await, 375_000_000, "held at once");

    // A retry is the same withdrawal, held once.
    let (status, again) = api.post("/v1/wallet/withdrawals", &alice, body).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["id"], w["id"]);
    assert_eq!(balance(&api, &alice).await, 375_000_000);

    let (_, list) = api.get("/v1/wallet/withdrawals", &alice).await;
    assert_eq!(list["withdrawals"].as_array().unwrap().len(), 1);
    let path = format!("/v1/wallet/withdrawals/{}", w["id"].as_str().unwrap());
    let (status, one) = api.get(&path, &alice).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(one["attempts"], json!([]));

    let (status, done) = api.post(&format!("{path}/cancel"), &alice, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(done["status"], "cancelled");
    assert_eq!(balance(&api, &alice).await, 500_000_000);
    let (status, body) = api.post(&format!("{path}/cancel"), &alice, json!({})).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::CONFLICT, "withdrawal_wrong_state")
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn requests_that_could_not_or_should_not_be_paid_are_refused_and_hold_nothing(pool: PgPool) {
    let chain = chain();
    let api = api(pool, &chain);
    let (bob, _) = player(&api, "bob", 50).await;
    let hot_muxed = AccountKey::parse(HOT).unwrap().muxed(7).to_m();
    let cases: Vec<(Value, StatusCode, &str)> = vec![
        (
            ask("0.5", DEST),
            StatusCode::UNPROCESSABLE_ENTITY,
            "below_minimum",
        ),
        (
            ask("1.00000001", DEST),
            StatusCode::BAD_REQUEST,
            "validation_failed",
        ),
        (
            ask("-5", DEST),
            StatusCode::BAD_REQUEST,
            "validation_failed",
        ),
        (
            ask("five", DEST),
            StatusCode::BAD_REQUEST,
            "validation_failed",
        ),
        (
            ask("5", "GNOTANADDRESS"),
            StatusCode::BAD_REQUEST,
            "validation_failed",
        ),
        (
            ask("5", HOT),
            StatusCode::UNPROCESSABLE_ENTITY,
            "destination_is_platform",
        ),
        (
            ask("5", &hot_muxed),
            StatusCode::UNPROCESSABLE_ENTITY,
            "destination_is_platform",
        ),
        (
            ask("5", TESTNET_USDC_ISSUER),
            StatusCode::UNPROCESSABLE_ENTITY,
            "destination_is_issuer",
        ),
        (
            ask("5", MISSING),
            StatusCode::UNPROCESSABLE_ENTITY,
            "destination_not_found",
        ),
        (
            ask("5", NO_TRUST),
            StatusCode::UNPROCESSABLE_ENTITY,
            "destination_cannot_receive",
        ),
        (ask("51", DEST), StatusCode::CONFLICT, "insufficient_funds"),
        (
            json!({ "client_request_id": Uuid::now_v7(), "amount": "5", "destination": DEST,
                    "memo": { "type": "text", "value": "x".repeat(29) } }),
            StatusCode::BAD_REQUEST,
            "validation_failed",
        ),
    ];
    for (body, status, expected) in cases {
        let (got, reply) = api.post("/v1/wallet/withdrawals", &bob, body.clone()).await;
        assert_eq!((got, code(&reply)), (status, expected), "{body}");
    }
    assert_eq!(balance(&api, &bob).await, 50 * UNIT);
    assert_eq!(api.count("SELECT count(*) FROM withdrawals").await, 0);

    // Unconfirmed email: no money moves.
    let carol = api.player("carol").await;
    let (status, reply) = api
        .post("/v1/wallet/withdrawals", &carol, ask("5", DEST))
        .await;
    assert_eq!(
        (status, code(&reply)),
        (StatusCode::FORBIDDEN, "email_not_verified")
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn the_destination_check_needs_the_network_but_a_retry_does_not(pool: PgPool) {
    let chain = chain();
    let api = api(pool, &chain);
    let (dan, _) = player(&api, "dan", 50).await;
    // An M... address is looked up by its G account, and needs no memo.
    let muxed = AccountKey::parse(DEST).unwrap().muxed(99).to_m();
    let first = ask("5", &muxed);
    let (status, w) = api
        .post("/v1/wallet/withdrawals", &dan, first.clone())
        .await;
    assert_eq!(status, StatusCode::CREATED, "{w}");
    assert_eq!(w["destination"], muxed.as_str());

    *chain.down.lock().unwrap() = true;
    let (status, reply) = api
        .post("/v1/wallet/withdrawals", &dan, ask("5", DEST))
        .await;
    assert_eq!(
        (status, code(&reply)),
        (StatusCode::SERVICE_UNAVAILABLE, "network_unavailable")
    );
    let lookups = *chain.lookups.lock().unwrap();
    let (status, again) = api.post("/v1/wallet/withdrawals", &dan, first).await;
    assert_eq!((status, &again["id"]), (StatusCode::OK, &w["id"]));
    assert_eq!(
        *chain.lookups.lock().unwrap(),
        lookups,
        "a retry does not ask the network"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn the_daily_limit_applies_across_requests(pool: PgPool) {
    let chain = chain();
    let api = api(pool, &chain);
    let (erin, _) = player(&api, "erin", 2_000).await;
    for _ in 0..10 {
        let (status, w) = api
            .post("/v1/wallet/withdrawals", &erin, ask("90", DEST))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{w}");
    }
    let (status, reply) = api
        .post("/v1/wallet/withdrawals", &erin, ask("101", DEST))
        .await;
    assert_eq!(
        (status, code(&reply)),
        (StatusCode::UNPROCESSABLE_ENTITY, "daily_limit_exceeded")
    );
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains(" 100 "),
        "{reply}"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn large_withdrawals_wait_for_an_approver_who_is_not_the_player(pool: PgPool) {
    let chain = chain();
    let api = api(pool.clone(), &chain);
    let (fay, fay_id) = player(&api, "fay", 1_000).await;
    let (boss, boss_id) = player(&api, "boss", 0).await;
    grant_role(&pool, boss_id, APPROVER_ROLE).await.unwrap();
    grant_role(&pool, fay_id, APPROVER_ROLE).await.unwrap();
    let (nobody, _) = player(&api, "nobody", 0).await;

    let (_, big) = api
        .post("/v1/wallet/withdrawals", &fay, ask("300", DEST))
        .await;
    assert_eq!(big["status"], "awaiting_approval");
    let (_, other) = api
        .post("/v1/wallet/withdrawals", &fay, ask("200", DEST))
        .await;
    assert_eq!(balance(&api, &fay).await, 500 * UNIT);

    // The admin routes do not exist for a player who is not an approver.
    let (status, _) = api.get("/v1/admin/withdrawals", &nobody).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let approve = format!(
        "/v1/admin/withdrawals/{}/approve",
        big["id"].as_str().unwrap()
    );
    let (status, _) = api.post(&approve, &nobody, json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, queue) = api.get("/v1/admin/withdrawals", &boss).await;
    assert_eq!(status, StatusCode::OK);
    let queue = queue["withdrawals"].as_array().unwrap();
    assert_eq!(queue.len(), 2);
    assert_eq!(queue[0]["user_id"], json!(fay_id));

    let (status, reply) = api.post(&approve, &fay, json!({})).await;
    assert_eq!(
        (status, code(&reply)),
        (StatusCode::FORBIDDEN, "own_withdrawal")
    );
    let (status, ok) = api.post(&approve, &boss, json!({})).await;
    assert_eq!(
        (status, ok["status"].as_str()),
        (StatusCode::OK, Some("approved"))
    );

    let reject = format!(
        "/v1/admin/withdrawals/{}/reject",
        other["id"].as_str().unwrap()
    );
    let (status, _) = api.post(&reject, &boss, json!({ "reason": " " })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, no) = api
        .post(
            &reject,
            &boss,
            json!({ "reason": "please verify your identity first" }),
        )
        .await;
    assert_eq!(
        (status, no["status"].as_str()),
        (StatusCode::OK, Some("rejected"))
    );
    assert_eq!(
        balance(&api, &fay).await,
        700 * UNIT,
        "the rejected 200 came back"
    );

    let path = format!("/v1/wallet/withdrawals/{}", other["id"].as_str().unwrap());
    let (_, seen) = api.get(&path, &fay).await;
    assert_eq!(seen["failure_reason"], "please verify your identity first");
    let (status, _) = api.post(&approve, &boss, json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "already approved");
    let (status, _) = api.get("/v1/admin/withdrawals?status=bogus", &boss).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn another_players_withdrawal_does_not_exist_for_you(pool: PgPool) {
    let chain = chain();
    let api = api(pool, &chain);
    let (gus, _) = player(&api, "gus", 50).await;
    let (hal, _) = player(&api, "hal", 0).await;
    let (_, w) = api
        .post("/v1/wallet/withdrawals", &gus, ask("5", DEST))
        .await;
    let path = format!("/v1/wallet/withdrawals/{}", w["id"].as_str().unwrap());
    assert_eq!(api.get(&path, &hal).await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        api.post(&format!("{path}/cancel"), &hal, json!({})).await.0,
        StatusCode::NOT_FOUND
    );
    let (_, list) = api.get("/v1/wallet/withdrawals", &hal).await;
    assert_eq!(list["withdrawals"], json!([]));
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn withdrawals_are_off_when_the_platform_has_no_wallet(pool: PgPool) {
    let api = Api::new(pool);
    let token = api.player("ivy").await;
    sqlx::query("UPDATE users SET email_verified_at = now()")
        .execute(&api.pool)
        .await
        .unwrap();
    let (status, reply) = api
        .post("/v1/wallet/withdrawals", &token, ask("5", DEST))
        .await;
    assert_eq!(
        (status, code(&reply)),
        (StatusCode::NOT_FOUND, "withdrawals_unavailable")
    );
}
