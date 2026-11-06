//! Withdrawals in Postgres (ADR 0001): the intent, its attempts, and the three ledger postings (hold, settle, refund).
//!
//! Every function that moves money changes the withdrawal's status **and** posts to the ledger in one database
//! transaction, so they cannot disagree. Every function locks the withdrawal's row first, so two callers acting on one
//! withdrawal (a worker and an approver, two workers) take turns. The migration enforces the same rules again:
//! states move only forward, one hold, one outcome (settle or refund), and one unresolved attempt at a time.
//!
//! Nothing here talks to Stellar. The worker builds and submits transactions and reports what happened through
//! [`record_attempt`], [`attempt_landed`], [`attempt_failed`], [`attempt_expired`] and [`give_up`].

use std::fmt;

use eschess_domain::money::{Amount, AssetId};
use eschess_ledger::{AccountId, Entry, IdempotencyKey, Posting, Side};
use eschess_stellar::tx::PaymentMemo;
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::ledger::{self, LedgerStoreError};
use crate::stellar::{system_account, user_wallet, StellarStoreError, CUSTODY_KIND};

/// The ledger account holding money that has left players' wallets and not yet left the platform's Stellar wallet.
/// A platform liability (credit-normal): its balance is the total of withdrawals that are held and not finished.
pub const PENDING_KIND: &str = "platform_withdrawals_pending";

/// The role that may approve or reject withdrawals above the automatic threshold.
pub const APPROVER_ROLE: &str = "withdrawal_approver";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    AwaitingApproval,
    Approved,
    Submitting,
    Confirmed,
    Failed,
    Cancelled,
    Rejected,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::AwaitingApproval => "awaiting_approval",
            Status::Approved => "approved",
            Status::Submitting => "submitting",
            Status::Confirmed => "confirmed",
            Status::Failed => "failed",
            Status::Cancelled => "cancelled",
            Status::Rejected => "rejected",
        }
    }

    pub fn parse(s: &str) -> Option<Status> {
        Some(match s {
            "awaiting_approval" => Status::AwaitingApproval,
            "approved" => Status::Approved,
            "submitting" => Status::Submitting,
            "confirmed" => Status::Confirmed,
            "failed" => Status::Failed,
            "cancelled" => Status::Cancelled,
            "rejected" => Status::Rejected,
            _ => return None,
        })
    }

    /// Finished: nothing more will happen to it.
    pub fn is_final(self) -> bool {
        matches!(
            self,
            Status::Confirmed | Status::Failed | Status::Cancelled | Status::Rejected
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Withdrawal {
    pub id: Uuid,
    pub user_id: Uuid,
    pub client_request_id: Uuid,
    pub amount: Amount,
    /// A `G...` or `M...` address.
    pub destination: String,
    pub memo: PaymentMemo,
    pub status: Status,
    pub decided_by: Option<Uuid>,
    pub failure_reason: Option<String>,
    pub requested_at_ms: i64,
    pub finished_at_ms: Option<i64>,
}

/// What a player asks for. The caller has already checked the destination is a valid address that is not ours.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewWithdrawal {
    pub user_id: Uuid,
    pub client_request_id: Uuid,
    pub amount: Amount,
    pub destination: String,
    pub memo: PaymentMemo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptStatus {
    Pending,
    Success,
    Failed,
    Expired,
}

impl AttemptStatus {
    fn parse(s: &str) -> AttemptStatus {
        match s {
            "pending" => AttemptStatus::Pending,
            "success" => AttemptStatus::Success,
            "failed" => AttemptStatus::Failed,
            _ => AttemptStatus::Expired,
        }
    }
}

/// A signed transaction, recorded **before** it is submitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAttempt {
    /// Lower-case hex.
    pub tx_hash: String,
    pub source_account: String,
    pub sequence: i64,
    pub fee_stroops: i64,
    /// Unix seconds.
    pub max_time: i64,
    pub envelope_xdr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    pub id: Uuid,
    pub withdrawal_id: Uuid,
    pub attempt_no: i32,
    pub tx_hash: String,
    pub source_account: String,
    pub sequence: i64,
    pub max_time: i64,
    pub envelope_xdr: String,
    pub status: AttemptStatus,
    pub result_code: Option<String>,
    pub fee_charged_stroops: Option<i64>,
}

#[derive(Debug)]
pub enum WithdrawalError {
    /// There is no such withdrawal (or it belongs to someone else: the caller is not told which).
    NotFound,
    /// The player's wallet does not hold enough.
    InsufficientFunds,
    /// This `client_request_id` was already used for a withdrawal with different details.
    RequestIdReused,
    /// The withdrawal is not in a state that allows this.
    WrongState(Status),
    /// The person is not a withdrawal approver.
    NotApprover,
    /// Nobody approves or rejects their own withdrawal.
    OwnWithdrawal,
    /// An earlier attempt might still land; a new transaction must wait until it is resolved.
    AttemptStillOpen,
    /// The attempt is already resolved differently.
    AttemptResolved(AttemptStatus),
    /// More than the player may still withdraw in the last 24 hours.
    DailyLimit {
        remaining_minor: i64,
    },
    /// Giving up is only possible when no attempt landed and none can still land.
    CannotGiveUp,
    Ledger(LedgerStoreError),
    Database(sqlx::Error),
}

impl fmt::Display for WithdrawalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WithdrawalError::NotFound => write!(f, "no such withdrawal"),
            WithdrawalError::InsufficientFunds => write!(f, "insufficient funds"),
            WithdrawalError::RequestIdReused => write!(
                f,
                "this client_request_id was already used for a different withdrawal"
            ),
            WithdrawalError::WrongState(s) => {
                write!(f, "the withdrawal is {} and cannot do that", s.as_str())
            }
            WithdrawalError::NotApprover => write!(f, "not a withdrawal approver"),
            WithdrawalError::OwnWithdrawal => {
                write!(f, "nobody approves or rejects their own withdrawal")
            }
            WithdrawalError::AttemptStillOpen => {
                write!(f, "an earlier attempt is not resolved and might still land")
            }
            WithdrawalError::AttemptResolved(s) => write!(f, "the attempt is already {s:?}"),
            WithdrawalError::CannotGiveUp => {
                write!(f, "cannot give up: an attempt landed or might still land")
            }
            WithdrawalError::DailyLimit { remaining_minor } => write!(
                f,
                "over the daily limit: {remaining_minor} (minor units) may still be withdrawn today"
            ),
            WithdrawalError::Ledger(e) => write!(f, "ledger error: {e}"),
            WithdrawalError::Database(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for WithdrawalError {}

impl From<sqlx::Error> for WithdrawalError {
    fn from(e: sqlx::Error) -> Self {
        if let sqlx::Error::Database(db) = &e {
            if db.constraint() == Some("withdrawal_attempts_one_open") {
                return WithdrawalError::AttemptStillOpen;
            }
        }
        match LedgerStoreError::from(e) {
            LedgerStoreError::Database(e) => WithdrawalError::Database(e),
            other => WithdrawalError::from(other),
        }
    }
}

impl From<LedgerStoreError> for WithdrawalError {
    fn from(e: LedgerStoreError) -> Self {
        match e {
            LedgerStoreError::InsufficientFunds => WithdrawalError::InsufficientFunds,
            other => WithdrawalError::Ledger(other),
        }
    }
}

impl From<StellarStoreError> for WithdrawalError {
    fn from(e: StellarStoreError) -> Self {
        match e {
            StellarStoreError::Database(e) => WithdrawalError::from(e),
            StellarStoreError::Ledger(e) => WithdrawalError::from(e),
            StellarStoreError::Posting(e) => WithdrawalError::Database(sqlx::Error::Protocol(e)),
        }
    }
}

type Result<T> = std::result::Result<T, WithdrawalError>;

const COLUMNS: &str = "id, user_id, client_request_id, asset, amount_minor, destination, memo_type, memo, status, \
     decided_by, failure_reason, (extract(epoch FROM requested_at) * 1000)::bigint AS requested_at_ms, \
     (extract(epoch FROM finished_at) * 1000)::bigint AS finished_at_ms";

const ATTEMPT_COLUMNS: &str =
    "id, withdrawal_id, attempt_no, tx_hash, source_account, sequence, max_time, \
     envelope_xdr, status, result_code, fee_charged_stroops";

fn memo_to_db(memo: &PaymentMemo) -> (&'static str, Option<String>) {
    let hex = |b: &[u8; 32]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    match memo {
        PaymentMemo::None => ("none", None),
        PaymentMemo::Id(id) => ("id", Some(id.to_string())),
        PaymentMemo::Text(t) => ("text", Some(t.clone())),
        PaymentMemo::Hash(h) => ("hash", Some(hex(h))),
        PaymentMemo::Return(h) => ("return", Some(hex(h))),
    }
}

fn memo_from_db(kind: &str, value: Option<&str>) -> PaymentMemo {
    let bytes = |s: &str| -> [u8; 32] {
        let v: Vec<u8> = (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("written by memo_to_db"))
            .collect();
        v.try_into().expect("written by memo_to_db")
    };
    let value = value.unwrap_or_default();
    match kind {
        "id" => PaymentMemo::Id(value.parse().expect("written by memo_to_db")),
        "text" => PaymentMemo::Text(value.to_string()),
        "hash" => PaymentMemo::Hash(bytes(value)),
        "return" => PaymentMemo::Return(bytes(value)),
        _ => PaymentMemo::None,
    }
}

fn row(r: &PgRow) -> Withdrawal {
    let asset = AssetId::parse(r.get::<&str, _>("asset")).expect("written from a valid AssetId");
    Withdrawal {
        id: r.get("id"),
        user_id: r.get("user_id"),
        client_request_id: r.get("client_request_id"),
        amount: Amount::new(r.get("amount_minor"), asset)
            .expect("the table refuses amounts below one"),
        destination: r.get("destination"),
        memo: memo_from_db(r.get("memo_type"), r.get("memo")),
        status: Status::parse(r.get("status")).expect("the table allows only known statuses"),
        decided_by: r.get("decided_by"),
        failure_reason: r.get("failure_reason"),
        requested_at_ms: r.get("requested_at_ms"),
        finished_at_ms: r.get("finished_at_ms"),
    }
}

fn attempt_row(r: &PgRow) -> Attempt {
    Attempt {
        id: r.get("id"),
        withdrawal_id: r.get("withdrawal_id"),
        attempt_no: r.get("attempt_no"),
        tx_hash: r.get("tx_hash"),
        source_account: r.get("source_account"),
        sequence: r.get("sequence"),
        max_time: r.get("max_time"),
        envelope_xdr: r.get("envelope_xdr"),
        status: AttemptStatus::parse(r.get("status")),
        result_code: r.get("result_code"),
        fee_charged_stroops: r.get("fee_charged_stroops"),
    }
}

/// The withdrawal, locked until the caller's transaction ends.
async fn lock(tx: &mut PgConnection, id: Uuid) -> Result<Withdrawal> {
    let r = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM withdrawals WHERE id = $1 FOR UPDATE"
    ))
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(WithdrawalError::NotFound)?;
    Ok(row(&r))
}

async fn set_status(
    tx: &mut PgConnection,
    id: Uuid,
    status: Status,
    reason: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "UPDATE withdrawals SET status = $2, failure_reason = COALESCE($3, failure_reason), \
         finished_at = CASE WHEN $4 THEN now() ELSE finished_at END WHERE id = $1",
    )
    .bind(id)
    .bind(status.as_str())
    .bind(reason)
    .bind(status.is_final())
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// The accounts a withdrawal's postings use: the player's wallet, the pending account, and custody.
struct Accounts {
    wallet: AccountId,
    pending: AccountId,
    custody: AccountId,
}

/// Creates the accounts if they do not exist yet. Called before a request's transaction begins (creating an account
/// is idempotent, and harmless if the request then fails).
async fn ensure_accounts(pool: &PgPool, user: Uuid, asset: &AssetId) -> Result<()> {
    user_wallet(pool, user, asset).await?;
    system_account(pool, PENDING_KIND, asset, Side::Credit).await?;
    system_account(pool, CUSTODY_KIND, asset, Side::Debit).await?;
    Ok(())
}

/// The accounts, read on the transaction's own connection. Reaching for a second connection from the pool while
/// holding this one would let a burst of requests take every connection and then wait forever for another.
async fn accounts(tx: &mut PgConnection, user: Uuid, asset: &AssetId) -> Result<Accounts> {
    let ids: Vec<(String, Uuid)> = sqlx::query_as(
        "SELECT kind, id FROM ledger_accounts WHERE asset = $2 \
         AND ((owner_id = $1 AND kind = 'user_wallet') OR (owner_id IS NULL AND kind IN ($3, $4)))",
    )
    .bind(user)
    .bind(asset.to_string())
    .bind(PENDING_KIND)
    .bind(CUSTODY_KIND)
    .fetch_all(&mut *tx)
    .await?;
    let find = |kind: &str| {
        ids.iter()
            .find(|(k, _)| k == kind)
            .map(|(_, id)| AccountId(*id))
            .ok_or_else(|| {
                WithdrawalError::Database(sqlx::Error::Protocol(format!(
                    "the {kind} account for {asset} does not exist (a withdrawal is held only after ensure_accounts)"
                )))
            })
    };
    Ok(Accounts {
        wallet: find("user_wallet")?,
        pending: find(PENDING_KIND)?,
        custody: find(CUSTODY_KIND)?,
    })
}

enum Step {
    Hold,
    Settle,
    Refund,
}

/// The posting for one step. Its idempotency key names the withdrawal and the step, so a step is posted once.
fn posting(w: &Withdrawal, step: Step, a: &Accounts) -> Result<Posting> {
    let (name, from, to) = match step {
        Step::Hold => ("hold", a.wallet, a.pending),
        Step::Settle => ("settle", a.pending, a.custody),
        Step::Refund => ("refund", a.pending, a.wallet),
    };
    let key = IdempotencyKey::new(&format!("wd:{}:{name}", w.id))
        .expect("a uuid and a step name make a valid key");
    let id = w.id.to_string();
    Posting::new(
        key,
        &format!("withdrawal_{name}"),
        Some(("withdrawal", &id)),
        vec![
            Entry::debit(from, w.amount.clone()),
            Entry::credit(to, w.amount.clone()),
        ],
    )
    .map_err(|e| WithdrawalError::Database(sqlx::Error::Protocol(e.to_string())))
}

async fn post_step(tx: &mut PgConnection, w: &Withdrawal, step: Step) -> Result<()> {
    let a = accounts(&mut *tx, w.user_id, w.amount.asset()).await?;
    ledger::post_in(tx, &posting(w, step, &a)?).await?;
    Ok(())
}

async fn commit(tx: sqlx::Transaction<'_, sqlx::Postgres>) -> Result<()> {
    tx.commit().await.map_err(WithdrawalError::from)
}

/// The limits a request is checked against, in the asset's minor units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// At or below this, a withdrawal is approved at once; above it, a person must approve it.
    pub auto_approve_up_to_minor: i64,
    /// The most a player may withdraw in any 24 hours (counting withdrawals that are not cancelled, rejected or failed).
    pub daily_limit_minor: i64,
}

/// Records a player's request and holds the money, in one transaction.
///
/// A retry with the same `client_request_id` and the same details returns the first withdrawal (`false`: not new),
/// even if the limits have been reached since; with different details it is refused. Requests from one player are
/// handled one at a time, so two at once cannot both slip under the daily limit.
pub async fn request(
    pool: &PgPool,
    new: &NewWithdrawal,
    policy: Policy,
) -> Result<(Withdrawal, bool)> {
    ensure_accounts(pool, new.user_id, new.amount.asset()).await?;
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(format!("withdrawals:{}", new.user_id))
        .execute(&mut *tx)
        .await?;

    let existing = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM withdrawals WHERE user_id = $1 AND client_request_id = $2"
    ))
    .bind(new.user_id)
    .bind(new.client_request_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(r) = existing {
        let w = row(&r);
        let same = w.amount == new.amount && w.destination == new.destination && w.memo == new.memo;
        return if same {
            Ok((w, false))
        } else {
            Err(WithdrawalError::RequestIdReused)
        };
    }

    let recent = withdrawn_recently(&mut *tx, new.user_id, new.amount.asset(), 24).await?;
    let remaining = (policy.daily_limit_minor - recent).max(0);
    if new.amount.minor() > remaining {
        return Err(WithdrawalError::DailyLimit {
            remaining_minor: remaining,
        });
    }

    let (memo_type, memo) = memo_to_db(&new.memo);
    let status = if new.amount.minor() <= policy.auto_approve_up_to_minor {
        Status::Approved
    } else {
        Status::AwaitingApproval
    };
    let r = sqlx::query(&format!(
        "INSERT INTO withdrawals (id, user_id, client_request_id, asset, amount_minor, destination, memo_type, memo, status) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING {COLUMNS}"
    ))
    .bind(Uuid::now_v7())
    .bind(new.user_id)
    .bind(new.client_request_id)
    .bind(new.amount.asset().to_string())
    .bind(new.amount.minor())
    .bind(&new.destination)
    .bind(memo_type)
    .bind(&memo)
    .bind(status.as_str())
    .fetch_one(&mut *tx)
    .await?;
    let w = row(&r);
    post_step(&mut tx, &w, Step::Hold).await?;
    commit(tx).await?;
    Ok((w, true))
}

const RECENT_TOTAL: &str = "SELECT COALESCE(sum(amount_minor), 0)::bigint FROM withdrawals WHERE user_id = $1 AND asset = $2 \
     AND requested_at > now() - make_interval(hours => $3) AND status NOT IN ('cancelled', 'rejected', 'failed')";

/// The player withdraws their request. Only before anything is signed (awaiting approval, or approved and not yet
/// sent); the money goes back to their wallet.
pub async fn cancel(pool: &PgPool, id: Uuid, user: Uuid) -> Result<Withdrawal> {
    let mut tx = pool.begin().await?;
    let w = lock(&mut tx, id).await?;
    if w.user_id != user {
        return Err(WithdrawalError::NotFound);
    }
    if !matches!(w.status, Status::AwaitingApproval | Status::Approved) {
        return Err(WithdrawalError::WrongState(w.status));
    }
    set_status(&mut tx, id, Status::Cancelled, None).await?;
    post_step(&mut tx, &w, Step::Refund).await?;
    commit(tx).await?;
    get(pool, id).await
}

async fn check_approver(tx: &mut PgConnection, w: &Withdrawal, person: Uuid) -> Result<()> {
    if !has_role(&mut *tx, person, APPROVER_ROLE).await? {
        return Err(WithdrawalError::NotApprover);
    }
    if w.user_id == person {
        return Err(WithdrawalError::OwnWithdrawal);
    }
    if w.status != Status::AwaitingApproval {
        return Err(WithdrawalError::WrongState(w.status));
    }
    Ok(())
}

/// An approver lets a withdrawal above the threshold go ahead.
pub async fn approve(pool: &PgPool, id: Uuid, approver: Uuid) -> Result<Withdrawal> {
    let mut tx = pool.begin().await?;
    let w = lock(&mut tx, id).await?;
    check_approver(&mut tx, &w, approver).await?;
    sqlx::query("UPDATE withdrawals SET status = 'approved', decided_by = $2, decided_at = now() WHERE id = $1")
        .bind(id)
        .bind(approver)
        .execute(&mut *tx)
        .await?;
    commit(tx).await?;
    get(pool, id).await
}

/// An approver turns a withdrawal down; the money goes back to the player.
pub async fn reject(pool: &PgPool, id: Uuid, approver: Uuid, reason: &str) -> Result<Withdrawal> {
    let mut tx = pool.begin().await?;
    let w = lock(&mut tx, id).await?;
    check_approver(&mut tx, &w, approver).await?;
    sqlx::query(
        "UPDATE withdrawals SET status = 'rejected', decided_by = $2, decided_at = now(), failure_reason = $3, \
         finished_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(approver)
    .bind(reason)
    .execute(&mut *tx)
    .await?;
    post_step(&mut tx, &w, Step::Refund).await?;
    commit(tx).await?;
    get(pool, id).await
}

/// Records a signed transaction for the withdrawal, **before** it is submitted. Refused while an earlier attempt is
/// unresolved (it might still land), and unless the withdrawal is approved or already being sent.
pub async fn record_attempt(pool: &PgPool, id: Uuid, a: &NewAttempt) -> Result<Attempt> {
    let mut tx = pool.begin().await?;
    let w = lock(&mut tx, id).await?;
    if !matches!(w.status, Status::Approved | Status::Submitting) {
        return Err(WithdrawalError::WrongState(w.status));
    }
    let landed: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM withdrawal_attempts WHERE withdrawal_id = $1 AND status = 'success')",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if landed {
        // Cannot happen while the state machine holds (a success confirms the withdrawal); refused regardless.
        return Err(WithdrawalError::WrongState(Status::Confirmed));
    }
    let r = sqlx::query(&format!(
        "INSERT INTO withdrawal_attempts (id, withdrawal_id, attempt_no, tx_hash, source_account, sequence, fee_stroops, max_time, envelope_xdr) \
         VALUES ($1, $2, (SELECT COALESCE(max(attempt_no), 0) + 1 FROM withdrawal_attempts WHERE withdrawal_id = $2), $3, $4, $5, $6, $7, $8) \
         RETURNING {ATTEMPT_COLUMNS}"
    ))
    .bind(Uuid::now_v7())
    .bind(id)
    .bind(&a.tx_hash)
    .bind(&a.source_account)
    .bind(a.sequence)
    .bind(a.fee_stroops)
    .bind(a.max_time)
    .bind(&a.envelope_xdr)
    .fetch_one(&mut *tx)
    .await?;
    if w.status == Status::Approved {
        set_status(&mut tx, id, Status::Submitting, None).await?;
    }
    commit(tx).await?;
    Ok(attempt_row(&r))
}

/// The attempt, locked, with its withdrawal locked first (always in that order, so callers cannot deadlock).
async fn lock_attempt(tx: &mut PgConnection, attempt: Uuid) -> Result<(Withdrawal, Attempt)> {
    let withdrawal_id: Uuid =
        sqlx::query_scalar("SELECT withdrawal_id FROM withdrawal_attempts WHERE id = $1")
            .bind(attempt)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(WithdrawalError::NotFound)?;
    let w = lock(&mut *tx, withdrawal_id).await?;
    let r = sqlx::query(&format!(
        "SELECT {ATTEMPT_COLUMNS} FROM withdrawal_attempts WHERE id = $1 FOR UPDATE"
    ))
    .bind(attempt)
    .fetch_one(&mut *tx)
    .await?;
    Ok((w, attempt_row(&r)))
}

async fn resolve(
    tx: &mut PgConnection,
    attempt: Uuid,
    status: &str,
    result_code: Option<&str>,
    fee_charged: Option<i64>,
) -> Result<()> {
    sqlx::query(
        "UPDATE withdrawal_attempts SET status = $2, result_code = $3, fee_charged_stroops = $4, resolved_at = now() \
         WHERE id = $1",
    )
    .bind(attempt)
    .bind(status)
    .bind(result_code)
    .bind(fee_charged)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// The attempt's transaction is on chain and paid: the withdrawal is confirmed and settled in the ledger. Reporting
/// the same success again changes nothing.
pub async fn attempt_landed(
    pool: &PgPool,
    attempt: Uuid,
    fee_charged_stroops: i64,
) -> Result<Withdrawal> {
    let mut tx = pool.begin().await?;
    let (w, a) = lock_attempt(&mut tx, attempt).await?;
    match a.status {
        AttemptStatus::Success => return get(pool, w.id).await,
        AttemptStatus::Pending => {}
        other => return Err(WithdrawalError::AttemptResolved(other)),
    }
    resolve(&mut tx, attempt, "success", None, Some(fee_charged_stroops)).await?;
    set_status(&mut tx, w.id, Status::Confirmed, None).await?;
    post_step(&mut tx, &w, Step::Settle).await?;
    commit(tx).await?;
    get(pool, w.id).await
}

/// The attempt's transaction is on chain but failed (it paid nothing and was charged its fee). The withdrawal stays
/// `submitting`: the caller decides whether a new attempt can succeed or to [`give_up`].
pub async fn attempt_failed(
    pool: &PgPool,
    attempt: Uuid,
    result_code: &str,
    fee_charged_stroops: i64,
) -> Result<Attempt> {
    let mut tx = pool.begin().await?;
    let (_, a) = lock_attempt(&mut tx, attempt).await?;
    match a.status {
        AttemptStatus::Failed => {}
        AttemptStatus::Pending => {
            resolve(
                &mut tx,
                attempt,
                "failed",
                Some(result_code),
                Some(fee_charged_stroops),
            )
            .await?
        }
        other => return Err(WithdrawalError::AttemptResolved(other)),
    }
    commit(tx).await?;
    get_attempt(pool, attempt).await
}

/// The attempt can never land: the network has closed a ledger after its `max_time` and its hash is not on chain. The
/// caller must have checked both; this is what allows a new attempt.
pub async fn attempt_expired(pool: &PgPool, attempt: Uuid) -> Result<Attempt> {
    let mut tx = pool.begin().await?;
    let (_, a) = lock_attempt(&mut tx, attempt).await?;
    match a.status {
        AttemptStatus::Expired => {}
        AttemptStatus::Pending => resolve(&mut tx, attempt, "expired", None, None).await?,
        other => return Err(WithdrawalError::AttemptResolved(other)),
    }
    commit(tx).await?;
    get_attempt(pool, attempt).await
}

/// Stops trying: the withdrawal fails and the money goes back to the player. Only when no attempt landed and none is
/// unresolved, so nothing can still pay out.
pub async fn give_up(pool: &PgPool, id: Uuid, reason: &str) -> Result<Withdrawal> {
    let mut tx = pool.begin().await?;
    let w = lock(&mut tx, id).await?;
    if w.status != Status::Submitting {
        return Err(WithdrawalError::WrongState(w.status));
    }
    let open_or_landed: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM withdrawal_attempts WHERE withdrawal_id = $1 AND status IN ('pending', 'success'))",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if open_or_landed {
        return Err(WithdrawalError::CannotGiveUp);
    }
    set_status(&mut tx, id, Status::Failed, Some(reason)).await?;
    post_step(&mut tx, &w, Step::Refund).await?;
    commit(tx).await?;
    get(pool, id).await
}

pub async fn get(pool: &PgPool, id: Uuid) -> Result<Withdrawal> {
    let r = sqlx::query(&format!("SELECT {COLUMNS} FROM withdrawals WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or(WithdrawalError::NotFound)?;
    Ok(row(&r))
}

/// A player's withdrawals, newest first.
pub async fn of_user(pool: &PgPool, user: Uuid, limit: i64) -> Result<Vec<Withdrawal>> {
    let rows = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM withdrawals WHERE user_id = $1 ORDER BY requested_at DESC, id DESC LIMIT $2"
    ))
    .bind(user)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row).collect())
}

/// Withdrawals in one state, oldest first (the approval queue, or the worker's to-do list).
pub async fn with_status(pool: &PgPool, status: Status, limit: i64) -> Result<Vec<Withdrawal>> {
    let rows = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM withdrawals WHERE status = $1 ORDER BY requested_at, id LIMIT $2"
    ))
    .bind(status.as_str())
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(row).collect())
}

/// How much of `asset` the player has asked to withdraw in the last `hours` hours, not counting withdrawals that were
/// cancelled, rejected or failed (that money never left). For the daily limit.
pub async fn withdrawn_recently<'e>(
    db: impl sqlx::PgExecutor<'e>,
    user: Uuid,
    asset: &AssetId,
    hours: i32,
) -> Result<i64> {
    Ok(sqlx::query_scalar(RECENT_TOTAL)
        .bind(user)
        .bind(asset.to_string())
        .bind(hours)
        .fetch_one(db)
        .await?)
}

pub async fn get_attempt(pool: &PgPool, attempt: Uuid) -> Result<Attempt> {
    let r = sqlx::query(&format!(
        "SELECT {ATTEMPT_COLUMNS} FROM withdrawal_attempts WHERE id = $1"
    ))
    .bind(attempt)
    .fetch_optional(pool)
    .await?
    .ok_or(WithdrawalError::NotFound)?;
    Ok(attempt_row(&r))
}

/// A withdrawal's attempts, first to last.
pub async fn attempts(pool: &PgPool, id: Uuid) -> Result<Vec<Attempt>> {
    let rows = sqlx::query(&format!(
        "SELECT {ATTEMPT_COLUMNS} FROM withdrawal_attempts WHERE withdrawal_id = $1 ORDER BY attempt_no"
    ))
    .bind(id)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(attempt_row).collect())
}

/// Every unresolved attempt, oldest first: the resolver's to-do list.
pub async fn pending_attempts(pool: &PgPool, limit: i64) -> Result<Vec<Attempt>> {
    let rows = sqlx::query(&format!(
        "SELECT {ATTEMPT_COLUMNS} FROM withdrawal_attempts WHERE status = 'pending' ORDER BY created_at, id LIMIT $1"
    ))
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.iter().map(attempt_row).collect())
}

pub async fn has_role<'e>(db: impl sqlx::PgExecutor<'e>, user: Uuid, role: &str) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM user_roles WHERE user_id = $1 AND role = $2)",
    )
    .bind(user)
    .bind(role)
    .fetch_one(db)
    .await?)
}

/// Gives someone a role. There is no endpoint for this on purpose: it is done by an operator.
pub async fn grant_role(pool: &PgPool, user: Uuid, role: &str) -> Result<()> {
    sqlx::query("INSERT INTO user_roles (user_id, role) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(user)
        .bind(role)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_memo_survives_the_database() {
        for memo in [
            PaymentMemo::None,
            PaymentMemo::Id(u64::MAX),
            PaymentMemo::Text("withdrawal 1".into()),
            PaymentMemo::Hash([0xab; 32]),
            PaymentMemo::Return([0x01; 32]),
        ] {
            let (kind, value) = memo_to_db(&memo);
            assert_eq!(memo_from_db(kind, value.as_deref()), memo);
        }
    }

    #[test]
    fn statuses_round_trip_and_only_four_are_final() {
        let all = [
            Status::AwaitingApproval,
            Status::Approved,
            Status::Submitting,
            Status::Confirmed,
            Status::Failed,
            Status::Cancelled,
            Status::Rejected,
        ];
        for s in all {
            assert_eq!(Status::parse(s.as_str()), Some(s));
        }
        assert_eq!(all.iter().filter(|s| s.is_final()).count(), 4);
    }
}
