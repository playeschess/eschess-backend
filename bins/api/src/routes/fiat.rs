//! Adding money from, and cashing out to, the player's own currency through a Stellar anchor (SEP-24).
//!
//! The anchor does the ID checks and takes or pays the fiat on its own screens. We open those screens for the player (logging
//! in to the anchor as them, with their own key) and hand back the address of the screen. After that:
//!   * a deposit is the anchor sending USDC to the player's own address, credited by the deposit watcher like any payment;
//!   * a cash-out is a withdrawal from the player's own account to the anchor, made by the worker (`bins/worker/src/fiat.rs`)
//!     with every withdrawal safeguard: limits, approval, the fair-play hold.
//!
//! The address of a screen carries a login token. It is returned once, to the player, and never stored or logged.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use eschess_stellar::sep::{Anchor, Kind as AnchorKind, SepError};
use eschess_storage::fiat::{self, FiatError, Kind, Session};
use eschess_storage::wallets::{self, SetupState};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::error::ApiError;
use crate::extract::AuthUser;
use crate::AppState;

fn unavailable() -> ApiError {
    ApiError::Coded(
        StatusCode::NOT_FOUND,
        "fiat_unavailable",
        "adding or cashing out in your own currency is not available on this server".into(),
    )
}

struct On<'a> {
    anchor: &'a dyn Anchor,
    vault: std::sync::Arc<dyn eschess_stellar::custody::KeyVault>,
    asset_code: String,
}

fn on(state: &AppState) -> Result<On<'_>, ApiError> {
    match (
        &state.config.fiat,
        &state.anchor,
        &state.vault,
        &state.config.stellar,
    ) {
        (Some(_), Some(anchor), Some(vault), Some(st)) => Ok(On {
            anchor: &**anchor,
            vault: vault.clone(),
            asset_code: st.usdc.code().to_string(),
        }),
        _ => Err(unavailable()),
    }
}

fn sep_error(e: SepError) -> ApiError {
    match e {
        SepError::Unavailable(m) => {
            tracing::warn!("anchor unavailable: {m}");
            ApiError::Coded(
                StatusCode::SERVICE_UNAVAILABLE,
                "anchor_unavailable",
                "our payment partner could not be reached; try again in a moment".into(),
            )
        }
        other => {
            // A refusal, a challenge we would not sign, or a bad stellar.toml: ours to look at, not the player's to fix.
            tracing::error!("anchor problem: {other}");
            ApiError::Coded(
                StatusCode::BAD_GATEWAY,
                "anchor_error",
                "our payment partner could not start that; try again later".into(),
            )
        }
    }
}

fn fiat_error(e: FiatError) -> ApiError {
    match e {
        FiatError::TooManyOpen => ApiError::Coded(
            StatusCode::CONFLICT,
            "too_many_open_sessions",
            "you already have unfinished payments with our partner; finish or wait for them first"
                .into(),
        ),
        FiatError::NotFound => ApiError::NotFound,
        FiatError::Database(e) => ApiError::internal(e),
    }
}

fn session_json(s: &Session) -> Value {
    json!({
        "id": s.id,
        "kind": s.kind.as_str(),
        "status": s.status.as_str(),
        "anchor_status": s.anchor_status,
        "amount": s.amount_in_minor.map(|m| {
            eschess_domain::money::Amount::new(m, eschess_domain::money::AssetId::native())
                .map(|a| a.to_decimal_string())
                .unwrap_or_default()
        }),
        "message": s.message,
        "more_info_url": s.more_info_url,
        "withdrawal_id": s.withdrawal_id,
        "created_at_ms": s.created_at_ms,
    })
}

/// Whether this is on, and the player's recent sessions.
pub async fn overview(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Value>, ApiError> {
    let available = on(&state).is_ok();
    let sessions = fiat::of_user(&state.pool, user.user_id, 20)
        .await
        .map_err(fiat_error)?;
    Ok(Json(json!({
        "available": available,
        "partner": state.config.fiat.as_ref().map(|f| f.domain.clone()),
        "sessions": sessions.iter().map(session_json).collect::<Vec<_>>(),
    })))
}

async fn start(state: &AppState, user: &AuthUser, kind: Kind) -> Result<Json<Value>, ApiError> {
    user.require_verified()?;
    let on = on(state)?;
    // The player's own account must be ready on the network: a deposit lands there and a cash-out leaves from there.
    wallets::ensure_wallet(&state.pool, &*on.vault, user.user_id)
        .await
        .map_err(ApiError::internal)?;
    wallets::request_setup(&state.pool, user.user_id)
        .await
        .map_err(ApiError::internal)?;
    if wallets::setup_state(&state.pool, user.user_id)
        .await
        .map_err(ApiError::internal)?
        != SetupState::Ready
    {
        return Err(ApiError::Coded(
            StatusCode::CONFLICT,
            "wallet_not_ready",
            "your wallet is still being set up; try again in a few seconds".into(),
        ));
    }
    fiat::check_can_open(&state.pool, user.user_id)
        .await
        .map_err(fiat_error)?;
    let signer = wallets::signer_for(&state.pool, on.vault.clone(), user.user_id)
        .await
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::internal("the player has no wallet key"))?;
    let address = eschess_stellar::signer::Signer::public_key(&signer);

    let token = on.anchor.authenticate(&signer).await.map_err(sep_error)?;
    let anchor_kind = match kind {
        Kind::Deposit => AnchorKind::Deposit,
        Kind::Withdraw => AnchorKind::Withdraw,
    };
    let screen = on
        .anchor
        .start(&token, anchor_kind, &on.asset_code, &address)
        .await
        .map_err(sep_error)?;
    let session = fiat::create(
        &state.pool,
        user.user_id,
        kind,
        on.anchor.home_domain(),
        &screen.id,
    )
    .await
    .map_err(fiat_error)?;
    // A deposit's USDC arrives at the player's own address: watch it closely for a while.
    if kind == Kind::Deposit {
        let _ = wallets::touch_watch(&state.pool, user.user_id).await;
    }
    let mut v = session_json(&session);
    v["url"] = json!(screen.url);
    Ok(Json(v))
}

/// Opens the anchor's screens to add money from the player's own currency.
pub async fn start_deposit(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Value>, ApiError> {
    start(&state, &user, Kind::Deposit).await
}

/// Opens the anchor's screens to cash out to the player's own currency.
pub async fn start_withdraw(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Value>, ApiError> {
    start(&state, &user, Kind::Withdraw).await
}

/// One of the player's sessions. Someone else's is "not found".
pub async fn get(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let s = fiat::get(&state.pool, user.user_id, id)
        .await
        .map_err(fiat_error)?;
    Ok(Json(session_json(&s)))
}
