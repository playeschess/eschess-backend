//! Playing chess: open a game, join one, make moves, resign, claim a draw.
//!
//! Who is calling comes from their access token, never from the request. The server alone decides what is legal, whose
//! turn it is and how much time is left; a request says only which move the player wants.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use eschess_domain::clock::{Category, TimeControl};
use eschess_storage::games::{self, GameView, MoveReceipt, Player, Scope};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::error::ApiError;
use crate::extract::AuthUser;
use crate::AppState;

fn player_json(p: &Option<Player>) -> Value {
    p.as_ref().map_or(
        Value::Null,
        |p| json!({ "id": p.id, "username": p.username }),
    )
}

fn category_name(c: Category) -> &'static str {
    match c {
        Category::Bullet => "bullet",
        Category::Blitz => "blitz",
        Category::Rapid => "rapid",
        Category::Classical => "classical",
    }
}

pub(crate) fn game_json(g: &GameView) -> Value {
    let category = TimeControl::new(g.base_ms as u64, g.increment_ms as u64)
        .map(|t| category_name(t.category()));
    json!({
        "id": g.id, "status": g.status, "created_by": g.created_by,
        "white": player_json(&g.white), "black": player_json(&g.black),
        "time_control": { "base_ms": g.base_ms, "increment_ms": g.increment_ms, "category": category.ok() },
        "rated": g.rated, "fen": g.fen, "ply": g.ply, "moves": g.moves, "turn": g.turn,
        "clock": { "white_ms": g.white_ms, "black_ms": g.black_ms, "as_of_ms": g.as_of_ms },
        "result": g.result, "end_reason": g.end_reason, "legal_moves": g.legal_moves, "event_seq": g.event_seq,
    })
}

fn move_json(m: &MoveReceipt) -> Value {
    json!({ "ply": m.ply, "uci": m.uci, "replayed": m.replayed, "charged_ms": m.charged_ms, "credited_ms": m.credited_ms })
}

#[derive(Deserialize)]
pub struct TimeControlBody {
    base_seconds: u64,
    #[serde(default)]
    increment_seconds: u64,
}

impl TimeControlBody {
    pub(crate) fn base_seconds(&self) -> u64 {
        self.base_seconds
    }
    pub(crate) fn increment_seconds(&self) -> u64 {
        self.increment_seconds
    }
}

#[derive(Deserialize)]
pub struct CreateBody {
    time_control: TimeControlBody,
    /// `white`, `black` or `random` (the default).
    color: Option<String>,
    /// Whether the game counts for ratings (the default) or is casual.
    rated: Option<bool>,
}

/// Opens a game for another player to join.
pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    Json(b): Json<CreateBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let color = b.color.as_deref().unwrap_or("random");
    if !["white", "black", "random"].contains(&color) {
        return Err(ApiError::Validation(
            "color must be white, black or random".into(),
        ));
    }
    let base_ms = b
        .time_control
        .base_seconds
        .checked_mul(1_000)
        .ok_or_else(|| ApiError::Validation("that time is too large".into()))?;
    let increment_ms = b
        .time_control
        .increment_seconds
        .checked_mul(1_000)
        .ok_or_else(|| ApiError::Validation("that increment is too large".into()))?;
    let id = games::create_game(
        &state.pool,
        user.user_id,
        base_ms,
        increment_ms,
        color,
        b.rated.unwrap_or(true),
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(game_json(
            &games::get_game(&state.pool, id, user.user_id).await?,
        )),
    ))
}

#[derive(Deserialize)]
pub struct ListQuery {
    /// `open` (default): games waiting for an opponent. `mine`: your games waiting or in progress. `finished`: your past games.
    scope: Option<String>,
    limit: Option<i64>,
}

pub async fn list(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    let scope = match q.scope.as_deref().unwrap_or("open") {
        "open" => Scope::Open,
        "mine" => Scope::MyActive,
        "finished" => Scope::MyFinished,
        _ => {
            return Err(ApiError::Validation(
                "scope must be open, mine or finished".into(),
            ))
        }
    };
    let found = games::list_games(&state.pool, user.user_id, scope, q.limit.unwrap_or(50)).await?;
    Ok(Json(
        json!({ "games": found.iter().map(game_json).collect::<Vec<_>>() }),
    ))
}

pub async fn get(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(game_json(
        &games::get_game(&state.pool, id, user.user_id).await?,
    )))
}

/// Joins an open game. If the creator asked for a random colour, the server flips the coin.
pub async fn join(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let creator_plays_white = rand::random::<bool>();
    Ok(Json(game_json(
        &games::join_game(&state.pool, id, user.user_id, creator_plays_white).await?,
    )))
}

pub async fn cancel(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    games::cancel_game(&state.pool, id, user.user_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct MoveBody {
    /// The move in UCI notation: `e2e4`, or `e7e8q` for a promotion.
    uci: String,
    /// Chosen by the player's app for each move and kept until the move is acknowledged. Sending it again (a retry
    /// after a dropped connection) returns the original outcome instead of playing the move twice.
    client_move_id: Uuid,
}

pub async fn play_move(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
    Json(b): Json<MoveBody>,
) -> Result<Json<Value>, ApiError> {
    let (game, receipt) = games::play_move(
        &state.pool,
        id,
        user.user_id,
        b.uci.trim(),
        b.client_move_id,
        0, // plain HTTP offers no way to measure lag, so no refund; the WebSocket route measures it
    )
    .await?;
    Ok(Json(
        json!({ "game": game_json(&game), "move": move_json(&receipt) }),
    ))
}

pub async fn resign(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(game_json(
        &games::resign(&state.pool, id, user.user_id).await?,
    )))
}

pub async fn claim_draw(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(game_json(
        &games::claim_draw(&state.pool, id, user.user_id).await?,
    )))
}
