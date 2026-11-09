//! Users' wallets against a real Postgres (`make dev`, then `make test-integration`): ADR 0006.

use std::sync::Arc;

use eschess_stellar::custody::{KeyVault, LocalVault};
use eschess_stellar::signer::Signer;
use eschess_stellar::tx::Network;
use eschess_storage::wallets::{address_of, ensure_wallet, signer_for};
use sqlx::PgPool;
use uuid::Uuid;

fn vault(seed: u8) -> Arc<dyn KeyVault> {
    Arc::new(LocalVault::from_bytes([seed; 32], 1, Network::Testnet).unwrap())
}

async fn user(pool: &PgPool) -> Uuid {
    let id = Uuid::now_v7();
    let name = format!("u{}", &id.simple().to_string()[20..]);
    sqlx::query("INSERT INTO users (id, email, username, password_hash) VALUES ($1, $2, $3, 'x')")
        .bind(id)
        .bind(format!("{name}@example.com"))
        .bind(&name)
        .execute(pool)
        .await
        .unwrap();
    id
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_user_gets_one_address_that_never_changes_and_that_signs(pool: PgPool) {
    let (v, u) = (vault(7), user(&pool).await);
    assert_eq!(address_of(&pool, u).await.unwrap(), None);
    let a = ensure_wallet(&pool, &*v, u).await.unwrap();
    assert_eq!(
        ensure_wallet(&pool, &*v, u).await.unwrap(),
        a,
        "asking again changes nothing"
    );
    assert_eq!(address_of(&pool, u).await.unwrap(), Some(a));

    let signer = signer_for(&pool, v.clone(), u).await.unwrap().unwrap();
    assert_eq!(signer.public_key(), a);
    let sig = signer.sign(&[3; 32]).await.unwrap();
    let key = ed25519_dalek::VerifyingKey::from_bytes(&a.0).unwrap();
    ed25519_dalek::Verifier::verify(&key, &[3; 32], &ed25519_dalek::Signature::from_bytes(&sig))
        .unwrap();

    // A different user gets a different address.
    let other = user(&pool).await;
    assert_ne!(ensure_wallet(&pool, &*v, other).await.unwrap(), a);
    assert!(signer_for(&pool, v.clone(), Uuid::now_v7())
        .await
        .unwrap()
        .is_none());
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn many_requests_at_once_make_one_wallet(pool: PgPool) {
    let (v, u) = (vault(7), user(&pool).await);
    let results = futures::future::join_all((0..8).map(|_| ensure_wallet(&pool, &*v, u))).await;
    let first = results[0].as_ref().unwrap();
    assert!(results.iter().all(|r| r.as_ref().unwrap() == first));
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM user_wallet_keys WHERE user_id = $1")
        .bind(u)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rows, 1);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn the_stored_key_is_ciphertext_and_will_not_open_in_another_users_row_or_with_another_master_key(
    pool: PgPool,
) {
    let (v, a, b) = (vault(7), user(&pool).await, user(&pool).await);
    let addr_a = ensure_wallet(&pool, &*v, a).await.unwrap();
    ensure_wallet(&pool, &*v, b).await.unwrap();

    // The stored bytes are not the key: they do not sign for the address, and the address is not hidden in them.
    let ct: Vec<u8> =
        sqlx::query_scalar("SELECT ciphertext FROM user_wallet_keys WHERE user_id = $1")
            .bind(a)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(ct.len(), 48);
    assert!(
        ed25519_dalek::SigningKey::from_bytes(ct[..32].try_into().unwrap())
            .verifying_key()
            .to_bytes()
            != addr_a.0
    );

    // Another master key cannot use it.
    let wrong = signer_for(&pool, vault(8), a).await.unwrap().unwrap();
    assert!(wrong.sign(&[0; 32]).await.is_err());

    // The database refuses to rewrite or delete keys, so a row cannot be swapped between users.
    for sql in [
        "UPDATE user_wallet_keys SET ciphertext = (SELECT ciphertext FROM user_wallet_keys WHERE user_id = $2) WHERE user_id = $1",
        "DELETE FROM user_wallet_keys WHERE user_id = $1",
    ] {
        assert!(sqlx::query(sql).bind(a).bind(b).execute(&pool).await.is_err(), "{sql}");
    }
    // And a bad address or wrong-sized ciphertext cannot be inserted.
    let c = user(&pool).await;
    assert!(sqlx::query("INSERT INTO user_wallet_keys (user_id, address, key_version, nonce, ciphertext) VALUES ($1, 'nope', 1, $2, $3)")
        .bind(c).bind(vec![0u8; 12]).bind(vec![0u8; 48]).execute(&pool).await.is_err());
}
