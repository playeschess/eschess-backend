//! A player's own wallet is made at signup when the platform has a master key (ADR 0006), and signing up works without one.

mod common;

use std::sync::Arc;

use axum::http::StatusCode;
use common::*;
use eschess_stellar::custody::{KeyVault, LocalVault};
use eschess_stellar::tx::Network;
use serde_json::json;
use sqlx::PgPool;

fn signup_body(name: &str) -> serde_json::Value {
    json!({ "email": format!("{name}@example.com"), "username": name, "password": "a-long-test-password-1" })
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn signing_up_makes_the_player_a_wallet_of_their_own(pool: PgPool) {
    let vault: Arc<dyn KeyVault> =
        Arc::new(LocalVault::from_bytes([9; 32], 1, Network::Testnet).unwrap());
    let api = Api::with_vault(pool, vault);
    let (status, b) = api.post("/v1/auth/signup", "", signup_body("ada")).await;
    assert_eq!(status, StatusCode::CREATED, "{b}");
    let (status, _) = api.post("/v1/auth/signup", "", signup_body("grace")).await;
    assert_eq!(status, StatusCode::CREATED);
    let addresses: Vec<String> =
        sqlx::query_scalar("SELECT address FROM user_wallet_keys ORDER BY created_at")
            .fetch_all(&api.pool)
            .await
            .unwrap();
    assert_eq!(addresses.len(), 2);
    assert_ne!(addresses[0], addresses[1]);
    assert!(addresses
        .iter()
        .all(|a| a.starts_with('G') && a.len() == 56));
    // Nothing the API says about the player includes a key or the address yet (the address is shown once it can receive money).
    assert!(!b.to_string().contains(&addresses[0]));
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn signing_up_works_without_a_master_key_and_makes_no_wallet(pool: PgPool) {
    let api = Api::new(pool);
    let (status, b) = api.post("/v1/auth/signup", "", signup_body("ada")).await;
    assert_eq!(status, StatusCode::CREATED, "{b}");
    assert_eq!(api.count("SELECT count(*) FROM user_wallet_keys").await, 0);
}
