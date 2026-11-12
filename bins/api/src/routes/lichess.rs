//! Linking the player's Lichess account (ADR 0005).
//!
//! The player logs in at Lichess (OAuth with PKCE) so we know the account is theirs. Three calls: `GET /v1/lichess`
//! says what is linked; `POST /v1/lichess/link/start` gives the address to send the player to; Lichess sends them back to
//! a page of the web app, which calls `POST /v1/lichess/link/finish` with the `code` and `state` it was given. We read the
//! account's id and username, end the Lichess token at once, and keep only the id and username.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use eschess_lichess::LichessError;
use eschess_storage::lichess::{self, LinkError};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::LichessConfig;
use crate::error::ApiError;
use crate::extract::AuthUser;
use crate::AppState;

fn enabled(state: &AppState) -> Result<(&LichessConfig, &dyn eschess_lichess::Lichess), ApiError> {
    match (&state.config.lichess, &state.lichess) {
        (Some(c), Some(l)) => Ok((c, &**l)),
        _ => Err(ApiError::Coded(
            StatusCode::NOT_FOUND,
            "lichess_unavailable",
            "linking a Lichess account is not available on this server".into(),
        )),
    }
}

fn linked_json(l: &lichess::Linked) -> Value {
    json!({ "lichess_id": l.lichess_id, "username": l.username, "url": format!("https://lichess.org/@/{}", l.lichess_id) })
}

/// What is linked, and whether linking is on.
pub async fn status(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Value>, ApiError> {
    let linked = lichess::linked(&state.pool, user.user_id)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({
        "available": state.config.lichess.is_some() && state.lichess.is_some(),
        "linked": linked.as_ref().map(linked_json),
    })))
}

/// Starts a login: where to send the player, and the `state` that comes back with them.
pub async fn start(State(state): State<AppState>, user: AuthUser) -> Result<Json<Value>, ApiError> {
    user.require_verified()?;
    let (cfg, _) = enabled(&state)?;
    let pkce = eschess_lichess::new_pkce();
    let st = eschess_lichess::new_state();
    lichess::begin(
        &state.pool,
        user.user_id,
        &st,
        &pkce.verifier,
        cfg.attempt_ttl_secs,
    )
    .await
    .map_err(link_error)?;
    let url = eschess_lichess::authorize_url(
        &cfg.base_url,
        &cfg.client_id,
        &cfg.redirect_uri,
        &pkce.challenge,
        &st,
    );
    Ok(Json(json!({ "authorize_url": url, "state": st })))
}

#[derive(Deserialize)]
pub struct Finish {
    pub code: String,
    pub state: String,
}

/// Finishes a login: trades the code for the account, links it, and ends the Lichess token.
pub async fn finish(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<Finish>,
) -> Result<Json<Value>, ApiError> {
    user.require_verified()?;
    let (cfg, lichess_api) = enabled(&state)?;
    // Each attempt can be finished once, and only by the player who started it.
    let Some(verifier) = lichess::take_attempt(&state.pool, user.user_id, &body.state)
        .await
        .map_err(ApiError::internal)?
    else {
        return Err(ApiError::Coded(
            StatusCode::BAD_REQUEST,
            "link_attempt_invalid",
            "this login is not valid any more; start again".into(),
        ));
    };
    let token = lichess_api
        .exchange_code(&body.code, &verifier, &cfg.redirect_uri, &cfg.client_id)
        .await
        .map_err(lichess_error)?;
    let account = lichess_api.account(&token).await;
    // Whatever the profile said, the token is not needed any more.
    if let Err(e) = lichess_api.revoke(&token).await {
        tracing::warn!("could not revoke a Lichess token: {e}");
    }
    let account = account.map_err(lichess_error)?;
    if account.disabled || account.tos_violation {
        return Err(ApiError::Coded(
            StatusCode::UNPROCESSABLE_ENTITY,
            "lichess_account_not_eligible",
            "this Lichess account is closed or marked by Lichess, so it cannot be used here".into(),
        ));
    }
    lichess::save(&state.pool, user.user_id, &account.id, &account.username)
        .await
        .map_err(link_error)?;
    let linked = lichess::linked(&state.pool, user.user_id)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(json!({ "linked": linked.as_ref().map(linked_json) })))
}

fn link_error(e: LinkError) -> ApiError {
    match e {
        LinkError::AlreadyLinked => ApiError::Coded(
            StatusCode::CONFLICT,
            "lichess_already_linked",
            "you already have a Lichess account linked; it cannot be changed here".into(),
        ),
        LinkError::AccountTaken => ApiError::Coded(
            StatusCode::CONFLICT,
            "lichess_account_taken",
            "this Lichess account is already used by another player".into(),
        ),
        LinkError::Database(e) => ApiError::internal(e),
    }
}

fn lichess_error(e: LichessError) -> ApiError {
    match e {
        LichessError::Rejected(_) => ApiError::Coded(
            StatusCode::BAD_REQUEST,
            "lichess_refused",
            "Lichess did not accept the login; start again".into(),
        ),
        LichessError::Unavailable(m) => {
            tracing::warn!("Lichess unavailable: {m}");
            ApiError::Coded(
                StatusCode::BAD_GATEWAY,
                "lichess_unavailable",
                "Lichess could not be reached; try again in a moment".into(),
            )
        }
    }
}
