//! The books-against-chain check (ADR 0006 §6): what the ledger says the platform holds against what the network says is
//! there, the history of those checks, and the halt that stops new outflows when they persistently disagree.
//!
//! This module only knows the ledger side and the record; reading the network is the worker's job. It never repairs a
//! difference: a person looks, and clears the halt saying why.

use eschess_domain::money::AssetId;
use eschess_ledger::Side;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::stellar::{system_account, CUSTODY_KIND};

pub type Result<T> = std::result::Result<T, sqlx::Error>;

/// What the ledger says the platform holds on the network in `asset`: the custody mirror. Deposits raise it; a
/// withdrawal lowers it only once it has really left.
pub async fn ledger_holds(pool: &PgPool, asset: &AssetId) -> Result<i64> {
    let account = system_account(pool, CUSTODY_KIND, asset, Side::Debit)
        .await
        .map_err(|e| match e {
            crate::stellar::StellarStoreError::Database(d) => d,
            other => sqlx::Error::Protocol(other.to_string()),
        })?;
    sqlx::query_scalar(
        "SELECT COALESCE((SELECT balance_minor FROM ledger_balances WHERE account_id = $1), 0)",
    )
    .bind(account.0)
    .fetch_one(pool)
    .await
}

/// Money in `asset` that is on the network but deliberately not (yet) in the ledger: deposits set aside for a person to
/// look at, and deposits recorded but not yet credited. Both are the platform's to account for, not a difference.
pub async fn on_chain_not_in_ledger(pool: &PgPool, asset: &AssetId) -> Result<i64> {
    sqlx::query_scalar(
        "SELECT COALESCE(SUM(amount_minor), 0)::bigint FROM stellar_deposits \
         WHERE asset = $1 AND status IN ('needs_review', 'pending') AND amount_minor IS NOT NULL",
    )
    .bind(asset.to_string())
    .fetch_one(pool)
    .await
}

/// Whether money is in motion: a withdrawal is being sent (it may or may not have landed yet), or a deposit has been seen
/// and not yet credited. While it is, a difference is not conclusive.
pub async fn money_in_motion(pool: &PgPool) -> Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM withdrawals WHERE status = 'submitting') \
             OR EXISTS (SELECT 1 FROM stellar_deposits WHERE status = 'pending')",
    )
    .fetch_one(pool)
    .await
}

/// The addresses of every player account that exists on the network (set up and ready).
pub async fn player_addresses(pool: &PgPool) -> Result<Vec<String>> {
    sqlx::query_scalar(
        "SELECT k.address FROM wallet_setup s JOIN user_wallet_keys k ON k.user_id = s.user_id \
         WHERE s.ready_at IS NOT NULL ORDER BY k.address",
    )
    .fetch_all(pool)
    .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Ok,
    Inconclusive,
    Mismatch,
}

impl Verdict {
    fn as_str(self) -> &'static str {
        match self {
            Verdict::Ok => "ok",
            Verdict::Inconclusive => "inconclusive",
            Verdict::Mismatch => "mismatch",
        }
    }
}

/// The verdict for a comparison. Pure, so it is tested without a database.
pub fn judge(ledger: i64, chain: i64, explained: i64, in_motion: bool) -> (i64, Verdict) {
    let difference = chain - ledger - explained;
    let verdict = if difference == 0 {
        Verdict::Ok
    } else if in_motion {
        Verdict::Inconclusive
    } else {
        Verdict::Mismatch
    };
    (difference, verdict)
}

/// One comparison, as recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub id: i64,
    pub asset: String,
    pub ledger_minor: i64,
    pub chain_minor: i64,
    pub hot_minor: i64,
    pub players_minor: i64,
    pub explained_minor: i64,
    pub difference_minor: i64,
    pub verdict: Verdict,
    pub accounts_read: i32,
}

/// What was read, ready to be judged and recorded.
#[derive(Debug, Clone, Copy)]
pub struct Reading {
    pub ledger: i64,
    pub hot: i64,
    pub players: i64,
    pub explained: i64,
    pub in_motion: bool,
    pub accounts_read: i32,
}

pub async fn record_check(pool: &PgPool, asset: &AssetId, r: Reading) -> Result<Check> {
    let chain = r.hot + r.players;
    let (difference, verdict) = judge(r.ledger, chain, r.explained, r.in_motion);
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO money_checks (asset, ledger_minor, chain_minor, hot_minor, players_minor, explained_minor, \
         difference_minor, verdict, accounts_read) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING id",
    )
    .bind(asset.to_string())
    .bind(r.ledger)
    .bind(chain)
    .bind(r.hot)
    .bind(r.players)
    .bind(r.explained)
    .bind(difference)
    .bind(verdict.as_str())
    .bind(r.accounts_read)
    .fetch_one(pool)
    .await?;
    Ok(Check {
        id,
        asset: asset.to_string(),
        ledger_minor: r.ledger,
        chain_minor: chain,
        hot_minor: r.hot,
        players_minor: r.players,
        explained_minor: r.explained,
        difference_minor: difference,
        verdict,
        accounts_read: r.accounts_read,
    })
}

/// Whether the last `n` checks for `asset` were all conclusive mismatches. An inconclusive or a clean check in between
/// breaks the run, so a difference that comes and goes while money moves never halts anything.
pub async fn persistent_mismatch(pool: &PgPool, asset: &AssetId, n: i64) -> Result<bool> {
    let verdicts: Vec<String> = sqlx::query_scalar(
        "SELECT verdict FROM money_checks WHERE asset = $1 ORDER BY id DESC LIMIT $2",
    )
    .bind(asset.to_string())
    .bind(n)
    .fetch_all(pool)
    .await?;
    Ok(verdicts.len() as i64 == n && verdicts.iter().all(|v| v == "mismatch"))
}

/// Starts a halt. Does nothing if one is already active. Returns whether a new one was started.
pub async fn halt(pool: &PgPool, reason: &str, check_id: Option<i64>) -> Result<bool> {
    Ok(sqlx::query(
        "INSERT INTO money_halts (reason, check_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(reason)
    .bind(check_id)
    .execute(pool)
    .await?
    .rows_affected()
        > 0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Halt {
    pub id: i64,
    pub reason: String,
    pub started_at_ms: i64,
}

pub async fn active_halt(pool: &PgPool) -> Result<Option<Halt>> {
    let row = sqlx::query(
        "SELECT id, reason, (extract(epoch FROM started_at) * 1000)::bigint AS ms FROM money_halts \
         WHERE cleared_at IS NULL",
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| Halt {
        id: r.get("id"),
        reason: r.get("reason"),
        started_at_ms: r.get("ms"),
    }))
}

/// Lets a person end the halt, saying who and why. Returns whether there was one to clear.
pub async fn clear_halt(pool: &PgPool, by: Uuid, note: &str) -> Result<bool> {
    Ok(sqlx::query(
        "UPDATE money_halts SET cleared_at = now(), cleared_by = $1, clear_note = $2 WHERE cleared_at IS NULL",
    )
    .bind(by)
    .bind(note.trim())
    .execute(pool)
    .await?
    .rows_affected()
        > 0)
}

/// The latest check for each asset.
pub async fn latest_checks(pool: &PgPool) -> Result<Vec<Check>> {
    let rows = sqlx::query(
        "SELECT DISTINCT ON (asset) id, asset, ledger_minor, chain_minor, hot_minor, players_minor, explained_minor, \
         difference_minor, verdict, accounts_read FROM money_checks ORDER BY asset, id DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| Check {
            id: r.get("id"),
            asset: r.get("asset"),
            ledger_minor: r.get("ledger_minor"),
            chain_minor: r.get("chain_minor"),
            hot_minor: r.get("hot_minor"),
            players_minor: r.get("players_minor"),
            explained_minor: r.get("explained_minor"),
            difference_minor: r.get("difference_minor"),
            verdict: match r.get::<String, _>("verdict").as_str() {
                "ok" => Verdict::Ok,
                "inconclusive" => Verdict::Inconclusive,
                _ => Verdict::Mismatch,
            },
            accounts_read: r.get("accounts_read"),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_difference_is_conclusive_only_when_no_money_is_in_motion() {
        assert_eq!(judge(500, 500, 0, false), (0, Verdict::Ok));
        assert_eq!(
            judge(500, 500, 0, true),
            (0, Verdict::Ok),
            "matching is fine whatever is moving"
        );
        assert_eq!(
            judge(500, 400, 0, false),
            (-100, Verdict::Mismatch),
            "the platform holds less than it owes"
        );
        assert_eq!(
            judge(500, 600, 0, false),
            (100, Verdict::Mismatch),
            "more than the ledger records"
        );
        assert_eq!(judge(500, 400, 0, true), (-100, Verdict::Inconclusive));
        // Money set aside for review is on the network and not in the ledger: that is not a difference.
        assert_eq!(judge(500, 650, 150, false), (0, Verdict::Ok));
        assert_eq!(judge(500, 650, 100, false), (50, Verdict::Mismatch));
    }
}
