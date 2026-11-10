//! The state of the books-against-chain check, for the people who approve withdrawals (ADR 0006 §6): the latest comparison
//! for each asset, whether new outflows are halted, and a way to end a halt once a person has found out why.
//! Everyone else gets 404, like the other admin routes.

use axum::extract::State;
use axum::Json;
use eschess_storage::reconcile;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::ApiError;
use crate::extract::AuthUser;
use crate::routes::withdrawals::approver;
use crate::AppState;

fn check_json(c: &reconcile::Check) -> Value {
    json!({
        "asset": c.asset, "verdict": format!("{:?}", c.verdict).to_lowercase(),
        "ledger_minor": c.ledger_minor, "chain_minor": c.chain_minor, "hot_minor": c.hot_minor,
        "players_minor": c.players_minor, "explained_minor": c.explained_minor,
        "difference_minor": c.difference_minor, "accounts_read": c.accounts_read,
    })
}

/// The latest comparison for each asset and the active halt, if any.
pub async fn status(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Value>, ApiError> {
    approver(&state, &user).await?;
    let checks = reconcile::latest_checks(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    let halt = reconcile::active_halt(&state.pool)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({
        "halted": halt.is_some(),
        "halt": halt.map(|h| json!({ "id": h.id, "reason": h.reason, "started_at_ms": h.started_at_ms })),
        "checks": checks.iter().map(check_json).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
pub struct ClearBody {
    /// What was found and why it is safe to carry on.
    note: String,
}

/// Ends the halt. Needs a written note; the person and the note are kept.
pub async fn clear(
    State(state): State<AppState>,
    user: AuthUser,
    Json(b): Json<ClearBody>,
) -> Result<Json<Value>, ApiError> {
    approver(&state, &user).await?;
    let note = b.note.trim();
    if note.is_empty() || note.chars().count() > 2_000 {
        return Err(ApiError::Validation(
            "note: 1 to 2000 characters, saying what was found and why it is safe to carry on"
                .into(),
        ));
    }
    let cleared = reconcile::clear_halt(&state.pool, user.user_id, note)
        .await
        .map_err(ApiError::internal)?;
    tracing::warn!(by = %user.user_id, cleared, "money halt cleared by a person");
    Ok(Json(json!({ "cleared": cleared })))
}
