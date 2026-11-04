//! Games in Postgres. Every operation that changes a game runs in one transaction that first locks the game's row,
//! so two requests for the same game (two moves, a move and a resignation) take turns and cannot both win.
//!
//! The rules themselves live in `eschess_game` and `eschess_domain`; this module loads the game, asks them what
//! happens, and saves the answer. It never decides whether a move is legal.

use std::fmt;

use eschess_domain::chess::{Color, Move, Position};
use eschess_domain::clock::{Clock, ClockState, LagPolicy, TimeControl};
use eschess_game::{ActiveGame, Ended, MoveOutcome, Refusal};
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

#[derive(Debug)]
pub enum GameError {
    /// No such game, or one the caller may not see (the two look the same, so ids cannot be probed).
    NotFound,
    /// Joining or cancelling a game that is not waiting for an opponent any more.
    NotOpen,
    /// Joining your own game.
    OwnGame,
    /// Moving in a game that is still waiting for an opponent.
    NotStarted,
    /// The player's app reused a `client_move_id` for a different move.
    MoveIdReused,
    /// The rules said no (illegal move, not your turn, game over ...).
    Refused(Refusal),
    InvalidTimeControl(&'static str),
    Database(sqlx::Error),
}

impl fmt::Display for GameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GameError::NotFound => write!(f, "game not found"),
            GameError::NotOpen => write!(f, "that game is not open"),
            GameError::OwnGame => write!(f, "you cannot join your own game"),
            GameError::NotStarted => {
                write!(f, "the game has not started: it is waiting for an opponent")
            }
            GameError::MoveIdReused => write!(
                f,
                "that client_move_id was already used for a different move"
            ),
            GameError::Refused(r) => write!(f, "{r}"),
            GameError::InvalidTimeControl(why) => write!(f, "{why}"),
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
    pub white: Option<Player>,
    pub black: Option<Player>,
    pub base_ms: i64,
    pub increment_ms: i64,
    pub fen: String,
    pub ply: i32,
    pub moves: Vec<String>,
    /// Whose turn it is ("white" or "black"), while the game is being played.
    pub turn: Option<&'static str>,
    /// Time left for each side as of `as_of_ms` (the server's time): the player to move is running down.
    pub white_ms: i64,
    pub black_ms: i64,
    pub as_of_ms: i64,
    pub result: Option<String>,
    pub end_reason: Option<String>,
    /// The moves the viewer may play right now: only filled in when it is their turn.
    pub legal_moves: Vec<String>,
}

/// What a move did, for the reply to the player who made it.
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
    white_id: Option<Uuid>,
    black_id: Option<Uuid>,
    base_ms: i64,
    increment_ms: i64,
    status: String,
    result: Option<String>,
    end_reason: Option<String>,
    fen: String,
    ply: i32,
    remaining: [i64; 2],
    quota: [i64; 2],
    turn_started_at_ms: Option<i64>,
}

const COLS: &str = "id, created_by, color_preference, white_id, black_id, base_ms, increment_ms, status, result, end_reason, fen, ply, white_remaining_ms, black_remaining_ms, white_lag_quota_ms, black_lag_quota_ms, turn_started_at_ms";

fn row_from(r: &sqlx::postgres::PgRow) -> GameRow {
    GameRow {
        id: r.get("id"),
        created_by: r.get("created_by"),
        color_preference: r.get("color_preference"),
        white_id: r.get("white_id"),
        black_id: r.get("black_id"),
        base_ms: r.get("base_ms"),
        increment_ms: r.get("increment_ms"),
        status: r.get("status"),
        result: r.get("result"),
        end_reason: r.get("end_reason"),
        fen: r.get("fen"),
        ply: r.get("ply"),
        remaining: [r.get("white_remaining_ms"), r.get("black_remaining_ms")],
        quota: [r.get("white_lag_quota_ms"), r.get("black_lag_quota_ms")],
        turn_started_at_ms: r.get("turn_started_at_ms"),
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

async fn finish(db: &mut PgConnection, id: Uuid, ended: Ended) -> Result<()> {
    sqlx::query("UPDATE games SET status = 'finished', result = $2, end_reason = $3, ended_at = now() WHERE id = $1 AND status = 'active'")
        .bind(id)
        .bind(ended.result.as_str())
        .bind(ended.reason.as_str())
        .execute(&mut *db)
        .await?;
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
        white: username(db, row.white_id).await?,
        black: username(db, row.black_id).await?,
        base_ms: row.base_ms,
        increment_ms: row.increment_ms,
        fen: row.fen.clone(),
        ply: row.ply,
        moves,
        turn,
        white_ms,
        black_ms,
        as_of_ms: now,
        result: row.result.clone(),
        end_reason: row.end_reason.clone(),
        legal_moves,
    })
}

/// Opens a game for someone to join. The colours are decided when they do.
pub async fn create_game(
    pool: &PgPool,
    creator: Uuid,
    base_ms: u64,
    increment_ms: u64,
    color_preference: &str,
) -> Result<Uuid> {
    let control = TimeControl::new(base_ms, increment_ms).map_err(|_| GameError::InvalidTimeControl("the base time must be between 1 second and 3 hours and the increment at most 3 minutes"))?;
    let id = Uuid::now_v7();
    let clock = Clock::start(control, LagPolicy::default(), 0).state();
    sqlx::query(
        "INSERT INTO games (id, created_by, color_preference, base_ms, increment_ms, fen, white_remaining_ms, black_remaining_ms, white_lag_quota_ms, black_lag_quota_ms)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
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
    .execute(pool)
    .await?;
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
             white_remaining_ms = $5, black_remaining_ms = $6, white_lag_quota_ms = $7, black_lag_quota_ms = $8
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
    sqlx::query("UPDATE games SET status = 'cancelled' WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
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
    // No lag measurement over plain HTTP yet, so no refund: the full time is charged.
    match eschess_game::play_move(&game, actor, uci, now as u64, 0)? {
        MoveOutcome::Flagged(ended) => {
            finish(&mut tx, id, ended).await?;
            tx.commit().await?;
            Err(GameError::Refused(Refusal::GameOver))
        }
        MoveOutcome::Played(applied) => {
            let ply = row.ply + 1;
            let state = applied.clock.state();
            let mover = game.color_of(actor).expect("checked by the move loop");
            sqlx::query("INSERT INTO game_moves (game_id, ply, uci, fen_after, mover_id, client_move_id, charged_ms, credited_ms, remaining_ms_after, played_at_ms) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)")
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
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "UPDATE games SET fen = $2, ply = $3, white_remaining_ms = $4, black_remaining_ms = $5, white_lag_quota_ms = $6, black_lag_quota_ms = $7, turn_started_at_ms = $8 WHERE id = $1",
            )
            .bind(id)
            .bind(&applied.fen_after)
            .bind(ply)
            .bind(state.remaining_ms[0] as i64)
            .bind(state.remaining_ms[1] as i64)
            .bind(state.lag_quota_ms[0] as i64)
            .bind(state.lag_quota_ms[1] as i64)
            .bind(state.turn_started_at_ms as i64)
            .execute(&mut *tx)
            .await?;
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
