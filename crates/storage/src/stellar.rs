//! Stellar deposits in Postgres: deposit ids, the record of every payment seen, and crediting players through the ledger.
//!
//! The order of events for a payment is what keeps money safe when things crash or repeat:
//!   1. record it (the operation id is the primary key, so it can be recorded only once),
//!   2. post it to the ledger under an idempotency key derived from the same operation id (so it can be posted only once),
//!   3. mark it credited.
//!
//! A crash between any two steps is repaired by simply handling the same payment again.

use std::fmt;

use eschess_domain::money::AssetId;
use eschess_ledger::{AccountId, Entry, IdempotencyKey, Posting, Side};
use eschess_stellar::deposit::deposit_id_of;
use eschess_stellar::{IncomingPayment, Memo};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::ledger::{self, LedgerStoreError};

/// The ledger account that mirrors what the platform wallet holds on Stellar, one per asset.
pub const CUSTODY_KIND: &str = "custody_stellar";

#[derive(Debug)]
pub enum StellarStoreError {
    Database(sqlx::Error),
    Ledger(LedgerStoreError),
    /// A posting could not be built (a bug: the amounts are positive and balanced by construction).
    Posting(String),
}

impl fmt::Display for StellarStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StellarStoreError::Database(e) => write!(f, "database error: {e}"),
            StellarStoreError::Ledger(e) => write!(f, "ledger error: {e}"),
            StellarStoreError::Posting(e) => write!(f, "could not build the ledger posting: {e}"),
        }
    }
}

impl std::error::Error for StellarStoreError {}

impl From<sqlx::Error> for StellarStoreError {
    fn from(e: sqlx::Error) -> Self {
        StellarStoreError::Database(e)
    }
}

impl From<LedgerStoreError> for StellarStoreError {
    fn from(e: LedgerStoreError) -> Self {
        StellarStoreError::Ledger(e)
    }
}

type Result<T> = std::result::Result<T, StellarStoreError>;

/// The player's deposit id, created the first time it is asked for.
pub async fn deposit_id(pool: &PgPool, user: Uuid) -> Result<i64> {
    sqlx::query("INSERT INTO deposit_ids (user_id) VALUES ($1) ON CONFLICT DO NOTHING")
        .bind(user)
        .execute(pool)
        .await?;
    Ok(
        sqlx::query_scalar("SELECT deposit_id FROM deposit_ids WHERE user_id = $1")
            .bind(user)
            .fetch_one(pool)
            .await?,
    )
}

/// The player whose own wallet address this is, if it is one (ADR 0006).
async fn user_for_own_address(pool: &PgPool, address: &str) -> Result<Option<Uuid>> {
    Ok(
        sqlx::query_scalar("SELECT user_id FROM user_wallet_keys WHERE address = $1")
            .bind(address)
            .fetch_optional(pool)
            .await?,
    )
}

async fn user_for_deposit_id(pool: &PgPool, id: i64) -> Result<Option<Uuid>> {
    Ok(
        sqlx::query_scalar("SELECT user_id FROM deposit_ids WHERE deposit_id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

/// The player's wallet account for an asset, created if they do not have one yet (players who signed up before an
/// asset was supported get theirs the first time it is needed).
pub async fn user_wallet(pool: &PgPool, user: Uuid, asset: &AssetId) -> Result<AccountId> {
    sqlx::query("INSERT INTO ledger_accounts (id, kind, asset, normal_side, allow_negative, owner_id) VALUES ($1, 'user_wallet', $2, 'credit', FALSE, $3) ON CONFLICT DO NOTHING")
        .bind(Uuid::now_v7())
        .bind(asset.to_string())
        .bind(user)
        .execute(pool)
        .await?;
    let id: Uuid = sqlx::query_scalar("SELECT id FROM ledger_accounts WHERE owner_id = $1 AND kind = 'user_wallet' AND asset = $2")
        .bind(user)
        .bind(asset.to_string())
        .fetch_one(pool)
        .await?;
    Ok(AccountId(id))
}

/// A platform-owned account (no owner) of this kind and asset, created if needed. Platform assets are debit-normal and
/// may not go below zero; platform liabilities are credit-normal.
pub async fn system_account(
    pool: &PgPool,
    kind: &str,
    asset: &AssetId,
    normal_side: Side,
) -> Result<AccountId> {
    sqlx::query("INSERT INTO ledger_accounts (id, kind, asset, normal_side, allow_negative, owner_id) VALUES ($1, $2, $3, $4, FALSE, NULL) ON CONFLICT DO NOTHING")
        .bind(Uuid::now_v7())
        .bind(kind)
        .bind(asset.to_string())
        .bind(normal_side.as_str())
        .execute(pool)
        .await?;
    let id: Uuid = sqlx::query_scalar(
        "SELECT id FROM ledger_accounts WHERE owner_id IS NULL AND kind = $1 AND asset = $2 ORDER BY id LIMIT 1",
    )
    .bind(kind)
    .bind(asset.to_string())
    .fetch_one(pool)
    .await?;
    Ok(AccountId(id))
}

/// What happened to a payment that was handled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The player's wallet was credited by this call.
    Credited(Uuid),
    /// Already dealt with earlier (credited, or set aside for review); nothing changed.
    AlreadyHandled,
    /// Set aside for a person to look at, with the reason.
    NeedsReview(String),
}

fn memo_parts(memo: &Memo) -> (Option<&'static str>, Option<String>) {
    match memo {
        Memo::None => (None, None),
        Memo::Text(t) => (Some("text"), Some(t.clone())),
        Memo::Id(i) => (Some("id"), Some(i.to_string())),
        Memo::Hash(h) => (Some("hash"), Some(h.clone())),
        Memo::Return(r) => (Some("return"), Some(r.clone())),
    }
}

/// Records a payment that arrived at the wallet and credits the player it is for, exactly once however many times it
/// is handled. `accepted` are the assets players may deposit; anything else is set aside for review.
pub async fn handle_payment(
    pool: &PgPool,
    p: &IncomingPayment,
    accepted: &[AssetId],
) -> Result<Outcome> {
    // Decide who it is for, or why it cannot be credited.
    let decision: std::result::Result<Uuid, String> = if !accepted.contains(&p.asset) {
        Err(format!(
            "{} is not an asset deposits are accepted in",
            p.asset
        ))
    } else if let Some(owner) = user_for_own_address(pool, &p.to).await? {
        // Money sent to a player's own address belongs to that player, whatever memo or muxed id it carries.
        Ok(owner)
    } else {
        match deposit_id_of(p) {
            Err(reason) => Err(reason),
            Ok(id) => match user_for_deposit_id(pool, id).await? {
                Some(user) => Ok(user),
                None => Err(format!("no player has deposit id {id}")),
            },
        }
    };
    let (memo_type, memo) = memo_parts(&p.memo);
    let inserted = sqlx::query(
        "INSERT INTO stellar_deposits (op_id, tx_hash, asset, amount_minor, from_account, to_account, mux_id, memo_type, memo, user_id, status, review_reason, chain_time)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) ON CONFLICT (op_id) DO NOTHING",
    )
    .bind(&p.op_id)
    .bind(&p.tx_hash)
    .bind(p.asset.to_string())
    .bind(p.amount.minor())
    .bind(&p.from)
    .bind(&p.to)
    .bind(p.mux_id.map(|m| m as i64))
    .bind(memo_type)
    .bind(memo)
    .bind(decision.as_ref().ok())
    .bind(if decision.is_ok() { "pending" } else { "needs_review" })
    .bind(decision.as_ref().err())
    .bind(&p.created_at)
    .execute(pool)
    .await?
    .rows_affected();

    // Whether this call recorded it or an earlier one did, carry on from what is stored.
    let row = sqlx::query("SELECT status, user_id FROM stellar_deposits WHERE op_id = $1")
        .bind(&p.op_id)
        .fetch_one(pool)
        .await?;
    let (status, user): (String, Option<Uuid>) = (row.get("status"), row.get("user_id"));
    match status.as_str() {
        "pending" => {
            let user = user.expect("a pending deposit always has a player");
            credit(pool, p, user).await?;
            Ok(Outcome::Credited(user))
        }
        "needs_review" if inserted == 1 => {
            Ok(Outcome::NeedsReview(decision.err().unwrap_or_default()))
        }
        _ => Ok(Outcome::AlreadyHandled),
    }
}

async fn credit(pool: &PgPool, p: &IncomingPayment, user: Uuid) -> Result<()> {
    let custody = system_account(pool, CUSTODY_KIND, &p.asset, Side::Debit).await?;
    let wallet = user_wallet(pool, user, &p.asset).await?;
    let posting = Posting::new(
        IdempotencyKey::new(&format!("stellar-deposit:{}", p.op_id))
            .map_err(|e| StellarStoreError::Posting(e.to_string()))?,
        "deposit",
        Some(("stellar_deposit", &p.op_id)),
        vec![
            Entry::debit(custody, p.amount.clone()),
            Entry::credit(wallet, p.amount.clone()),
        ],
    )
    .map_err(|e| StellarStoreError::Posting(e.to_string()))?;
    ledger::post(pool, &posting).await?;
    sqlx::query("UPDATE stellar_deposits SET status = 'credited', credited_at = now() WHERE op_id = $1 AND status = 'pending'")
        .bind(&p.op_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Sets aside a record that looked like money for us but could not be read.
pub async fn record_unreadable(pool: &PgPool, op_id: &str, reason: &str) -> Result<()> {
    sqlx::query("INSERT INTO stellar_deposits (op_id, status, review_reason) VALUES ($1, 'needs_review', $2) ON CONFLICT (op_id) DO NOTHING")
        .bind(op_id)
        .bind(reason)
        .execute(pool)
        .await?;
    Ok(())
}

/// Where the named reader has got to (a Horizon paging token).
pub async fn cursor(pool: &PgPool, name: &str) -> Result<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT paging_token FROM stellar_cursors WHERE name = $1")
            .bind(name)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn set_cursor(pool: &PgPool, name: &str, token: &str) -> Result<()> {
    sqlx::query("INSERT INTO stellar_cursors (name, paging_token) VALUES ($1, $2) ON CONFLICT (name) DO UPDATE SET paging_token = $2, updated_at = now()")
        .bind(name)
        .bind(token)
        .execute(pool)
        .await?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepositRow {
    pub op_id: String,
    pub asset: Option<String>,
    pub amount_minor: Option<i64>,
    pub status: String,
    pub review_reason: Option<String>,
    pub chain_time: Option<String>,
}

fn deposit_row(r: &sqlx::postgres::PgRow) -> DepositRow {
    DepositRow {
        op_id: r.get("op_id"),
        asset: r.get("asset"),
        amount_minor: r.get("amount_minor"),
        status: r.get("status"),
        review_reason: r.get("review_reason"),
        chain_time: r.get("chain_time"),
    }
}

/// A player's own deposits, newest first.
pub async fn deposits_of(pool: &PgPool, user: Uuid, limit: i64) -> Result<Vec<DepositRow>> {
    let rows = sqlx::query("SELECT op_id, asset, amount_minor, status, review_reason, chain_time FROM stellar_deposits WHERE user_id = $1 ORDER BY observed_at DESC, op_id DESC LIMIT $2")
        .bind(user)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(deposit_row).collect())
}

/// Everything waiting for a person to look at, oldest first.
pub async fn needing_review(pool: &PgPool) -> Result<Vec<DepositRow>> {
    let rows = sqlx::query("SELECT op_id, asset, amount_minor, status, review_reason, chain_time FROM stellar_deposits WHERE status = 'needs_review' ORDER BY observed_at, op_id")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(deposit_row).collect())
}
