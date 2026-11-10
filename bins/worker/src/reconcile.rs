//! The books-against-chain check (ADR 0006 §6).
//!
//! Every round, for each asset: what the ledger says the platform holds, against what the network says is there (the
//! platform wallet plus every player's own account). Each comparison is recorded. A difference while money is in motion
//! (a withdrawal being sent, a deposit being credited) proves nothing and is called inconclusive. A difference with
//! nothing in motion is a mismatch, and after several mismatches in a row **new outflows stop** until a person looks:
//! withdrawals are not sent, staked games are not started or settled, deposits keep being credited. Nothing is repaired
//! automatically: a difference might be a bug, a stolen key or a missed payment, and the cause decides the cure.

use eschess_domain::money::AssetId;
use eschess_domain::stellar::AccountKey;
use eschess_stellar::Chain;
use eschess_storage::reconcile::{self as store, Check, Reading, Verdict};
use sqlx::PgPool;

#[derive(Debug, Clone)]
pub struct ReconcileConfig {
    /// The platform's own wallet on the network.
    pub hot_account: AccountKey,
    /// The assets customers hold (USDC).
    pub assets: Vec<AssetId>,
    /// How many conclusive mismatches in a row start a halt.
    pub mismatches_to_halt: i64,
}

/// `RECONCILE_MISMATCHES_TO_HALT` (default 3, from 2 to 20).
pub fn mismatches_to_halt(get: &dyn Fn(&str) -> Option<String>) -> Result<i64, String> {
    match get("RECONCILE_MISMATCHES_TO_HALT")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        None => Ok(3),
        Some(v) => {
            let n: i64 = v
                .parse()
                .map_err(|_| format!("RECONCILE_MISMATCHES_TO_HALT: {v:?} is not a number"))?;
            if !(2..=20).contains(&n) {
                return Err(format!(
                    "RECONCILE_MISMATCHES_TO_HALT must be 2 to 20, not {n}"
                ));
            }
            Ok(n)
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct ReconcileReport {
    pub skipped: bool,
    pub checks: Vec<Check>,
    /// A halt was started this round.
    pub halted: bool,
}

pub async fn tick(
    pool: &PgPool,
    chain: &dyn Chain,
    config: &ReconcileConfig,
) -> Result<ReconcileReport, String> {
    let mut lock = pool.begin().await.map_err(|e| e.to_string())?;
    let got: bool = sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock(hashtext(current_database() || ':' || $1))",
    )
    .bind("money-check")
    .fetch_one(&mut *lock)
    .await
    .map_err(|e| e.to_string())?;
    if !got {
        return Ok(ReconcileReport {
            skipped: true,
            ..ReconcileReport::default()
        });
    }
    let mut report = ReconcileReport::default();
    for asset in &config.assets {
        // Money in motion is looked at before and after the network is read: if any was moving at either moment, a
        // difference does not prove anything.
        let motion_before = store::money_in_motion(pool)
            .await
            .map_err(|e| e.to_string())?;
        let ledger = store::ledger_holds(pool, asset)
            .await
            .map_err(|e| e.to_string())?;
        let explained = store::on_chain_not_in_ledger(pool, asset)
            .await
            .map_err(|e| e.to_string())?;

        let hot = chain
            .account(&config.hot_account.to_g())
            .await
            .map_err(|e| e.to_string())?
            .map_or(0, |a| a.balance_of(asset));
        let addresses = store::player_addresses(pool)
            .await
            .map_err(|e| e.to_string())?;
        let mut players = 0i64;
        for address in &addresses {
            // An account the network does not know holds nothing.
            players += chain
                .account(address)
                .await
                .map_err(|e| e.to_string())?
                .map_or(0, |a| a.balance_of(asset));
        }
        let motion_after = store::money_in_motion(pool)
            .await
            .map_err(|e| e.to_string())?;

        let check = store::record_check(
            pool,
            asset,
            Reading {
                ledger,
                hot,
                players,
                explained,
                in_motion: motion_before || motion_after,
                accounts_read: addresses.len() as i32 + 1,
            },
        )
        .await
        .map_err(|e| e.to_string())?;
        match check.verdict {
            Verdict::Ok => {}
            Verdict::Inconclusive => tracing::info!(
                asset = %check.asset, difference = check.difference_minor,
                "books against chain: a difference while money is moving; looking again"
            ),
            Verdict::Mismatch => {
                tracing::error!(
                    asset = %check.asset, ledger = check.ledger_minor, chain = check.chain_minor,
                    difference = check.difference_minor,
                    "BOOKS DO NOT MATCH THE CHAIN: the network holds {} against the ledger's {} (difference {})",
                    check.chain_minor, check.ledger_minor, check.difference_minor
                );
                if store::persistent_mismatch(pool, asset, config.mismatches_to_halt)
                    .await
                    .map_err(|e| e.to_string())?
                {
                    let reason = format!(
                        "{}: the network holds {} but the ledger says {} (difference {}), {} checks in a row",
                        check.asset,
                        check.chain_minor,
                        check.ledger_minor,
                        check.difference_minor,
                        config.mismatches_to_halt
                    );
                    if store::halt(pool, &reason, Some(check.id))
                        .await
                        .map_err(|e| e.to_string())?
                    {
                        tracing::error!("MONEY HALTED: {reason}. New withdrawals and staked games are stopped until a person clears it.");
                        report.halted = true;
                    }
                }
            }
        }
        report.checks.push(check);
    }
    lock.commit().await.map_err(|e| e.to_string())?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_run_of_mismatches_that_halts_defaults_to_three_and_refuses_nonsense() {
        let get = |v: Option<&'static str>| move |_: &str| v.map(String::from);
        assert_eq!(mismatches_to_halt(&get(None)), Ok(3));
        assert_eq!(mismatches_to_halt(&get(Some("5"))), Ok(5));
        for bad in ["1", "21", "often", "-3"] {
            assert!(mismatches_to_halt(&get(Some(bad))).is_err(), "{bad}");
        }
    }
}
