use axum::extract::State;
use axum::Json;
use eschess_storage::{identity, ratings};
use serde_json::{json, Value};

use crate::error::ApiError;
use crate::extract::AuthUser;
use crate::AppState;

/// The signed-in player and their wallet. Balances come from the ledger. Amounts are whole minor units sent as
/// strings, so no client can lose precision, with the asset named by code and issuer.
pub async fn me(State(state): State<AppState>, user: AuthUser) -> Result<Json<Value>, ApiError> {
    let player = identity::user_by_id(&state.pool, user.user_id)
        .await
        .map_err(ApiError::internal)?
        .ok_or(ApiError::Unauthorized)?;
    let wallet = identity::wallet(&state.pool, user.user_id)
        .await
        .map_err(ApiError::internal)?;
    let mine = ratings::for_player(&state.pool, user.user_id)
        .await
        .map_err(ApiError::internal)?;
    let ratings: Vec<Value> = mine.iter().map(super::ratings::rating_json).collect();
    let balances: Vec<Value> = wallet.iter().map(|w| json!({ "account_id": w.account.0, "asset_id": w.balance.asset.to_string(), "amount_minor": w.balance.minor.to_string() })).collect();
    Ok(Json(
        json!({ "id": player.id, "email": player.email, "username": player.username, "email_verified": player.email_verified, "wallet": balances, "ratings": ratings }),
    ))
}
