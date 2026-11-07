//! Account closure (ADR 0003 §3): proposed by confirming a case, effective only when a second reviewer approves, and
//! then the player can use their wallet and nothing else.

mod common;

use axum::http::StatusCode;
use common::*;
use eschess_fairplay::RiskScore;
use eschess_storage::fairplay::{game_record, open_case, record_scores};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

/// Signs a player up, confirms their email, and returns their token and id.
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

async fn make_reviewer(api: &Api, id: Uuid) {
    sqlx::query("INSERT INTO user_roles (user_id, role) VALUES ($1, 'fairplay_reviewer')")
        .bind(id)
        .execute(&api.pool)
        .await
        .unwrap();
}

/// A finished game: `white` plays e2e4, `black` e7e5 (over HTTP), then White resigns.
async fn finished_game(api: &Api, white: &str, black: &str) -> Uuid {
    let (_, g) = api
        .post(
            "/v1/games",
            white,
            json!({ "time_control": { "base_seconds": 180, "increment_seconds": 2 }, "color": "white" }),
        )
        .await;
    let id = id_of(&g);
    assert_eq!(
        api.post(&format!("/v1/games/{id}/join"), black, json!({}))
            .await
            .0,
        StatusCode::OK
    );
    for (who, uci) in [(white, "e2e4"), (black, "e7e5")] {
        let (status, body) = api
            .post(
                &format!("/v1/games/{id}/moves"),
                who,
                json!({ "uci": uci, "client_move_id": Uuid::now_v7() }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    assert_eq!(
        api.post(&format!("/v1/games/{id}/resign"), white, json!({}))
            .await
            .0,
        StatusCode::OK
    );
    id
}

/// Scores `suspect` at `score` in the game, as a scorer run by the worker would; returns their open case's id.
async fn flag(api: &Api, game: Uuid, suspect: Uuid, score: u8) -> Uuid {
    let record = game_record(&api.pool, game).await.unwrap().unwrap();
    let scores = [RiskScore {
        player_id: suspect,
        score,
        scorer: "test-1".into(),
        summary: "moves matched the engine's first choice unusually often".into(),
        evidence: json!({ "matched": 2, "of": 2 }),
    }];
    record_scores(&api.pool, &record, "test-1", &scores, 70)
        .await
        .unwrap();
    open_case(&api.pool, suspect).await.unwrap().unwrap().id
}

async fn reviewer(api: &Api, name: &str) -> (String, Uuid) {
    let (t, id) = person(api, name).await;
    make_reviewer(api, id).await;
    (t, id)
}

fn game_body() -> Value {
    json!({ "time_control": { "base_seconds": 180, "increment_seconds": 2 }, "rated": false })
}

/// A case about ada, confirmed with a closure by `proposer`.
async fn proposed(api: &Api, ada: &str, ada_id: Uuid, grace: &str, proposer: &str) -> Uuid {
    let game = finished_game(api, ada, grace).await;
    let case = flag(api, game, ada_id, 95).await;
    let (status, b) = api
        .post(
            &format!("/v1/admin/fairplay/cases/{case}/confirm"),
            proposer,
            json!({ "reason": "engine-like play", "games": [game], "sanction": { "kind": "closure" } }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");
    case
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_closure_needs_a_second_reviewer_and_then_leaves_only_the_wallet(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, ada_id) = person(&api, "ada").await;
    let (grace, _) = person(&api, "grace").await;
    let (hopper, hopper_id) = person(&api, "hopper").await;
    let (r1, _) = reviewer(&api, "rone").await;
    let (r2, _) = reviewer(&api, "rtwo").await;
    let case = proposed(&api, &ada, ada_id, &grace, &r1).await;

    // Proposed is not closed: ada plays on, and has a live game and a waiting one.
    let (status, live) = api.post("/v1/games", &ada, game_body()).await;
    assert_eq!(status, StatusCode::CREATED);
    let live = id_of(&live);
    assert_eq!(
        api.post(&format!("/v1/games/{live}/join"), &hopper, json!({}))
            .await
            .0,
        StatusCode::OK
    );
    let (_, waiting) = api.post("/v1/games", &ada, game_body()).await;
    let waiting = id_of(&waiting);

    // Not by the proposer, the player, a bystander, or a player in the case.
    let approve = format!("/v1/admin/fairplay/cases/{case}/closure/approve");
    let body = json!({ "reason": "agreed" });
    for who in [&r1, &ada, &grace] {
        assert_eq!(
            api.post(&approve, who, body.clone()).await.0,
            StatusCode::NOT_FOUND
        );
    }
    let (_, list) = api.get("/v1/admin/fairplay/closures", &r2).await;
    assert_eq!(list["closures"][0]["case_id"], json!(case));

    let (status, b) = api.post(&approve, &r2, body.clone()).await;
    assert_eq!(status, StatusCode::OK, "{b}");
    // Once only.
    assert_eq!(api.post(&approve, &r2, body).await.0, StatusCode::NOT_FOUND);

    // Closed: no play, in any form...
    for (path, b) in [("/v1/games", game_body()), ("/v1/matchmaking", game_body())] {
        let (status, b) = api.post(path, &ada, b).await;
        assert_eq!(
            (status, code(&b)),
            (StatusCode::FORBIDDEN, "account_closed"),
            "{b}"
        );
    }
    assert_eq!(api.get("/v1/games", &ada).await.0, StatusCode::FORBIDDEN);
    // ...but the wallet and their own record are open.
    assert_eq!(api.get("/v1/me", &ada).await.0, StatusCode::OK);
    assert_ne!(
        code(&api.get("/v1/wallet/withdrawals", &ada).await.1),
        "account_closed"
    );

    // Their live game was resigned (hopper won) and their waiting game cancelled.
    let (live_status, result): (String, Option<String>) =
        sqlx::query_as("SELECT status, result FROM games WHERE id = $1")
            .bind(live)
            .fetch_one(&api.pool)
            .await
            .unwrap();
    assert_eq!(live_status, "finished", "{result:?}");
    let hopper_won: bool = sqlx::query_scalar("SELECT (white_id = $2 AND result = 'white') OR (black_id = $2 AND result = 'black') FROM games WHERE id = $1")
        .bind(live).bind(hopper_id).fetch_one(&api.pool).await.unwrap();
    assert!(hopper_won);
    let waiting_status: String = sqlx::query_scalar("SELECT status FROM games WHERE id = $1")
        .bind(waiting)
        .fetch_one(&api.pool)
        .await
        .unwrap();
    assert_ne!(waiting_status, "waiting");
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_declined_closure_restricts_nothing(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, ada_id) = person(&api, "ada").await;
    let (grace, _) = person(&api, "grace").await;
    let (r1, _) = reviewer(&api, "rone").await;
    let (r2, _) = reviewer(&api, "rtwo").await;
    let case = proposed(&api, &ada, ada_id, &grace, &r1).await;

    let (status, b) = api
        .post(
            &format!("/v1/admin/fairplay/cases/{case}/closure/decline"),
            &r2,
            json!({ "reason": "too harsh for this evidence" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert_eq!(
        api.post("/v1/games", &ada, game_body()).await.0,
        StatusCode::CREATED
    );
    let (_, list) = api.get("/v1/admin/fairplay/closures", &r2).await;
    assert_eq!(list["closures"], json!([]));
    // A reason is required, checked before anything else.
    assert_eq!(
        api.post(
            &format!("/v1/admin/fairplay/cases/{case}/closure/approve"),
            &r2,
            json!({ "reason": "  " })
        )
        .await
        .0,
        StatusCode::BAD_REQUEST,
    );
}

/// Ada's closure, proposed by `r1` and approved by `r2`.
async fn closed_ada(api: &Api, ada: &str, ada_id: Uuid, grace: &str, r1: &str, r2: &str) -> Uuid {
    let case = proposed(api, ada, ada_id, grace, r1).await;
    let (status, b) = api
        .post(
            &format!("/v1/admin/fairplay/cases/{case}/closure/approve"),
            r2,
            json!({ "reason": "agreed" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");
    case
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_closed_player_appeals_once_and_a_reviewer_outside_the_case_can_overturn_it(
    pool: PgPool,
) {
    let api = Api::new(pool);
    let (ada, ada_id) = person(&api, "ada").await;
    let (grace, _) = person(&api, "grace").await;
    let (r1, _) = reviewer(&api, "rone").await;
    let (r2, _) = reviewer(&api, "rtwo").await;
    let (r3, _) = reviewer(&api, "rthree").await;
    closed_ada(&api, &ada, ada_id, &grace, &r1, &r2).await;

    // A closed player can see their sanction (without the case behind it) and appeal it.
    let (status, mine) = api.get("/v1/me/sanctions", &ada).await;
    assert_eq!(status, StatusCode::OK, "{mine}");
    let s = &mine["sanctions"][0];
    assert_eq!(
        (
            s["kind"].as_str(),
            s["in_force"].as_bool(),
            s["appeal"].is_null()
        ),
        (Some("closure"), Some(true), true)
    );
    assert!(s.get("case_id").is_none() && s.get("reason").is_none());
    let id = s["id"].as_str().unwrap().to_string();
    let appeal_path = format!("/v1/me/sanctions/{id}/appeal");

    assert_eq!(
        api.post(&appeal_path, &ada, json!({ "statement": "  " }))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        api.post(&appeal_path, &grace, json!({ "statement": "not mine" }))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let (status, b) = api
        .post(
            &appeal_path,
            &ada,
            json!({ "statement": "I played those games myself." }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");
    let (status, b) = api
        .post(&appeal_path, &ada, json!({ "statement": "again" }))
        .await;
    assert_eq!(
        (status, code(&b)),
        (StatusCode::CONFLICT, "already_appealed")
    );

    // Not decided by the proposer, the closure's reviewer, the player, a player in the case, or a non-reviewer.
    let (_, list) = api.get("/v1/admin/fairplay/appeals", &r3).await;
    let appeal = list["appeals"][0]["id"].as_str().unwrap().to_string();
    assert_eq!(
        list["appeals"][0]["statement"],
        "I played those games myself."
    );
    let overturn = format!("/v1/admin/fairplay/appeals/{appeal}/overturn");
    let reason = json!({ "reason": "timing fits a slow connection" });
    for who in [&r1, &r2, &grace] {
        assert_eq!(
            api.post(&overturn, who, reason.clone()).await.0,
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        api.get("/v1/me/sanctions", &ada).await.1["sanctions"][0]["in_force"],
        true
    );

    let (status, b) = api.post(&overturn, &r3, reason.clone()).await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert_eq!(
        api.post(&overturn, &r3, reason).await.0,
        StatusCode::NOT_FOUND,
        "decided once"
    );

    // The restriction is gone at once; the sanction stays on record.
    assert_eq!(
        api.post("/v1/games", &ada, game_body()).await.0,
        StatusCode::CREATED
    );
    let (_, mine) = api.get("/v1/me/sanctions", &ada).await;
    assert_eq!(
        (
            mine["sanctions"][0]["in_force"].as_bool(),
            mine["sanctions"][0]["appeal"].as_str()
        ),
        (Some(false), Some("overturned"))
    );
    assert_eq!(
        api.count("SELECT count(*) FROM fairplay_sanctions").await,
        1
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn an_upheld_appeal_changes_nothing_and_a_warning_cannot_be_appealed(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, ada_id) = person(&api, "ada").await;
    let (grace, _) = person(&api, "grace").await;
    let (r1, _) = reviewer(&api, "rone").await;
    let (r2, _) = reviewer(&api, "rtwo").await;
    let (r3, _) = reviewer(&api, "rthree").await;
    closed_ada(&api, &ada, ada_id, &grace, &r1, &r2).await;
    let (_, mine) = api.get("/v1/me/sanctions", &ada).await;
    let id = mine["sanctions"][0]["id"].as_str().unwrap().to_string();
    api.post(
        &format!("/v1/me/sanctions/{id}/appeal"),
        &ada,
        json!({ "statement": "please" }),
    )
    .await;
    let (_, list) = api.get("/v1/admin/fairplay/appeals", &r3).await;
    let appeal = list["appeals"][0]["id"].as_str().unwrap().to_string();
    let (status, _) = api
        .post(
            &format!("/v1/admin/fairplay/appeals/{appeal}/uphold"),
            &r3,
            json!({ "reason": "the evidence stands" }),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        api.post("/v1/games", &ada, game_body()).await.0,
        StatusCode::FORBIDDEN
    );
    let (_, mine) = api.get("/v1/me/sanctions", &ada).await;
    assert_eq!(
        (
            mine["sanctions"][0]["in_force"].as_bool(),
            mine["sanctions"][0]["appeal"].as_str()
        ),
        (Some(true), Some("upheld"))
    );
    // Once decided, a second appeal is still refused.
    let (status, b) = api
        .post(
            &format!("/v1/me/sanctions/{id}/appeal"),
            &ada,
            json!({ "statement": "again" }),
        )
        .await;
    assert_eq!(
        (status, code(&b)),
        (StatusCode::CONFLICT, "already_appealed")
    );

    // A warning restricts nothing and is not appealable.
    let (bob, bob_id) = person(&api, "bob").await;
    let game = finished_game(&api, &bob, &grace).await;
    let case = flag(&api, game, bob_id, 95).await;
    let (status, b) = api
        .post(
            &format!("/v1/admin/fairplay/cases/{case}/confirm"),
            &r3,
            json!({ "reason": "minor", "games": [game], "sanction": { "kind": "warning" } }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{b}");
    let (_, mine) = api.get("/v1/me/sanctions", &bob).await;
    let wid = mine["sanctions"][0]["id"].as_str().unwrap().to_string();
    assert_eq!(
        api.post(
            &format!("/v1/me/sanctions/{wid}/appeal"),
            &bob,
            json!({ "statement": "x" })
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}
