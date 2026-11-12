//! Staked games played on Lichess (ADR 0005): what the worker needs to make each game on Lichess and to read its result.
//! The result itself is recorded with [`crate::games::finish_external`].

use sqlx::{PgPool, Row};
use uuid::Uuid;

/// A staked Lichess game whose challenge has not been made yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToCreate {
    pub id: Uuid,
    /// Lichess ids of the two players.
    pub white: String,
    pub black: String,
    pub base_ms: i64,
    pub increment_ms: i64,
    pub rated: bool,
    /// Seconds since the game started on our side.
    pub age_secs: i64,
}

/// Active Lichess games with no Lichess game yet, oldest first.
pub async fn to_create(pool: &PgPool, limit: i64) -> Result<Vec<ToCreate>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT g.id, wl.lichess_id AS white, bl.lichess_id AS black, g.base_ms, g.increment_ms, g.rated, \
                COALESCE(extract(epoch FROM now() - g.started_at), 0)::bigint AS age_secs \
         FROM games g \
         JOIN lichess_links wl ON wl.user_id = g.white_id \
         JOIN lichess_links bl ON bl.user_id = g.black_id \
         WHERE g.venue = 'lichess' AND g.status = 'active' AND g.lichess_game_id IS NULL \
         ORDER BY g.started_at, g.id LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| ToCreate {
            id: r.get("id"),
            white: r.get("white"),
            black: r.get("black"),
            base_ms: r.get("base_ms"),
            increment_ms: r.get("increment_ms"),
            rated: r.get("rated"),
            age_secs: r.get("age_secs"),
        })
        .collect())
}

/// The Lichess challenge for the game was made: remember its id and when it expires. Returns `false` if the game already
/// has a Lichess game (a repeat changes nothing). The id is also the unique key: one Lichess game, one pot.
pub async fn set_challenge(
    pool: &PgPool,
    game: Uuid,
    lichess_game_id: &str,
    expires_at_ms: i64,
) -> Result<bool, sqlx::Error> {
    let done = sqlx::query(
        "UPDATE games SET lichess_game_id = $2, lichess_challenge_expires_at = to_timestamp($3::double precision / 1000.0) \
         WHERE id = $1 AND venue = 'lichess' AND lichess_game_id IS NULL AND status = 'active'",
    )
    .bind(game)
    .bind(lichess_game_id)
    .bind(expires_at_ms)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// A Lichess game being played, or waiting to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InPlay {
    pub id: Uuid,
    pub lichess_game_id: String,
    pub white: String,
    pub black: String,
    /// Whether the challenge has expired and a game not started by now never will.
    pub challenge_expired: bool,
}

/// Active Lichess games that have a Lichess game, least recently looked at first (they all are, in id order).
pub async fn in_play(pool: &PgPool, limit: i64) -> Result<Vec<InPlay>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT g.id, g.lichess_game_id, wl.lichess_id AS white, bl.lichess_id AS black, \
                COALESCE(g.lichess_challenge_expires_at < now() - interval '60 seconds', false) AS expired \
         FROM games g \
         JOIN lichess_links wl ON wl.user_id = g.white_id \
         JOIN lichess_links bl ON bl.user_id = g.black_id \
         WHERE g.venue = 'lichess' AND g.status = 'active' AND g.lichess_game_id IS NOT NULL \
         ORDER BY g.id LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| InPlay {
            id: r.get("id"),
            lichess_game_id: r.get("lichess_game_id"),
            white: r.get("white"),
            black: r.get("black"),
            challenge_expired: r.get("expired"),
        })
        .collect())
}
