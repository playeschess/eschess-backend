//! Reading and writing the ledger. The database enforces the rules (see the ledger migration); this module
//! checks the same things first so callers get a clear error, and keeps the posting itself atomic.

use std::fmt;

use eschess_domain::money::AssetId;
use eschess_ledger::{AccountId, Posting, Side};
use sqlx::{PgConnection, PgExecutor, PgPool, Row};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct NewAccount {
    pub kind: String,
    pub asset: AssetId,
    /// Which side raises the balance: `Credit` for a customer's wallet, `Debit` for the platform's own assets.
    pub normal_side: Side,
    /// Only for accounts that may legitimately go below zero. Customer accounts never may.
    pub allow_negative: bool,
    pub owner_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostOutcome {
    /// Recorded now, as this transaction.
    Posted(Uuid),
    /// This idempotency key was already recorded with the same content; nothing changed.
    Replayed(Uuid),
}

#[derive(Debug)]
pub enum LedgerStoreError {
    UnknownAccount(AccountId),
    /// An entry's asset is not the account's asset.
    AssetMismatch(AccountId),
    InsufficientFunds,
    /// The key was already used for a different posting.
    IdempotencyConflict,
    /// The database refused an unbalanced transaction (the Rust check should make this unreachable).
    Unbalanced,
    Database(sqlx::Error),
}

impl fmt::Display for LedgerStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LedgerStoreError::UnknownAccount(a) => write!(f, "unknown account {a}"),
            LedgerStoreError::AssetMismatch(a) => {
                write!(f, "entry asset does not match account {a}")
            }
            LedgerStoreError::InsufficientFunds => write!(f, "insufficient funds"),
            LedgerStoreError::IdempotencyConflict => write!(
                f,
                "this idempotency key was already used for a different posting"
            ),
            LedgerStoreError::Unbalanced => {
                write!(f, "the database refused an unbalanced transaction")
            }
            LedgerStoreError::Database(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for LedgerStoreError {}

impl From<sqlx::Error> for LedgerStoreError {
    fn from(e: sqlx::Error) -> Self {
        if let sqlx::Error::Database(db) = &e {
            if db.constraint() == Some("ledger_balances_not_overdrawn") {
                return LedgerStoreError::InsufficientFunds;
            }
            if db.message().contains("is not balanced") || db.message().contains("has no entries") {
                return LedgerStoreError::Unbalanced;
            }
        }
        LedgerStoreError::Database(e)
    }
}

type Result<T> = std::result::Result<T, LedgerStoreError>;

/// Works on a pool or inside a transaction, so an account can be created atomically with whatever owns it.
pub async fn create_account<'e>(
    db: impl PgExecutor<'e>,
    account: &NewAccount,
) -> Result<AccountId> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO ledger_accounts (id, kind, asset, normal_side, allow_negative, owner_id) VALUES ($1,$2,$3,$4,$5,$6)")
        .bind(id)
        .bind(&account.kind)
        .bind(account.asset.to_string())
        .bind(account.normal_side.as_str())
        .bind(account.allow_negative)
        .bind(account.owner_id)
        .execute(db)
        .await?;
    Ok(AccountId(id))
}

/// Records a posting atomically: every entry and the balance changes they cause commit together or not at all.
///
/// The same key with the same content returns [`PostOutcome::Replayed`] and changes nothing, however many times
/// it is sent and even if copies arrive at once. The same key with different content is an error.
pub async fn post(pool: &PgPool, posting: &Posting) -> Result<PostOutcome> {
    let mut tx = pool.begin().await?;
    let outcome = post_in(&mut tx, posting).await?;
    tx.commit().await?; // the deferred balance check runs here
    Ok(outcome)
}

/// [`post`], inside a transaction the caller owns, so a posting can commit together with the change that causes it
/// (a withdrawal's status, say) or not at all. The deferred balance check runs when the caller commits; map an error
/// from that commit with `LedgerStoreError::from`.
pub async fn post_in(tx: &mut PgConnection, posting: &Posting) -> Result<PostOutcome> {
    // Copies of one request queue up here, so the second sees the first one's result.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(format!("ledger:{}", posting.key().as_str()))
        .execute(&mut *tx)
        .await?;

    let fingerprint = posting.fingerprint();
    if let Some(row) =
        sqlx::query("SELECT id, fingerprint FROM ledger_transactions WHERE idempotency_key = $1")
            .bind(posting.key().as_str())
            .fetch_optional(&mut *tx)
            .await?
    {
        let (id, stored): (Uuid, String) = (row.get("id"), row.get("fingerprint"));
        return if stored == fingerprint {
            Ok(PostOutcome::Replayed(id))
        } else {
            Err(LedgerStoreError::IdempotencyConflict)
        };
    }

    // Clear errors first; the database repeats these checks as a backstop.
    let ids: Vec<Uuid> = posting.entries().iter().map(|e| e.account.0).collect();
    let known: Vec<(Uuid, String)> =
        sqlx::query("SELECT id, asset FROM ledger_accounts WHERE id = ANY($1)")
            .bind(&ids)
            .fetch_all(&mut *tx)
            .await?
            .iter()
            .map(|r| (r.get("id"), r.get("asset")))
            .collect();
    for e in posting.entries() {
        match known.iter().find(|(id, _)| *id == e.account.0) {
            None => return Err(LedgerStoreError::UnknownAccount(e.account)),
            Some((_, asset)) if *asset != e.amount.asset().to_string() => {
                return Err(LedgerStoreError::AssetMismatch(e.account))
            }
            Some(_) => {}
        }
    }

    let transaction_id = Uuid::now_v7();
    let (reference_type, reference_id) = posting
        .reference()
        .map_or((None, None), |(t, i)| (Some(t), Some(i)));
    sqlx::query("INSERT INTO ledger_transactions (id, idempotency_key, fingerprint, kind, reference_type, reference_id) VALUES ($1,$2,$3,$4,$5,$6)")
        .bind(transaction_id)
        .bind(posting.key().as_str())
        .bind(&fingerprint)
        .bind(posting.kind())
        .bind(reference_type)
        .bind(reference_id)
        .execute(&mut *tx)
        .await?;

    // Always touch accounts in the same order, so two postings that involve the same accounts in opposite
    // directions queue up instead of deadlocking.
    let mut entries: Vec<_> = posting.entries().iter().collect();
    entries.sort_by_key(|e| (e.account, e.side));
    for e in entries {
        sqlx::query("INSERT INTO ledger_entries (transaction_id, account_id, asset, side, amount_minor) VALUES ($1,$2,$3,$4,$5)")
            .bind(transaction_id)
            .bind(e.account.0)
            .bind(e.amount.asset().to_string())
            .bind(e.side.as_str())
            .bind(e.amount.minor())
            .execute(&mut *tx)
            .await?;
    }
    Ok(PostOutcome::Posted(transaction_id))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Balance {
    pub asset: AssetId,
    /// Can be negative only for accounts created with `allow_negative`.
    pub minor: i64,
}

pub async fn balance(pool: &PgPool, account: AccountId) -> Result<Balance> {
    let row = sqlx::query("SELECT a.asset, COALESCE(b.balance_minor, 0) AS minor FROM ledger_accounts a LEFT JOIN ledger_balances b ON b.account_id = a.id WHERE a.id = $1")
        .bind(account.0)
        .fetch_optional(pool)
        .await?
        .ok_or(LedgerStoreError::UnknownAccount(account))?;
    let asset = AssetId::parse(row.get::<&str, _>("asset"))
        .expect("assets are written by this module from valid AssetIds");
    Ok(Balance {
        asset,
        minor: row.get("minor"),
    })
}

/// An account whose stored running balance disagrees with its entries. There should never be any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    pub account: AccountId,
    pub stored: i64,
    pub computed: i64,
}

/// Recomputes every balance from the entries and reports any account that disagrees with the stored balance.
pub async fn reconcile(pool: &PgPool) -> Result<Vec<Mismatch>> {
    let rows = sqlx::query(
        "SELECT a.id, COALESCE(b.balance_minor, 0) AS stored,
                COALESCE(sum(CASE WHEN e.side = a.normal_side THEN e.amount_minor ELSE -e.amount_minor END), 0)::bigint AS computed
         FROM ledger_accounts a
         LEFT JOIN ledger_balances b ON b.account_id = a.id
         LEFT JOIN ledger_entries e ON e.account_id = a.id
         GROUP BY a.id, b.balance_minor
         HAVING COALESCE(b.balance_minor, 0) <> COALESCE(sum(CASE WHEN e.side = a.normal_side THEN e.amount_minor ELSE -e.amount_minor END), 0)",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| Mismatch {
            account: AccountId(r.get("id")),
            stored: r.get("stored"),
            computed: r.get("computed"),
        })
        .collect())
}
