//! Deposits into the player's wallet over Stellar.
//!
//! A player is given two equivalent ways to pay: an `M...` address (the platform wallet with the player's deposit id built
//! in), or the plain wallet address plus a memo id. Money sent either way reaches the same player.

use axum::extract::{Query, State};
use axum::Json;
use eschess_domain::money::{Amount, AssetId};
use eschess_storage::stellar;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::StellarConfig;
use crate::error::ApiError;
use crate::extract::AuthUser;
use crate::AppState;

fn enabled(state: &AppState) -> Result<&StellarConfig, ApiError> {
    state.config.stellar.as_ref().ok_or_else(|| {
        ApiError::Coded(
            axum::http::StatusCode::NOT_FOUND,
            "deposits_unavailable",
            "deposits are not available on this server".into(),
        )
    })
}

/// How to deposit USDC: the M-address, or the wallet address with a memo.
pub async fn deposit_info(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Value>, ApiError> {
    user.require_verified()?;
    let st = enabled(&state)?;
    let id = stellar::deposit_id(&state.pool, user.user_id)
        .await
        .map_err(ApiError::internal)?;
    // Make sure the wallet the deposit will be credited to exists (players who signed up earlier may not have one yet).
    stellar::user_wallet(&state.pool, user.user_id, &st.usdc)
        .await
        .map_err(ApiError::internal)?;
    let muxed = st.hot_account.muxed(id as u64).to_m();
    // The first deposit intent is also the moment to set up the player's own account on the network (ADR 0006). Their
    // own address is not offered as a place to send money yet: nothing watches it until deposits move over.
    let own_wallet = match &state.vault {
        None => None,
        Some(vault) => {
            let setup = async {
                eschess_storage::wallets::ensure_wallet(&state.pool, &**vault, user.user_id)
                    .await?;
                eschess_storage::wallets::request_setup(&state.pool, user.user_id).await?;
                eschess_storage::wallets::setup_state(&state.pool, user.user_id).await
            }
            .await;
            match setup {
                Ok(eschess_storage::wallets::SetupState::Ready) => Some("ready"),
                Ok(_) => Some("setting_up"),
                Err(e) => {
                    tracing::warn!(user = %user.user_id, "could not request the player's wallet setup: {e}");
                    None
                }
            }
        }
    };
    Ok(Json(json!({
        "own_wallet": own_wallet.map(|status| json!({ "status": status })),
        "network": st.network,
        "asset": { "code": st.usdc.code(), "issuer": st.usdc.issuer(), "id": st.usdc.to_string() },
        "muxed_address": muxed,
        "account": st.hot_account.to_g(),
        "memo": { "type": "id", "value": id.to_string() },
        "instructions": "Send USDC either to muxed_address with no memo, or to account with the memo above. Only this USDC (from this issuer) is credited; anything else is held for manual review.",
    })))
}

fn deposit_json(d: &stellar::DepositRow) -> Value {
    let amount = match (&d.asset, d.amount_minor) {
        (Some(asset), Some(minor)) => AssetId::parse(asset)
            .ok()
            .and_then(|a| Amount::new(minor, a).ok())
            .map(|a| a.to_decimal_string()),
        _ => None,
    };
    json!({
        "id": d.op_id, "asset": d.asset, "amount": amount, "amount_minor": d.amount_minor.map(|m| m.to_string()),
        "status": d.status, "chain_time": d.chain_time,
    })
}

#[derive(Deserialize)]
pub struct ListQuery {
    limit: Option<i64>,
}

/// The player's own deposits, newest first.
pub async fn deposits(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    let rows = stellar::deposits_of(
        &state.pool,
        user.user_id,
        q.limit.unwrap_or(50).clamp(1, 200),
    )
    .await
    .map_err(ApiError::internal)?;
    Ok(Json(
        json!({ "deposits": rows.iter().map(deposit_json).collect::<Vec<_>>() }),
    ))
}
