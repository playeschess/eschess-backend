//! A suspended player cannot start or join rated games, in any of the three ways in (create, join, matchmaking), while
//! unrated play stays open and a warning or an expired suspension restricts nothing (ADR 0003 §3).

mod common;

use axum::http::StatusCode;
use common::*;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

async fn person(api: &Api, name: &str) -> (String, Uuid) {
    let token = api.player(name).await;
    let id = sqlx::query_scalar(
        "UPDATE users SET email_verified_at = now() WHERE username = $1 RETURNING id",
    )
    .bind(name)
    .fetch_one(&api.pool)
    .await
    .unwrap();
    (token, id)
}

/// Records a confirmed case about `player` with a sanction: `days` from now (negative = already over), or a warning.
async fn sanction(api: &Api, player: Uuid, kind: &str, days: Option<i32>) {
    let reviewer = Uuid::now_v7();
    sqlx::query("INSERT INTO users (id, email, username, password_hash) VALUES ($1, $2, $3, 'x')")
        .bind(reviewer)
        .bind(format!("{reviewer}@example.com"))
        .bind(format!("r{}", &reviewer.simple().to_string()[20..]))
        .execute(&api.pool)
        .await
        .unwrap();
    let case = Uuid::now_v7();
    sqlx::query("INSERT INTO fairplay_cases (id, player_id, status, decided_by, decided_at, reason) VALUES ($1, $2, 'confirmed', $3, now(), 'test')")
        .bind(case).bind(player).bind(reviewer)
        .execute(&api.pool).await.unwrap();
    // Backdated by two days so that an already-finished suspension is possible.
    sqlx::query("INSERT INTO fairplay_sanctions (id, case_id, player_id, kind, imposed_at, ends_at) VALUES ($1, $2, $3, $4, now() - interval '2 days', CASE WHEN $5::int IS NULL THEN NULL ELSE now() + make_interval(days => $5::int) END)")
        .bind(Uuid::now_v7()).bind(case).bind(player).bind(kind).bind(days)
        .execute(&api.pool).await.unwrap();
}

fn game(rated: bool) -> Value {
    json!({ "time_control": { "base_seconds": 180, "increment_seconds": 2 }, "rated": rated })
}

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_suspended_player_cannot_play_rated_by_any_route_but_can_play_unrated(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, ada_id) = person(&api, "ada").await;
    let (grace, _) = person(&api, "grace").await;
    sanction(&api, ada_id, "suspension", Some(5)).await;

    // Creating, queueing.
    let (status, b) = api.post("/v1/games", &ada, game(true)).await;
    assert_eq!(
        (status, code(&b)),
        (StatusCode::FORBIDDEN, "rated_play_suspended"),
        "{b}"
    );
    assert!(
        b["error"]["message"].as_str().unwrap().contains("UTC"),
        "{b}"
    );
    let (status, b) = api.post("/v1/matchmaking", &ada, game(true)).await;
    assert_eq!(
        (status, code(&b)),
        (StatusCode::FORBIDDEN, "rated_play_suspended"),
        "{b}"
    );

    // Joining someone else's rated game.
    let (_, g) = api.post("/v1/games", &grace, game(true)).await;
    let (status, b) = api
        .post(&format!("/v1/games/{}/join", id_of(&g)), &ada, json!({}))
        .await;
    assert_eq!(
        (status, code(&b)),
        (StatusCode::FORBIDDEN, "rated_play_suspended"),
        "{b}"
    );

    // Unrated play is untouched.
    let (status, _) = api.post("/v1/games", &ada, game(false)).await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, g) = api.post("/v1/games", &grace, game(false)).await;
    let (status, _) = api
        .post(&format!("/v1/games/{}/join", id_of(&g)), &ada, json!({}))
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn warnings_and_finished_suspensions_restrict_nothing(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, ada_id) = person(&api, "ada").await;
    let (bob, bob_id) = person(&api, "bob").await;
    sanction(&api, ada_id, "warning", None).await;
    sanction(&api, bob_id, "suspension", Some(-1)).await;
    for token in [&ada, &bob] {
        let (status, b) = api.post("/v1/games", token, game(true)).await;
        assert_eq!(status, StatusCode::CREATED, "{b}");
    }
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_player_suspended_while_queued_is_not_paired_for_rated_play(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, ada_id) = person(&api, "ada").await;
    let (grace, _) = person(&api, "grace").await;
    assert_eq!(
        api.post("/v1/matchmaking", &ada, game(true)).await.0,
        StatusCode::OK
    );
    sanction(&api, ada_id, "suspension", Some(5)).await;

    let (status, b) = api.post("/v1/matchmaking", &grace, game(true)).await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "queued", "paired with a suspended player: {b}");
}
