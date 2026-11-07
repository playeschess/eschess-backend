//! The fair-play job (ADR 0002 §7): score finished games that the configured scorer has not looked at yet.
//!
//! Each round takes a database lock per scorer (so two workers do not score the same games twice over) and handles a
//! batch. Storing a result is idempotent anyway, so a crash or a second worker can never record a game twice.
//!
//! A game that cannot be scored is recorded as such, with the reason, and not retried: its stored moves are
//! inconsistent, the scorer panicked, or it returned scores that break the rules. One bad game never blocks the rest.
//!
//! In this repository the scorer is [`eschess_fairplay::NoopScorer`]. The private repository runs this same job with
//! its own scorer.

use std::panic::{catch_unwind, AssertUnwindSafe};

use eschess_fairplay::Scorer;
use eschess_storage::fairplay::{self, RecordError, Recorded};
use sqlx::PgPool;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FairplayConfig {
    /// A score at or above this opens a case (or adds to the player's open one). 1 to 100.
    pub case_threshold: u8,
    /// Games handled per round.
    pub batch: i64,
}

impl FairplayConfig {
    /// `FAIRPLAY_CASE_THRESHOLD` (default 70) and `FAIRPLAY_BATCH` (default 50). A value that is present but not valid
    /// is an error, so a typo cannot silently change which games reach a person.
    pub fn from_lookup(get: &dyn Fn(&str) -> Option<String>) -> Result<FairplayConfig, String> {
        let read = |name: &str, default: i64| -> Result<i64, String> {
            match get(name).map(|v| v.trim().to_string()) {
                None => Ok(default),
                Some(v) if v.is_empty() => Ok(default),
                Some(v) => v
                    .parse()
                    .map_err(|_| format!("{name}: {v:?} is not a number")),
            }
        };
        let threshold = read("FAIRPLAY_CASE_THRESHOLD", 70)?;
        if !(1..=100).contains(&threshold) {
            return Err(format!(
                "FAIRPLAY_CASE_THRESHOLD must be 1 to 100, not {threshold}"
            ));
        }
        let batch = read("FAIRPLAY_BATCH", 50)?;
        if !(1..=1_000).contains(&batch) {
            return Err(format!("FAIRPLAY_BATCH must be 1 to 1000, not {batch}"));
        }
        Ok(FairplayConfig {
            case_threshold: threshold as u8,
            batch,
        })
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FairplayReport {
    /// Another worker held the lock; nothing was done.
    pub skipped: bool,
    pub scored: usize,
    pub unscorable: usize,
    /// Cases opened or added to.
    pub cases: usize,
}

/// One round: score up to `config.batch` games `scorer` has not looked at.
pub async fn tick(
    pool: &PgPool,
    scorer: &dyn Scorer,
    config: &FairplayConfig,
) -> Result<FairplayReport, String> {
    let name = scorer.name().to_string();
    let mut lock = pool.begin().await.map_err(|e| e.to_string())?;
    // Advisory locks are shared by every database on the server, so the key names this database: rounds for
    // different databases (parallel test databases, say) must not make each other skip.
    let got: bool = sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock(hashtext(current_database() || ':' || $1))",
    )
    .bind(format!("fairplay:{name}"))
    .fetch_one(&mut *lock)
    .await
    .map_err(|e| e.to_string())?;
    if !got {
        return Ok(FairplayReport {
            skipped: true,
            ..FairplayReport::default()
        });
    }
    let mut report = FairplayReport::default();
    let games = fairplay::unscored_games(pool, &name, config.batch)
        .await
        .map_err(|e| e.to_string())?;
    for id in games {
        let record = match fairplay::game_record(pool, id).await {
            Ok(Some(r)) => r,
            Ok(None) => continue, // not finished after all (cannot happen: only finished games are listed)
            Err(RecordError::Inconsistent(why)) => {
                unscorable(pool, id, &name, &why, &mut report).await?;
                continue;
            }
            Err(RecordError::Database(e)) => return Err(e.to_string()),
        };
        // The scorer is code this repository cannot see. A panic in it must cost one game, not the worker.
        let scores = match catch_unwind(AssertUnwindSafe(|| scorer.score(&record))) {
            Ok(s) => s,
            Err(_) => {
                unscorable(pool, id, &name, "the scorer panicked", &mut report).await?;
                continue;
            }
        };
        match fairplay::record_scores(pool, &record, &name, &scores, config.case_threshold)
            .await
            .map_err(|e| e.to_string())?
        {
            Recorded::Scored { cases, .. } => {
                report.scored += 1;
                report.cases += cases.len();
            }
            Recorded::Refused(why) => {
                tracing::warn!("fair play: game {id} not scored by {name}: {why}");
                report.unscorable += 1;
            }
            Recorded::AlreadyRun => {}
        }
    }
    lock.commit().await.map_err(|e| e.to_string())?;
    Ok(report)
}

async fn unscorable(
    pool: &PgPool,
    id: uuid::Uuid,
    scorer: &str,
    why: &str,
    report: &mut FairplayReport,
) -> Result<(), String> {
    tracing::warn!("fair play: game {id} cannot be scored by {scorer}: {why}");
    if fairplay::record_unscorable(pool, id, scorer, why)
        .await
        .map_err(|e| e.to_string())?
    {
        report.unscorable += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn load(pairs: &[(&str, &str)]) -> Result<FairplayConfig, String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        FairplayConfig::from_lookup(&|n| map.get(n).cloned())
    }

    #[test]
    fn defaults_and_limits() {
        assert_eq!(
            load(&[]).unwrap(),
            FairplayConfig {
                case_threshold: 70,
                batch: 50
            }
        );
        assert_eq!(
            load(&[("FAIRPLAY_CASE_THRESHOLD", "100")])
                .unwrap()
                .case_threshold,
            100
        );
        for bad in ["0", "101", "-5", "seventy", "70.5"] {
            assert!(load(&[("FAIRPLAY_CASE_THRESHOLD", bad)]).is_err(), "{bad}");
        }
        assert!(load(&[("FAIRPLAY_BATCH", "0")]).is_err());
        assert!(load(&[("FAIRPLAY_BATCH", "1001")]).is_err());
    }
}
