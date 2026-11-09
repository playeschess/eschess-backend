//! Staked games through the API (ADR 0004 step 1): the stake is one of the offered amounts, is held when a game is
//! created and when it is joined, and comes back when a waiting game is cancelled.

mod common;

use axum::http::StatusCode;
use common::*;
use eschess_api::config::{StellarConfig, TESTNET_HORIZON_URL, TESTNET_USDC_ISSUER};
use eschess_api::Config;
use eschess_domain::money::{Amount, AssetId};
use eschess_domain::stellar::AccountKey;
use eschess_ledger::{Entry, IdempotencyKey, Posting, Side};
use eschess_storage::ledger::{balance, post};
use eschess_storage::stellar::{system_account, user_wallet, CUSTODY_KIND};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

const HOT: &str = "GBL4BGHUU6TAX7WXPSXAES7XBKEPJWLOO3HBIOYHNJH4KMLQZIQVZIF5";

fn usdc() -> AssetId {
    AssetId::credit("USDC", TESTNET_USDC_ISSUER).unwrap()
}

fn config(stakes: &[i64]) -> Config {
    Config {
        stellar: Some(StellarConfig {
            network: "testnet".into(),
            horizon_url: TESTNET_HORIZON_URL.into(),
            hot_account: AccountKey::parse(HOT).unwrap(),
            usdc: usdc(),
            poll_ms: 5_000,
            withdraw: Default::default(),
        }),
        stakes_minor: stakes.to_vec(),
        ..Config::default()
    }
}

/// A confirmed player holding `usdc_minor` of USDC.
async fn funded(api: &Api, name: &str, usdc_minor: i64) -> (String, Uuid) {
    let token = api.player(name).await;
    let id: Uuid = sqlx::query_scalar(
        "UPDATE users SET email_verified_at = now() WHERE username = $1 RETURNING id",
    )
    .bind(name)
    .fetch_one(&api.pool)
    .await
    .unwrap();
    let custody = system_account(&api.pool, CUSTODY_KIND, &usdc(), Side::Debit)
        .await
        .unwrap();
    let wallet = user_wallet(&api.pool, id, &usdc()).await.unwrap();
    let amount = Amount::new(usdc_minor, usdc()).unwrap();
    post(
        &api.pool,
        &Posting::new(
            IdempotencyKey::new(&format!("test-deposit:{id}")).unwrap(),
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
    (token, id)
}

async fn wallet(api: &Api, id: Uuid) -> i64 {
    balance(
        &api.pool,
        user_wallet(&api.pool, id, &usdc()).await.unwrap(),
    )
    .await
    .unwrap()
    .minor
}

fn body(stake: Option<&str>, rated: bool) -> Value {
    let mut b =
        json!({ "time_control": { "base_seconds": 180, "increment_seconds": 2 }, "rated": rated });
    if let Some(s) = stake {
        b["stake"] = json!(s);
    }
    b
}

const FIVE: i64 = 50_000_000; // USDC has 7 decimals on Stellar

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_stake_is_held_when_created_and_joined_and_returned_on_cancel(pool: PgPool) {
    let api = Api::with_config(pool, config(&[FIVE, 5 * FIVE]));
    let (ada, ada_id) = funded(&api, "ada", 10 * FIVE).await;
    let (grace, grace_id) = funded(&api, "grace", 10 * FIVE).await;

    let (status, g) = api.post("/v1/games", &ada, body(Some("5"), true)).await;
    assert_eq!(status, StatusCode::CREATED, "{g}");
    assert_eq!(g["stake"]["minor"], FIVE);
    assert_eq!(wallet(&api, ada_id).await, 9 * FIVE);

    let id = id_of(&g);
    let (status, b) = api
        .post(&format!("/v1/games/{id}/join"), &grace, json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert_eq!(wallet(&api, grace_id).await, 9 * FIVE);

    // A waiting game that is cancelled gives the creator's stake back.
    let (_, g2) = api.post("/v1/games", &ada, body(Some("5"), true)).await;
    assert_eq!(wallet(&api, ada_id).await, 8 * FIVE);
    let (status, _) = api
        .post(&format!("/v1/games/{}/cancel", id_of(&g2)), &ada, json!({}))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(wallet(&api, ada_id).await, 9 * FIVE);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn only_offered_amounts_rated_games_and_covered_stakes_are_accepted(pool: PgPool) {
    let api = Api::with_config(pool, config(&[FIVE]));
    let (ada, ada_id) = funded(&api, "ada", 2 * FIVE).await;
    let (poor, _) = funded(&api, "poor", FIVE - 1).await;

    for (stake, rated, want) in [
        (Some("7"), true, StatusCode::BAD_REQUEST), // not an offered amount
        (Some("five"), true, StatusCode::BAD_REQUEST), // not an amount
        (Some("5"), false, StatusCode::BAD_REQUEST), // unrated games cannot be staked
    ] {
        let (status, b) = api.post("/v1/games", &ada, body(stake, rated)).await;
        assert_eq!(status, want, "{b}");
    }
    let (status, b) = api.post("/v1/games", &poor, body(Some("5"), true)).await;
    assert_eq!(
        (status, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "insufficient_funds"),
        "{b}"
    );
    assert_eq!(
        wallet(&api, ada_id).await,
        2 * FIVE,
        "refusals moved no money"
    );
    assert_eq!(api.count("SELECT count(*) FROM games").await, 0);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn staked_games_are_off_unless_configured(pool: PgPool) {
    let api = Api::with_config(pool, config(&[]));
    let (ada, ada_id) = funded(&api, "ada", 2 * FIVE).await;
    let (status, b) = api.post("/v1/games", &ada, body(Some("5"), true)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(wallet(&api, ada_id).await, 2 * FIVE);
}

fn queue_body(stake: Option<&str>) -> Value {
    let mut b =
        json!({ "time_control": { "base_seconds": 180, "increment_seconds": 2 }, "rated": true });
    if let Some(s) = stake {
        b["stake"] = json!(s);
    }
    b
}

async fn state(api: &Api, token: &str) -> Value {
    api.get("/v1/matchmaking", token).await.1["status"].clone()
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn two_players_asking_for_the_same_stake_are_paired_and_both_stakes_held(pool: PgPool) {
    let api = Api::with_config(pool, config(&[FIVE, 5 * FIVE]));
    let (ada, ada_id) = funded(&api, "ada", 10 * FIVE).await;
    let (grace, grace_id) = funded(&api, "grace", 10 * FIVE).await;

    let (status, b) = api
        .post("/v1/matchmaking", &ada, queue_body(Some("5")))
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "queued");
    assert_eq!(
        wallet(&api, ada_id).await,
        10 * FIVE,
        "nothing is held while waiting"
    );

    let (_, b) = api
        .post("/v1/matchmaking", &grace, queue_body(Some("5")))
        .await;
    assert_eq!(b["status"], "matched", "{b}");
    assert_eq!(
        (wallet(&api, ada_id).await, wallet(&api, grace_id).await),
        (9 * FIVE, 9 * FIVE)
    );
    let game = id_of(&api.get("/v1/matchmaking", &ada).await.1["game"]);
    let (_, g) = api.get(&format!("/v1/games/{game}"), &ada).await;
    assert_eq!(
        (g["status"].as_str(), g["stake"]["minor"].as_i64()),
        (Some("active"), Some(FIVE))
    );
    assert!(api.count("SELECT count(*) FROM games").await == 1);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn different_stakes_and_staked_versus_unstaked_never_meet(pool: PgPool) {
    let api = Api::with_config(pool, config(&[FIVE, 5 * FIVE]));
    let (ada, _) = funded(&api, "ada", 10 * FIVE).await;
    let (grace, _) = funded(&api, "grace", 10 * FIVE).await;
    let (bob, _) = funded(&api, "bob", 10 * FIVE).await;
    api.post("/v1/matchmaking", &ada, queue_body(Some("5")))
        .await;
    let (_, b) = api
        .post("/v1/matchmaking", &grace, queue_body(Some("25")))
        .await;
    assert_eq!(b["status"], "queued", "{b}");
    let (_, b) = api.post("/v1/matchmaking", &bob, queue_body(None)).await;
    assert_eq!(b["status"], "queued", "{b}");
    assert_eq!(api.count("SELECT count(*) FROM games").await, 0);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_player_who_cannot_stake_is_refused_up_front_or_dropped_when_it_comes_to_pairing(
    pool: PgPool,
) {
    let api = Api::with_config(pool, config(&[FIVE]));
    let (ada, ada_id) = funded(&api, "ada", 5 * FIVE).await;
    let (grace, grace_id) = funded(&api, "grace", 5 * FIVE).await;
    let (poor, _) = funded(&api, "poor", FIVE - 1).await;

    // Plainly unable: refused at the door.
    let (status, b) = api
        .post("/v1/matchmaking", &poor, queue_body(Some("5")))
        .await;
    assert_eq!(
        (status, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "insufficient_funds"),
        "{b}"
    );

    // Ada queues, then spends her money (a withdrawal, say) before anyone is found.
    api.post("/v1/matchmaking", &ada, queue_body(Some("5")))
        .await;
    let custody = system_account(&api.pool, CUSTODY_KIND, &usdc(), Side::Debit)
        .await
        .unwrap();
    let wallet_acct = user_wallet(&api.pool, ada_id, &usdc()).await.unwrap();
    let all = Amount::new(5 * FIVE, usdc()).unwrap();
    post(
        &api.pool,
        &Posting::new(
            IdempotencyKey::new("ada-spends-it").unwrap(),
            "withdrawal_request",
            None,
            vec![
                Entry::debit(wallet_acct, all.clone()),
                Entry::credit(custody, all),
            ],
        )
        .unwrap(),
    )
    .await
    .unwrap();

    // Grace arrives: the pair fails on Ada's stake, Ada is dropped, Grace waits, and no money moved.
    let (_, b) = api
        .post("/v1/matchmaking", &grace, queue_body(Some("5")))
        .await;
    assert_eq!(b["status"], "queued", "{b}");
    assert_eq!(
        api.get("/v1/matchmaking", &ada).await.0,
        StatusCode::NOT_FOUND,
        "Ada left the queue"
    );
    assert_eq!(wallet(&api, grace_id).await, 5 * FIVE);
    assert_eq!(
        api.count("SELECT count(*) FROM games").await,
        0,
        "the failed pair left no game behind"
    );

    // Someone else with the money arrives and takes Grace's open slot.
    let (bob, _) = funded(&api, "bob", 5 * FIVE).await;
    let (_, b) = api
        .post("/v1/matchmaking", &bob, queue_body(Some("5")))
        .await;
    assert_eq!(b["status"], "matched", "{b}");
    assert_eq!(wallet(&api, grace_id).await, 4 * FIVE);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_player_with_a_case_opened_while_queued_is_not_staked_into_a_game(pool: PgPool) {
    let api = Api::with_config(pool, config(&[FIVE]));
    let (ada, ada_id) = funded(&api, "ada", 5 * FIVE).await;
    let (grace, grace_id) = funded(&api, "grace", 5 * FIVE).await;
    api.post("/v1/matchmaking", &ada, queue_body(Some("5")))
        .await;
    sqlx::query("INSERT INTO fairplay_cases (id, player_id) VALUES ($1, $2)")
        .bind(Uuid::now_v7())
        .bind(ada_id)
        .execute(&api.pool)
        .await
        .unwrap();
    let (_, b) = api
        .post("/v1/matchmaking", &grace, queue_body(Some("5")))
        .await;
    assert_eq!(b["status"], "queued", "{b}");
    assert_eq!(
        (wallet(&api, ada_id).await, wallet(&api, grace_id).await),
        (5 * FIVE, 5 * FIVE)
    );
}
