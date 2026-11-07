//! Fair play in Postgres (ADR 0002): the record of a finished game a scorer is given, and storing what it returns (runs,
//! scores, and the review cases that high scores open).

use eschess_domain::chess::{Color, Position};
use eschess_fairplay::{GameRecord, MoveRecord, RiskScore, TimeControl, Transport};
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

/// What happened when a scorer's result was stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recorded {
    /// Stored now: this many scores, and the cases (opened or added to) for scores at or above the threshold.
    Scored { scores: usize, cases: Vec<Uuid> },
    /// The scores broke the rules ([`eschess_fairplay::check`]); the game is recorded as unscorable with the reason.
    Refused(String),
    /// This scorer had already looked at this game; nothing changed.
    AlreadyRun,
}

#[derive(Debug)]
pub enum FairplayStoreError {
    /// The case threshold must be 1 to 100 (0 would open a case for every game).
    InvalidThreshold(u8),
    Database(sqlx::Error),
}

impl std::fmt::Display for FairplayStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FairplayStoreError::InvalidThreshold(t) => {
                write!(f, "the case threshold must be 1 to 100, not {t}")
            }
            FairplayStoreError::Database(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for FairplayStoreError {}

impl From<sqlx::Error> for FairplayStoreError {
    fn from(e: sqlx::Error) -> Self {
        FairplayStoreError::Database(e)
    }
}

/// Finished games `scorer` has not looked at yet, oldest first.
///
/// This walks finished games against the runs index rather than keeping a cursor: games do not always finish in the
/// order their end is timestamped (two transactions can commit out of order), and a cursor would skip one for good.
pub async fn unscored_games(
    pool: &PgPool,
    scorer: &str,
    limit: i64,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT g.id FROM games g WHERE g.status = 'finished' \
         AND NOT EXISTS (SELECT 1 FROM fairplay_runs r WHERE r.game_id = g.id AND r.scorer = $1) \
         ORDER BY g.ended_at, g.id LIMIT $2",
    )
    .bind(scorer)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Records that `scorer` could not score the game, and why, so it is not retried and a person can look. Returns
/// `false` if the scorer had already looked at the game.
pub async fn record_unscorable(
    pool: &PgPool,
    game_id: Uuid,
    scorer: &str,
    reason: &str,
) -> Result<bool, sqlx::Error> {
    let done = sqlx::query(
        "INSERT INTO fairplay_runs (game_id, scorer, outcome, reason) VALUES ($1, $2, 'unscorable', $3) \
         ON CONFLICT DO NOTHING",
    )
    .bind(game_id)
    .bind(scorer)
    .bind(reason)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Stores what `scorer` returned for `game`, all in one transaction: the run, the scores, and for each score at or
/// above `case_threshold`, the game added to the player's open case (opening one if there is none).
///
/// Running again for the same game and scorer changes nothing ([`Recorded::AlreadyRun`]), even when two workers do it
/// at once. Scores that break the rules are not stored; the game is recorded as unscorable instead.
pub async fn record_scores(
    pool: &PgPool,
    game: &GameRecord,
    scorer: &str,
    scores: &[RiskScore],
    case_threshold: u8,
) -> Result<Recorded, FairplayStoreError> {
    if !(1..=eschess_fairplay::MAX_SCORE).contains(&case_threshold) {
        return Err(FairplayStoreError::InvalidThreshold(case_threshold));
    }
    if let Err(e) = eschess_fairplay::check(scorer, game, scores) {
        let reason = format!("the scorer returned scores that break the rules: {e}");
        return Ok(
            if record_unscorable(pool, game.game_id, scorer, &reason).await? {
                Recorded::Refused(reason)
            } else {
                Recorded::AlreadyRun
            },
        );
    }

    let mut tx = pool.begin().await?;
    // A second worker scoring the same game waits here until the first commits, then finds the run already there.
    let new = sqlx::query(
        "INSERT INTO fairplay_runs (game_id, scorer, outcome) VALUES ($1, $2, 'scored') ON CONFLICT DO NOTHING",
    )
    .bind(game.game_id)
    .bind(scorer)
    .execute(&mut *tx)
    .await?;
    if new.rows_affected() == 0 {
        return Ok(Recorded::AlreadyRun);
    }

    let mut cases = Vec::new();
    for s in scores {
        sqlx::query(
            "INSERT INTO fairplay_scores (game_id, player_id, scorer, score, summary, evidence) \
             VALUES ($1, $2, $3, $4, $5, $6::jsonb)",
        )
        .bind(game.game_id)
        .bind(s.player_id)
        .bind(scorer)
        .bind(s.score as i16)
        .bind(&s.summary)
        .bind(s.evidence.to_string())
        .execute(&mut *tx)
        .await?;
        if s.score < case_threshold {
            continue;
        }
        let case = open_case_for(&mut tx, s.player_id).await?;
        sqlx::query(
            "INSERT INTO fairplay_case_games (case_id, game_id, player_id, scorer) VALUES ($1, $2, $3, $4)",
        )
        .bind(case)
        .bind(game.game_id)
        .bind(s.player_id)
        .bind(scorer)
        .execute(&mut *tx)
        .await?;
        cases.push(case);
    }
    tx.commit().await?;
    Ok(Recorded::Scored {
        scores: scores.len(),
        cases,
    })
}

/// The player's open case, locked until the transaction ends (so a reviewer cannot close it while a game is being
/// added), opening one if there is none.
async fn open_case_for(tx: &mut sqlx::PgConnection, player: Uuid) -> Result<Uuid, sqlx::Error> {
    loop {
        if let Some(id) = sqlx::query_scalar(
            "SELECT id FROM fairplay_cases WHERE player_id = $1 AND status = 'open' FOR UPDATE",
        )
        .bind(player)
        .fetch_optional(&mut *tx)
        .await?
        {
            return Ok(id);
        }
        // Two workers can both find no open case; the unique index lets only one open it, and the other goes round
        // again and finds it.
        let opened: Option<Uuid> = sqlx::query_scalar(
            "INSERT INTO fairplay_cases (id, player_id) VALUES ($1, $2) \
             ON CONFLICT (player_id) WHERE status = 'open' DO NOTHING RETURNING id",
        )
        .bind(Uuid::now_v7())
        .bind(player)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(id) = opened {
            return Ok(id);
        }
    }
}

/// A score as stored.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredScore {
    pub game_id: Uuid,
    pub player_id: Uuid,
    pub scorer: String,
    pub score: u8,
    pub summary: String,
    pub evidence: serde_json::Value,
}

/// Every score for a game, by every scorer.
pub async fn scores_of_game(pool: &PgPool, game: Uuid) -> Result<Vec<StoredScore>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT game_id, player_id, scorer, score, summary, evidence::text AS evidence FROM fairplay_scores \
         WHERE game_id = $1 ORDER BY scorer, player_id",
    )
    .bind(game)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| StoredScore {
            game_id: r.get("game_id"),
            player_id: r.get("player_id"),
            scorer: r.get("scorer"),
            score: r.get::<i16, _>("score") as u8,
            summary: r.get("summary"),
            evidence: serde_json::from_str(r.get("evidence")).unwrap_or(serde_json::Value::Null),
        })
        .collect())
}

/// How a scorer's run on a game ended: `None` if it has not run, otherwise the outcome and, for an unscorable game, why.
pub async fn run_of(
    pool: &PgPool,
    game: Uuid,
    scorer: &str,
) -> Result<Option<(String, Option<String>)>, sqlx::Error> {
    sqlx::query_as("SELECT outcome, reason FROM fairplay_runs WHERE game_id = $1 AND scorer = $2")
        .bind(game)
        .bind(scorer)
        .fetch_optional(pool)
        .await
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Case {
    pub id: Uuid,
    pub player_id: Uuid,
    pub status: String,
    /// The games in the case, oldest addition first.
    pub games: Vec<Uuid>,
}

/// The player's open case, if they have one.
pub async fn open_case(pool: &PgPool, player: Uuid) -> Result<Option<Case>, sqlx::Error> {
    let Some(r) = sqlx::query(
        "SELECT id, player_id, status FROM fairplay_cases WHERE player_id = $1 AND status = 'open'",
    )
    .bind(player)
    .fetch_optional(pool)
    .await?
    else {
        return Ok(None);
    };
    let id: Uuid = r.get("id");
    let games = sqlx::query_scalar(
        "SELECT game_id FROM fairplay_case_games WHERE case_id = $1 GROUP BY game_id ORDER BY min(added_at), game_id",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    Ok(Some(Case {
        id,
        player_id: r.get("player_id"),
        status: r.get("status"),
        games,
    }))
}
