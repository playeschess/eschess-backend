//! The settle job (ADR 0004 §5): pays or returns the pot of each finished staked game, once, after fair play has
//! finished with it.
//!
//! It waits for `scorer` (the name of the fair-play scorer whose verdict to wait for) to have looked at the game, and for
//! both players to have no open case. If the scorer is not running, games stay unsettled rather than pay unreviewed:
//! that is deliberate. It is off unless `SETTLE_SCORER` names a scorer, so a worker that runs only the no-op scorer
//! cannot settle real money by accident.

use eschess_storage::stakes::{self, Settled, Waiting, MAX_FEE_BPS};
use sqlx::PgPool;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettleConfig {
    /// The fair-play scorer whose run each game must have, such as `baseline-1`.
    pub scorer: String,
    /// The platform's cut of a decisive pot, in basis points of the pot. 0 by default.
    pub fee_bps: u32,
    /// Games handled per round.
    pub batch: i64,
}

impl SettleConfig {
    /// `None` when `SETTLE_SCORER` is unset (settlement off). A value that is present but invalid is an error.
    /// `STAKE_FEE_BPS` defaults to 0 and may not exceed [`MAX_FEE_BPS`]; `SETTLE_BATCH` defaults to 50.
    pub fn from_lookup(
        get: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Option<SettleConfig>, String> {
        let Some(scorer) = get("SETTLE_SCORER")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
        else {
            return Ok(None);
        };
        if scorer.is_empty()
            || scorer.len() > 64
            || !scorer
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
        {
            return Err(format!("SETTLE_SCORER: {scorer:?} is not a scorer name"));
        }
        let number = |name: &str, default: i64| -> Result<i64, String> {
            match get(name)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
            {
                None => Ok(default),
                Some(v) => v
                    .parse()
                    .map_err(|_| format!("{name}: {v:?} is not a number")),
            }
        };
        let fee = number("STAKE_FEE_BPS", 0)?;
        if !(0..=MAX_FEE_BPS as i64).contains(&fee) {
            return Err(format!(
                "STAKE_FEE_BPS must be 0 to {MAX_FEE_BPS}, not {fee}"
            ));
        }
        let batch = number("SETTLE_BATCH", 50)?;
        if !(1..=1_000).contains(&batch) {
            return Err(format!("SETTLE_BATCH must be 1 to 1000, not {batch}"));
        }
        Ok(Some(SettleConfig {
            scorer,
            fee_bps: fee as u32,
            batch,
        }))
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SettleReport {
    /// Another worker held the lock; nothing was done.
    pub skipped: bool,
    pub paid: usize,
    pub returned: usize,
    pub confiscated: usize,
    /// Left for a later round: fair play has not finished with them, or a case is open.
    pub waiting: usize,
    /// Games whose settlement failed (logged); they are retried next round.
    pub failed: usize,
    /// Assets whose escrow did not equal the stakes of unsettled games this round. Should always be 0; each one is
    /// logged as an error.
    pub escrow_mismatches: usize,
}

/// One round: settle up to `config.batch` finished staked games.
pub async fn tick(pool: &PgPool, config: &SettleConfig) -> Result<SettleReport, String> {
    let mut lock = pool.begin().await.map_err(|e| e.to_string())?;
    // Names the database, as the other jobs do, so rounds on different databases never make each other skip.
    let got: bool = sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock(hashtext(current_database() || ':' || $1))",
    )
    .bind("stake-settle")
    .fetch_one(&mut *lock)
    .await
    .map_err(|e| e.to_string())?;
    if !got {
        return Ok(SettleReport {
            skipped: true,
            ..SettleReport::default()
        });
    }
    let mut report = SettleReport::default();
    // The alarm (ADR 0004 §7): escrow must always equal the stakes of unsettled games. If it does not, something is
    // wrong with the money, and paying more pots out of it would hide or compound the problem: stop the line, say so
    // loudly every round, and repair nothing automatically.
    for m in stakes::escrow_mismatches(pool)
        .await
        .map_err(|e| e.to_string())?
    {
        tracing::error!(
            asset = %m.asset, held = m.held_minor, expected = m.expected_minor,
            "ESCROW MISMATCH: the stake escrow does not equal the stakes of unsettled games; settlement is stopped until a person investigates"
        );
        report.escrow_mismatches += 1;
    }
    if report.escrow_mismatches > 0 {
        lock.commit().await.map_err(|e| e.to_string())?;
        return Ok(report);
    }
    for game in stakes::unsettled(pool, config.batch)
        .await
        .map_err(|e| e.to_string())?
    {
        match stakes::settle(pool, game, &config.scorer, config.fee_bps).await {
            Ok(Ok(Settled::Paid { winner, fee_minor })) => {
                tracing::info!(%game, %winner, fee_minor, "staked game settled: pot paid");
                report.paid += 1;
            }
            Ok(Ok(Settled::Returned)) => {
                tracing::info!(%game, "staked game settled: stakes returned");
                report.returned += 1;
            }
            Ok(Ok(Settled::Confiscated { to })) => {
                tracing::info!(%game, %to, "staked game settled: pot to the opponent of a confirmed cheat");
                report.confiscated += 1;
            }
            Ok(Err(Waiting::NotScored | Waiting::OpenCase | Waiting::Halted)) => {
                report.waiting += 1
            }
            Ok(Err(Waiting::NothingToDo)) => {}
            Err(e) => {
                tracing::error!(%game, "settling a staked game failed (will retry): {e}");
                report.failed += 1;
            }
        }
    }
    lock.commit().await.map_err(|e| e.to_string())?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(pairs: &[(&str, &str)]) -> Result<Option<SettleConfig>, String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        SettleConfig::from_lookup(&|n| pairs.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone()))
    }

    #[test]
    fn settlement_is_off_without_a_scorer_and_refuses_bad_settings() {
        assert_eq!(cfg(&[]), Ok(None));
        assert_eq!(cfg(&[("SETTLE_SCORER", "  ")]), Ok(None));
        let on = cfg(&[("SETTLE_SCORER", "baseline-1")]).unwrap().unwrap();
        assert_eq!(
            (on.scorer.as_str(), on.fee_bps, on.batch),
            ("baseline-1", 0, 50)
        );
        let fee = cfg(&[("SETTLE_SCORER", "baseline-1"), ("STAKE_FEE_BPS", "250")])
            .unwrap()
            .unwrap();
        assert_eq!(fee.fee_bps, 250);
        for bad in [
            vec![("SETTLE_SCORER", "Baseline 1")],
            vec![("SETTLE_SCORER", "baseline-1"), ("STAKE_FEE_BPS", "2001")],
            vec![("SETTLE_SCORER", "baseline-1"), ("STAKE_FEE_BPS", "-1")],
            vec![("SETTLE_SCORER", "baseline-1"), ("STAKE_FEE_BPS", "five")],
            vec![("SETTLE_SCORER", "baseline-1"), ("SETTLE_BATCH", "0")],
        ] {
            assert!(cfg(&bad).is_err(), "{bad:?}");
        }
    }
}
