//! Games in Postgres. Every operation that changes a game runs in one transaction that first locks the game's row,
//! so two requests for the same game (two moves, a move and a resignation) take turns and cannot both win.
//!
//! The rules themselves live in `eschess_game` and `eschess_domain`; this module loads the game, asks them what
//! happens, and saves the answer. It never decides whether a move is legal.

use std::fmt;

use eschess_domain::chess::{Color, Move, Position};
use eschess_domain::clock::{Clock, ClockState, LagPolicy, TimeControl};
use eschess_game::{
    offers_after_move, ActiveGame, Answered, DrawOffers, Ended, MoveOutcome, Refusal,
};
use sqlx::{PgConnection, PgPool, Row};

use crate::ratings;
use crate::stakes::{self, Stake};
use uuid::Uuid;

#[derive(Debug)]
pub enum GameError {
    /// No such game, or one the caller may not see (the two look the same, so ids cannot be probed).
    NotFound,
    /// Joining or cancelling a game that is not waiting for an opponent any more.
    NotOpen,
    /// Joining your own game.
    OwnGame,
    /// The creator already has the most open games a player may have.
    TooManyOpenGames,
    /// Leaving the matchmaking queue after a game was already found.
    AlreadyMatched,
    /// Moving in a game that is still waiting for an opponent.
    NotStarted,
    /// The player's rated play is suspended until this time (a fair-play sanction, ADR 0003).
    Suspended(String),
    /// The player's app reused a `client_move_id` for a different move.
    MoveIdReused,
    /// The rules said no (illegal move, not your turn, game over ...).
    Refused(Refusal),
    InvalidTimeControl(&'static str),
    /// A staked game needs a confirmed email (ADR 0004 §2).
    EmailNotConfirmed,
    /// A player with an open fair-play case, a suspension or a closure cannot stake (ADR 0004 §2).
    StakingRestricted,
    /// The wallet cannot cover the stake.
    InsufficientFunds,
    /// Staked games are not offered, or not at that amount, or not for that kind of game.
    StakeNotOffered(&'static str),
    /// The ledger refused a posting for a reason that is not the player's (a bug or a broken invariant).
    Ledger(String),
    Database(sqlx::Error),
}

impl fmt::Display for GameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GameError::NotFound => write!(f, "game not found"),
            GameError::NotOpen => write!(f, "that game is not open"),
            GameError::OwnGame => write!(f, "you cannot join your own game"),
            GameError::TooManyOpenGames => {
                write!(f, "you already have {MAX_OPEN_GAMES} open games")
            }
            GameError::AlreadyMatched => write!(f, "a game was already found for you"),
            GameError::Suspended(until) => {
                write!(f, "your rated play is suspended until {until}")
            }
            GameError::NotStarted => {
                write!(f, "the game has not started: it is waiting for an opponent")
            }
            GameError::MoveIdReused => write!(
                f,
                "that client_move_id was already used for a different move"
            ),
            GameError::Refused(r) => write!(f, "{r}"),
            GameError::InvalidTimeControl(why) => write!(f, "{why}"),
            GameError::EmailNotConfirmed => write!(f, "confirm your email before staking money"),
            GameError::StakingRestricted => write!(f, "you cannot stake money right now"),
            GameError::InsufficientFunds => write!(f, "your balance does not cover that stake"),
            GameError::StakeNotOffered(why) => write!(f, "{why}"),
            GameError::Ledger(e) => write!(f, "ledger error: {e}"),
            GameError::Database(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for GameError {}

impl From<sqlx::Error> for GameError {
    fn from(e: sqlx::Error) -> Self {
        GameError::Database(e)
    }
}

impl From<Refusal> for GameError {
    fn from(r: Refusal) -> Self {
        GameError::Refused(r)
    }
}

/// The most games one player may have open (waiting for an opponent) at a time, so nobody can fill the lobby.
pub const MAX_OPEN_GAMES: i64 = 3;

type Result<T> = std::result::Result<T, GameError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Player {
    pub id: Uuid,
    pub username: String,
}

/// A game as a player sees it.
#[derive(Debug, Clone)]
pub struct GameView {
    pub id: Uuid,
    pub status: String,
    pub created_by: Uuid,
    pub color_preference: String,
    /// Whether the result counts for ratings. Fixed when the game is opened.
    pub rated: bool,
    /// What each player puts in, in minor units, and the asset; both `None` for an unstaked game.
    pub stake_minor: Option<i64>,
    pub stake_asset: Option<String>,
    pub white: Option<Player>,
    pub black: Option<Player>,
    pub base_ms: i64,
    pub increment_ms: i64,
    pub fen: String,
    pub ply: i32,
    /// Goes up by one on every change to the game. A client that sees a gap has missed an update.
    pub event_seq: i32,
    pub moves: Vec<String>,
    /// Whose turn it is ("white" or "black"), while the game is being played.
    pub turn: Option<&'static str>,
    /// Time left for each side as of `as_of_ms` (the server's time): the player to move is running down.
    pub white_ms: i64,
    pub black_ms: i64,
    pub as_of_ms: i64,
    pub result: Option<String>,
    pub end_reason: Option<String>,
    /// The colour that has a draw offer waiting ("white" or "black"), if any.
    pub draw_offer: Option<String>,
    /// The moves the viewer may play right now: only filled in when it is their turn.
    pub legal_moves: Vec<String>,
}

/// What a move did, for the reply to the player who made it.
/// How a move reached the server, and what the server measured about the connection. Recorded with the move for fair
/// play (ADR 0002 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Plain HTTP: the server cannot measure the player's lag, so none is refunded.
    Http,
    /// The WebSocket, with the lag the server measured for this connection.
    WebSocket { measured_lag_ms: u64 },
}

impl Transport {
    fn measured_lag_ms(self) -> Option<u64> {
        match self {
            Transport::Http => None,
            Transport::WebSocket { measured_lag_ms } => Some(measured_lag_ms),
        }
    }

    fn as_db(self) -> &'static str {
        match self {
            Transport::Http => "http",
            Transport::WebSocket { .. } => "ws",
        }
    }
}

#[derive(Debug, Clone)]
pub struct MoveReceipt {
    pub ply: i32,
    pub uci: String,
    /// This `client_move_id` had already been played: nothing new happened and this is the original outcome.
    pub replayed: bool,
    pub charged_ms: i64,
    pub credited_ms: i64,
}

struct GameRow {
    id: Uuid,
    created_by: Uuid,
    color_preference: String,
    rated: bool,
    stake_minor: Option<i64>,
    stake_asset: Option<String>,
    white_id: Option<Uuid>,
    black_id: Option<Uuid>,
    base_ms: i64,
    increment_ms: i64,
    status: String,
    result: Option<String>,
    end_reason: Option<String>,
    fen: String,
    ply: i32,
    event_seq: i32,
    remaining: [i64; 2],
    quota: [i64; 2],
    turn_started_at_ms: Option<i64>,
    draw_offered_by: Option<String>,
    offer_ply: [Option<i32>; 2],
}

const COLS: &str = "id, created_by, color_preference, rated, stake_minor, stake_asset, white_id, black_id, base_ms, increment_ms, status, result, end_reason, fen, ply, event_seq, white_remaining_ms, black_remaining_ms, white_lag_quota_ms, black_lag_quota_ms, turn_started_at_ms, draw_offered_by, white_offer_ply, black_offer_ply";

fn row_from(r: &sqlx::postgres::PgRow) -> GameRow {
    GameRow {
        id: r.get("id"),
        created_by: r.get("created_by"),
        color_preference: r.get("color_preference"),
        rated: r.get("rated"),
        stake_minor: r.get("stake_minor"),
        stake_asset: r.get("stake_asset"),
        white_id: r.get("white_id"),
        black_id: r.get("black_id"),
        base_ms: r.get("base_ms"),
        increment_ms: r.get("increment_ms"),
        status: r.get("status"),
        result: r.get("result"),
        end_reason: r.get("end_reason"),
        fen: r.get("fen"),
        ply: r.get("ply"),
        event_seq: r.get("event_seq"),
        remaining: [r.get("white_remaining_ms"), r.get("black_remaining_ms")],
        quota: [r.get("white_lag_quota_ms"), r.get("black_lag_quota_ms")],
        turn_started_at_ms: r.get("turn_started_at_ms"),
        draw_offered_by: r.get("draw_offered_by"),
        offer_ply: [r.get("white_offer_ply"), r.get("black_offer_ply")],
    }
}

/// The server's time in milliseconds. Every server takes it from the database, so no two disagree about the clock.
async fn now_ms(db: &mut PgConnection) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT (extract(epoch from clock_timestamp()) * 1000)::bigint")
            .fetch_one(&mut *db)
            .await?,
    )
}

async fn load_locked(db: &mut PgConnection, id: Uuid) -> Result<GameRow> {
    let r = sqlx::query(&format!(
        "SELECT {COLS} FROM games WHERE id = $1 FOR UPDATE"
    ))
    .bind(id)
    .fetch_optional(&mut *db)
    .await?
    .ok_or(GameError::NotFound)?;
    Ok(row_from(&r))
}

async fn moves_of(db: &mut PgConnection, id: Uuid) -> Result<Vec<String>> {
    Ok(
        sqlx::query_scalar("SELECT uci FROM game_moves WHERE game_id = $1 ORDER BY ply")
            .bind(id)
            .fetch_all(&mut *db)
            .await?,
    )
}

/// A game being played, rebuilt from its row and its moves.
fn active_game(row: &GameRow, moves: &[String]) -> Result<ActiveGame> {
    let (Some(white), Some(black), Some(started)) =
        (row.white_id, row.black_id, row.turn_started_at_ms)
    else {
        return Err(GameError::Refused(Refusal::CorruptHistory));
    };
    let control = TimeControl::new(row.base_ms as u64, row.increment_ms as u64)
        .map_err(|_| GameError::Refused(Refusal::CorruptHistory))?;
    let state = ClockState {
        control,
        policy: LagPolicy::default(),
        remaining_ms: [row.remaining[0] as u64, row.remaining[1] as u64],
        lag_quota_ms: [row.quota[0] as u64, row.quota[1] as u64],
        turn: if row.ply % 2 == 0 {
            Color::White
        } else {
            Color::Black
        },
        turn_started_at_ms: started as u64,
    };
    let clock = Clock::from_state(state).map_err(|e| GameError::Refused(Refusal::Clock(e)))?;
    let moves = moves
        .iter()
        .map(|m| Move::from_uci(m).ok_or(GameError::Refused(Refusal::CorruptHistory)))
        .collect::<Result<Vec<_>>>()?;
    Ok(ActiveGame {
        white,
        black,
        moves,
        clock,
    })
}

/// Tells every server (and so every connected player) that this game has changed. Postgres delivers the notification
/// when the transaction commits, so it is sent if and only if the change really happened.
async fn notify(db: &mut PgConnection, id: Uuid) -> Result<()> {
    sqlx::query("SELECT pg_notify('game_events', $1)")
        .bind(id.to_string())
        .execute(&mut *db)
        .await?;
    Ok(())
}

async fn finish(db: &mut PgConnection, id: Uuid, ended: Ended) -> Result<()> {
    notify(db, id).await?;
    sqlx::query("UPDATE games SET status = 'finished', result = $2, end_reason = $3, ended_at = now(), draw_offered_by = NULL, event_seq = event_seq + 1 WHERE id = $1 AND status = 'active'")
        .bind(id)
        .bind(ended.result.as_str())
        .bind(ended.reason.as_str())
        .execute(&mut *db)
        .await?;
    rate(db, id, ended).await
}

/// Updates both players' ratings for a finished game, if it was played enough to count.
async fn rate(db: &mut PgConnection, id: Uuid, ended: Ended) -> Result<()> {
    let r = sqlx::query(
        "SELECT white_id, black_id, ply, base_ms, increment_ms, rated FROM games WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&mut *db)
    .await?;
    if !r.get::<bool, _>("rated") || r.get::<i32, _>("ply") < ratings::MIN_PLIES_FOR_RATING {
        return Ok(());
    }
    let (Some(white), Some(black)) = (
        r.get::<Option<Uuid>, _>("white_id"),
        r.get::<Option<Uuid>, _>("black_id"),
    ) else {
        return Ok(());
    };
    let Ok(control) = TimeControl::new(
        r.get::<i64, _>("base_ms") as u64,
        r.get::<i64, _>("increment_ms") as u64,
    ) else {
        return Ok(());
    };
    let score = match ended.result {
        eschess_game::GameResult::White => 1.0,
        eschess_game::GameResult::Draw => 0.5,
        eschess_game::GameResult::Black => 0.0,
    };
    ratings::apply_game(db, id, control.category().as_str(), white, black, score).await?;
    Ok(())
}

/// If the player to move has run out of time (and no move of theirs can still be in flight), end the game now. Any
/// request that touches a game does this first, so a game never sits "active" after a flag has fallen.
async fn settle_flag(
    db: &mut PgConnection,
    row: &GameRow,
    moves: &[String],
    at_ms: i64,
) -> Result<Option<Ended>> {
    if row.status != "active" {
        return Ok(None);
    }
    let game = active_game(row, moves)?;
    let ended = eschess_game::check_flag(&game, at_ms as u64)?;
    if let Some(ended) = ended {
        finish(db, row.id, ended).await?;
    }
    Ok(ended)
}

async fn username(db: &mut PgConnection, id: Option<Uuid>) -> Result<Option<Player>> {
    let Some(id) = id else { return Ok(None) };
    let name: Option<String> = sqlx::query_scalar("SELECT username FROM users WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut *db)
        .await?;
    Ok(name.map(|username| Player { id, username }))
}

/// Builds what `viewer` is allowed to see, or `NotFound`: a game in progress or finished is visible only to its two
/// players, a waiting game to everyone (it is in the lobby), a cancelled one to its creator.
async fn view(db: &mut PgConnection, id: Uuid, viewer: Uuid) -> Result<GameView> {
    let r = sqlx::query(&format!("SELECT {COLS} FROM games WHERE id = $1"))
        .bind(id)
        .fetch_optional(&mut *db)
        .await?
        .ok_or(GameError::NotFound)?;
    let row = row_from(&r);
    let is_player = row.white_id == Some(viewer) || row.black_id == Some(viewer);
    let visible = match row.status.as_str() {
        "waiting" => true,
        "cancelled" => row.created_by == viewer,
        _ => is_player,
    };
    if !visible {
        return Err(GameError::NotFound);
    }
    let moves = moves_of(db, id).await?;
    let now = now_ms(db).await?;

    let (mut white_ms, mut black_ms, mut turn, mut legal_moves) =
        (row.remaining[0], row.remaining[1], None, Vec::new());
    if row.status == "active" {
        let game = active_game(&row, &moves)?;
        white_ms = game.clock.time_left_ms(Color::White, now as u64) as i64;
        black_ms = game.clock.time_left_ms(Color::Black, now as u64) as i64;
        let to_move = game.clock.turn();
        turn = Some(if to_move == Color::White {
            "white"
        } else {
            "black"
        });
        if game.player(to_move) == viewer {
            legal_moves = game
                .rules()?
                .position()
                .legal_moves()
                .iter()
                .map(|m| m.to_uci())
                .collect();
        }
    }
    Ok(GameView {
        id: row.id,
        status: row.status.clone(),
        created_by: row.created_by,
        color_preference: row.color_preference.clone(),
        rated: row.rated,
        stake_minor: row.stake_minor,
        stake_asset: row.stake_asset.clone(),
        white: username(db, row.white_id).await?,
        black: username(db, row.black_id).await?,
        base_ms: row.base_ms,
        increment_ms: row.increment_ms,
        fen: row.fen.clone(),
        ply: row.ply,
        event_seq: row.event_seq,
        moves,
        turn,
        white_ms,
        black_ms,
        as_of_ms: now,
        result: row.result.clone(),
        end_reason: row.end_reason.clone(),
        draw_offer: row.draw_offered_by.clone(),
        legal_moves,
    })
}

/// Opens a game for someone to join. The colours are decided when they do. A player may have only a few open at once.
pub async fn create_game(
    pool: &PgPool,
    creator: Uuid,
    base_ms: u64,
    increment_ms: u64,
    color_preference: &str,
    rated: bool,
    stake: Option<&Stake>,
) -> Result<Uuid> {
    let control = TimeControl::new(base_ms, increment_ms).map_err(|_| GameError::InvalidTimeControl("the base time must be between 1 second and 3 hours and the increment at most 3 minutes"))?;
    if stake.is_some() && !rated {
        return Err(GameError::StakeNotOffered("only rated games can be staked"));
    }
    let id = Uuid::now_v7();
    let clock = Clock::start(control, LagPolicy::default(), 0).state();
    let mut tx = pool.begin().await?;
    if rated {
        if let Some(until) = crate::fairplay::suspended_until(&mut *tx, creator).await? {
            return Err(GameError::Suspended(until));
        }
    }
    // Two requests from one player at the same moment must not both slip under the limit.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(format!("open-games:{creator}"))
        .execute(&mut *tx)
        .await?;
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM games WHERE created_by = $1 AND status = 'waiting'",
    )
    .bind(creator)
    .fetch_one(&mut *tx)
    .await?;
    if open >= MAX_OPEN_GAMES {
        return Err(GameError::TooManyOpenGames);
    }
    sqlx::query(
        "INSERT INTO games (id, created_by, color_preference, rated, base_ms, increment_ms, fen, white_remaining_ms, black_remaining_ms, white_lag_quota_ms, black_lag_quota_ms, stake_minor, stake_asset)
         VALUES ($1,$2,$3,$11,$4,$5,$6,$7,$8,$9,$10,$12,$13)",
    )
    .bind(id)
    .bind(creator)
    .bind(color_preference)
    .bind(base_ms as i64)
    .bind(increment_ms as i64)
    .bind(Position::starting().to_fen())
    .bind(clock.remaining_ms[0] as i64)
    .bind(clock.remaining_ms[1] as i64)
    .bind(clock.lag_quota_ms[0] as i64)
    .bind(clock.lag_quota_ms[1] as i64)
    .bind(rated)
    .bind(stake.map(|s| s.minor))
    .bind(stake.map(|s| s.asset.to_string()))
    .execute(&mut *tx)
    .await?;
    // The creator's stake is held in the same transaction, so a game never exists without its first stake.
    if let Some(stake) = stake {
        stakes::hold(&mut tx, id, creator, stake).await?;
    }
    tx.commit().await?;
    Ok(id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Games waiting for an opponent, other than the viewer's own.
    Open,
    /// The viewer's own games that are waiting or being played (what an app loads after a restart or reconnect).
    MyActive,
    /// The viewer's finished games, newest first.
    MyFinished,
}

pub async fn list_games(
    pool: &PgPool,
    viewer: Uuid,
    scope: Scope,
    limit: i64,
) -> Result<Vec<GameView>> {
    let sql = match scope {
        Scope::Open => "SELECT id FROM games WHERE status = 'waiting' AND created_by <> $1 ORDER BY created_at DESC, id DESC LIMIT $2",
        Scope::MyActive => "SELECT id FROM games WHERE status IN ('waiting', 'active') AND (created_by = $1 OR white_id = $1 OR black_id = $1) ORDER BY created_at DESC, id DESC LIMIT $2",
        Scope::MyFinished => "SELECT id FROM games WHERE status = 'finished' AND (white_id = $1 OR black_id = $1) ORDER BY ended_at DESC, id DESC LIMIT $2",
    };
    let ids: Vec<Uuid> = sqlx::query_scalar(sql)
        .bind(viewer)
        .bind(limit.clamp(1, 100))
        .fetch_all(pool)
        .await?;
    let mut conn = pool.acquire().await?;
    let mut out = Vec::new();
    for id in ids {
        // Listing also settles any game whose flag has fallen, so the answer is never stale.
        out.push(get_game_on(&mut conn, id, viewer).await?);
    }
    Ok(out)
}

async fn get_game_on(conn: &mut PgConnection, id: Uuid, viewer: Uuid) -> Result<GameView> {
    // Settle a fallen flag in its own short transaction first, then read.
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    if let Ok(row) = load_locked(&mut tx, id).await {
        if row.status == "active" {
            let moves = moves_of(&mut tx, id).await?;
            let now = now_ms(&mut tx).await?;
            settle_flag(&mut tx, &row, &moves, now).await?;
        }
    }
    tx.commit().await?;
    view(conn, id, viewer).await
}

pub async fn get_game(pool: &PgPool, id: Uuid, viewer: Uuid) -> Result<GameView> {
    let mut conn = pool.acquire().await?;
    get_game_on(&mut conn, id, viewer).await
}

/// Joins a waiting game, starting it: the colours are assigned and White's clock starts now. `creator_plays_white`
/// is only used when the creator asked for a random colour; the caller supplies the coin flip so this stays
/// deterministic and testable.
pub async fn join_game(
    pool: &PgPool,
    id: Uuid,
    joiner: Uuid,
    creator_plays_white: bool,
) -> Result<GameView> {
    let mut tx = pool.begin().await?;
    let row = load_locked(&mut tx, id).await?;
    if row.status != "waiting" {
        return Err(GameError::NotOpen);
    }
    if row.created_by == joiner {
        return Err(GameError::OwnGame);
    }
    if row.rated {
        if let Some(until) = crate::fairplay::suspended_until(&mut *tx, joiner).await? {
            return Err(GameError::Suspended(until));
        }
    }
    // The joiner's stake is held in the transaction that starts the game: a game never starts with one stake in. If it
    // cannot be held the join fails and nothing moves.
    if let Some(stake) = stakes::stake_of(row.stake_minor, row.stake_asset.as_deref())? {
        stakes::hold(&mut tx, id, joiner, &stake).await?;
    }
    let creator_white = match row.color_preference.as_str() {
        "white" => true,
        "black" => false,
        _ => creator_plays_white,
    };
    let (white, black) = if creator_white {
        (row.created_by, joiner)
    } else {
        (joiner, row.created_by)
    };
    let now = now_ms(&mut tx).await?;
    let control = TimeControl::new(row.base_ms as u64, row.increment_ms as u64)
        .map_err(|_| GameError::Refused(Refusal::CorruptHistory))?;
    let clock = Clock::start(control, LagPolicy::default(), now as u64).state();
    sqlx::query(
        "UPDATE games SET status = 'active', white_id = $2, black_id = $3, started_at = now(), turn_started_at_ms = $4,
             white_remaining_ms = $5, black_remaining_ms = $6, white_lag_quota_ms = $7, black_lag_quota_ms = $8, event_seq = event_seq + 1
         WHERE id = $1",
    )
    .bind(id)
    .bind(white)
    .bind(black)
    .bind(now)
    .bind(clock.remaining_ms[0] as i64)
    .bind(clock.remaining_ms[1] as i64)
    .bind(clock.lag_quota_ms[0] as i64)
    .bind(clock.lag_quota_ms[1] as i64)
    .execute(&mut *tx)
    .await?;
    notify(&mut tx, id).await?;
    tx.commit().await?;
    let mut conn = pool.acquire().await?;
    view(&mut conn, id, joiner).await
}

/// The creator withdraws a game nobody has joined.
pub async fn cancel_game(pool: &PgPool, id: Uuid, creator: Uuid) -> Result<()> {
    let mut tx = pool.begin().await?;
    let row = load_locked(&mut tx, id).await?;
    if row.created_by != creator {
        return Err(GameError::NotFound);
    }
    if row.status != "waiting" {
        return Err(GameError::NotOpen);
    }
    sqlx::query("UPDATE games SET status = 'cancelled', event_seq = event_seq + 1 WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    if let Some(stake) = stakes::stake_of(row.stake_minor, row.stake_asset.as_deref())? {
        stakes::release(&mut tx, id, creator, &stake).await?;
    }
    notify(&mut tx, id).await?;
    tx.commit().await?;
    Ok(())
}

/// Locks a game for a player's action, checking they belong to it, and settles a fallen flag. Returns the row, the
/// moves, and the end that was settled (if any).
async fn lock_for_player(
    tx: &mut PgConnection,
    id: Uuid,
    actor: Uuid,
) -> Result<(GameRow, Vec<String>, i64)> {
    let row = load_locked(tx, id).await?;
    if row.white_id != Some(actor) && row.black_id != Some(actor) {
        // A waiting game's creator is also "in" it, but cannot act until someone joins.
        return Err(if row.status == "waiting" && row.created_by == actor {
            GameError::NotStarted
        } else {
            GameError::NotFound
        });
    }
    let moves = moves_of(tx, id).await?;
    let now = now_ms(tx).await?;
    Ok((row, moves, now))
}

/// Plays a move. Sending the same `client_move_id` again (a retry after a dropped connection) returns the original
/// outcome and changes nothing, even if the opponent has moved in the meantime.
pub async fn play_move(
    pool: &PgPool,
    id: Uuid,
    actor: Uuid,
    uci: &str,
    client_move_id: Uuid,
    transport: Transport,
) -> Result<(GameView, MoveReceipt)> {
    let mut tx = pool.begin().await?;
    let (row, moves, now) = lock_for_player(&mut tx, id, actor).await?;

    // A retry: answer with what happened the first time.
    if let Some(prev) = sqlx::query("SELECT ply, uci, mover_id, charged_ms, credited_ms FROM game_moves WHERE game_id = $1 AND client_move_id = $2").bind(id).bind(client_move_id).fetch_optional(&mut *tx).await? {
        let (same_mover, same_move): (bool, bool) = (prev.get::<Uuid, _>("mover_id") == actor, prev.get::<String, _>("uci") == uci);
        if !(same_mover && same_move) {
            return Err(GameError::MoveIdReused);
        }
        let receipt = MoveReceipt { ply: prev.get("ply"), uci: uci.to_string(), replayed: true, charged_ms: prev.get("charged_ms"), credited_ms: prev.get("credited_ms") };
        drop(tx);
        let mut conn = pool.acquire().await?;
        return Ok((view(&mut conn, id, actor).await?, receipt));
    }

    if row.status != "active" {
        return Err(if row.status == "waiting" {
            GameError::NotStarted
        } else {
            GameError::Refused(Refusal::GameOver)
        });
    }
    if settle_flag(&mut tx, &row, &moves, now).await?.is_some() {
        tx.commit().await?; // the game ended on time before this move arrived
        return Err(GameError::Refused(Refusal::GameOver));
    }

    let game = active_game(&row, &moves)?;
    // The lag is what the server itself measured for this player's connection (none over plain HTTP, which offers no
    // way to measure it): the clock refunds part of it, within its caps and quota.
    let measured_lag_ms = transport.measured_lag_ms();
    match eschess_game::play_move(&game, actor, uci, now as u64, measured_lag_ms.unwrap_or(0))? {
        MoveOutcome::Flagged(ended) => {
            finish(&mut tx, id, ended).await?;
            tx.commit().await?;
            Err(GameError::Refused(Refusal::GameOver))
        }
        MoveOutcome::Played(applied) => {
            let ply = row.ply + 1;
            let state = applied.clock.state();
            let mover = game.color_of(actor).expect("checked by the move loop");
            sqlx::query("INSERT INTO game_moves (game_id, ply, uci, fen_after, mover_id, client_move_id, charged_ms, credited_ms, remaining_ms_after, played_at_ms, transport, lag_measured_ms) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)")
                .bind(id)
                .bind(ply)
                .bind(applied.mv.to_uci())
                .bind(&applied.fen_after)
                .bind(actor)
                .bind(client_move_id)
                .bind(applied.charged_ms as i64)
                .bind(applied.credited_ms as i64)
                .bind(applied.clock.banked_ms(mover) as i64)
                .bind(now)
                .bind(transport.as_db())
                .bind(measured_lag_ms.map(|l| l as i64))
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "UPDATE games SET fen = $2, ply = $3, white_remaining_ms = $4, black_remaining_ms = $5, white_lag_quota_ms = $6, black_lag_quota_ms = $7, turn_started_at_ms = $8, draw_offered_by = $9, event_seq = event_seq + 1 WHERE id = $1",
            )
            .bind(id)
            .bind(&applied.fen_after)
            .bind(ply)
            .bind(state.remaining_ms[0] as i64)
            .bind(state.remaining_ms[1] as i64)
            .bind(state.lag_quota_ms[0] as i64)
            .bind(state.lag_quota_ms[1] as i64)
            .bind(state.turn_started_at_ms as i64)
            .bind(offers_after_move(&offers_of(&row), mover).pending.map(color_name))
            .execute(&mut *tx)
            .await?;
            notify(&mut tx, id).await?;
            if let Some(ended) = applied.ended {
                finish(&mut tx, id, ended).await?;
            }
            tx.commit().await?;
            let receipt = MoveReceipt {
                ply,
                uci: applied.mv.to_uci(),
                replayed: false,
                charged_ms: applied.charged_ms as i64,
                credited_ms: applied.credited_ms as i64,
            };
            let mut conn = pool.acquire().await?;
            Ok((view(&mut conn, id, actor).await?, receipt))
        }
    }
}

fn color_name(c: Color) -> &'static str {
    match c {
        Color::White => "white",
        Color::Black => "black",
    }
}

fn offers_of(row: &GameRow) -> DrawOffers {
    DrawOffers {
        pending: match row.draw_offered_by.as_deref() {
            Some("white") => Some(Color::White),
            Some("black") => Some(Color::Black),
            _ => None,
        },
        last_offer_ply: [
            row.offer_ply[0].map(|p| p as u32),
            row.offer_ply[1].map(|p| p as u32),
        ],
    }
}

/// Makes a draw offer, or, if the opponent already has one waiting, agrees the draw.
pub async fn offer_draw(pool: &PgPool, id: Uuid, actor: Uuid) -> Result<GameView> {
    change_offers(pool, id, actor, |game, offers| {
        eschess_game::offer_draw(game, actor, offers)
    })
    .await
}

/// Accepts (the game is drawn) or declines the draw offer waiting for the player. The player who made the offer may
/// decline it too, which withdraws it.
pub async fn answer_draw_offer(
    pool: &PgPool,
    id: Uuid,
    actor: Uuid,
    accept: bool,
) -> Result<GameView> {
    change_offers(pool, id, actor, |game, offers| {
        eschess_game::answer_draw_offer(game, actor, offers, accept)
    })
    .await
}

async fn change_offers(
    pool: &PgPool,
    id: Uuid,
    actor: Uuid,
    decide: impl FnOnce(&ActiveGame, &DrawOffers) -> std::result::Result<Answered, Refusal>,
) -> Result<GameView> {
    let mut tx = pool.begin().await?;
    let (row, moves, now) = lock_for_player(&mut tx, id, actor).await?;
    if row.status != "active" {
        return Err(if row.status == "waiting" {
            GameError::NotStarted
        } else {
            GameError::Refused(Refusal::GameOver)
        });
    }
    if settle_flag(&mut tx, &row, &moves, now).await?.is_some() {
        tx.commit().await?;
        return Err(GameError::Refused(Refusal::GameOver));
    }
    let game = active_game(&row, &moves)?;
    let answered = decide(&game, &offers_of(&row))?;
    if let Some(ended) = answered.ended {
        finish(&mut tx, id, ended).await?;
    }
    // (A finished game has its offer cleared by `finish`; the offer bookkeeping below is harmless for it.)
    sqlx::query("UPDATE games SET draw_offered_by = $2, white_offer_ply = $3, black_offer_ply = $4, event_seq = event_seq + $5 WHERE id = $1")
        .bind(id)
        .bind(answered.offers.pending.map(color_name).filter(|_| answered.ended.is_none()))
        .bind(answered.offers.last_offer_ply[0].map(|p| p as i32))
        .bind(answered.offers.last_offer_ply[1].map(|p| p as i32))
        .bind(if answered.ended.is_some() { 0 } else { 1 })
        .execute(&mut *tx)
        .await?;
    if answered.ended.is_none() {
        notify(&mut tx, id).await?;
    }
    tx.commit().await?;
    let mut conn = pool.acquire().await?;
    view(&mut conn, id, actor).await
}

async fn end_by_player(
    pool: &PgPool,
    id: Uuid,
    actor: Uuid,
    decide: impl FnOnce(&ActiveGame, Uuid) -> std::result::Result<Ended, Refusal>,
) -> Result<GameView> {
    let mut tx = pool.begin().await?;
    let (row, moves, now) = lock_for_player(&mut tx, id, actor).await?;
    if row.status != "active" {
        return Err(if row.status == "waiting" {
            GameError::NotStarted
        } else {
            GameError::Refused(Refusal::GameOver)
        });
    }
    if settle_flag(&mut tx, &row, &moves, now).await?.is_some() {
        tx.commit().await?;
        return Err(GameError::Refused(Refusal::GameOver));
    }
    let game = active_game(&row, &moves)?;
    let ended = decide(&game, actor)?;
    finish(&mut tx, id, ended).await?;
    tx.commit().await?;
    let mut conn = pool.acquire().await?;
    view(&mut conn, id, actor).await
}

pub async fn resign(pool: &PgPool, id: Uuid, actor: Uuid) -> Result<GameView> {
    end_by_player(pool, id, actor, eschess_game::resign).await
}

pub async fn claim_draw(pool: &PgPool, id: Uuid, actor: Uuid) -> Result<GameView> {
    end_by_player(pool, id, actor, eschess_game::claim_draw).await
}

/// A read-only look at a game as `viewer` may see it. Unlike [`get_game`] it never changes anything, so it is cheap and
/// safe to call often (live updates, clock syncs).
pub async fn peek_game(pool: &PgPool, id: Uuid, viewer: Uuid) -> Result<GameView> {
    let mut conn = pool.acquire().await?;
    view(&mut conn, id, viewer).await
}

/// Has the player to move in this game run out of time, with nothing left that could save a move in flight? A cheap
/// check for a timer; if it says yes, call [`get_game`], which settles the game.
pub async fn flag_due(pool: &PgPool, id: Uuid) -> Result<bool> {
    let due: Option<bool> = sqlx::query_scalar(
        "SELECT (extract(epoch from clock_timestamp()) * 1000)::bigint >=
                turn_started_at_ms + CASE WHEN ply % 2 = 0 THEN white_remaining_ms ELSE black_remaining_ms END
                + LEAST($2::bigint, CASE WHEN ply % 2 = 0 THEN white_lag_quota_ms ELSE black_lag_quota_ms END)
         FROM games WHERE id = $1 AND status = 'active'",
    )
    .bind(id)
    .bind(LagPolicy::default().max_credit_per_move_ms as i64)
    .fetch_optional(pool)
    .await?;
    Ok(due.unwrap_or(false))
}

/// Creates a game that starts at once between two players, for matchmaking. `first` is the one who has waited longer and
/// is recorded as the creator. Runs inside the caller's transaction.
pub(crate) async fn start_matched(
    db: &mut PgConnection,
    first: Uuid,
    second: Uuid,
    control: TimeControl,
    rated: bool,
    first_plays_white: bool,
) -> Result<Uuid> {
    let id = Uuid::now_v7();
    let now = now_ms(db).await?;
    let clock = Clock::start(control, LagPolicy::default(), now as u64).state();
    let (white, black) = if first_plays_white {
        (first, second)
    } else {
        (second, first)
    };
    sqlx::query(
        "INSERT INTO games (id, created_by, color_preference, rated, white_id, black_id, base_ms, increment_ms, status, fen,
             white_remaining_ms, black_remaining_ms, white_lag_quota_ms, black_lag_quota_ms, turn_started_at_ms, started_at, event_seq)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,'active',$9,$10,$11,$12,$13,$14,now(),1)",
    )
    .bind(id)
    .bind(first)
    .bind(if first_plays_white { "white" } else { "black" })
    .bind(rated)
    .bind(white)
    .bind(black)
    .bind(control.base_ms() as i64)
    .bind(control.increment_ms() as i64)
    .bind(Position::starting().to_fen())
    .bind(clock.remaining_ms[0] as i64)
    .bind(clock.remaining_ms[1] as i64)
    .bind(clock.lag_quota_ms[0] as i64)
    .bind(clock.lag_quota_ms[1] as i64)
    .bind(now)
    .execute(&mut *db)
    .await?;
    Ok(id)
}
