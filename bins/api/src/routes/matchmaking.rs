//! Quick pairing: ask for a game and be matched with someone of similar strength.
//!
//! `POST /v1/matchmaking {time_control, rated?}` joins the queue (and pairs at once if someone suitable is waiting);
//! `GET /v1/matchmaking` says whether you are still waiting or a game is ready; `DELETE` leaves the queue. A
//! background task keeps pairing people as their acceptable rating gap widens with waiting time.

use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use eschess_storage::games;
use eschess_storage::matchmaking::{self, Status};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::PgPool;

use super::games::{game_json, TimeControlBody};
use crate::error::ApiError;
use crate::extract::AuthUser;
use crate::AppState;

#[derive(Deserialize)]
pub struct QueueBody {
    time_control: TimeControlBody,
    rated: Option<bool>,
}

async fn status_json(state: &AppState, user: &AuthUser, status: Status) -> Result<Value, ApiError> {
    Ok(match status {
        Status::Queued { waited_ms } => json!({ "status": "queued", "waited_ms": waited_ms }),
        Status::Matched { game_id } => {
            let game = games::get_game(&state.pool, game_id, user.user_id).await?;
            json!({ "status": "matched", "game": game_json(&game) })
        }
    })
}

pub async fn join(
    State(state): State<AppState>,
    user: AuthUser,
    Json(b): Json<QueueBody>,
) -> Result<Json<Value>, ApiError> {
    let base_ms = b
        .time_control
        .base_seconds()
        .checked_mul(1_000)
        .ok_or_else(|| ApiError::Validation("that time is too large".into()))?;
    let increment_ms = b
        .time_control
        .increment_seconds()
        .checked_mul(1_000)
        .ok_or_else(|| ApiError::Validation("that increment is too large".into()))?;
    let status = matchmaking::enqueue(
        &state.pool,
        user.user_id,
        base_ms,
        increment_ms,
        b.rated.unwrap_or(true),
        &mut rand::random::<bool>,
    )
    .await?;
    Ok(Json(status_json(&state, &user, status).await?))
}

pub async fn status(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Value>, ApiError> {
    match matchmaking::status(&state.pool, user.user_id).await? {
        Some(s) => Ok(Json(status_json(&state, &user, s).await?)),
        None => Err(ApiError::Coded(
            StatusCode::NOT_FOUND,
            "not_queued",
            "you are not waiting for a game".into(),
        )),
    }
}

pub async fn leave(State(state): State<AppState>, user: AuthUser) -> Result<StatusCode, ApiError> {
    if matchmaking::leave(&state.pool, user.user_id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::Coded(
            StatusCode::NOT_FOUND,
            "not_queued",
            "you are not waiting for a game".into(),
        ))
    }
}

/// Pairs waiting players every `every`, so the widening rating window turns into games without anyone asking.
pub fn spawn_matchmaker(pool: PgPool, every: Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(every).await;
            match matchmaking::sweep(&pool, &mut rand::random::<bool>).await {
                Ok(0) => {}
                Ok(n) => tracing::info!("matchmaking started {n} game(s)"),
                Err(e) => tracing::warn!("matchmaking sweep failed: {e}"),
            }
        }
    })
}
