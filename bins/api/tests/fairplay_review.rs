//! Fair-play review through the API (`make dev`, then `make test-integration`): who may see cases, what a reviewer is
//! shown, and how a case is decided. Games are played through the API and scored with a test scorer, as the worker would.

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

fn decide_body(reason: &str) -> Value {
    json!({ "reason": reason, "games": [] })
}

/// A decision on the games the reviewer looked at.
fn decision(reason: &str, games: &[Uuid]) -> Value {
    json!({ "reason": reason, "games": games })
}

/// A confirmation: the decision plus the sanction it carries.
fn confirmation(reason: &str, games: &[Uuid], sanction: Value) -> Value {
    json!({ "reason": reason, "games": games, "sanction": sanction })
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn only_reviewers_know_the_routes_exist(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, ada_id) = person(&api, "ada").await;
    let (grace, _) = person(&api, "grace").await;
    let game = finished_game(&api, &ada, &grace).await;
    let case = flag(&api, game, ada_id, 90).await;
    let (nobody, _) = person(&api, "nobody").await;

    for (status, body) in [
        api.get("/v1/admin/fairplay/cases", &nobody).await,
        api.get(&format!("/v1/admin/fairplay/cases/{case}"), &nobody)
            .await,
        api.post(
            &format!("/v1/admin/fairplay/cases/{case}/clear"),
            &nobody,
            decide_body("x"),
        )
        .await,
        api.post(
            &format!("/v1/admin/fairplay/cases/{case}/confirm"),
            &nobody,
            decide_body("x"),
        )
        .await,
        // The suspect cannot find out they are being looked at.
        api.get(&format!("/v1/admin/fairplay/cases/{case}"), &ada)
            .await,
    ] {
        assert_eq!(
            (status, code(&body)),
            (StatusCode::NOT_FOUND, "not_found"),
            "{body}"
        );
    }
    let (status, _) = api
        .call(
            axum::http::Method::GET,
            "/v1/admin/fairplay/cases",
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A reviewer must have confirmed their email.
    let unconfirmed = api.player("newbie").await;
    let id: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE username = 'newbie'")
        .fetch_one(&api.pool)
        .await
        .unwrap();
    make_reviewer(&api, id).await;
    let (status, body) = api.get("/v1/admin/fairplay/cases", &unconfirmed).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "email_not_verified")
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_reviewer_sees_the_games_and_the_evidence_then_decides_once(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, ada_id) = person(&api, "ada").await;
    let (grace, _) = person(&api, "grace").await;
    let (rev, rev_id) = person(&api, "rev").await;
    make_reviewer(&api, rev_id).await;
    let game = finished_game(&api, &ada, &grace).await;
    let case = flag(&api, game, ada_id, 88).await;

    let (status, list) = api.get("/v1/admin/fairplay/cases", &rev).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let row = &list["cases"][0];
    assert_eq!(row["id"], json!(case));
    assert_eq!(row["player"]["username"], "ada");
    assert_eq!(
        (
            row["status"].as_str(),
            row["games"].as_i64(),
            row["top_score"].as_i64()
        ),
        (Some("open"), Some(1), Some(88))
    );

    let (status, detail) = api
        .get(&format!("/v1/admin/fairplay/cases/{case}"), &rev)
        .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    let g = &detail["game_records"][0];
    assert_eq!(g["game_id"], json!(game));
    assert_eq!(
        (g["result"].as_str(), g["end_reason"].as_str()),
        (Some("black"), Some("resignation"))
    );
    assert_eq!(
        g["time_control"],
        json!({ "base_ms": 180_000, "increment_ms": 2_000 })
    );
    let moves = g["moves"].as_array().unwrap();
    assert_eq!(moves.len(), 2);
    assert_eq!(
        (moves[0]["uci"].as_str(), moves[0]["mover"].as_str()),
        (Some("e2e4"), Some("white"))
    );
    assert_eq!(moves[0]["transport"], "http");
    assert_eq!(moves[0]["lag_measured_ms"], Value::Null);
    assert_eq!(
        g["scores"],
        json!([{ "scorer": "test-1", "score": 88, "summary": "moves matched the engine's first choice unusually often", "evidence": { "matched": 2, "of": 2 } }])
    );

    // A decision needs a reason.
    for reason in ["", "   "] {
        let (status, body) = api
            .post(
                &format!("/v1/admin/fairplay/cases/{case}/clear"),
                &rev,
                decision(reason, &[game]),
            )
            .await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::BAD_REQUEST, "validation_failed")
        );
    }
    let (status, done) = api
        .post(
            &format!("/v1/admin/fairplay/cases/{case}/clear"),
            &rev,
            decision("two book moves; nothing unusual", &[game]),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(done["status"], "cleared");
    assert_eq!(done["decided_by"], json!(rev_id));
    assert_eq!(done["reason"], "two book moves; nothing unusual");

    // Final: it cannot be decided again, either way.
    for route in ["clear", "confirm"] {
        let (status, body) = api
            .post(
                &format!("/v1/admin/fairplay/cases/{case}/{route}"),
                &rev,
                if route == "confirm" {
                    confirmation("again", &[game], json!({ "kind": "warning" }))
                } else {
                    decision("again", &[game])
                },
            )
            .await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::CONFLICT, "case_already_decided")
        );
    }
    // It has left the open queue and is listed with the cleared ones.
    let (_, open) = api.get("/v1/admin/fairplay/cases", &rev).await;
    assert_eq!(open["cases"], json!([]));
    let (_, cleared) = api
        .get("/v1/admin/fairplay/cases?status=cleared", &rev)
        .await;
    assert_eq!(cleared["cases"][0]["id"], json!(case));
    let (status, body) = api
        .get("/v1/admin/fairplay/cases?status=pending", &rev)
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::BAD_REQUEST, "validation_failed")
    );

    // A later suspicious game opens a fresh case, which can be confirmed.
    let game2 = finished_game(&api, &ada, &grace).await;
    let case2 = flag(&api, game2, ada_id, 95).await;
    assert_ne!(case2, case);
    let (status, confirmed) = api
        .post(
            &format!("/v1/admin/fairplay/cases/{case2}/confirm"),
            &rev,
            confirmation(
                "same pattern again across both games",
                &[game2],
                json!({ "kind": "suspension", "days": 14 }),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{confirmed}");
    assert_eq!(confirmed["status"], "confirmed");
    // The sanction was written with the decision.
    let (kind, days_left): (String, f64) = sqlx::query_as(
        "SELECT kind, (EXTRACT(EPOCH FROM (ends_at - imposed_at)) / 86400)::float8 FROM fairplay_sanctions WHERE case_id = $1",
    )
    .bind(case2)
    .fetch_one(&api.pool)
    .await
    .unwrap();
    assert_eq!(
        (kind.as_str(), days_left.round() as i64),
        ("suspension", 14)
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn confirming_needs_a_sensible_sanction_and_clearing_has_none(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, ada_id) = person(&api, "ada").await;
    let (grace, _) = person(&api, "grace").await;
    let (rev, rev_id) = person(&api, "rev").await;
    make_reviewer(&api, rev_id).await;
    let game = finished_game(&api, &ada, &grace).await;
    let case = flag(&api, game, ada_id, 95).await;
    let confirm = format!("/v1/admin/fairplay/cases/{case}/confirm");
    let clear = format!("/v1/admin/fairplay/cases/{case}/clear");

    for body in [
        decision("no sanction", &[game]),
        confirmation(
            "zero days",
            &[game],
            json!({ "kind": "suspension", "days": 0 }),
        ),
        confirmation(
            "too long",
            &[game],
            json!({ "kind": "suspension", "days": 366 }),
        ),
    ] {
        let (status, b) = api.post(&confirm, &rev, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{b}");
    }
    // A sanction of a kind that does not exist is not understood at all.
    let (status, b) = api
        .post(
            &confirm,
            &rev,
            confirmation("nonsense", &[game], json!({ "kind": "exile" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    let (status, b) = api
        .post(
            &clear,
            &rev,
            confirmation("sanction on a clear", &[game], json!({ "kind": "warning" })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{b}");

    // None of the refusals decided anything or wrote a sanction.
    let (open, sanctions): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM fairplay_cases WHERE status = 'open'), (SELECT count(*) FROM fairplay_sanctions)",
    )
    .fetch_one(&api.pool)
    .await
    .unwrap();
    assert_eq!((open, sanctions), (1, 0));
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn nobody_reviews_a_case_about_themselves_or_a_game_they_played_in(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, ada_id) = person(&api, "ada").await;
    // Grace is a reviewer, and Ada's opponent.
    let (grace, grace_id) = person(&api, "grace").await;
    make_reviewer(&api, grace_id).await;
    make_reviewer(&api, ada_id).await;
    let (rev, rev_id) = person(&api, "rev").await;
    make_reviewer(&api, rev_id).await;
    let game = finished_game(&api, &ada, &grace).await;
    let case = flag(&api, game, ada_id, 90).await;

    for who in [&ada, &grace] {
        let (_, list) = api.get("/v1/admin/fairplay/cases", who).await;
        assert_eq!(list["cases"], json!([]), "not even listed");
        let (status, _) = api
            .get(&format!("/v1/admin/fairplay/cases/{case}"), who)
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = api
            .post(
                &format!("/v1/admin/fairplay/cases/{case}/clear"),
                who,
                decision("fine", &[game]),
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    // Someone uninvolved sees it.
    let (_, list) = api.get("/v1/admin/fairplay/cases", &rev).await;
    assert_eq!(list["cases"][0]["id"], json!(case));

    // The database refuses it too, whatever the code does.
    let refused = sqlx::query(
        "UPDATE fairplay_cases SET status = 'cleared', decided_by = $2, decided_at = now(), reason = 'fine' WHERE id = $1",
    )
    .bind(case)
    .bind(grace_id)
    .execute(&api.pool)
    .await
    .unwrap_err();
    assert!(
        refused.to_string().contains("played in one of its games"),
        "{refused}"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_game_added_after_the_reviewer_looked_stops_the_decision_until_they_look_again(
    pool: PgPool,
) {
    let api = Api::new(pool);
    let (ada, ada_id) = person(&api, "ada").await;
    let (grace, _) = person(&api, "grace").await;
    let (rev, rev_id) = person(&api, "rev").await;
    make_reviewer(&api, rev_id).await;
    let first = finished_game(&api, &ada, &grace).await;
    let case = flag(&api, first, ada_id, 90).await;
    let (_, seen) = api
        .get(&format!("/v1/admin/fairplay/cases/{case}"), &rev)
        .await;
    assert_eq!(seen["game_records"].as_array().unwrap().len(), 1);

    // Meanwhile the worker adds another suspicious game to the same open case.
    let second = finished_game(&api, &ada, &grace).await;
    assert_eq!(flag(&api, second, ada_id, 80).await, case);

    let (status, body) = api
        .post(
            &format!("/v1/admin/fairplay/cases/{case}/clear"),
            &rev,
            decision("looked fine", &[first]),
        )
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::CONFLICT, "case_changed"),
        "{body}"
    );
    // An empty or made-up list is refused the same way.
    for games in [vec![], vec![first, second, Uuid::now_v7()]] {
        let (status, body) = api
            .post(
                &format!("/v1/admin/fairplay/cases/{case}/clear"),
                &rev,
                decision("looked fine", &games),
            )
            .await;
        assert_eq!(
            (status, code(&body)),
            (StatusCode::CONFLICT, "case_changed")
        );
    }

    // Looking again shows both games, and the decision on both goes through.
    let (_, again) = api
        .get(&format!("/v1/admin/fairplay/cases/{case}"), &rev)
        .await;
    let ids: Vec<Uuid> = again["game_records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["game_id"].as_str().unwrap().parse().unwrap())
        .collect();
    assert_eq!(ids, [first, second], "oldest first");
    let (status, done) = api
        .post(
            &format!("/v1/admin/fairplay/cases/{case}/confirm"),
            &rev,
            confirmation(
                "the same pattern in both games",
                &ids,
                json!({ "kind": "warning" }),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(done["status"], "confirmed");
}
