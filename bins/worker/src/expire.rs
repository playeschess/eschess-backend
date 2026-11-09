//! Expires staked games nobody joined (ADR 0004): the creator's stake is locked while the game waits, so a game that
//! waits too long is cancelled and the stake returned, the same way the creator's own cancel does it. Unstaked games
//! hold no money and are left alone.

use eschess_storage::{games, stakes};
use sqlx::PgPool;

/// How long a staked game may wait for an opponent: 10 minutes unless `STAKED_GAME_WAIT_SECS` says otherwise
/// (60 seconds to 24 hours).
pub const DEFAULT_WAIT_SECS: i64 = 600;

pub fn wait_secs(get: &dyn Fn(&str) -> Option<String>) -> Result<i64, String> {
    match get("STAKED_GAME_WAIT_SECS")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        None => Ok(DEFAULT_WAIT_SECS),
        Some(v) => {
            let n: i64 = v
                .parse()
                .map_err(|_| format!("STAKED_GAME_WAIT_SECS: {v:?} is not a number"))?;
            if !(60..=86_400).contains(&n) {
                return Err(format!(
                    "STAKED_GAME_WAIT_SECS must be 60 to 86400, not {n}"
                ));
            }
            Ok(n)
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ExpireReport {
    pub skipped: bool,
    pub cancelled: usize,
}

/// One round: cancel up to 100 expired staked games and return their stakes.
pub async fn tick(pool: &PgPool, wait_secs: i64) -> Result<ExpireReport, String> {
    let mut lock = pool.begin().await.map_err(|e| e.to_string())?;
    let got: bool = sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock(hashtext(current_database() || ':' || $1))",
    )
    .bind("staked-game-expiry")
    .fetch_one(&mut *lock)
    .await
    .map_err(|e| e.to_string())?;
    if !got {
        return Ok(ExpireReport {
            skipped: true,
            ..ExpireReport::default()
        });
    }
    let mut report = ExpireReport::default();
    for (game, creator) in stakes::expired_waiting(pool, wait_secs, 100)
        .await
        .map_err(|e| e.to_string())?
    {
        match games::cancel_game(pool, game, creator).await {
            Ok(()) => {
                tracing::info!(%game, %creator, "staked game expired unjoined; stake returned");
                report.cancelled += 1;
            }
            // Joined or cancelled a moment ago: nothing to do.
            Err(games::GameError::NotOpen) | Err(games::GameError::NotFound) => {}
            Err(e) => tracing::error!(%game, "expiring a staked game failed (will retry): {e}"),
        }
    }
    lock.commit().await.map_err(|e| e.to_string())?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wait_defaults_to_ten_minutes_and_refuses_nonsense() {
        let get = |v: Option<&'static str>| move |_: &str| v.map(String::from);
        assert_eq!(wait_secs(&get(None)), Ok(600));
        assert_eq!(wait_secs(&get(Some(" "))), Ok(600));
        assert_eq!(wait_secs(&get(Some("120"))), Ok(120));
        for bad in ["59", "86401", "soon", "-5"] {
            assert!(wait_secs(&get(Some(bad))).is_err(), "{bad}");
        }
    }
}
