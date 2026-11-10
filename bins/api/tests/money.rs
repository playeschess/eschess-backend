//! The money-check admin routes (ADR 0006 §6): only people who approve withdrawals see them.

mod common;

use axum::http::StatusCode;
use common::*;
use serde_json::json;
use sqlx::PgPool;

async fn verified(api: &Api, name: &str) -> String {
    let token = api.player(name).await;
    sqlx::query("UPDATE users SET email_verified_at = now() WHERE username = $1")
        .bind(name)
        .execute(&api.pool)
        .await
        .unwrap();
    token
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn only_approvers_see_the_state_of_the_books_and_can_clear_a_halt_with_a_note(pool: PgPool) {
    let api = Api::new(pool);
    let boss = verified(&api, "boss").await;
    let nobody = verified(&api, "nobody").await;
    sqlx::query("INSERT INTO user_roles (user_id, role) SELECT id, 'withdrawal_approver' FROM users WHERE username = 'boss'")
        .execute(&api.pool).await.unwrap();

    // Everyone else is told there is nothing here.
    assert_eq!(
        api.get("/v1/admin/money", &nobody).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        api.post(
            "/v1/admin/money/halt/clear",
            &nobody,
            json!({ "note": "x" })
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );

    let (status, b) = api.get("/v1/admin/money", &boss).await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert_eq!(
        (b["halted"].as_bool(), b["checks"].as_array().map(Vec::len)),
        (Some(false), Some(0))
    );

    eschess_storage::reconcile::halt(
        &api.pool,
        "USDC: the network holds 90 but the ledger says 100",
        None,
    )
    .await
    .unwrap();
    let (_, b) = api.get("/v1/admin/money", &boss).await;
    assert_eq!(b["halted"], true);
    assert!(b["halt"]["reason"].as_str().unwrap().contains("90"));

    // A note is required, and it is kept with who wrote it.
    let (status, _) = api
        .post("/v1/admin/money/halt/clear", &boss, json!({ "note": "  " }))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(api.get("/v1/admin/money", &boss).await.1["halted"], true);
    let (status, b) = api
        .post(
            "/v1/admin/money/halt/clear",
            &boss,
            json!({ "note": "a test transfer, confirmed on the explorer" }),
        )
        .await;
    assert_eq!(
        (status, b["cleared"].as_bool()),
        (StatusCode::OK, Some(true))
    );
    assert_eq!(api.get("/v1/admin/money", &boss).await.1["halted"], false);
    let note: String = sqlx::query_scalar("SELECT clear_note FROM money_halts")
        .fetch_one(&api.pool)
        .await
        .unwrap();
    assert!(note.contains("explorer"));
}
