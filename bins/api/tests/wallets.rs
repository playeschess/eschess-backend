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

fn stellar_config() -> eschess_api::Config {
    use eschess_api::config::{StellarConfig, TESTNET_HORIZON_URL, TESTNET_USDC_ISSUER};
    eschess_api::Config {
        stellar: Some(StellarConfig {
            network: "testnet".into(),
            horizon_url: TESTNET_HORIZON_URL.into(),
            hot_account: eschess_domain::stellar::AccountKey::parse(
                "GBL4BGHUU6TAX7WXPSXAES7XBKEPJWLOO3HBIOYHNJH4KMLQZIQVZIF5",
            )
            .unwrap(),
            usdc: eschess_domain::money::AssetId::credit("USDC", TESTNET_USDC_ISSUER).unwrap(),
            poll_ms: 5_000,
            withdraw: Default::default(),
        }),
        ..eschess_api::Config::default()
    }
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn the_first_deposit_intent_asks_for_the_players_account_to_be_set_up_once(pool: PgPool) {
    let vault: Arc<dyn KeyVault> =
        Arc::new(LocalVault::from_bytes([9; 32], 1, Network::Testnet).unwrap());
    let api = Api::with_vault_and_config(pool, vault, stellar_config());
    let token = api.player("ada").await;
    sqlx::query("UPDATE users SET email_verified_at = now() WHERE username = 'ada'")
        .execute(&api.pool)
        .await
        .unwrap();
    assert_eq!(
        api.count("SELECT count(*) FROM wallet_setup").await,
        0,
        "signing up asks for nothing"
    );

    let (status, b) = api.get("/v1/wallet/deposit-info", &token).await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert_eq!(b["own_wallet"]["status"], "setting_up");
    assert_eq!(api.count("SELECT count(*) FROM wallet_setup").await, 1);
    // The player's own address is not offered as a place to send money yet, and no key is ever shown.
    let address: String = sqlx::query_scalar("SELECT address FROM user_wallet_keys")
        .fetch_one(&api.pool)
        .await
        .unwrap();
    assert!(!b.to_string().contains(&address));

    // Again changes nothing; once the worker has finished it says so.
    api.get("/v1/wallet/deposit-info", &token).await;
    assert_eq!(api.count("SELECT count(*) FROM wallet_setup").await, 1);
    sqlx::query("UPDATE wallet_setup SET ready_at = now()")
        .execute(&api.pool)
        .await
        .unwrap();
    let (_, b) = api.get("/v1/wallet/deposit-info", &token).await;
    assert_eq!(b["own_wallet"]["status"], "ready");
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn without_a_master_key_deposit_info_is_unchanged_and_asks_for_nothing(pool: PgPool) {
    let api = Api::with_config(pool, stellar_config());
    let token = api.player("ada").await;
    sqlx::query("UPDATE users SET email_verified_at = now() WHERE username = 'ada'")
        .execute(&api.pool)
        .await
        .unwrap();
    let (status, b) = api.get("/v1/wallet/deposit-info", &token).await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert!(b["own_wallet"].is_null());
    assert_eq!(api.count("SELECT count(*) FROM wallet_setup").await, 0);
}
