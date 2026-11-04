//! Ratings in Postgres. The arithmetic is `eschess_domain::rating`; this module loads the two players' ratings,
//! applies one finished game to both, and saves the result, all inside the transaction that ends the game.

use eschess_domain::rating::{self, Game, Rating};
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

/// A game counts for ratings only if each side has moved at least this many times, so abandoning a game at once, or
/// "winning" one with no play, cannot be used to trade rating.
pub const MIN_PLIES_FOR_RATING: i32 = 4;

#[derive(Debug, Clone, PartialEq)]
pub struct PlayerRating {
    pub category: String,
    pub rating: f64,
    pub deviation: f64,
    pub volatility: f64,
    pub games_played: i32,
}

/// A rating is "established" once the deviation is low enough that the number means something.
pub const ESTABLISHED_DEVIATION: f64 = 110.0;

async fn load_locked(
    db: &mut PgConnection,
    user: Uuid,
    category: &str,
) -> Result<(Rating, i32), sqlx::Error> {
    // Make sure the row exists so it can be locked, then lock it.
    sqlx::query(
        "INSERT INTO ratings (user_id, category, rating, deviation, volatility) VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
    )
    .bind(user)
    .bind(category)
    .bind(Rating::NEW.rating)
    .bind(Rating::NEW.deviation)
    .bind(Rating::NEW.volatility)
    .execute(&mut *db)
    .await?;
    let r = sqlx::query("SELECT rating, deviation, volatility, games_played FROM ratings WHERE user_id = $1 AND category = $2 FOR UPDATE")
        .bind(user)
        .bind(category)
        .fetch_one(&mut *db)
        .await?;
    Ok((
        Rating {
            rating: r.get("rating"),
            deviation: r.get("deviation"),
            volatility: r.get("volatility"),
        },
        r.get("games_played"),
    ))
}

/// Applies a finished game to both players. `white_score` is 1.0, 0.5 or 0.0. Each side is updated against the
/// opponent's rating from *before* this game. Called inside the transaction that finishes the game.
pub(crate) async fn apply_game(
    db: &mut PgConnection,
    game_id: Uuid,
    category: &str,
    white: Uuid,
    black: Uuid,
    white_score: f64,
) -> Result<(), sqlx::Error> {
    // Always lock in the same order (by user id), so two games finishing together cannot deadlock.
    let (first, second) = if white < black {
        (white, black)
    } else {
        (black, white)
    };
    let a = load_locked(db, first, category).await?;
    let b = load_locked(db, second, category).await?;
    let ((w_old, w_n), (b_old, b_n)) = if first == white { (a, b) } else { (b, a) };

    let w_new = rating::update(
        &w_old,
        &[Game {
            opponent: b_old,
            score: white_score,
        }],
        rating::DEFAULT_TAU,
    );
    let b_new = rating::update(
        &b_old,
        &[Game {
            opponent: w_old,
            score: 1.0 - white_score,
        }],
        rating::DEFAULT_TAU,
    );

    for (user, old, new, n) in [(white, w_old, w_new, w_n), (black, b_old, b_new, b_n)] {
        sqlx::query("UPDATE ratings SET rating = $3, deviation = $4, volatility = $5, games_played = $6, updated_at = now() WHERE user_id = $1 AND category = $2")
            .bind(user).bind(category).bind(new.rating).bind(new.deviation).bind(new.volatility).bind(n + 1)
            .execute(&mut *db).await?;
        sqlx::query("INSERT INTO rating_history (game_id, user_id, category, rating_before, deviation_before, volatility_before, rating_after, deviation_after, volatility_after) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)")
            .bind(game_id).bind(user).bind(category)
            .bind(old.rating).bind(old.deviation).bind(old.volatility)
            .bind(new.rating).bind(new.deviation).bind(new.volatility)
            .execute(&mut *db).await?;
    }
    Ok(())
}

/// A player's ratings in every class they have played (newcomer values are not stored, so none appear for unplayed ones).
pub async fn for_player(pool: &PgPool, user: Uuid) -> Result<Vec<PlayerRating>, sqlx::Error> {
    let rows = sqlx::query("SELECT category, rating, deviation, volatility, games_played FROM ratings WHERE user_id = $1 ORDER BY category")
        .bind(user)
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(row).collect())
}

fn row(r: &sqlx::postgres::PgRow) -> PlayerRating {
    PlayerRating {
        category: r.get("category"),
        rating: r.get("rating"),
        deviation: r.get("deviation"),
        volatility: r.get("volatility"),
        games_played: r.get("games_played"),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LeaderboardEntry {
    pub username: String,
    pub rating: f64,
    pub deviation: f64,
    pub games_played: i32,
}

/// The best players in a class. Only established ratings (low deviation) are listed, so a newcomer's lucky first
/// game cannot top the board.
pub async fn leaderboard(
    pool: &PgPool,
    category: &str,
    limit: i64,
) -> Result<Vec<LeaderboardEntry>, sqlx::Error> {
    let rows = sqlx::query("SELECT u.username, r.rating, r.deviation, r.games_played FROM ratings r JOIN users u ON u.id = r.user_id WHERE r.category = $1 AND r.deviation <= $2 ORDER BY r.rating DESC, u.username LIMIT $3")
        .bind(category)
        .bind(ESTABLISHED_DEVIATION)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(rows
        .iter()
        .map(|r| LeaderboardEntry {
            username: r.get("username"),
            rating: r.get("rating"),
            deviation: r.get("deviation"),
            games_played: r.get("games_played"),
        })
        .collect())
}
