//! Which Lichess account a player plays with (ADR 0005). The login itself is `eschess-lichess`; this is what we keep.

use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug)]
pub enum LinkError {
    /// This player already has a Lichess account linked. It cannot be changed here.
    AlreadyLinked,
    /// Another player already uses this Lichess account.
    AccountTaken,
    Database(sqlx::Error),
}

impl From<sqlx::Error> for LinkError {
    fn from(e: sqlx::Error) -> Self {
        LinkError::Database(e)
    }
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkError::AlreadyLinked => write!(f, "this player already has a Lichess account"),
            LinkError::AccountTaken => write!(f, "this Lichess account belongs to another player"),
            LinkError::Database(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for LinkError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Linked {
    pub lichess_id: String,
    pub username: String,
}

/// The player's linked Lichess account, if any.
pub async fn linked(pool: &PgPool, user: Uuid) -> Result<Option<Linked>, sqlx::Error> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT lichess_id, username FROM lichess_links WHERE user_id = $1")
            .bind(user)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(lichess_id, username)| Linked {
        lichess_id,
        username,
    }))
}

/// Starts a login attempt for a player who has no Lichess account linked yet. Old, unused attempts of theirs are dropped.
pub async fn begin(
    pool: &PgPool,
    user: Uuid,
    state: &str,
    verifier: &str,
    ttl_secs: i64,
) -> Result<(), LinkError> {
    if linked(pool, user).await?.is_some() {
        return Err(LinkError::AlreadyLinked);
    }
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM lichess_link_attempts WHERE user_id = $1 AND (used_at IS NOT NULL OR expires_at < now())")
        .bind(user)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO lichess_link_attempts (state, user_id, verifier, expires_at) \
         VALUES ($1, $2, $3, now() + make_interval(secs => $4::double precision))",
    )
    .bind(state)
    .bind(user)
    .bind(verifier)
    .bind(ttl_secs as f64)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Uses up the attempt this `state` started for this player and returns its verifier. `None` if there is no such attempt,
/// it belongs to someone else, it expired, or it was used before: each attempt can be finished once.
pub async fn take_attempt(
    pool: &PgPool,
    user: Uuid,
    state: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(
        "UPDATE lichess_link_attempts SET used_at = now() \
         WHERE state = $1 AND user_id = $2 AND used_at IS NULL AND expires_at > now() RETURNING verifier",
    )
    .bind(state)
    .bind(user)
    .fetch_optional(pool)
    .await
}

/// Links the Lichess account to the player, for good. Repeating the very same link is not an error and changes nothing.
pub async fn save(
    pool: &PgPool,
    user: Uuid,
    lichess_id: &str,
    username: &str,
) -> Result<(), LinkError> {
    let mut tx = pool.begin().await?;
    // Serialise on the player, so two finishes at once cannot both pass the checks.
    sqlx::query("SELECT id FROM users WHERE id = $1 FOR UPDATE")
        .bind(user)
        .fetch_optional(&mut *tx)
        .await?;
    let mine: Option<String> =
        sqlx::query_scalar("SELECT lichess_id FROM lichess_links WHERE user_id = $1")
            .bind(user)
            .fetch_optional(&mut *tx)
            .await?;
    match mine {
        Some(id) if id == lichess_id => return Ok(()),
        Some(_) => return Err(LinkError::AlreadyLinked),
        None => {}
    }
    let inserted = sqlx::query(
        "INSERT INTO lichess_links (user_id, lichess_id, username) VALUES ($1, $2, $3) ON CONFLICT (lichess_id) DO NOTHING",
    )
    .bind(user)
    .bind(lichess_id)
    .bind(username)
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() == 0 {
        return Err(LinkError::AccountTaken);
    }
    tx.commit().await?;
    Ok(())
}
