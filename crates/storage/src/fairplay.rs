//! Reading finished games for fair play (ADR 0002): everything a scorer is given, built from `games` and `game_moves`.

use eschess_domain::chess::{Color, Position};
use eschess_fairplay::{GameRecord, MoveRecord, TimeControl, Transport};
use eschess_game::{EndReason, GameResult};
use sqlx::{PgPool, Row};
use uuid::Uuid;

#[derive(Debug)]
pub enum RecordError {
    /// The stored game contradicts itself (a move by someone who was not to move, a gap in the plies, ...). Never
    /// expected; reported rather than scored, because a score built on it would mislead a reviewer.
    Inconsistent(String),
    Database(sqlx::Error),
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecordError::Inconsistent(m) => write!(f, "the stored game is inconsistent: {m}"),
            RecordError::Database(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for RecordError {}

impl From<sqlx::Error> for RecordError {
    fn from(e: sqlx::Error) -> Self {
        RecordError::Database(e)
    }
}

/// The record of a **finished** game, or `None` if there is no such game or it has not finished. Only finished games
/// are scored: a game in progress could still change, and scoring it would show a scorer half the evidence.
pub async fn game_record(pool: &PgPool, id: Uuid) -> Result<Option<GameRecord>, RecordError> {
    let Some(g) = sqlx::query(
        "SELECT white_id, black_id, base_ms, increment_ms, rated, result, end_reason FROM games \
         WHERE id = $1 AND status = 'finished'",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    else {
        return Ok(None);
    };
    let bad = |m: String| RecordError::Inconsistent(format!("game {id}: {m}"));
    let white_id: Uuid = g.get("white_id");
    let black_id: Uuid = g.get("black_id");
    let result_text: String = g.get("result");
    let result = GameResult::parse(&result_text)
        .ok_or_else(|| bad(format!("unknown result {result_text:?}")))?;
    let end_reason = g
        .get::<Option<String>, _>("end_reason")
        .and_then(|r| EndReason::parse(&r));

    let rows = sqlx::query(
        "SELECT ply, uci, fen_after, mover_id, charged_ms, credited_ms, remaining_ms_after, played_at_ms, transport, \
         lag_measured_ms FROM game_moves WHERE game_id = $1 ORDER BY ply",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;

    let mut moves = Vec::with_capacity(rows.len());
    let mut fen_before = Position::starting().to_fen();
    for (i, r) in rows.iter().enumerate() {
        let ply: i32 = r.get("ply");
        if ply != i as i32 + 1 {
            return Err(bad(format!("expected ply {}, found {ply}", i + 1)));
        }
        let mover = if ply % 2 == 1 {
            Color::White
        } else {
            Color::Black
        };
        let mover_id: Uuid = r.get("mover_id");
        let expected = match mover {
            Color::White => white_id,
            Color::Black => black_id,
        };
        if mover_id != expected {
            return Err(bad(format!("ply {ply} was played by someone not to move")));
        }
        let non_negative = |col: &str| -> Result<u64, RecordError> {
            let v: i64 = r.get(col);
            u64::try_from(v).map_err(|_| bad(format!("ply {ply}: {col} is negative")))
        };
        let fen_after: String = r.get("fen_after");
        moves.push(MoveRecord {
            ply: ply as u32,
            uci: r.get("uci"),
            fen_before: std::mem::replace(&mut fen_before, fen_after.clone()),
            fen_after,
            mover,
            think_ms: non_negative("charged_ms")?,
            lag_credited_ms: non_negative("credited_ms")?,
            lag_measured_ms: r.get::<Option<i64>, _>("lag_measured_ms").map(|v| v as u64),
            transport: Transport::from_db(r.get::<Option<&str>, _>("transport")),
            remaining_ms_after: non_negative("remaining_ms_after")?,
            played_at_ms: r.get("played_at_ms"),
        });
    }

    let base: i64 = g.get("base_ms");
    let increment: i64 = g.get("increment_ms");
    Ok(Some(GameRecord {
        game_id: id,
        white_id,
        black_id,
        time_control: TimeControl {
            base_ms: base as u64,
            increment_ms: increment as u64,
        },
        rated: g.get("rated"),
        result,
        end_reason,
        moves,
    }))
}
