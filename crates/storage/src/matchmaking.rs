//! The matchmaking queue in Postgres. The rules for who may play whom are `eschess_domain::pairing`; this module keeps
//! the queue, runs the pairing, and creates the game, all in one transaction.
//!
//! Pairing takes one global advisory lock, so two servers (or the background sweeper and a request) never pair the same
//! player twice. That serialises matchmaking, which is cheap work on a small table; if it ever becomes a bottleneck the
//! lock can be split per bucket (time control and rated flag).

use std::collections::HashMap;

use eschess_domain::clock::TimeControl;
use eschess_domain::pairing::{self, Waiting};
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::games::{start_matched, GameError};
use crate::stakes::{self, Stake};

/// What players are grouped by: time control, rated, and the stake asked for (none, or an amount and its asset).
type Bucket = (i64, i64, bool, Option<i64>, Option<String>);

type Result<T> = std::result::Result<T, GameError>;

/// A player who has been waiting this long without a game is dropped from the queue.
pub const WAIT_TIMEOUT_MS: i64 = 120_000;
/// How long a found game is remembered for a player who was not looking at that moment.
pub const MATCHED_KEEP_MS: i64 = 600_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Queued { waited_ms: i64 },
    Matched { game_id: Uuid },
}

async fn lock(db: &mut PgConnection) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('matchmaking'))")
        .execute(&mut *db)
        .await?;
    Ok(())
}

/// Removes players who waited too long and found games nobody collected.
async fn clean(db: &mut PgConnection) -> Result<()> {
    sqlx::query("DELETE FROM match_queue WHERE (game_id IS NULL AND joined_at < clock_timestamp() - make_interval(secs => $1 / 1000.0)) OR (game_id IS NOT NULL AND matched_at < clock_timestamp() - make_interval(secs => $2 / 1000.0))")
        .bind(WAIT_TIMEOUT_MS as f64)
        .bind(MATCHED_KEEP_MS as f64)
        .execute(&mut *db)
        .await?;
    Ok(())
}

/// Pairs whoever can be paired right now. `coin` decides colours (true: the longer-waiting player has White).
async fn pair_waiting(
    db: &mut PgConnection,
    coin: &mut (dyn FnMut() -> bool + Send),
) -> Result<usize> {
    let rows = sqlx::query("SELECT user_id, base_ms, increment_ms, rated, stake_minor, stake_asset, rating, (extract(epoch from clock_timestamp() - joined_at) * 1000)::bigint AS waited_ms FROM match_queue q WHERE game_id IS NULL AND (NOT rated OR NOT EXISTS (SELECT 1 FROM fairplay_restrictions s WHERE s.player_id = q.user_id AND s.kind = 'suspension')) ORDER BY joined_at, user_id")
        .fetch_all(&mut *db)
        .await?;
    // Players are grouped by what they asked for; only players in the same group can meet.
    let mut buckets: HashMap<Bucket, Vec<(Uuid, Waiting)>> = HashMap::new();
    for r in &rows {
        let bucket = (
            r.get("base_ms"),
            r.get("increment_ms"),
            r.get("rated"),
            r.get("stake_minor"),
            r.get::<Option<String>, _>("stake_asset"),
        );
        let list = buckets.entry(bucket).or_default();
        let waiting = Waiting {
            id: list.len() as u64,
            rating: r.get("rating"),
            waited_ms: r.get::<i64, _>("waited_ms").max(0) as u64,
        };
        list.push((r.get("user_id"), waiting));
    }
    let mut made = 0;
    for ((base_ms, increment_ms, rated, stake_minor, stake_asset), list) in buckets {
        let stake = stakes::stake_of(stake_minor, stake_asset.as_deref())?;
        let control = TimeControl::new(base_ms as u64, increment_ms as u64)
            .map_err(|_| GameError::InvalidTimeControl("a queued time control is invalid"))?;
        let waiting: Vec<Waiting> = list.iter().map(|(_, w)| *w).collect();
        for (a, b) in pairing::pair(&waiting) {
            let (a, b) = (&list[a as usize], &list[b as usize]);
            // The one who waited longer is recorded as the creator.
            let (first, second) = if a.1.waited_ms >= b.1.waited_ms {
                (a.0, b.0)
            } else {
                (b.0, a.0)
            };
            let game = match start_pair(db, first, second, control, rated, coin(), stake.as_ref())
                .await?
            {
                Pair::Started(game) => game,
                // A player who cannot stake any more (spent the money, was restricted since) leaves the queue; the
                // other keeps waiting for someone else. Nothing was held for the pair.
                Pair::Paused => continue,
                Pair::Refused(who) => {
                    sqlx::query("DELETE FROM match_queue WHERE user_id = $1 AND game_id IS NULL")
                        .bind(who)
                        .execute(&mut *db)
                        .await?;
                    continue;
                }
            };
            sqlx::query("UPDATE match_queue SET game_id = $1, matched_at = clock_timestamp() WHERE user_id = ANY($2)")
                .bind(game)
                .bind(vec![first, second])
                .execute(&mut *db)
                .await?;
            // Tell each player (on whichever server they are connected to) once this commits.
            for user in [first, second] {
                sqlx::query("SELECT pg_notify('user_events', $1)")
                    .bind(format!("{user}:{game}"))
                    .execute(&mut *db)
                    .await?;
            }
            made += 1;
        }
    }
    Ok(made)
}

enum Pair {
    Started(Uuid),
    /// This player could not put up the stake, so no game was made.
    Refused(Uuid),
    /// New stakes are paused (the books do not match the chain): nobody is dropped, the pair just waits.
    Paused,
}

/// Starts the game for a pair and, for a staked pair, holds both stakes, all or nothing: in a savepoint, so a player who
/// cannot pay undoes the game and the other player's hold without aborting the rest of the pairing round.
async fn start_pair(
    db: &mut PgConnection,
    first: Uuid,
    second: Uuid,
    control: TimeControl,
    rated: bool,
    first_plays_white: bool,
    stake: Option<&Stake>,
) -> Result<Pair> {
    let Some(stake) = stake else {
        return Ok(Pair::Started(
            start_matched(db, first, second, control, rated, first_plays_white, None).await?,
        ));
    };
    let mut sp = sqlx::Connection::begin(&mut *db).await?;
    let game = start_matched(
        &mut sp,
        first,
        second,
        control,
        rated,
        first_plays_white,
        Some(stake),
    )
    .await?;
    for player in [first, second] {
        match stakes::hold(&mut sp, game, player, stake).await {
            Ok(()) => {}
            Err(
                GameError::InsufficientFunds
                | GameError::StakingRestricted
                | GameError::EmailNotConfirmed,
            ) => {
                sp.rollback().await?;
                return Ok(Pair::Refused(player));
            }
            Err(GameError::MoneyHalted) => {
                sp.rollback().await?;
                return Ok(Pair::Paused);
            }
            Err(other) => return Err(other),
        }
    }
    sp.commit().await?;
    Ok(Pair::Started(game))
}

async fn status_of(db: &mut PgConnection, user: Uuid) -> Result<Option<Status>> {
    let row = sqlx::query("SELECT game_id, (extract(epoch from clock_timestamp() - joined_at) * 1000)::bigint AS waited_ms FROM match_queue WHERE user_id = $1")
        .bind(user)
        .fetch_optional(&mut *db)
        .await?;
    Ok(row.map(|r| match r.get::<Option<Uuid>, _>("game_id") {
        Some(game_id) => Status::Matched { game_id },
        None => Status::Queued {
            waited_ms: r.get("waited_ms"),
        },
    }))
}

/// Joins the queue for a game with this time control, then tries to pair straight away. Asking again replaces the
/// player's earlier request (so changing your mind about the time control is just asking again).
pub async fn enqueue(
    pool: &PgPool,
    user: Uuid,
    base_ms: u64,
    increment_ms: u64,
    rated: bool,
    stake: Option<&Stake>,
    coin: &mut (dyn FnMut() -> bool + Send),
) -> Result<Status> {
    if stake.is_some() && !rated {
        return Err(GameError::StakeNotOffered("only rated games can be staked"));
    }
    let control = TimeControl::new(base_ms, increment_ms).map_err(|_| {
        GameError::InvalidTimeControl("the base time must be between 1 second and 3 hours and the increment at most 3 minutes")
    })?;
    let category = control.category().as_str();
    let mut tx = pool.begin().await?;
    if rated {
        if let Some(until) = crate::fairplay::suspended_until(&mut *tx, user).await? {
            return Err(GameError::Suspended(until));
        }
    }
    // Fail early, and say why, for a player who plainly cannot stake. Nothing is held yet; the stake is held when two
    // players are paired, and checked again then.
    if let Some(stake) = stake {
        stakes::require_can_stake(&mut tx, user).await?;
        if !stakes::can_cover(&mut tx, user, stake).await? {
            return Err(GameError::InsufficientFunds);
        }
    }
    lock(&mut tx).await?;
    clean(&mut tx).await?;
    let rating: Option<f64> =
        sqlx::query_scalar("SELECT rating FROM ratings WHERE user_id = $1 AND category = $2")
            .bind(user)
            .bind(category)
            .fetch_optional(&mut *tx)
            .await?;
    sqlx::query(
        "INSERT INTO match_queue (user_id, base_ms, increment_ms, rated, category, rating, stake_minor, stake_asset) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)
         ON CONFLICT (user_id) DO UPDATE SET base_ms = $2, increment_ms = $3, rated = $4, category = $5, rating = $6, stake_minor = $7, stake_asset = $8,
             joined_at = clock_timestamp(), game_id = NULL, matched_at = NULL",
    )
    .bind(user)
    .bind(base_ms as i64)
    .bind(increment_ms as i64)
    .bind(rated)
    .bind(category)
    .bind(rating.unwrap_or(eschess_domain::rating::Rating::NEW.rating))
    .bind(stake.map(|s| s.minor))
    .bind(stake.map(|s| s.asset.to_string()))
    .execute(&mut *tx)
    .await?;
    pair_waiting(&mut tx, coin).await?;
    let status = status_of(&mut tx, user)
        .await?
        .expect("the row was just written");
    tx.commit().await?;
    Ok(status)
}

/// Where the player stands: still waiting, or a game is ready. `None` if they are not in the queue.
pub async fn status(pool: &PgPool, user: Uuid) -> Result<Option<Status>> {
    let mut conn = pool.acquire().await?;
    let st = status_of(&mut conn, user).await?;
    // A wait that has run out is not reported as still going.
    Ok(match st {
        Some(Status::Queued { waited_ms }) if waited_ms > WAIT_TIMEOUT_MS => None,
        other => other,
    })
}

/// Leaves the queue. `Ok(false)` if they were not waiting; an error if a game was already found for them.
pub async fn leave(pool: &PgPool, user: Uuid) -> Result<bool> {
    let mut tx = pool.begin().await?;
    lock(&mut tx).await?;
    let out = match status_of(&mut tx, user).await? {
        None => false,
        Some(Status::Matched { .. }) => return Err(GameError::AlreadyMatched),
        Some(Status::Queued { .. }) => {
            sqlx::query("DELETE FROM match_queue WHERE user_id = $1")
                .bind(user)
                .execute(&mut *tx)
                .await?;
            true
        }
    };
    tx.commit().await?;
    Ok(out)
}

/// One round of housekeeping and pairing for everyone waiting: the windows widen as people wait, so pairs that were
/// not possible a moment ago may be possible now. Run by a background task every second or two. Returns how many games
/// were started.
pub async fn sweep(pool: &PgPool, coin: &mut (dyn FnMut() -> bool + Send)) -> Result<usize> {
    let mut tx = pool.begin().await?;
    lock(&mut tx).await?;
    clean(&mut tx).await?;
    let made = pair_waiting(&mut tx, coin).await?;
    tx.commit().await?;
    Ok(made)
}
