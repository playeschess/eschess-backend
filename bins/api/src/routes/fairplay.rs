//! Fair-play review (ADR 0002 §5), for people with the `fairplay_reviewer` role.
//!
//! Reviewers see open cases oldest first, with each game's moves, clocks and lag and the scorers' summaries and
//! evidence, and clear or confirm a case with a written reason and the list of games they looked at (refused if the case
//! has gained a game since). A score is not a verdict: the decision is the
//! reviewer's, and in this baseline it is recorded and does nothing else.
//!
//! Everyone else, and a reviewer looking at a case about themselves or about a game they played in, is told "not
//! found": the routes do not announce themselves, and a player cannot learn they are under review.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use eschess_fairplay::{GameRecord, Transport};
use eschess_storage::fairplay::{self, CaseSummary, DecideError, Decision, StoredScore};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::error::ApiError;
use crate::extract::AuthUser;
use crate::AppState;

async fn reviewer(state: &AppState, user: &AuthUser) -> Result<(), ApiError> {
    if !fairplay::is_reviewer(&state.pool, user.user_id)
        .await
        .map_err(ApiError::internal)?
    {
        return Err(ApiError::NotFound);
    }
    user.require_verified()
}

fn case_json(c: &CaseSummary) -> Value {
    json!({
        "id": c.id, "player": { "id": c.player_id, "username": c.username }, "status": c.status,
        "opened_at_ms": c.opened_at_ms, "games": c.games, "top_score": c.top_score,
        "decided_by": c.decided_by, "decided_at_ms": c.decided_at_ms, "reason": c.reason,
    })
}

fn color(c: eschess_domain::chess::Color) -> &'static str {
    match c {
        eschess_domain::chess::Color::White => "white",
        eschess_domain::chess::Color::Black => "black",
    }
}

fn game_json(g: &GameRecord, scores: &[StoredScore]) -> Value {
    let moves: Vec<Value> = g
        .moves
        .iter()
        .map(|m| {
            json!({
                "ply": m.ply, "uci": m.uci, "mover": color(m.mover), "fen_after": m.fen_after,
                "think_ms": m.think_ms, "lag_measured_ms": m.lag_measured_ms, "lag_credited_ms": m.lag_credited_ms,
                "transport": match m.transport { Transport::WebSocket => "ws", Transport::Http => "http", Transport::Unknown => "unknown" },
                "remaining_ms_after": m.remaining_ms_after, "played_at_ms": m.played_at_ms,
            })
        })
        .collect();
    json!({
        "game_id": g.game_id, "white_id": g.white_id, "black_id": g.black_id,
        "result": g.result.as_str(), "end_reason": g.end_reason.map(|r| r.as_str()), "rated": g.rated,
        "time_control": { "base_ms": g.time_control.base_ms, "increment_ms": g.time_control.increment_ms },
        "moves": moves,
        "scores": scores.iter().map(|s| json!({
            "scorer": s.scorer, "score": s.score, "summary": s.summary, "evidence": s.evidence,
        })).collect::<Vec<_>>(),
    })
}

#[derive(Deserialize)]
pub struct ListQuery {
    status: Option<String>,
    limit: Option<i64>,
}

/// The queue: open cases oldest first (or decided ones, with `?status=cleared|confirmed`).
pub async fn list(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    reviewer(&state, &user).await?;
    let status = q.status.as_deref().unwrap_or("open");
    if !["open", "cleared", "confirmed"].contains(&status) {
        return Err(ApiError::Validation(format!(
            "status: open, cleared or confirmed, not {status:?}"
        )));
    }
    let cases = fairplay::cases_for_reviewer(
        &state.pool,
        user.user_id,
        status,
        q.limit.unwrap_or(50).clamp(1, 200),
    )
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(
        json!({ "cases": cases.iter().map(case_json).collect::<Vec<_>>() }),
    ))
}

/// One case, with everything needed to decide it.
pub async fn get(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    reviewer(&state, &user).await?;
    let detail = fairplay::case_for_reviewer(&state.pool, id, user.user_id)
        .await
        .map_err(ApiError::internal)?
        .ok_or(ApiError::NotFound)?;
    let mut out = case_json(&detail.case);
    out["game_records"] = detail
        .games
        .iter()
        .map(|g| game_json(&g.record, &g.scores))
        .collect();
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct DecideBody {
    reason: String,
    /// The ids of the games the reviewer looked at (`game_records[].game_id` from the case). If the case has gained a
    /// game since, the decision is refused so the reviewer can look again.
    games: Vec<Uuid>,
}

async fn decide(
    state: &AppState,
    user: &AuthUser,
    id: Uuid,
    decision: Decision,
    body: &DecideBody,
) -> Result<Json<Value>, ApiError> {
    reviewer(state, user).await?;
    let case = fairplay::decide(
        &state.pool,
        id,
        user.user_id,
        decision,
        &body.reason,
        &body.games,
    )
    .await
    .map_err(|e| match e {
        DecideError::NotFound => ApiError::NotFound,
        DecideError::AlreadyDecided(s) => ApiError::Coded(
            StatusCode::CONFLICT,
            "case_already_decided",
            format!("this case was already {s}; a decision is final"),
        ),
        DecideError::InvalidReason => ApiError::Validation(
            "reason: 1 to 2000 characters, saying what was looked at and why".into(),
        ),
        DecideError::CaseChanged => ApiError::Coded(
            StatusCode::CONFLICT,
            "case_changed",
            "the case's games are not the ones you looked at (one was added since); open it again"
                .into(),
        ),
        DecideError::Database(e) => ApiError::internal(e),
    })?;
    tracing::info!(case = %id, reviewer = %user.user_id, decision = decision.as_str(), "fair-play case decided");
    Ok(Json(case_json(&case)))
}

/// Nothing wrong was found.
pub async fn clear(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
    Json(b): Json<DecideBody>,
) -> Result<Json<Value>, ApiError> {
    decide(&state, &user, id, Decision::Clear, &b).await
}

/// Unfair play was found. Recorded; no automatic consequence in this baseline.
pub async fn confirm(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
    Json(b): Json<DecideBody>,
) -> Result<Json<Value>, ApiError> {
    decide(&state, &user, id, Decision::Confirm, &b).await
}
