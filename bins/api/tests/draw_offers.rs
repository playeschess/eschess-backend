//! Draw offers (`make dev`, then `make test-integration`).

mod common;

use axum::http::StatusCode;
use common::*;
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

/// A started 5+0 game with `ada` as White and `grace` as Black.
async fn start(api: &Api) -> (String, String, Uuid) {
    let (ada, grace) = (api.player("ada").await, api.player("grace").await);
    let (_, g) = api
        .post(
            "/v1/games",
            &ada,
            json!({ "time_control": { "base_seconds": 300 }, "color": "white" }),
        )
        .await;
    let id = id_of(&g);
    api.post(&format!("/v1/games/{id}/join"), &grace, json!({}))
        .await;
    (ada, grace, id)
}

async fn mv(api: &Api, id: Uuid, who: &str, uci: &str) -> (StatusCode, Value) {
    api.post(
        &format!("/v1/games/{id}/moves"),
        who,
        json!({ "uci": uci, "client_move_id": Uuid::now_v7() }),
    )
    .await
}

async fn offer(api: &Api, id: Uuid, who: &str) -> (StatusCode, Value) {
    api.post(&format!("/v1/games/{id}/draw-offer"), who, json!({}))
        .await
}

async fn answer(api: &Api, id: Uuid, who: &str, what: &str) -> (StatusCode, Value) {
    api.post(&format!("/v1/games/{id}/draw-offer/{what}"), who, json!({}))
        .await
}

fn code(b: &Value) -> Option<&str> {
    b["error"]["code"].as_str()
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn an_accepted_offer_ends_the_game_as_a_draw_by_agreement(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, grace, id) = start(&api).await;
    for (who, uci) in [
        (&ada, "e2e4"),
        (&grace, "e7e5"),
        (&ada, "g1f3"),
        (&grace, "b8c6"),
    ] {
        assert_eq!(mv(&api, id, who, uci).await.0, StatusCode::OK);
    }

    // Ada offers; both players see whose offer it is, and the game goes on.
    let (s, g) = offer(&api, id, &ada).await;
    assert_eq!(
        (s, g["draw_offer"].as_str(), g["status"].as_str()),
        (StatusCode::OK, Some("white"), Some("active"))
    );
    assert_eq!(
        api.get(&format!("/v1/games/{id}"), &grace).await.1["draw_offer"],
        "white"
    );

    let (s, g) = answer(&api, id, &grace, "accept").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        (
            g["status"].as_str(),
            g["result"].as_str(),
            g["end_reason"].as_str()
        ),
        (Some("finished"), Some("draw"), Some("agreement"))
    );
    assert!(g["draw_offer"].is_null());

    // It was a rated game of enough length, so it counts: equal newcomers drawing keep 1500 but now have a game each.
    let (_, me) = api.get("/v1/me", &ada).await;
    let blitz = me["ratings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["category"] == "blitz")
        .unwrap();
    assert_eq!(
        (blitz["rating"].as_i64(), blitz["games_played"].as_i64()),
        (Some(1500), Some(1))
    );
    // And nothing more can be played or offered.
    assert_eq!(code(&mv(&api, id, &ada, "d2d4").await.1), Some("game_over"));
    assert_eq!(code(&offer(&api, id, &ada).await.1), Some("game_over"));
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn declining_clears_the_offer_and_a_second_offer_needs_a_move_in_between(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, grace, id) = start(&api).await;
    mv(&api, id, &ada, "e2e4").await;

    offer(&api, id, &ada).await;
    let (s, g) = answer(&api, id, &grace, "decline").await;
    assert_eq!((s, g["status"].as_str()), (StatusCode::OK, Some("active")));
    assert!(g["draw_offer"].is_null());

    // Same position: Ada may not offer again straight away (nor while one is waiting).
    let (s, b) = offer(&api, id, &ada).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, Some("draw_already_offered"))
    );
    // Grace is not held back by Ada's offers, and her offer also stands until answered.
    assert_eq!(offer(&api, id, &grace).await.1["draw_offer"], "black");
    assert_eq!(
        code(&offer(&api, id, &grace).await.1),
        Some("draw_already_offered")
    );
    answer(&api, id, &ada, "decline").await;

    // After a move, a new offer is allowed.
    mv(&api, id, &grace, "e7e5").await;
    assert_eq!(offer(&api, id, &ada).await.0, StatusCode::OK);
    // The offerer can withdraw by declining her own.
    let (_, g) = answer(&api, id, &ada, "decline").await;
    assert!(g["draw_offer"].is_null());
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn only_the_recipients_move_counts_as_declining(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, grace, id) = start(&api).await;
    offer(&api, id, &ada).await; // before any move: allowed
                                 // Ada keeps playing: her offer stays.
    let (_, g) = mv(&api, id, &ada, "e2e4").await;
    assert_eq!(g["game"]["draw_offer"], "white");
    // Grace plays instead of answering: that declines it.
    let (_, g) = mv(&api, id, &grace, "e7e5").await;
    assert!(g["draw_offer"].is_null());
    assert_eq!(
        code(&answer(&api, id, &grace, "accept").await.1),
        Some("no_draw_offer")
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn offers_to_each_other_at_once_are_an_agreement(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, grace, id) = start(&api).await;
    offer(&api, id, &ada).await;
    let (s, g) = offer(&api, id, &grace).await;
    assert_eq!(
        (s, g["end_reason"].as_str()),
        (StatusCode::OK, Some("agreement"))
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn the_wrong_people_and_the_wrong_moments_are_refused(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, grace, id) = start(&api).await;
    let stranger = api.player("stranger").await;

    assert_eq!(
        code(&answer(&api, id, &grace, "accept").await.1),
        Some("no_draw_offer")
    );
    offer(&api, id, &ada).await;
    assert_eq!(
        code(&answer(&api, id, &ada, "accept").await.1),
        Some("own_draw_offer")
    );
    // A stranger is told the game does not exist, whichever door they knock on.
    for (s, _) in [
        offer(&api, id, &stranger).await,
        answer(&api, id, &stranger, "accept").await,
        answer(&api, id, &stranger, "decline").await,
    ] {
        assert_eq!(s, StatusCode::NOT_FOUND);
    }
    // A game nobody has joined yet cannot have draw offers.
    let (_, w) = api
        .post(
            "/v1/games",
            &ada,
            json!({ "time_control": { "base_seconds": 60 } }),
        )
        .await;
    let (s, b) = offer(&api, id_of(&w), &ada).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, Some("game_not_started"))
    );
    // The database itself refuses an offer on a game that is not active.
    api.post(&format!("/v1/games/{id}/resign"), &grace, json!({}))
        .await;
    assert!(
        sqlx::query("UPDATE games SET draw_offered_by = 'white' WHERE id = $1")
            .bind(id)
            .execute(&api.pool)
            .await
            .is_err()
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn an_acceptance_racing_a_move_leaves_one_consistent_game(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, grace, id) = start(&api).await;
    mv(&api, id, &ada, "e2e4").await;
    offer(&api, id, &ada).await;

    // Grace accepts and plays at the very same moment: the game's lock lets one go first.
    let (accept, play) = futures::future::join(
        answer(&api, id, &grace, "accept"),
        mv(&api, id, &grace, "e7e5"),
    )
    .await;
    let (_, g) = api.get(&format!("/v1/games/{id}"), &ada).await;
    if g["status"] == "finished" {
        assert_eq!(
            (accept.0, g["end_reason"].as_str()),
            (StatusCode::OK, Some("agreement"))
        );
        assert_eq!(code(&play.1), Some("game_over"), "the move came too late");
        assert_eq!(g["ply"], 1);
    } else {
        // The move came first, which declined the offer, so the acceptance found nothing to accept.
        assert_eq!(
            (play.0, code(&accept.1)),
            (StatusCode::OK, Some("no_draw_offer"))
        );
        assert!(g["draw_offer"].is_null());
        assert_eq!(g["ply"], 2);
    }
}
