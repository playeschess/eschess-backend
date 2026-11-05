//! Quick pairing (`make dev`, then `make test-integration`).

mod common;

use axum::http::{Method, StatusCode};
use common::*;
use serde_json::{json, Value};
use sqlx::PgPool;

fn blitz() -> Value {
    json!({ "time_control": { "base_seconds": 300 } })
}

async fn queue(api: &Api, token: &str, body: Value) -> (StatusCode, Value) {
    api.post("/v1/matchmaking", token, body).await
}

/// Sets someone's blitz rating directly, so pairing by strength can be tested without playing games.
async fn rate(api: &Api, username: &str, rating: f64) {
    sqlx::query("INSERT INTO ratings (user_id, category, rating, deviation, volatility) SELECT id, 'blitz', $2, 60, 0.06 FROM users WHERE username = $1")
        .bind(username)
        .bind(rating)
        .execute(&api.pool)
        .await
        .unwrap();
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn two_similar_players_are_paired_and_both_find_their_game(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, grace) = (api.player("ada").await, api.player("grace").await);

    let (s, first) = queue(&api, &ada, blitz()).await;
    assert_eq!(
        (s, first["status"].as_str()),
        (StatusCode::OK, Some("queued"))
    );
    assert_eq!(api.get("/v1/matchmaking", &ada).await.1["status"], "queued");

    // The second player's request completes the pair at once.
    let (s, second) = queue(&api, &grace, blitz()).await;
    assert_eq!(
        (s, second["status"].as_str()),
        (StatusCode::OK, Some("matched"))
    );
    let game = &second["game"];
    assert_eq!(
        (game["status"].as_str(), game["rated"].as_bool()),
        (Some("active"), Some(true))
    );
    let names = [
        game["white"]["username"].as_str().unwrap(),
        game["black"]["username"].as_str().unwrap(),
    ];
    assert!(
        names.contains(&"ada") && names.contains(&"grace") && names[0] != names[1],
        "{names:?}"
    );

    // The player who was waiting finds the same game when they look.
    let (_, found) = api.get("/v1/matchmaking", &ada).await;
    assert_eq!(
        (found["status"].as_str(), &found["game"]["id"]),
        (Some("matched"), &game["id"])
    );
    // And it really is playable: whoever has White can move.
    let (white, name) = if names[0] == "ada" {
        (&ada, "ada")
    } else {
        (&grace, "grace")
    };
    let (s, _) = api
        .post(
            &format!("/v1/games/{}/moves", game["id"].as_str().unwrap()),
            white,
            json!({ "uci": "e2e4", "client_move_id": uuid::Uuid::now_v7() }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{name} has White");
    // Both are out of the waiting state: nobody is in two games.
    assert_eq!(api.count("SELECT count(*) FROM games").await, 1);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn players_who_asked_for_different_things_are_never_paired(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, grace, linus) = (
        api.player("ada").await,
        api.player("grace").await,
        api.player("linus").await,
    );
    queue(&api, &ada, blitz()).await;
    let (_, other_speed) = queue(
        &api,
        &grace,
        json!({ "time_control": { "base_seconds": 60 } }),
    )
    .await;
    assert_eq!(other_speed["status"], "queued");
    let (_, casual) = queue(
        &api,
        &linus,
        json!({ "time_control": { "base_seconds": 300 }, "rated": false }),
    )
    .await;
    assert_eq!(
        casual["status"], "queued",
        "rated and casual are separate pools"
    );
    assert_eq!(api.count("SELECT count(*) FROM games").await, 0);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn distant_players_wait_until_their_window_has_widened(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, grace) = (api.player("ada").await, api.player("grace").await);
    rate(&api, "ada", 1500.0).await;
    rate(&api, "grace", 1900.0).await;
    queue(&api, &ada, blitz()).await;
    let (_, g) = queue(&api, &grace, blitz()).await;
    assert_eq!(
        g["status"], "queued",
        "400 points apart is too far for new arrivals"
    );
    assert_eq!(
        eschess_storage::matchmaking::sweep(&api.pool, &mut || true)
            .await
            .unwrap(),
        0
    );

    // After 20 seconds both will accept up to 600 points (100 + 25 per second), so the next sweep pairs them.
    sqlx::query("UPDATE match_queue SET joined_at = joined_at - interval '20 seconds'")
        .execute(&api.pool)
        .await
        .unwrap();
    assert_eq!(
        eschess_storage::matchmaking::sweep(&api.pool, &mut || true)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        api.get("/v1/matchmaking", &ada).await.1["status"],
        "matched"
    );
    assert_eq!(
        api.get("/v1/matchmaking", &grace).await.1["status"],
        "matched"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn leaving_the_queue_and_the_edge_cases(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, grace) = (api.player("ada").await, api.player("grace").await);
    let delete = |token: String| {
        let api = &api;
        async move {
            api.call(Method::DELETE, "/v1/matchmaking", Some(&token), None)
                .await
        }
    };

    // Not queued at all.
    assert_eq!(
        api.get("/v1/matchmaking", &ada).await.0,
        StatusCode::NOT_FOUND
    );
    let (s, b) = delete(ada.clone()).await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::NOT_FOUND, Some("not_queued"))
    );

    // Queue, change your mind about the speed: still one place in the queue, now for the new speed.
    queue(&api, &ada, blitz()).await;
    queue(
        &api,
        &ada,
        json!({ "time_control": { "base_seconds": 60 } }),
    )
    .await;
    assert_eq!(api.count("SELECT count(*) FROM match_queue").await, 1);
    assert_eq!(
        api.count("SELECT count(*) FROM match_queue WHERE base_ms = 60000")
            .await,
        1
    );
    assert_eq!(delete(ada.clone()).await.0, StatusCode::NO_CONTENT);
    assert_eq!(
        api.get("/v1/matchmaking", &ada).await.0,
        StatusCode::NOT_FOUND
    );

    // Once a game has been found you cannot leave: you have a game to play.
    queue(&api, &ada, blitz()).await;
    queue(&api, &grace, blitz()).await;
    let (s, b) = delete(ada.clone()).await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("already_matched"))
    );

    // Bad requests and no sign-in.
    assert_eq!(
        queue(&api, &ada, json!({ "time_control": { "base_seconds": 0 } }))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        api.call(Method::POST, "/v1/matchmaking", None, Some(blitz()))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn someone_who_waits_too_long_is_dropped(pool: PgPool) {
    let api = Api::new(pool);
    let ada = api.player("ada").await;
    queue(&api, &ada, blitz()).await;
    sqlx::query("UPDATE match_queue SET joined_at = joined_at - interval '3 minutes'")
        .execute(&api.pool)
        .await
        .unwrap();
    assert_eq!(
        api.get("/v1/matchmaking", &ada).await.0,
        StatusCode::NOT_FOUND
    );
    eschess_storage::matchmaking::sweep(&api.pool, &mut || true)
        .await
        .unwrap();
    assert_eq!(api.count("SELECT count(*) FROM match_queue").await, 0);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_crowd_asking_at_once_is_paired_without_anyone_playing_twice(pool: PgPool) {
    let api = Api::new(pool);
    let mut tokens = Vec::new();
    for i in 0..10 {
        tokens.push(api.player(&format!("player{i}")).await);
    }
    futures::future::join_all(tokens.iter().map(|t| queue(&api, t, blitz()))).await;
    // Whatever order the requests landed in, everyone ends up in exactly one game, and five games exist.
    assert_eq!(api.count("SELECT count(*) FROM games").await, 5);
    assert_eq!(
        api.count("SELECT count(DISTINCT game_id) FROM match_queue")
            .await,
        5
    );
    assert_eq!(
        api.count("SELECT count(*) FROM match_queue WHERE game_id IS NULL")
            .await,
        0
    );
    assert_eq!(api.count("SELECT count(DISTINCT p) FROM (SELECT white_id AS p FROM games UNION ALL SELECT black_id FROM games) x").await, 10);
}
