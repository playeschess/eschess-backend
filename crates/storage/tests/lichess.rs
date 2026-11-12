//! Linking a Lichess account to a player, against a real Postgres (`make dev`, then `make test-integration`).

use eschess_storage::lichess::{self, LinkError};
use sqlx::PgPool;
use uuid::Uuid;

async fn player(pool: &PgPool) -> Uuid {
    let id = Uuid::now_v7();
    let name = format!("u{}", &id.simple().to_string()[20..]);
    sqlx::query("INSERT INTO users (id, email, username, password_hash, email_verified_at) VALUES ($1, $2, $3, 'x', now())")
        .bind(id)
        .bind(format!("{name}@example.com"))
        .bind(&name)
        .execute(pool)
        .await
        .unwrap();
    id
}

const STATE: &str = "0123456789abcdef0123456789abcdef0123456789abcdef";
const VERIFIER: &str = "0123456789abcdef0123456789abcdef0123456789abcdef";

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn an_attempt_can_be_finished_once_only_by_its_owner_and_only_in_time(pool: PgPool) {
    let (a, b) = (player(&pool).await, player(&pool).await);
    lichess::begin(&pool, a, STATE, VERIFIER, 600)
        .await
        .unwrap();
    assert_eq!(
        lichess::take_attempt(&pool, b, STATE).await.unwrap(),
        None,
        "not someone else's"
    );
    assert_eq!(lichess::take_attempt(&pool, a, "nope").await.unwrap(), None);
    assert_eq!(
        lichess::take_attempt(&pool, a, STATE)
            .await
            .unwrap()
            .as_deref(),
        Some(VERIFIER)
    );
    assert_eq!(
        lichess::take_attempt(&pool, a, STATE).await.unwrap(),
        None,
        "once"
    );

    let late = format!("{STATE}2");
    lichess::begin(&pool, a, &late, VERIFIER, 600)
        .await
        .unwrap();
    sqlx::query("UPDATE lichess_link_attempts SET expires_at = now() - interval '1 second' WHERE state = $1")
        .bind(&late)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        lichess::take_attempt(&pool, a, &late).await.unwrap(),
        None,
        "expired"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn one_player_one_account_for_good(pool: PgPool) {
    let (a, b) = (player(&pool).await, player(&pool).await);
    assert_eq!(lichess::linked(&pool, a).await.unwrap(), None);
    lichess::save(&pool, a, "magnus", "MagNus").await.unwrap();
    assert_eq!(
        lichess::linked(&pool, a).await.unwrap().unwrap().username,
        "MagNus"
    );

    lichess::save(&pool, a, "magnus", "MagNus").await.unwrap();
    assert!(matches!(
        lichess::save(&pool, a, "hikaru", "Hikaru").await,
        Err(LinkError::AlreadyLinked)
    ));
    assert!(matches!(
        lichess::save(&pool, b, "magnus", "MagNus").await,
        Err(LinkError::AccountTaken)
    ));
    assert_eq!(lichess::linked(&pool, b).await.unwrap(), None);

    // A player who has linked cannot start another login, and the link cannot be edited or removed.
    assert!(matches!(
        lichess::begin(&pool, a, STATE, VERIFIER, 600).await,
        Err(LinkError::AlreadyLinked)
    ));
    assert!(
        sqlx::query("UPDATE lichess_links SET lichess_id = 'other' WHERE user_id = $1")
            .bind(a)
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(sqlx::query("DELETE FROM lichess_links WHERE user_id = $1")
        .bind(a)
        .execute(&pool)
        .await
        .is_err());
}
