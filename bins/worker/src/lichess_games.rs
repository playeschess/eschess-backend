//! Staked games played on Lichess (ADR 0005): makes each game on Lichess for its two players, and reads its result.
//!
//! Each round does two things. First, every staked Lichess game with no Lichess game yet gets a challenge only its two
//! players can join (casual, with the clock the stake was made with, expiring by itself). Second, every game that has a
//! Lichess game is looked at: when it is over the result is recorded once and settlement takes over; a game nobody
//! started before its challenge expired is a no-show and the stakes come back.
//!
//! A result is never guessed. Before a result is recorded, the game's moves and clocks are read from Lichess and stored,
//! so the fair-play scorer can look at the game like any other. A game Lichess ended for cheating opens a review case; one
//! with a status this code does not know, whose moves cannot be followed, or whose players or variant are not what we asked
//! for, is left alone and logged for a person.

use eschess_lichess::{GameStatus, Lichess, LichessError, NewChallenge, Side};
use eschess_storage::{fairplay, games, lichess_games};
use sqlx::PgPool;

#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// How long the two players have to start the game before it is called a no-show.
    pub start_window_secs: i64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            start_window_secs: 600,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub skipped: bool,
    pub created: usize,
    pub finished: usize,
    pub no_shows: usize,
    /// Games left for a person (cheating, unknown or unexpected answers).
    pub held: usize,
}

/// What recording a finished Lichess game should say, or `None` if it must wait for a person.
fn verdict(status: &GameStatus, winner: Option<Side>) -> Option<&'static str> {
    match status {
        GameStatus::Mate
        | GameStatus::Resign
        | GameStatus::Timeout
        | GameStatus::OutOfTime
        | GameStatus::VariantEnd => match winner {
            Some(Side::White) => Some("white"),
            Some(Side::Black) => Some("black"),
            None => None,
        },
        GameStatus::Stalemate
        | GameStatus::Draw
        | GameStatus::InsufficientMaterialClaim
        | GameStatus::Aborted
        | GameStatus::NoStart => Some("draw"),
        // Lichess ended it for cheating, or said something we do not understand: a person decides.
        GameStatus::Cheat
        | GameStatus::UnknownFinish
        | GameStatus::Created
        | GameStatus::Started
        | GameStatus::Other(_) => None,
    }
}

pub async fn tick(
    pool: &PgPool,
    lichess: &dyn Lichess,
    now_ms: i64,
    cfg: Config,
) -> Result<Report, String> {
    let mut lock = pool.begin().await.map_err(|e| e.to_string())?;
    let got: bool = sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock(hashtext(current_database() || ':' || $1))",
    )
    .bind("lichess-games")
    .fetch_one(&mut *lock)
    .await
    .map_err(|e| e.to_string())?;
    if !got {
        return Ok(Report {
            skipped: true,
            ..Report::default()
        });
    }
    let mut report = Report::default();

    for g in lichess_games::to_create(pool, 50)
        .await
        .map_err(|e| e.to_string())?
    {
        // A game that cannot be made on Lichess for twice the start window is given up: the stakes come back.
        if g.age_secs > 2 * cfg.start_window_secs {
            tracing::warn!(game = %g.id, "no Lichess game could be made in time; returning the stakes");
            if record(pool, g.id, "draw", "lichess:no_challenge", 0).await {
                report.no_shows += 1;
            }
            continue;
        }
        let expires_at_ms = now_ms + cfg.start_window_secs * 1000;
        let made = lichess
            .create_open_challenge(&NewChallenge {
                white: g.white.clone(),
                black: g.black.clone(),
                clock_limit_secs: (g.base_ms / 1000) as u32,
                clock_increment_secs: (g.increment_ms / 1000) as u32,
                // Casual: a rated game would move the players' Lichess ratings for our stakes.
                rated: false,
                expires_at_ms,
            })
            .await;
        match made {
            Ok(c) => match lichess_games::set_challenge(pool, g.id, &c.id, expires_at_ms).await {
                Ok(true) => {
                    tracing::info!(game = %g.id, lichess = %c.id, "Lichess game made");
                    report.created += 1;
                }
                Ok(false) => {}
                Err(e) => tracing::error!(game = %g.id, "saving the Lichess game failed: {e}"),
            },
            Err(LichessError::Unavailable(m)) => {
                tracing::warn!(game = %g.id, "Lichess unavailable (will retry): {m}")
            }
            Err(LichessError::Rejected(m)) => {
                tracing::error!(game = %g.id, "Lichess refused the game (will retry until it is given up): {m}")
            }
        }
    }

    for g in lichess_games::in_play(pool, 100)
        .await
        .map_err(|e| e.to_string())?
    {
        let seen = match lichess.game(&g.lichess_game_id).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(game = %g.id, "reading the Lichess game failed (will retry): {e}");
                continue;
            }
        };
        let Some(lg) = seen else {
            // Lichess has no game: the players have not started it. After the challenge expired, they never will.
            if g.challenge_expired && record(pool, g.id, "draw", "lichess:no_show", 0).await {
                report.no_shows += 1;
            }
            continue;
        };
        if lg.id != g.lichess_game_id
            || lg.variant != "standard"
            || lg.white.as_deref() != Some(g.white.as_str())
            || lg.black.as_deref() != Some(g.black.as_str())
        {
            tracing::error!(game = %g.id, lichess = %lg.id, "the Lichess game is not the one we asked for (players or variant differ); leaving it for a person");
            report.held += 1;
            continue;
        }
        if lg.status == GameStatus::Created {
            if g.challenge_expired && record(pool, g.id, "draw", "lichess:no_show", 0).await {
                report.no_shows += 1;
            }
            continue;
        }
        if !lg.status.is_over() {
            continue;
        }
        // The game is over. Bring its moves here first, so fair play can look at them: a result is never paid on a game
        // whose moves we could not follow.
        if lg.ply > 0 {
            let moves = match lichess_games::convert_moves(&lg.moves, &lg.clocks_cs, g.increment_ms)
            {
                Ok(m) => m,
                Err(why) => {
                    tracing::error!(game = %g.id, lichess = %lg.id, "could not follow the Lichess game's moves ({why}); leaving it for a person");
                    report.held += 1;
                    continue;
                }
            };
            if let Err(e) = lichess_games::import_moves(pool, g.id, &moves).await {
                tracing::error!(game = %g.id, "storing the Lichess game's moves failed (will retry): {e}");
                continue;
            }
        }
        // Lichess ended the game because a player cheated. We do not pay on that alone: it opens a review case against
        // the player who lost (the other won), the game is recorded as a draw, and a reviewer decides. Confirmed, the
        // honest player gets the pot; cleared, the stakes come back.
        if lg.status == GameStatus::Cheat {
            let cheater = match lg.winner {
                Some(Side::White) => g.black_user,
                Some(Side::Black) => g.white_user,
                None => {
                    tracing::error!(game = %g.id, lichess = %lg.id, "Lichess ended the game for cheating without a winner; leaving it for a person");
                    report.held += 1;
                    continue;
                }
            };
            if let Err(e) = fairplay::record_external_flag(
                pool,
                g.id,
                cheater,
                "lichess",
                "Lichess ended this game because the player was found to be cheating",
            )
            .await
            {
                tracing::error!(game = %g.id, "opening the fair-play case failed (will retry): {e}");
                continue;
            }
            if record(pool, g.id, "draw", "lichess:cheat", lg.ply).await {
                tracing::warn!(game = %g.id, %cheater, "Lichess ended the game for cheating; a review case is open");
                report.finished += 1;
            }
            continue;
        }
        match verdict(&lg.status, lg.winner) {
            Some(result) => {
                if record(
                    pool,
                    g.id,
                    result,
                    &format!("lichess:{:?}", lg.status).to_lowercase(),
                    lg.ply,
                )
                .await
                {
                    report.finished += 1;
                }
            }
            None => {
                tracing::error!(game = %g.id, lichess = %lg.id, status = ?lg.status, "Lichess game ended in a way that needs a person (an unexpected status); stakes stay held");
                report.held += 1;
            }
        }
    }
    lock.commit().await.map_err(|e| e.to_string())?;
    Ok(report)
}

/// Records a result; `true` if this call recorded it.
async fn record(pool: &PgPool, game: uuid::Uuid, result: &str, reason: &str, ply: i32) -> bool {
    match games::finish_external(pool, game, result, reason, ply).await {
        Ok(done) => done,
        Err(e) => {
            tracing::error!(%game, "recording the Lichess result failed (will retry): {e}");
            false
        }
    }
}
