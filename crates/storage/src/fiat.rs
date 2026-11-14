//! Fiat sessions: a player's visits to a Stellar anchor's screens to add money from, or cash out to, their own currency.
//! See the migration for how the money moves. The anchor's own transaction stays the anchor's; this is our record of it.

use sqlx::{PgPool, Row};
use uuid::Uuid;

/// Most sessions a player may have open at once, so opening screens cannot be used to flood the anchor or us.
pub const MAX_OPEN_PER_PLAYER: i64 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Deposit,
    Withdraw,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Deposit => "deposit",
            Kind::Withdraw => "withdraw",
        }
    }
    fn parse(s: &str) -> Kind {
        if s == "withdraw" {
            Kind::Withdraw
        } else {
            Kind::Deposit
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    InProgress,
    Sending,
    Completed,
    Failed,
    Expired,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::InProgress => "in_progress",
            Status::Sending => "sending",
            Status::Completed => "completed",
            Status::Failed => "failed",
            Status::Expired => "expired",
        }
    }
    fn parse(s: &str) -> Status {
        match s {
            "sending" => Status::Sending,
            "completed" => Status::Completed,
            "failed" => Status::Failed,
            "expired" => Status::Expired,
            _ => Status::InProgress,
        }
    }
    pub fn is_final(self) -> bool {
        matches!(self, Status::Completed | Status::Failed | Status::Expired)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: Uuid,
    pub user_id: Uuid,
    pub kind: Kind,
    pub anchor_domain: String,
    pub anchor_tx_id: String,
    pub status: Status,
    pub anchor_status: Option<String>,
    pub amount_in_minor: Option<i64>,
    pub message: Option<String>,
    pub more_info_url: Option<String>,
    pub withdrawal_id: Option<Uuid>,
    pub created_at_ms: i64,
}

#[derive(Debug)]
pub enum FiatError {
    /// The player already has the most open sessions allowed.
    TooManyOpen,
    NotFound,
    Database(sqlx::Error),
}

impl From<sqlx::Error> for FiatError {
    fn from(e: sqlx::Error) -> Self {
        FiatError::Database(e)
    }
}

impl std::fmt::Display for FiatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FiatError::TooManyOpen => {
                write!(f, "you already have {MAX_OPEN_PER_PLAYER} open sessions")
            }
            FiatError::NotFound => write!(f, "no such session"),
            FiatError::Database(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for FiatError {}

const COLS: &str = "id, user_id, kind, anchor_domain, anchor_tx_id, status, anchor_status, amount_in_minor, message, more_info_url, withdrawal_id, (extract(epoch FROM created_at) * 1000)::bigint AS created_ms";

fn row(r: &sqlx::postgres::PgRow) -> Session {
    Session {
        id: r.get("id"),
        user_id: r.get("user_id"),
        kind: Kind::parse(&r.get::<String, _>("kind")),
        anchor_domain: r.get("anchor_domain"),
        anchor_tx_id: r.get("anchor_tx_id"),
        status: Status::parse(&r.get::<String, _>("status")),
        anchor_status: r.get("anchor_status"),
        amount_in_minor: r.get("amount_in_minor"),
        message: r.get("message"),
        more_info_url: r.get("more_info_url"),
        withdrawal_id: r.get("withdrawal_id"),
        created_at_ms: r.get("created_ms"),
    }
}

/// Whether the player may open another session now. Checked before the anchor is asked to open one.
pub async fn check_can_open(pool: &PgPool, user: Uuid) -> Result<(), FiatError> {
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM fiat_sessions WHERE user_id = $1 AND finished_at IS NULL",
    )
    .bind(user)
    .fetch_one(pool)
    .await?;
    if open >= MAX_OPEN_PER_PLAYER {
        return Err(FiatError::TooManyOpen);
    }
    Ok(())
}

/// Records a session the anchor has opened. The same anchor transaction is never recorded twice.
pub async fn create(
    pool: &PgPool,
    user: Uuid,
    kind: Kind,
    anchor_domain: &str,
    anchor_tx_id: &str,
) -> Result<Session, FiatError> {
    check_can_open(pool, user).await?;
    let r = sqlx::query(&format!(
        "INSERT INTO fiat_sessions (id, user_id, kind, anchor_domain, anchor_tx_id) VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (anchor_domain, anchor_tx_id) DO UPDATE SET updated_at = fiat_sessions.updated_at \
         WHERE fiat_sessions.user_id = $2 RETURNING {COLS}"
    ))
    .bind(Uuid::now_v7())
    .bind(user)
    .bind(kind.as_str())
    .bind(anchor_domain)
    .bind(anchor_tx_id)
    .fetch_optional(pool)
    .await?;
    r.map(|r| row(&r)).ok_or(FiatError::NotFound)
}

/// A player's own session.
pub async fn get(pool: &PgPool, user: Uuid, id: Uuid) -> Result<Session, FiatError> {
    sqlx::query(&format!(
        "SELECT {COLS} FROM fiat_sessions WHERE id = $1 AND user_id = $2"
    ))
    .bind(id)
    .bind(user)
    .fetch_optional(pool)
    .await?
    .map(|r| row(&r))
    .ok_or(FiatError::NotFound)
}

/// A player's recent sessions, newest first.
pub async fn of_user(pool: &PgPool, user: Uuid, limit: i64) -> Result<Vec<Session>, FiatError> {
    let rows = sqlx::query(&format!(
        "SELECT {COLS} FROM fiat_sessions WHERE user_id = $1 ORDER BY created_at DESC, id DESC LIMIT $2"
    ))
    .bind(user)
    .bind(limit.clamp(1, 50))
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row).collect())
}

/// Sessions the worker still has to look after, least recently looked at first.
pub async fn open(pool: &PgPool, limit: i64) -> Result<Vec<Session>, FiatError> {
    let rows = sqlx::query(&format!(
        "SELECT {COLS} FROM fiat_sessions WHERE finished_at IS NULL ORDER BY updated_at, id LIMIT $1"
    ))
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row).collect())
}

/// What the anchor said, and what that makes of the session. A final session is never changed again.
pub async fn record(
    pool: &PgPool,
    id: Uuid,
    status: Status,
    anchor_status: &str,
    amount_in_minor: Option<i64>,
    message: Option<&str>,
    more_info_url: Option<&str>,
) -> Result<bool, FiatError> {
    let done = sqlx::query(
        "UPDATE fiat_sessions SET status = $2, anchor_status = $3, \
             amount_in_minor = COALESCE($4, amount_in_minor), message = COALESCE($5, message), \
             more_info_url = COALESCE($6, more_info_url), updated_at = now(), \
             finished_at = CASE WHEN $2 IN ('completed', 'failed', 'expired') THEN now() ELSE NULL END \
         WHERE id = $1 AND finished_at IS NULL",
    )
    .bind(id)
    .bind(status.as_str())
    .bind(anchor_status)
    .bind(amount_in_minor)
    .bind(message.map(|m| m.chars().take(500).collect::<String>()))
    .bind(more_info_url.map(|m| m.chars().take(500).collect::<String>()))
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Notes that the worker looked at the session and found nothing new, so it moves to the back of the queue.
pub async fn touch(pool: &PgPool, id: Uuid) -> Result<(), FiatError> {
    sqlx::query(
        "UPDATE fiat_sessions SET updated_at = now() WHERE id = $1 AND finished_at IS NULL",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// A cash-out's withdrawal has been made: remember it, once, and mark the session as sending.
pub async fn attach_withdrawal(
    pool: &PgPool,
    id: Uuid,
    withdrawal: Uuid,
) -> Result<bool, FiatError> {
    let done = sqlx::query(
        "UPDATE fiat_sessions SET withdrawal_id = $2, status = 'sending', updated_at = now() \
         WHERE id = $1 AND kind = 'withdraw' AND withdrawal_id IS NULL AND finished_at IS NULL",
    )
    .bind(id)
    .bind(withdrawal)
    .execute(pool)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Sessions the player left unfinished for longer than `hours` are given up, so they stop being looked at.
pub async fn expire_stale(pool: &PgPool, hours: i32) -> Result<u64, FiatError> {
    let done = sqlx::query(
        "UPDATE fiat_sessions SET status = 'expired', finished_at = now(), updated_at = now(), \
             message = COALESCE(message, 'not finished in time') \
         WHERE finished_at IS NULL AND withdrawal_id IS NULL AND created_at < now() - make_interval(hours => $1)",
    )
    .bind(hours)
    .execute(pool)
    .await?;
    Ok(done.rows_affected())
}
