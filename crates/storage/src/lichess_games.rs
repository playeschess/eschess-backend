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
    /// Our two players, and the clock the game was made with.
    pub white_user: Uuid,
    pub black_user: Uuid,
    pub increment_ms: i64,
    pub lichess_game_id: String,
    pub white: String,
    pub black: String,
    /// Whether the challenge has expired and a game not started by now never will.
    pub challenge_expired: bool,
}

/// Active Lichess games that have a Lichess game, least recently looked at first (they all are, in id order).
pub async fn in_play(pool: &PgPool, limit: i64) -> Result<Vec<InPlay>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT g.id, g.white_id, g.black_id, g.increment_ms, g.lichess_game_id, wl.lichess_id AS white, bl.lichess_id AS black, \
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
            white_user: r.get("white_id"),
            black_user: r.get("black_id"),
            increment_ms: r.get("increment_ms"),
            lichess_game_id: r.get("lichess_game_id"),
            white: r.get("white"),
            black: r.get("black"),
            challenge_expired: r.get("expired"),
        })
        .collect())
}

/// One move of a Lichess game, ready to store for scoring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedMove {
    pub ply: i32,
    pub uci: String,
    pub fen_after: String,
    /// The side that played it: White plays the odd plies.
    pub white_moved: bool,
    /// Time the mover spent thinking, from their clock. The first move of each side is free on Lichess.
    pub think_ms: i64,
    pub remaining_ms_after: i64,
    /// Time since the game began, as the sum of the thinking so far.
    pub played_at_ms: i64,
}

/// Turns the SAN moves and clock readings Lichess gave into moves we can score. Every move must be exactly one legal
/// move, and there must be one clock reading per move: anything else is an error, never a guess, so a score is never built
/// on a game we could not follow. `clocks_cs` are centiseconds, each after its move.
pub fn convert_moves(
    sans: &[String],
    clocks_cs: &[i64],
    increment_ms: i64,
) -> Result<Vec<ImportedMove>, String> {
    use eschess_domain::chess::Position;
    if clocks_cs.len() != sans.len() {
        return Err(format!(
            "{} moves but {} clock readings",
            sans.len(),
            clocks_cs.len()
        ));
    }
    let mut position = Position::starting();
    let mut out = Vec::with_capacity(sans.len());
    let mut elapsed = 0i64;
    for (i, san) in sans.iter().enumerate() {
        let m = position
            .move_from_san(san)
            .ok_or_else(|| format!("move {} ({san:?}) is not one legal move", i + 1))?;
        position = position.make_move(m);
        let remaining = clocks_cs[i]
            .checked_mul(10)
            .filter(|r| *r >= 0)
            .ok_or_else(|| format!("move {}: the clock reading is not usable", i + 1))?;
        // The same side's reading before this move is two plies back; the first move of each side costs nothing.
        let think = if i < 2 {
            0
        } else {
            (clocks_cs[i - 2] * 10 + increment_ms - remaining).max(0)
        };
        elapsed += think;
        out.push(ImportedMove {
            ply: i as i32 + 1,
            uci: m.to_uci(),
            fen_after: position.to_fen(),
            white_moved: i % 2 == 0,
            think_ms: think,
            remaining_ms_after: remaining,
            played_at_ms: elapsed,
        });
    }
    Ok(out)
}

/// Stores the moves of a Lichess game so the fair-play scorer can read them, once: a repeat adds nothing. The game must be
/// a Lichess game that is `active` or `finished`.
pub async fn import_moves(
    pool: &PgPool,
    game: Uuid,
    moves: &[ImportedMove],
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let Some(row) = sqlx::query(
        "SELECT white_id, black_id FROM games WHERE id = $1 AND venue = 'lichess' AND status IN ('active', 'finished') FOR UPDATE",
    )
    .bind(game)
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Err(sqlx::Error::RowNotFound);
    };
    let (white, black): (Uuid, Uuid) = (row.get("white_id"), row.get("black_id"));
    for m in moves {
        sqlx::query(
            "INSERT INTO game_moves (game_id, ply, uci, fen_after, mover_id, client_move_id, charged_ms, credited_ms, remaining_ms_after, played_at_ms) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, 0, $8, $9) ON CONFLICT (game_id, ply) DO NOTHING",
        )
        .bind(game)
        .bind(m.ply)
        .bind(&m.uci)
        .bind(&m.fen_after)
        .bind(if m.white_moved { white } else { black })
        .bind(Uuid::now_v7())
        .bind(m.think_ms)
        .bind(m.remaining_ms_after)
        .bind(m.played_at_ms)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sans(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn think_time_comes_from_the_clock_and_the_first_moves_are_free() {
        // 5+3: each side starts with 300 s and no one is charged for the first move. White then spends 5 s (the clock
        // shows 298 s after the 3 s increment); Black shows 301 s, so spent 2 s.
        let m = convert_moves(
            &sans("e4 e5 Nf3 Nc6"),
            &[30_000, 30_000, 29_800, 30_100],
            3_000,
        )
        .unwrap();
        assert_eq!(
            m.iter().map(|x| x.uci.as_str()).collect::<Vec<_>>(),
            ["e2e4", "e7e5", "g1f3", "b8c6"]
        );
        assert_eq!(
            m.iter().map(|x| x.think_ms).collect::<Vec<_>>(),
            [0, 0, 5_000, 2_000]
        );
        assert_eq!(
            m.iter().map(|x| x.white_moved).collect::<Vec<_>>(),
            [true, false, true, false]
        );
        assert_eq!(m[2].remaining_ms_after, 298_000);
        assert_eq!(m[3].played_at_ms, 7_000);
        assert!(m[0]
            .fen_after
            .starts_with("rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b"));
    }

    #[test]
    fn what_cannot_be_followed_is_an_error_not_a_guess() {
        assert!(convert_moves(&sans("e4 e5 Nf6"), &[1, 1, 1], 0)
            .unwrap_err()
            .contains("Nf6"));
        assert!(convert_moves(&sans("e4 e5"), &[1], 0)
            .unwrap_err()
            .contains("clock"));
        assert!(convert_moves(&sans("e4"), &[-5], 0).is_err());
        assert_eq!(convert_moves(&[], &[], 0).unwrap(), vec![]);
    }
}
