//! Ratings: how finished games move them, and how they are shown (`make dev`, then `make test-integration`).

mod common;

use axum::http::StatusCode;
use common::*;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

/// A started game between two players, `base` seconds each, with the first player as White.
async fn start(api: &Api, white: &str, black: &str, base: u64) -> Uuid {
    let (_, g) = api
        .post(
            "/v1/games",
            white,
            json!({ "time_control": { "base_seconds": base }, "color": "white" }),
        )
        .await;
    let id = id_of(&g);
    assert_eq!(
        api.post(&format!("/v1/games/{id}/join"), black, json!({}))
            .await
            .0,
        StatusCode::OK
    );
    id
}

async fn play(api: &Api, game: Uuid, who: &str, uci: &str) {
    let (s, b) = api
        .post(
            &format!("/v1/games/{game}/moves"),
            who,
            json!({ "uci": uci, "client_move_id": Uuid::now_v7() }),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{uci}: {b}");
}

/// Fool's mate: Black wins in four plies, the shortest game that is rated.
async fn foolsmate(api: &Api, game: Uuid, white: &str, black: &str) {
    for (who, uci) in [
        (white, "f2f3"),
        (black, "e7e5"),
        (white, "g2g4"),
        (black, "d8h4"),
    ] {
        play(api, game, who, uci).await;
    }
}

async fn rating_of(api: &Api, token: &str, category: &str) -> Option<Value> {
    let (_, me) = api.get("/v1/me", token).await;
    me["ratings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["category"] == category)
        .cloned()
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_finished_game_moves_both_ratings_and_is_recorded_once(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, grace) = (api.player("ada").await, api.player("grace").await);
    assert!(
        rating_of(&api, &ada, "blitz").await.is_none(),
        "no rating until a rated game is played"
    );

    let id = start(&api, &ada, &grace, 300).await;
    foolsmate(&api, id, &ada, &grace).await;

    let (a, g) = (
        rating_of(&api, &ada, "blitz").await.unwrap(),
        rating_of(&api, &grace, "blitz").await.unwrap(),
    );
    assert!(
        a["rating"].as_i64().unwrap() < 1500 && g["rating"].as_i64().unwrap() > 1500,
        "{a} {g}"
    );
    assert_eq!(
        a["rating"].as_i64().unwrap() + g["rating"].as_i64().unwrap(),
        3000,
        "two equal newcomers: what one wins the other loses"
    );
    assert_eq!(
        (a["games_played"].as_i64(), a["provisional"].as_bool()),
        (Some(1), Some(true))
    );
    assert!(a["deviation"].as_i64().unwrap() < 350);

    // One history row each, tying the numbers to the game.
    assert_eq!(api.count("SELECT count(*) FROM rating_history").await, 2);
    // Asking for the game again changes nothing.
    api.get(&format!("/v1/games/{id}"), &ada).await;
    assert_eq!(api.count("SELECT count(*) FROM rating_history").await, 2);
    // History cannot be rewritten.
    assert!(sqlx::query("UPDATE rating_history SET rating_after = 9999")
        .execute(&api.pool)
        .await
        .is_err());
    assert!(sqlx::query("DELETE FROM rating_history")
        .execute(&api.pool)
        .await
        .is_err());
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_game_abandoned_early_does_not_count(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, grace) = (api.player("ada").await, api.player("grace").await);
    let id = start(&api, &ada, &grace, 300).await;
    play(&api, id, &ada, "e2e4").await;
    assert_eq!(
        api.post(&format!("/v1/games/{id}/resign"), &grace, json!({}))
            .await
            .0,
        StatusCode::OK
    );
    // Finished, but only one ply was played: no rating is created or changed, and nothing is recorded.
    assert!(rating_of(&api, &ada, "blitz").await.is_none());
    assert_eq!(api.count("SELECT count(*) FROM ratings").await, 0);
    assert_eq!(api.count("SELECT count(*) FROM rating_history").await, 0);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_resignation_after_enough_play_counts(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, grace) = (api.player("ada").await, api.player("grace").await);
    let id = start(&api, &ada, &grace, 300).await;
    for (who, uci) in [
        (&ada, "e2e4"),
        (&grace, "e7e5"),
        (&ada, "g1f3"),
        (&grace, "b8c6"),
    ] {
        play(&api, id, who, uci).await;
    }
    api.post(&format!("/v1/games/{id}/resign"), &ada, json!({}))
        .await;
    assert!(
        rating_of(&api, &ada, "blitz").await.unwrap()["rating"]
            .as_i64()
            .unwrap()
            < 1500
    );
    assert!(
        rating_of(&api, &grace, "blitz").await.unwrap()["rating"]
            .as_i64()
            .unwrap()
            > 1500
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn each_speed_class_has_its_own_rating(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, grace) = (api.player("ada").await, api.player("grace").await);
    let id = start(&api, &ada, &grace, 60).await; // 1+0 is bullet
    foolsmate(&api, id, &ada, &grace).await;
    assert!(rating_of(&api, &ada, "bullet").await.is_some());
    assert!(
        rating_of(&api, &ada, "blitz").await.is_none(),
        "a bullet game does not touch blitz"
    );

    let id = start(&api, &ada, &grace, 600).await; // 10+0 is rapid
    foolsmate(&api, id, &ada, &grace).await;
    assert!(rating_of(&api, &ada, "rapid").await.is_some());
    assert_eq!(api.count("SELECT count(*) FROM ratings").await, 4);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_flag_fall_is_rated_like_any_other_result(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, grace) = (api.player("ada").await, api.player("grace").await);
    let id = start(&api, &ada, &grace, 300).await;
    for (who, uci) in [
        (&ada, "e2e4"),
        (&grace, "e7e5"),
        (&ada, "g1f3"),
        (&grace, "b8c6"),
    ] {
        play(&api, id, who, uci).await;
    }
    // White's clock runs out; the next request that touches the game notices.
    sqlx::query("UPDATE games SET turn_started_at_ms = turn_started_at_ms - 900000 WHERE id = $1")
        .bind(id)
        .execute(&api.pool)
        .await
        .unwrap();
    let (_, g) = api.get(&format!("/v1/games/{id}"), &grace).await;
    assert_eq!(g["end_reason"], "timeout");
    assert!(
        rating_of(&api, &grace, "blitz").await.unwrap()["rating"]
            .as_i64()
            .unwrap()
            > 1500
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn the_leaderboard_lists_only_established_players_best_first(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, grace, linus) = (
        api.player("ada").await,
        api.player("grace").await,
        api.player("linus").await,
    );
    let id = start(&api, &ada, &grace, 300).await;
    foolsmate(&api, id, &ada, &grace).await;

    // Everyone is still a newcomer, so nobody is listed.
    let (s, board) = api.get("/v1/leaderboard?category=blitz", &linus).await;
    assert_eq!(s, StatusCode::OK);
    assert!(board["players"].as_array().unwrap().is_empty());

    // Pretend both have played a lot (a low deviation); now they appear, the higher rating first.
    sqlx::query("UPDATE ratings SET deviation = 60")
        .execute(&api.pool)
        .await
        .unwrap();
    let (_, board) = api.get("/v1/leaderboard?category=blitz", &linus).await;
    let names: Vec<&str> = board["players"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["username"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["grace", "ada"]);
    assert_eq!(board["players"][0]["rank"], 1);
    assert!(
        api.get("/v1/leaderboard?category=rapid", &linus).await.1["players"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    assert_eq!(
        api.get("/v1/leaderboard?category=nope", &linus).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        api.call(axum::http::Method::GET, "/v1/leaderboard", None, None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn many_games_finishing_at_once_never_deadlock_or_lose_a_rating_update(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, grace) = (api.player("ada").await, api.player("grace").await);
    // Three games between the same pair, played to the end together, with the colours swapped in some of them.
    let mut games = Vec::new();
    for i in 0..3 {
        let (w, b) = if i % 2 == 0 {
            (&ada, &grace)
        } else {
            (&grace, &ada)
        };
        games.push((start(&api, w, b, 300).await, w.clone(), b.clone()));
    }
    futures::future::join_all(games.iter().map(|(id, w, b)| foolsmate(&api, *id, w, b))).await;
    assert_eq!(api.count("SELECT count(*) FROM rating_history").await, 6);
    assert_eq!(
        api.count("SELECT sum(games_played)::bigint FROM ratings")
            .await,
        6
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_casual_game_never_touches_ratings_and_rated_is_the_default(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, grace) = (api.player("ada").await, api.player("grace").await);

    // Not saying anything means rated; the choice is shown to everyone who sees the game, lobby included.
    let (_, g) = api
        .post(
            "/v1/games",
            &ada,
            json!({ "time_control": { "base_seconds": 300 } }),
        )
        .await;
    assert_eq!(g["rated"], true);
    let (_, g) = api
        .post(
            "/v1/games",
            &ada,
            json!({ "time_control": { "base_seconds": 300 }, "rated": false, "color": "white" }),
        )
        .await;
    assert_eq!(g["rated"], false);
    let casual = id_of(&g);
    let lobby = api.get("/v1/games", &grace).await.1;
    let flags: Vec<bool> = lobby["games"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["rated"].as_bool().unwrap())
        .collect();
    assert!(flags.contains(&true) && flags.contains(&false), "{flags:?}");
    let bad = api
        .post(
            "/v1/games",
            &ada,
            json!({ "time_control": { "base_seconds": 300 }, "rated": "yes" }),
        )
        .await
        .0;
    assert!(bad.is_client_error(), "{bad}");

    // A whole casual game, to checkmate: it is finished and recorded, but nobody's rating exists or changes.
    api.post(&format!("/v1/games/{casual}/join"), &grace, json!({}))
        .await;
    foolsmate(&api, casual, &ada, &grace).await;
    let (_, done) = api.get(&format!("/v1/games/{casual}"), &ada).await;
    assert_eq!(
        (done["status"].as_str(), done["rated"].as_bool()),
        (Some("finished"), Some(false))
    );
    assert_eq!(api.count("SELECT count(*) FROM ratings").await, 0);
    assert_eq!(api.count("SELECT count(*) FROM rating_history").await, 0);
    assert!(rating_of(&api, &grace, "blitz").await.is_none());
}
