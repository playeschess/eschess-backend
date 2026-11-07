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

/// The role that may see and decide fair-play cases.
pub const REVIEWER_ROLE: &str = "fairplay_reviewer";

pub async fn is_reviewer(pool: &PgPool, user: Uuid) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM user_roles WHERE user_id = $1 AND role = $2)")
        .bind(user)
        .bind(REVIEWER_ROLE)
        .fetch_one(pool)
        .await
}

/// A case in the queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseSummary {
    pub id: Uuid,
    pub player_id: Uuid,
    pub username: String,
    pub status: String,
    pub opened_at_ms: i64,
    pub games: i64,
    /// The highest score among the case's games.
    pub top_score: u8,
    pub decided_by: Option<Uuid>,
    pub decided_at_ms: Option<i64>,
    pub reason: Option<String>,
}

const CASE_SUMMARY: &str = "SELECT c.id, c.player_id, u.username, c.status, \
     (extract(epoch FROM c.opened_at) * 1000)::bigint AS opened_at_ms, c.decided_by, \
     (extract(epoch FROM c.decided_at) * 1000)::bigint AS decided_at_ms, c.reason, \
     (SELECT count(DISTINCT cg.game_id) FROM fairplay_case_games cg WHERE cg.case_id = c.id) AS games, \
     (SELECT COALESCE(max(s.score), 0::smallint) FROM fairplay_case_games cg JOIN fairplay_scores s \
         ON (s.game_id, s.player_id, s.scorer) = (cg.game_id, cg.player_id, cg.scorer) WHERE cg.case_id = c.id) AS top_score \
     FROM fairplay_cases c JOIN users u ON u.id = c.player_id";

fn case_summary(r: &sqlx::postgres::PgRow) -> CaseSummary {
    CaseSummary {
        id: r.get("id"),
        player_id: r.get("player_id"),
        username: r.get("username"),
        status: r.get("status"),
        opened_at_ms: r.get("opened_at_ms"),
        games: r.get("games"),
        top_score: r.get::<i16, _>("top_score") as u8,
        decided_by: r.get("decided_by"),
        decided_at_ms: r.get("decided_at_ms"),
        reason: r.get("reason"),
    }
}

/// Cases in one state, oldest first, leaving out any case `viewer` may not review (about them, or about a game they
/// played in).
pub async fn cases_for_reviewer(
    pool: &PgPool,
    viewer: Uuid,
    status: &str,
    limit: i64,
) -> Result<Vec<CaseSummary>, sqlx::Error> {
    let rows = sqlx::query(&format!(
        "{CASE_SUMMARY} WHERE c.status = $1 AND c.player_id <> $2 AND NOT EXISTS ( \
             SELECT 1 FROM fairplay_case_games cg JOIN games g ON g.id = cg.game_id \
              WHERE cg.case_id = c.id AND $2 IN (g.white_id, g.black_id)) \
         ORDER BY c.opened_at, c.id LIMIT $3"
    ))
    .bind(status)
    .bind(viewer)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(case_summary).collect())
}

/// One game in a case: the whole record, and every score the player got for it.
#[derive(Debug, Clone, PartialEq)]
pub struct CaseGame {
    pub record: GameRecord,
    pub scores: Vec<StoredScore>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CaseDetail {
    pub case: CaseSummary,
    pub games: Vec<CaseGame>,
}

/// Whether `viewer` played in any game of the case (or is the player it is about).
async fn involved(pool: &PgPool, case: Uuid, viewer: Uuid) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM fairplay_cases WHERE id = $1 AND player_id = $2) \
             OR EXISTS (SELECT 1 FROM fairplay_case_games cg JOIN games g ON g.id = cg.game_id \
                         WHERE cg.case_id = $1 AND $2 IN (g.white_id, g.black_id))",
    )
    .bind(case)
    .bind(viewer)
    .fetch_one(pool)
    .await
}

/// Everything a reviewer needs to decide: the case, each game's moves and clocks, and the scorers' summaries and
/// evidence. `None` if there is no such case **or `viewer` may not review it**: they are not told which.
pub async fn case_for_reviewer(
    pool: &PgPool,
    case: Uuid,
    viewer: Uuid,
) -> Result<Option<CaseDetail>, RecordError> {
    let Some(r) = sqlx::query(&format!("{CASE_SUMMARY} WHERE c.id = $1"))
        .bind(case)
        .fetch_optional(pool)
        .await?
    else {
        return Ok(None);
    };
    if involved(pool, case, viewer).await? {
        return Ok(None);
    }
    let summary = case_summary(&r);
    let game_ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT game_id FROM fairplay_case_games WHERE case_id = $1 GROUP BY game_id ORDER BY min(added_at), game_id",
    )
    .bind(case)
    .fetch_all(pool)
    .await?;
    let mut games = Vec::with_capacity(game_ids.len());
    for id in game_ids {
        let record = game_record(pool, id).await?.ok_or_else(|| {
            RecordError::Inconsistent(format!("game {id} is in a case but is not finished"))
        })?;
        let scores = scores_of_game(pool, id)
            .await?
            .into_iter()
            .filter(|s| s.player_id == summary.player_id)
            .collect();
        games.push(CaseGame { record, scores });
    }
    Ok(Some(CaseDetail {
        case: summary,
        games,
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Nothing wrong was found.
    Clear,
    /// Unfair play was found. In this baseline it is recorded and does nothing else (ADR 0002 §6).
    Confirm,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Clear => "cleared",
            Decision::Confirm => "confirmed",
        }
    }
}

/// What a confirmed case does to the player (ADR 0003 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sanction {
    /// Recorded; restricts nothing.
    Warning,
    /// No rated or staked play for this many days (1 to 365).
    Suspension { days: u32 },
    /// Proposed by the confirming reviewer; takes effect only when a second reviewer approves it.
    Closure,
}

pub const MAX_SUSPENSION_DAYS: u32 = 365;

#[derive(Debug)]
pub enum DecideError {
    /// Confirming needs a sanction, clearing must not have one, and a suspension is 1 to 365 days.
    BadSanction(&'static str),
    /// No such case, or the person may not review it (no role, or involved): they are not told which.
    NotFound,
    /// Already decided; a decision is final.
    AlreadyDecided(String),
    /// The reason is empty or longer than 2,000 characters.
    InvalidReason,
    /// The case's games are not the ones the reviewer says they looked at (a game was added since): look again.
    CaseChanged,
    Database(sqlx::Error),
}

impl std::fmt::Display for DecideError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecideError::NotFound => write!(f, "no such case"),
            DecideError::AlreadyDecided(s) => write!(f, "the case is already {s}"),
            DecideError::BadSanction(why) => write!(f, "{why}"),
            DecideError::InvalidReason => write!(f, "a reason of 1 to 2000 characters is required"),
            DecideError::CaseChanged => write!(f, "the case has changed since it was looked at"),
            DecideError::Database(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for DecideError {}

impl From<sqlx::Error> for DecideError {
    fn from(e: sqlx::Error) -> Self {
        DecideError::Database(e)
    }
}

/// A reviewer clears or confirms an open case, with a written reason. Final: a decided case never changes.
///
/// `seen` is the games the reviewer looked at. A worker can add a game to an open case at any moment, so the decision
/// is refused ([`DecideError::CaseChanged`]) unless `seen` is exactly the case's games: nobody clears evidence they
/// never saw. The case is locked while this is checked, so no game can be added in between (one that arrives after the
/// decision opens a new case).
pub async fn decide(
    pool: &PgPool,
    case: Uuid,
    reviewer: Uuid,
    decision: Decision,
    reason: &str,
    seen: &[Uuid],
    sanction: Option<Sanction>,
) -> Result<CaseSummary, DecideError> {
    match (decision, sanction) {
        (Decision::Confirm, None) => {
            return Err(DecideError::BadSanction(
                "confirming a case needs a sanction",
            ))
        }
        (Decision::Clear, Some(_)) => {
            return Err(DecideError::BadSanction("clearing a case has no sanction"))
        }
        (_, Some(Sanction::Suspension { days })) if !(1..=MAX_SUSPENSION_DAYS).contains(&days) => {
            return Err(DecideError::BadSanction("a suspension is 1 to 365 days"))
        }
        _ => {}
    }
    let reason = reason.trim();
    if reason.is_empty() || reason.chars().count() > 2_000 {
        return Err(DecideError::InvalidReason);
    }
    let mut tx = pool.begin().await?;
    let Some(status) = sqlx::query_scalar::<_, String>(
        "SELECT status FROM fairplay_cases WHERE id = $1 FOR UPDATE",
    )
    .bind(case)
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Err(DecideError::NotFound);
    };
    // Checked inside the lock, so a game added just before cannot slip past the "played in it" rule.
    let allowed: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM user_roles WHERE user_id = $2 AND role = $3) \
         AND NOT EXISTS (SELECT 1 FROM fairplay_cases WHERE id = $1 AND player_id = $2) \
         AND NOT EXISTS (SELECT 1 FROM fairplay_case_games cg JOIN games g ON g.id = cg.game_id \
                          WHERE cg.case_id = $1 AND $2 IN (g.white_id, g.black_id))",
    )
    .bind(case)
    .bind(reviewer)
    .bind(REVIEWER_ROLE)
    .fetch_one(&mut *tx)
    .await?;
    if !allowed {
        return Err(DecideError::NotFound);
    }
    if status != "open" {
        return Err(DecideError::AlreadyDecided(status));
    }
    let mut games: Vec<Uuid> =
        sqlx::query_scalar("SELECT DISTINCT game_id FROM fairplay_case_games WHERE case_id = $1")
            .bind(case)
            .fetch_all(&mut *tx)
            .await?;
    let mut seen = seen.to_vec();
    games.sort();
    seen.sort();
    seen.dedup();
    if games != seen {
        return Err(DecideError::CaseChanged);
    }
    sqlx::query(
        "UPDATE fairplay_cases SET status = $2, decided_by = $3, decided_at = now(), reason = $4 WHERE id = $1",
    )
    .bind(case)
    .bind(decision.as_str())
    .bind(reviewer)
    .bind(reason)
    .execute(&mut *tx)
    .await?;
    if let Some(sanction) = sanction {
        let days = match sanction {
            Sanction::Warning | Sanction::Closure => None,
            Sanction::Suspension { days } => Some(days as i32),
        };
        let kind = match sanction {
            Sanction::Warning => "warning",
            Sanction::Suspension { .. } => "suspension",
            Sanction::Closure => "closure",
        };
        sqlx::query(
            "INSERT INTO fairplay_sanctions (id, case_id, player_id, kind, ends_at) \
             SELECT $1, c.id, c.player_id, $2, CASE WHEN $3::int IS NULL THEN NULL ELSE now() + make_interval(days => $3::int) END \
             FROM fairplay_cases c WHERE c.id = $4",
        )
        .bind(Uuid::now_v7())
        .bind(kind)
        .bind(days)
        .bind(case)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    let r = sqlx::query(&format!("{CASE_SUMMARY} WHERE c.id = $1"))
        .bind(case)
        .fetch_one(pool)
        .await?;
    Ok(case_summary(&r))
}

/// When the player's rated play is suspended until (ADR 0003 §3), as text for the player, or `None` if it is not. A
/// warning restricts nothing. Where several suspensions overlap, the latest end counts.
pub async fn suspended_until<'e>(
    db: impl sqlx::PgExecutor<'e>,
    player: Uuid,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT to_char(max(ends_at) AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI \"UTC\"') \
         FROM fairplay_restrictions WHERE player_id = $1 AND kind = 'suspension'",
    )
    .bind(player)
    .fetch_one(db)
    .await
}

/// Whether the account was closed (a closure approved by a second reviewer). Permanent in this version.
pub async fn is_closed<'e>(
    db: impl sqlx::PgExecutor<'e>,
    player: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM fairplay_restrictions WHERE player_id = $1 AND kind = 'closure')",
    )
    .bind(player)
    .fetch_one(db)
    .await
}

/// A closure waiting for its second reviewer.
#[derive(Debug, Clone)]
pub struct PendingClosure {
    pub case_id: Uuid,
    pub player_id: Uuid,
    pub username: String,
    pub proposed_by: Uuid,
    pub reason: String,
}

/// Closures proposed and not yet reviewed, oldest first.
pub async fn pending_closures(pool: &PgPool) -> Result<Vec<PendingClosure>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT c.id, c.player_id, u.username, c.decided_by, c.reason FROM fairplay_sanctions z \
         JOIN fairplay_cases c ON c.id = z.case_id JOIN users u ON u.id = c.player_id \
         WHERE z.kind = 'closure' AND NOT EXISTS (SELECT 1 FROM fairplay_closure_reviews r WHERE r.sanction_id = z.id) \
         ORDER BY z.imposed_at, z.id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| PendingClosure {
            case_id: r.get("id"),
            player_id: r.get("player_id"),
            username: r.get("username"),
            proposed_by: r.get("decided_by"),
            reason: r.get("reason"),
        })
        .collect())
}

#[derive(Debug)]
pub enum ClosureError {
    /// No such pending closure, or the person may not review it (not a reviewer, the proposer, involved): not told which.
    NotFound,
    InvalidReason,
    Database(sqlx::Error),
}

impl std::fmt::Display for ClosureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClosureError::NotFound => write!(f, "no such closure to review"),
            ClosureError::InvalidReason => {
                write!(f, "a reason of 1 to 2000 characters is required")
            }
            ClosureError::Database(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for ClosureError {}

impl From<sqlx::Error> for ClosureError {
    fn from(e: sqlx::Error) -> Self {
        ClosureError::Database(e)
    }
}

/// A second reviewer approves or declines the closure proposed with `case`. Approving closes the account at once:
/// their open games are cancelled, their live games resigned (the opponents win), and they leave the queue. Declining
/// leaves the case confirmed with no restriction.
pub async fn review_closure(
    pool: &PgPool,
    case: Uuid,
    reviewer: Uuid,
    approve: bool,
    reason: &str,
) -> Result<(), ClosureError> {
    let reason = reason.trim();
    if reason.is_empty() || reason.chars().count() > 2_000 {
        return Err(ClosureError::InvalidReason);
    }
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT 1 FROM fairplay_cases WHERE id = $1 FOR UPDATE")
        .bind(case)
        .fetch_optional(&mut *tx)
        .await?;
    let row: Option<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT z.id, z.player_id FROM fairplay_sanctions z WHERE z.case_id = $1 AND z.kind = 'closure' \
         AND NOT EXISTS (SELECT 1 FROM fairplay_closure_reviews r WHERE r.sanction_id = z.id) \
         AND EXISTS (SELECT 1 FROM user_roles WHERE user_id = $2 AND role = $3) \
         AND $2 <> z.player_id AND $2 <> (SELECT decided_by FROM fairplay_cases WHERE id = $1) \
         AND NOT EXISTS (SELECT 1 FROM fairplay_case_games cg JOIN games g ON g.id = cg.game_id \
                          WHERE cg.case_id = $1 AND $2 IN (g.white_id, g.black_id))",
    )
    .bind(case)
    .bind(reviewer)
    .bind(REVIEWER_ROLE)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((sanction, player)) = row else {
        return Err(ClosureError::NotFound);
    };
    sqlx::query("INSERT INTO fairplay_closure_reviews (sanction_id, reviewer_id, decision, reason) VALUES ($1, $2, $3, $4)")
        .bind(sanction)
        .bind(reviewer)
        .bind(if approve { "approved" } else { "declined" })
        .bind(reason)
        .execute(&mut *tx)
        .await?;
    if approve {
        sqlx::query("DELETE FROM match_queue WHERE user_id = $1 AND game_id IS NULL")
            .bind(player)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    if approve {
        end_games_of(pool, player).await?;
    }
    Ok(())
}

/// Cancels the closed player's open games and resigns their live ones. Best effort per game: a game that ended in the
/// meantime is already over, which is the goal.
async fn end_games_of(pool: &PgPool, player: Uuid) -> Result<(), sqlx::Error> {
    let games: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id, status FROM games WHERE status IN ('waiting', 'active') \
         AND (created_by = $1 OR white_id = $1 OR black_id = $1)",
    )
    .bind(player)
    .fetch_all(pool)
    .await?;
    for (id, status) in games {
        let _ = if status == "waiting" {
            crate::games::cancel_game(pool, id, player)
                .await
                .map(|_| ())
        } else {
            crate::games::resign(pool, id, player).await.map(|_| ())
        };
    }
    Ok(())
}

/// A sanction as its player sees it: the kind and dates only, never the case behind it.
#[derive(Debug, Clone)]
pub struct MySanction {
    pub id: Uuid,
    pub kind: String,
    pub imposed_at_ms: i64,
    pub ends_at_ms: Option<i64>,
    /// Restricting the player right now.
    pub in_force: bool,
    /// `pending`, `upheld` or `overturned`; `None` if not appealed.
    pub appeal: Option<String>,
    /// A closure still waiting for its second reviewer.
    pub awaiting_second_reviewer: bool,
}

pub async fn my_sanctions(pool: &PgPool, player: Uuid) -> Result<Vec<MySanction>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT z.id, z.kind, (extract(epoch FROM z.imposed_at) * 1000)::bigint AS imposed_at_ms, \
                (extract(epoch FROM z.ends_at) * 1000)::bigint AS ends_at_ms, \
                EXISTS (SELECT 1 FROM fairplay_restrictions r WHERE r.sanction_id = z.id) AS in_force, \
                (SELECT CASE WHEN d.decision IS NULL THEN 'pending' ELSE d.decision END FROM fairplay_appeals a \
                   LEFT JOIN fairplay_appeal_decisions d ON d.appeal_id = a.id WHERE a.sanction_id = z.id) AS appeal, \
                (z.kind = 'closure' AND NOT EXISTS (SELECT 1 FROM fairplay_closure_reviews cr WHERE cr.sanction_id = z.id)) AS waiting \
         FROM fairplay_sanctions z WHERE z.player_id = $1 ORDER BY z.imposed_at DESC, z.id",
    )
    .bind(player)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| MySanction {
            id: r.get("id"),
            kind: r.get("kind"),
            imposed_at_ms: r.get("imposed_at_ms"),
            ends_at_ms: r.get("ends_at_ms"),
            in_force: r.get("in_force"),
            appeal: r.get("appeal"),
            awaiting_second_reviewer: r.get("waiting"),
        })
        .collect())
}

#[derive(Debug)]
pub enum AppealError {
    /// Not their sanction, a warning, or not restricting them (over, overturned, or a closure not yet approved).
    NotFound,
    AlreadyAppealed,
    InvalidStatement,
    Database(sqlx::Error),
}

impl std::fmt::Display for AppealError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppealError::NotFound => write!(f, "no such sanction to appeal"),
            AppealError::AlreadyAppealed => write!(
                f,
                "this sanction was already appealed; an appeal is made once"
            ),
            AppealError::InvalidStatement => {
                write!(f, "a statement of 1 to 4000 characters is required")
            }
            AppealError::Database(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for AppealError {}

impl From<sqlx::Error> for AppealError {
    fn from(e: sqlx::Error) -> Self {
        AppealError::Database(e)
    }
}

/// The player appeals a sanction that is restricting them, once, in writing.
pub async fn file_appeal(
    pool: &PgPool,
    player: Uuid,
    sanction: Uuid,
    statement: &str,
) -> Result<Uuid, AppealError> {
    let statement = statement.trim();
    if statement.is_empty() || statement.chars().count() > 4_000 {
        return Err(AppealError::InvalidStatement);
    }
    let in_force: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM fairplay_restrictions WHERE sanction_id = $1 AND player_id = $2)",
    )
    .bind(sanction)
    .bind(player)
    .fetch_one(pool)
    .await?;
    if !in_force {
        // An appealed-and-upheld sanction is still in force; say "already" for it rather than "no such".
        let appealed: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM fairplay_appeals WHERE sanction_id = $1 AND player_id = $2)",
        )
        .bind(sanction)
        .bind(player)
        .fetch_one(pool)
        .await?;
        return Err(if appealed {
            AppealError::AlreadyAppealed
        } else {
            AppealError::NotFound
        });
    }
    let id = Uuid::now_v7();
    match sqlx::query("INSERT INTO fairplay_appeals (id, sanction_id, player_id, statement) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(sanction)
        .bind(player)
        .bind(statement)
        .execute(pool)
        .await
    {
        Ok(_) => Ok(id),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Err(AppealError::AlreadyAppealed),
        Err(e) => Err(e.into()),
    }
}

#[derive(Debug, Clone)]
pub struct PendingAppeal {
    pub id: Uuid,
    pub case_id: Uuid,
    pub player_id: Uuid,
    pub username: String,
    pub kind: String,
    pub statement: String,
    pub filed_at_ms: i64,
}

/// Appeals nobody has decided, oldest first.
pub async fn pending_appeals(pool: &PgPool) -> Result<Vec<PendingAppeal>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT a.id, z.case_id, a.player_id, u.username, z.kind, a.statement, \
                (extract(epoch FROM a.filed_at) * 1000)::bigint AS filed_at_ms \
         FROM fairplay_appeals a JOIN fairplay_sanctions z ON z.id = a.sanction_id JOIN users u ON u.id = a.player_id \
         WHERE NOT EXISTS (SELECT 1 FROM fairplay_appeal_decisions d WHERE d.appeal_id = a.id) \
         ORDER BY a.filed_at, a.id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| PendingAppeal {
            id: r.get("id"),
            case_id: r.get("case_id"),
            player_id: r.get("player_id"),
            username: r.get("username"),
            kind: r.get("kind"),
            statement: r.get("statement"),
            filed_at_ms: r.get("filed_at_ms"),
        })
        .collect())
}

/// A reviewer who had no part in the sanction upholds or overturns it. Overturning lifts the restriction at once (the
/// sanction stays on record). Refused, without saying why, to the player, whoever confirmed the case or reviewed its
/// closure, and anyone who played in its games.
pub async fn decide_appeal(
    pool: &PgPool,
    appeal: Uuid,
    reviewer: Uuid,
    overturn: bool,
    reason: &str,
) -> Result<(), ClosureError> {
    let reason = reason.trim();
    if reason.is_empty() || reason.chars().count() > 2_000 {
        return Err(ClosureError::InvalidReason);
    }
    let mut tx = pool.begin().await?;
    let allowed: bool = sqlx::query_scalar(
        "SELECT EXISTS ( \
           SELECT 1 FROM fairplay_appeals a JOIN fairplay_sanctions z ON z.id = a.sanction_id \
             JOIN fairplay_cases c ON c.id = z.case_id \
           WHERE a.id = $1 \
             AND NOT EXISTS (SELECT 1 FROM fairplay_appeal_decisions d WHERE d.appeal_id = a.id) \
             AND EXISTS (SELECT 1 FROM user_roles WHERE user_id = $2 AND role = $3) \
             AND $2 NOT IN (z.player_id, c.decided_by) \
             AND NOT EXISTS (SELECT 1 FROM fairplay_closure_reviews r WHERE r.sanction_id = z.id AND r.reviewer_id = $2) \
             AND NOT EXISTS (SELECT 1 FROM fairplay_case_games cg JOIN games g ON g.id = cg.game_id \
                              WHERE cg.case_id = c.id AND $2 IN (g.white_id, g.black_id)))",
    )
    .bind(appeal)
    .bind(reviewer)
    .bind(REVIEWER_ROLE)
    .fetch_one(&mut *tx)
    .await?;
    if !allowed {
        return Err(ClosureError::NotFound);
    }
    match sqlx::query("INSERT INTO fairplay_appeal_decisions (appeal_id, reviewer_id, decision, reason) VALUES ($1, $2, $3, $4)")
        .bind(appeal)
        .bind(reviewer)
        .bind(if overturn { "overturned" } else { "upheld" })
        .bind(reason)
        .execute(&mut *tx)
        .await
    {
        Ok(_) => {}
        // Two reviewers at once: the second finds it decided.
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => return Err(ClosureError::NotFound),
        Err(e) => return Err(e.into()),
    }
    tx.commit().await?;
    Ok(())
}
