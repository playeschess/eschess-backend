//! Playing chess through the real API against a real Postgres (`make dev`, then `make test-integration`).

mod common;

use axum::http::StatusCode;
use common::*;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

const START_FEN: &str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";

/// A fresh player pair and an active 5+3 game in which `ada` has White. Returns (api, ada, grace, game id).
async fn started(pool: PgPool) -> (Api, String, String, Uuid) {
    let api = Api::new(pool);
    let (ada, grace) = (api.player("ada").await, api.player("grace").await);
    let (_, g) = api.post("/v1/games", &ada, json!({ "time_control": { "base_seconds": 300, "increment_seconds": 3 }, "color": "white" })).await;
    let id = id_of(&g);
    let (status, joined) = api
        .post(&format!("/v1/games/{id}/join"), &grace, json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "{joined}");
    (api, ada, grace, id)
}

async fn mv(api: &Api, token: &str, game: Uuid, uci: &str) -> (StatusCode, Value) {
    api.post(
        &format!("/v1/games/{game}/moves"),
        token,
        json!({ "uci": uci, "client_move_id": Uuid::now_v7() }),
    )
    .await
}

/// Sends a move with a chosen `client_move_id` (so a test can send the same one twice).
async fn send(
    api: &Api,
    game: Uuid,
    token: &str,
    uci: &str,
    client_move_id: Uuid,
) -> (StatusCode, Value) {
    api.post(
        &format!("/v1/games/{game}/moves"),
        token,
        json!({ "uci": uci, "client_move_id": client_move_id }),
    )
    .await
}

async fn play(api: &Api, game: Uuid, ada: &str, grace: &str, moves: &str) {
    for (i, m) in moves.split_whitespace().enumerate() {
        let (status, body) = mv(api, if i % 2 == 0 { ada } else { grace }, game, m).await;
        assert_eq!(status, StatusCode::OK, "{m}: {body}");
    }
}

/// Pretend `ms` milliseconds have passed since the current turn began.
async fn age_turn(api: &Api, game: Uuid, ms: i64) {
    sqlx::query("UPDATE games SET turn_started_at_ms = turn_started_at_ms - $2 WHERE id = $1")
        .bind(game)
        .bind(ms)
        .execute(&api.pool)
        .await
        .unwrap();
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_game_is_opened_listed_joined_and_started(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, grace) = (api.player("ada").await, api.player("grace").await);

    let (status, g) = api.post("/v1/games", &ada, json!({ "time_control": { "base_seconds": 300, "increment_seconds": 3 }, "color": "white" })).await;
    assert_eq!(status, StatusCode::CREATED, "{g}");
    assert_eq!(
        (
            g["status"].as_str(),
            g["white"].is_null(),
            g["black"].is_null()
        ),
        (Some("waiting"), true, true)
    );
    assert_eq!(
        (
            g["time_control"]["base_ms"].as_i64(),
            g["time_control"]["increment_ms"].as_i64(),
            g["time_control"]["category"].as_str()
        ),
        (Some(300_000), Some(3_000), Some("blitz"))
    );
    let id = id_of(&g);

    // It shows up in other players' lobby, and in the creator's own list, but not in the creator's lobby.
    assert_eq!(
        api.get("/v1/games", &grace).await.1["games"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        api.get("/v1/games", &ada).await.1["games"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        api.get("/v1/games?scope=mine", &ada).await.1["games"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let (status, g) = api
        .post(&format!("/v1/games/{id}/join"), &grace, json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "{g}");
    assert_eq!(g["status"], "active");
    assert_eq!(
        (
            g["white"]["username"].as_str(),
            g["black"]["username"].as_str()
        ),
        (Some("ada"), Some("grace"))
    );
    assert_eq!(
        (g["fen"].as_str(), g["ply"].as_i64(), g["turn"].as_str()),
        (Some(START_FEN), Some(0), Some("white"))
    );
    // White's clock has just started running; Black's is untouched.
    let white_ms = g["clock"]["white_ms"].as_i64().unwrap();
    assert!((299_000..=300_000).contains(&white_ms), "{white_ms}");
    assert_eq!(g["clock"]["black_ms"], 300_000);
    assert!(
        g["legal_moves"].as_array().unwrap().is_empty(),
        "grace is not to move, so she is offered no moves"
    );
    let (_, g) = api.get(&format!("/v1/games/{id}"), &ada).await;
    assert_eq!(
        g["legal_moves"].as_array().unwrap().len(),
        20,
        "ada, to move, is told her 20 legal moves"
    );

    assert_eq!(
        api.get("/v1/games", &grace).await.1["games"]
            .as_array()
            .unwrap()
            .len(),
        0,
        "no longer open"
    );
    assert_eq!(
        api.get("/v1/games?scope=mine", &grace).await.1["games"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // It cannot be joined again, by the creator or by anyone else.
    let (status, body) = api
        .post(&format!("/v1/games/{id}/join"), &ada, json!({}))
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::CONFLICT, "game_not_open")
    );
    let third = api.player("third").await;
    let (status, body) = api
        .post(&format!("/v1/games/{id}/join"), &third, json!({}))
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::CONFLICT, "game_not_open")
    );
    assert_eq!(
        api.post(
            &format!("/v1/games/{}/join", Uuid::now_v7()),
            &third,
            json!({})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn you_cannot_join_your_own_game_and_colours_follow_the_creators_choice(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, grace) = (api.player("ada").await, api.player("grace").await);
    let (_, g) = api
        .post(
            "/v1/games",
            &ada,
            json!({ "time_control": { "base_seconds": 60 }, "color": "black" }),
        )
        .await;
    let id = id_of(&g);
    let (status, body) = api
        .post(&format!("/v1/games/{id}/join"), &ada, json!({}))
        .await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, "own_game"));
    let (_, joined) = api
        .post(&format!("/v1/games/{id}/join"), &grace, json!({}))
        .await;
    assert_eq!(
        (
            joined["black"]["username"].as_str(),
            joined["white"]["username"].as_str()
        ),
        (Some("ada"), Some("grace"))
    );

    // A random colour is decided by the server when the opponent joins: always two different colours, and over many
    // games both outcomes happen.
    let mut ada_white = 0;
    for _ in 0..40 {
        let (_, g) = api
            .post(
                "/v1/games",
                &ada,
                json!({ "time_control": { "base_seconds": 60 } }),
            )
            .await;
        let (_, j) = api
            .post(&format!("/v1/games/{}/join", id_of(&g)), &grace, json!({}))
            .await;
        assert_ne!(j["white"]["id"], j["black"]["id"]);
        ada_white += (j["white"]["username"] == "ada") as u32;
    }
    assert!(
        (1..40).contains(&ada_white),
        "a fair coin gave ada White in {ada_white} of 40"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn bad_requests_to_open_a_game_are_refused(pool: PgPool) {
    let api = Api::new(pool);
    let ada = api.player("ada").await;
    for (body, why) in [
        (json!({ "time_control": { "base_seconds": 0 } }), "no time"),
        (
            json!({ "time_control": { "base_seconds": 99_999 } }),
            "more than 3 hours",
        ),
        (
            json!({ "time_control": { "base_seconds": 60, "increment_seconds": 999 } }),
            "increment over 3 minutes",
        ),
        (
            json!({ "time_control": { "base_seconds": u64::MAX } }),
            "an absurd number",
        ),
        (
            json!({ "time_control": { "base_seconds": 60 }, "color": "purple" }),
            "unknown colour",
        ),
    ] {
        let (status, resp) = api.post("/v1/games", &ada, body).await;
        assert_eq!(
            (status, code(&resp)),
            (StatusCode::BAD_REQUEST, "validation_failed"),
            "{why}"
        );
    }
    assert_eq!(
        api.post("/v1/games", &ada, json!({})).await.0,
        StatusCode::UNPROCESSABLE_ENTITY,
        "missing time control"
    );
    assert_eq!(
        api.call(
            axum::http::Method::POST,
            "/v1/games",
            None,
            Some(json!({ "time_control": { "base_seconds": 60 } }))
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(api.count("SELECT count(*) FROM games").await, 0);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn moves_are_played_in_turn_and_the_clock_follows(pool: PgPool) {
    let (api, ada, grace, id) = started(pool).await;
    let (status, r) = mv(&api, &ada, id, "e2e4").await;
    assert_eq!(status, StatusCode::OK, "{r}");
    assert_eq!(
        r["game"]["fen"],
        "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq e3 0 1"
    );
    assert_eq!(
        (r["game"]["ply"].as_i64(), r["game"]["turn"].as_str()),
        (Some(1), Some("black"))
    );
    assert_eq!(
        (
            r["move"]["ply"].as_i64(),
            r["move"]["uci"].as_str(),
            r["move"]["replayed"].as_bool()
        ),
        (Some(1), Some("e2e4"), Some(false))
    );
    // White paid almost nothing for a near-instant move and earned the 3 s increment.
    let white_ms = r["game"]["clock"]["white_ms"].as_i64().unwrap();
    assert!((300_000..=303_000).contains(&white_ms), "{white_ms}");
    // Black's clock started running when White moved: a few milliseconds ticked, and nothing has been charged yet.
    let black_ms = r["game"]["clock"]["black_ms"].as_i64().unwrap();
    assert!((299_000..=300_000).contains(&black_ms), "{black_ms}");
    // Black, now to move, is offered Black's moves.
    let (_, g) = api.get(&format!("/v1/games/{id}"), &grace).await;
    assert_eq!(g["legal_moves"].as_array().unwrap().len(), 20);

    let (status, r) = mv(&api, &grace, id, "e7e5").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        (r["game"]["ply"].as_i64(), r["game"]["moves"].to_string()),
        (Some(2), json!(["e2e4", "e7e5"]).to_string())
    );
    assert_eq!(api.count("SELECT count(*) FROM game_moves").await, 2);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn only_the_player_whose_turn_it_is_can_move_and_strangers_cannot_even_see_the_game(
    pool: PgPool,
) {
    let (api, ada, grace, id) = started(pool).await;
    let stranger = api.player("stranger").await;

    let (status, body) = mv(&api, &grace, id, "e7e5").await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::CONFLICT, "not_your_turn")
    );
    assert_eq!(
        mv(&api, &stranger, id, "e2e4").await.0,
        StatusCode::NOT_FOUND,
        "a stranger is told there is no such game"
    );
    assert_eq!(
        api.get(&format!("/v1/games/{id}"), &stranger).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        api.post(&format!("/v1/games/{id}/resign"), &stranger, json!({}))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        api.call(
            axum::http::Method::POST,
            &format!("/v1/games/{id}/moves"),
            None,
            Some(json!({ "uci": "e2e4", "client_move_id": Uuid::now_v7() }))
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    // The server never believes a colour, a position or a turn from the request: extra fields are simply ignored.
    let (status, r) = api.post(&format!("/v1/games/{id}/moves"), &grace, json!({ "uci": "e7e5", "client_move_id": Uuid::now_v7(), "turn": "black", "fen": "x", "player": "grace" })).await;
    assert_eq!((status, code(&r)), (StatusCode::CONFLICT, "not_your_turn"));
    assert_eq!(api.count("SELECT count(*) FROM game_moves").await, 0);
    assert_eq!(mv(&api, &ada, id, "e2e4").await.0, StatusCode::OK);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_mistake_changes_nothing_and_costs_no_time(pool: PgPool) {
    let (api, ada, _grace, id) = started(pool).await;
    age_turn(&api, id, 4_000).await;
    for (uci, want_status, want_code) in [
        ("e2e5", StatusCode::UNPROCESSABLE_ENTITY, "illegal_move"),
        ("e7e5", StatusCode::UNPROCESSABLE_ENTITY, "illegal_move"),
        ("hello", StatusCode::BAD_REQUEST, "invalid_move_notation"),
        ("e2e9", StatusCode::BAD_REQUEST, "invalid_move_notation"),
        ("e1g1", StatusCode::UNPROCESSABLE_ENTITY, "illegal_move"),
    ] {
        let (status, body) = mv(&api, &ada, id, uci).await;
        assert_eq!((status, code(&body)), (want_status, want_code), "{uci}");
    }
    let (_, g) = api.get(&format!("/v1/games/{id}"), &ada).await;
    assert_eq!(
        (g["ply"].as_i64(), g["fen"].as_str()),
        (Some(0), Some(START_FEN))
    );
    // The four seconds that passed are still ticking, not charged for the mistakes: the real move is charged once.
    let (status, r) = mv(&api, &ada, id, "e2e4").await;
    assert_eq!(status, StatusCode::OK);
    let charged = r["move"]["charged_ms"].as_i64().unwrap();
    assert!((4_000..6_000).contains(&charged), "charged {charged}");
    assert_eq!(api.count("SELECT count(*) FROM game_moves").await, 1);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_retried_move_returns_the_original_outcome_and_is_never_played_twice(pool: PgPool) {
    let (api, ada, grace, id) = started(pool).await;
    let first_id = Uuid::now_v7();
    let (status, first) = send(&api, id, &ada, "e2e4", first_id).await;
    assert_eq!(
        (status, first["move"]["replayed"].as_bool()),
        (StatusCode::OK, Some(false))
    );

    // The app never saw the answer and sends it again: same outcome, nothing new happens.
    let (status, again) = send(&api, id, &ada, "e2e4", first_id).await;
    assert_eq!(
        (
            status,
            again["move"]["replayed"].as_bool(),
            again["game"]["ply"].as_i64()
        ),
        (StatusCode::OK, Some(true), Some(1))
    );
    assert_eq!(again["move"]["ply"], first["move"]["ply"]);

    // Even after the opponent has replied, the retry still gets the original answer and the game is untouched.
    assert_eq!(
        send(&api, id, &grace, "e7e5", Uuid::now_v7()).await.0,
        StatusCode::OK
    );
    let (status, late) = send(&api, id, &ada, "e2e4", first_id).await;
    assert_eq!(
        (
            status,
            late["move"]["replayed"].as_bool(),
            late["move"]["ply"].as_i64(),
            late["game"]["ply"].as_i64()
        ),
        (StatusCode::OK, Some(true), Some(1), Some(2))
    );

    // The same id cannot be reused for a different move, or by the other player.
    let (status, body) = send(&api, id, &ada, "d2d4", first_id).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::CONFLICT, "client_move_id_reused")
    );
    let (status, body) = send(&api, id, &grace, "e2e4", first_id).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::CONFLICT, "client_move_id_reused")
    );
    assert_eq!(api.count("SELECT count(*) FROM game_moves").await, 2);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn ten_simultaneous_copies_of_one_move_play_it_once(pool: PgPool) {
    let (api, ada, _grace, id) = started(pool).await;
    let cid = Uuid::now_v7();
    let path = format!("/v1/games/{id}/moves");
    let body = json!({ "uci": "e2e4", "client_move_id": cid });
    let results =
        futures::future::join_all((0..10).map(|_| api.post(&path, &ada, body.clone()))).await;
    assert!(
        results.iter().all(|(s, _)| *s == StatusCode::OK),
        "{:?}",
        results
            .iter()
            .map(|(s, b)| (s.as_u16(), code(b).to_string()))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        results
            .iter()
            .filter(|(_, b)| b["move"]["replayed"] == false)
            .count(),
        1,
        "exactly one actually played the move"
    );
    assert_eq!(api.count("SELECT count(*) FROM game_moves").await, 1);
    assert_eq!(api.count("SELECT ply::bigint FROM games").await, 1);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn two_different_moves_sent_at_once_by_one_player_cannot_both_be_played(pool: PgPool) {
    let (api, ada, _grace, id) = started(pool).await;
    let path = format!("/v1/games/{id}/moves");
    let moves = ["e2e4", "d2d4", "g1f3", "c2c4", "b1c3", "a2a3"];
    let results = futures::future::join_all(moves.iter().map(|m| {
        api.post(
            &path,
            &ada,
            json!({ "uci": m, "client_move_id": Uuid::now_v7() }),
        )
    }))
    .await;
    assert_eq!(
        results.iter().filter(|(s, _)| *s == StatusCode::OK).count(),
        1,
        "one move wins the race"
    );
    assert_eq!(
        results
            .iter()
            .filter(|(s, b)| *s == StatusCode::CONFLICT && code(b) == "not_your_turn")
            .count(),
        5
    );
    assert_eq!(api.count("SELECT count(*) FROM game_moves").await, 1);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn checkmate_ends_the_game_and_nothing_more_can_be_played(pool: PgPool) {
    let (api, ada, grace, id) = started(pool).await;
    play(&api, id, &ada, &grace, "f2f3 e7e5 g2g4").await;
    let (status, r) = mv(&api, &grace, id, "d8h4").await;
    assert_eq!(status, StatusCode::OK, "{r}");
    assert_eq!(
        (
            r["game"]["status"].as_str(),
            r["game"]["result"].as_str(),
            r["game"]["end_reason"].as_str()
        ),
        (Some("finished"), Some("black"), Some("checkmate"))
    );
    assert!(r["game"]["turn"].is_null() && r["game"]["legal_moves"].as_array().unwrap().is_empty());

    let (status, body) = mv(&api, &ada, id, "a2a3").await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, "game_over"));
    assert_eq!(
        api.post(&format!("/v1/games/{id}/resign"), &ada, json!({}))
            .await
            .0,
        StatusCode::CONFLICT
    );
    // It moves from "my games" to "finished", for both players.
    for token in [&ada, &grace] {
        assert_eq!(
            api.get("/v1/games?scope=mine", token).await.1["games"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            api.get("/v1/games?scope=finished", token).await.1["games"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_player_can_resign_on_either_turn_and_only_once(pool: PgPool) {
    let (api, ada, grace, id) = started(pool).await;
    let stranger = api.player("stranger").await;
    assert_eq!(
        api.post(&format!("/v1/games/{id}/resign"), &stranger, json!({}))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    // It is Ada's turn, but Grace may resign all the same.
    let (status, g) = api
        .post(&format!("/v1/games/{id}/resign"), &grace, json!({}))
        .await;
    assert_eq!(status, StatusCode::OK, "{g}");
    assert_eq!(
        (
            g["status"].as_str(),
            g["result"].as_str(),
            g["end_reason"].as_str()
        ),
        (Some("finished"), Some("white"), Some("resignation"))
    );
    assert_eq!(
        code(
            &api.post(&format!("/v1/games/{id}/resign"), &ada, json!({}))
                .await
                .1
        ),
        "game_over"
    );
    assert_eq!(code(&mv(&api, &ada, id, "e2e4").await.1), "game_over");
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_draw_can_be_claimed_by_repetition_but_not_before(pool: PgPool) {
    let (api, ada, grace, id) = started(pool).await;
    let claim = |token: String| {
        let path = format!("/v1/games/{id}/claim-draw");
        let api = &api;
        async move { api.post(&path, &token, json!({})).await }
    };
    let (status, body) = claim(ada.clone()).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::CONFLICT, "nothing_to_claim")
    );
    play(
        &api,
        id,
        &ada,
        &grace,
        "g1f3 g8f6 f3g1 f6g8 g1f3 g8f6 f3g1 f6g8",
    )
    .await;
    let (status, body) = claim(grace.clone()).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::OK.max(StatusCode::CONFLICT), "not_your_turn"),
        "only the player to move may claim"
    );
    let (status, g) = claim(ada.clone()).await;
    assert_eq!(status, StatusCode::OK, "{g}");
    assert_eq!(
        (g["result"].as_str(), g["end_reason"].as_str()),
        (Some("draw"), Some("threefold_repetition"))
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn the_clock_runs_down_for_the_player_to_move_and_a_fallen_flag_ends_the_game(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, grace) = (api.player("ada").await, api.player("grace").await);
    let (_, g) = api
        .post(
            "/v1/games",
            &ada,
            json!({ "time_control": { "base_seconds": 10 }, "color": "white" }),
        )
        .await;
    let id = id_of(&g);
    api.post(&format!("/v1/games/{id}/join"), &grace, json!({}))
        .await;

    // Three seconds in, White has about 7 s left and Black still has all 10.
    age_turn(&api, id, 3_000).await;
    let (_, g) = api.get(&format!("/v1/games/{id}"), &grace).await;
    let white_ms = g["clock"]["white_ms"].as_i64().unwrap();
    assert!((6_000..=7_000).contains(&white_ms), "{white_ms}");
    assert_eq!(g["clock"]["black_ms"], 10_000);
    assert_eq!(g["status"], "active");

    // Well past the flag: the next look at the game, by anyone, finds it over, and White (to move) lost on time.
    age_turn(&api, id, 30_000).await;
    let (_, g) = api.get(&format!("/v1/games/{id}"), &grace).await;
    assert_eq!(
        (
            g["status"].as_str(),
            g["result"].as_str(),
            g["end_reason"].as_str()
        ),
        (Some("finished"), Some("black"), Some("timeout"))
    );
    assert_eq!(
        code(&mv(&api, &ada, id, "e2e4").await.1),
        "game_over",
        "a move after the flag is too late"
    );
    assert_eq!(api.count("SELECT count(*) FROM game_moves").await, 0);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_move_that_arrives_after_time_ran_out_does_not_count(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, grace) = (api.player("ada").await, api.player("grace").await);
    let (_, g) = api
        .post(
            "/v1/games",
            &ada,
            json!({ "time_control": { "base_seconds": 10 }, "color": "white" }),
        )
        .await;
    let id = id_of(&g);
    api.post(&format!("/v1/games/{id}/join"), &grace, json!({}))
        .await;
    age_turn(&api, id, 60_000).await;
    let (status, body) = mv(&api, &ada, id, "e2e4").await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, "game_over"));
    let (_, g) = api.get(&format!("/v1/games/{id}"), &ada).await;
    assert_eq!(
        (
            g["end_reason"].as_str(),
            g["result"].as_str(),
            g["ply"].as_i64()
        ),
        (Some("timeout"), Some("black"), Some(0)),
        "the late move was not played"
    );
    // Listing a player's games also settles fallen flags.
    assert_eq!(
        api.get("/v1/games?scope=mine", &ada).await.1["games"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn who_can_see_what(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, grace, stranger) = (
        api.player("ada").await,
        api.player("grace").await,
        api.player("stranger").await,
    );
    let (_, g) = api
        .post(
            "/v1/games",
            &ada,
            json!({ "time_control": { "base_seconds": 60 } }),
        )
        .await;
    let id = id_of(&g);
    // Waiting games are public (they are in the lobby), but nobody can move in one before an opponent joins.
    assert_eq!(
        api.get(&format!("/v1/games/{id}"), &stranger).await.0,
        StatusCode::OK
    );
    let (status, body) = mv(&api, &ada, id, "e2e4").await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::CONFLICT, "game_not_started")
    );
    assert_eq!(
        mv(&api, &stranger, id, "e2e4").await.0,
        StatusCode::NOT_FOUND
    );

    // Only the creator can withdraw it, and a withdrawn game is the creator's business alone.
    assert_eq!(
        api.post(&format!("/v1/games/{id}/cancel"), &stranger, json!({}))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        api.post(&format!("/v1/games/{id}/cancel"), &ada, json!({}))
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        api.get(&format!("/v1/games/{id}"), &ada).await.1["status"],
        "cancelled"
    );
    assert_eq!(
        api.get(&format!("/v1/games/{id}"), &stranger).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        code(
            &api.post(&format!("/v1/games/{id}/join"), &grace, json!({}))
                .await
                .1
        ),
        "game_not_open"
    );

    // A game in progress is visible to its two players only, and cannot be cancelled.
    let (_, g) = api
        .post(
            "/v1/games",
            &ada,
            json!({ "time_control": { "base_seconds": 60 } }),
        )
        .await;
    let live = id_of(&g);
    api.post(&format!("/v1/games/{live}/join"), &grace, json!({}))
        .await;
    assert_eq!(
        api.get(&format!("/v1/games/{live}"), &stranger).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        api.get("/v1/games?scope=mine", &stranger).await.1["games"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        code(
            &api.post(&format!("/v1/games/{live}/cancel"), &ada, json!({}))
                .await
                .1
        ),
        "game_not_open"
    );
    assert_eq!(
        api.get("/v1/games?scope=nonsense", &ada).await.0,
        StatusCode::BAD_REQUEST
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn any_server_can_carry_on_a_game_because_the_state_is_in_the_database(pool: PgPool) {
    let (api, ada, grace, id) = started(pool).await;
    play(&api, id, &ada, &grace, "e2e4 e7e5").await;
    // A different server process (as after a restart or on another machine) continues the same game.
    let other = api.another_server();
    let (status, r) = mv(&other, &ada, id, "g1f3").await;
    assert_eq!(status, StatusCode::OK, "{r}");
    assert_eq!(r["game"]["ply"], 3);
    let (_, g) = other.get("/v1/games?scope=mine", &grace).await;
    assert_eq!(
        g["games"][0]["moves"],
        json!(["e2e4", "e7e5", "g1f3"]),
        "a player who reconnects finds the game exactly as it is"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn the_database_itself_protects_the_history_and_the_shape_of_games(pool: PgPool) {
    let (api, ada, grace, id) = started(pool).await;
    play(&api, id, &ada, &grace, "e2e4").await;
    assert!(
        sqlx::query("UPDATE game_moves SET uci = 'a2a3'")
            .execute(&api.pool)
            .await
            .is_err(),
        "moves cannot be rewritten"
    );
    assert!(
        sqlx::query("DELETE FROM game_moves")
            .execute(&api.pool)
            .await
            .is_err(),
        "or deleted"
    );
    assert!(
        sqlx::query("UPDATE games SET black_id = white_id WHERE id = $1")
            .bind(id)
            .execute(&api.pool)
            .await
            .is_err(),
        "a player cannot play themselves"
    );
    assert!(
        sqlx::query("UPDATE games SET status = 'finished' WHERE id = $1")
            .bind(id)
            .execute(&api.pool)
            .await
            .is_err(),
        "finished needs a result"
    );
    assert!(
        sqlx::query("UPDATE games SET result = 'white' WHERE id = $1")
            .bind(id)
            .execute(&api.pool)
            .await
            .is_err(),
        "a result needs a finished game"
    );
    assert_eq!(api.count("SELECT count(*) FROM game_moves").await, 1);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_player_can_have_only_three_open_games_even_when_asking_at_once(pool: PgPool) {
    let api = Api::new(pool);
    let ada = api.player("ada").await;
    let grace = api.player("grace").await;
    let body = json!({ "time_control": { "base_seconds": 60 } });

    // Ten requests at the same moment: exactly three get through.
    let attempts =
        futures::future::join_all((0..10).map(|_| api.post("/v1/games", &ada, body.clone()))).await;
    let created = attempts
        .iter()
        .filter(|(s, _)| *s == StatusCode::CREATED)
        .count();
    assert_eq!(created, 3);
    for (s, b) in attempts.iter().filter(|(s, _)| *s != StatusCode::CREATED) {
        assert_eq!(
            (*s, b["error"]["code"].as_str()),
            (StatusCode::CONFLICT, Some("too_many_open_games"))
        );
    }
    // It is a per-player limit, not a global one.
    assert_eq!(
        api.post("/v1/games", &grace, body.clone()).await.0,
        StatusCode::CREATED
    );

    // Cancelling one frees a place; a game that has started no longer counts as open.
    let (_, mine) = api.get("/v1/games?scope=mine", &ada).await;
    let first = id_of(&mine["games"][0]);
    assert_eq!(
        api.post(&format!("/v1/games/{first}/cancel"), &ada, json!({}))
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    let (s, g) = api.post("/v1/games", &ada, body.clone()).await;
    assert_eq!(s, StatusCode::CREATED);
    api.post(&format!("/v1/games/{}/join", id_of(&g)), &grace, json!({}))
        .await;
    assert_eq!(
        api.post("/v1/games", &ada, body).await.0,
        StatusCode::CREATED
    );
}
