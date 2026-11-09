//! Signup, login, token refresh, logout and session management.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use eschess_auth::password::{hash_password, spend_the_same_time, verify_password};
use eschess_auth::token::{hash_token, new_token};
use eschess_auth::validate::{check_password, normalize_email, normalize_username};
use eschess_notification::Message;
use eschess_storage::identity::{self, Rotation, TokenPurpose};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::error::ApiError;
use crate::extract::AuthUser;
use crate::AppState;

/// Password hashing is slow on purpose, so it runs off the async worker threads.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(ApiError::internal)
}

fn device_label(label: Option<String>) -> Result<Option<String>, ApiError> {
    match label
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
    {
        Some(l) if l.chars().count() > 64 => Err(ApiError::Validation(
            "device label must be at most 64 characters".into(),
        )),
        other => Ok(other),
    }
}

/// Makes a session and returns its token pair.
async fn start_session(
    state: &AppState,
    user_id: Uuid,
    label: Option<&str>,
) -> Result<Value, ApiError> {
    let (access, refresh) = (new_token(), new_token());
    identity::create_session(
        &state.pool,
        user_id,
        label,
        &hash_token(&access),
        &hash_token(&refresh),
        state.config.lifetimes(),
    )
    .await
    .map_err(ApiError::internal)?;
    Ok(token_json(&access, &refresh, state.config.access_secs))
}

fn token_json(access: &str, refresh: &str, expires_in: i64) -> Value {
    json!({ "access_token": access, "refresh_token": refresh, "token_type": "Bearer", "expires_in": expires_in })
}

#[derive(Deserialize)]
pub struct SignupBody {
    email: String,
    username: String,
    password: String,
    device_label: Option<String>,
}

/// Creates the player and their custodial wallet(s) together, then signs them in. An email or username that is
/// already taken is refused with a clear error, so the player can fix it.
pub async fn signup(
    State(state): State<AppState>,
    Json(b): Json<SignupBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let email = normalize_email(&b.email)?;
    let username = normalize_username(&b.username)?;
    check_password(&b.password, &email, &username)?;
    let label = device_label(b.device_label)?;

    let password = b.password;
    let hash = blocking(move || hash_password(&password)).await??;
    let user = identity::create_user_with_wallets(
        &state.pool,
        &email,
        &username,
        &hash,
        &state.config.wallet_assets,
    )
    .await?;

    // Their own wallet address (ADR 0006), made now because it costs nothing. If it cannot be made, signing up still
    // works and the wallet is made the first time it is needed.
    if let Some(vault) = &state.vault {
        if let Err(e) =
            eschess_storage::wallets::ensure_wallet(&state.pool, &**vault, user.id).await
        {
            tracing::warn!(user = %user.id, "could not create the player's wallet at signup: {e}");
        }
    }

    // Ask them to confirm their address. The first email is always allowed; this also starts the resend cooldown.
    identity::allow_email(
        &state.pool,
        &user.email,
        "verify_email",
        state.config.email_cooldown_secs,
    )
    .await
    .map_err(ApiError::internal)?;
    send_verification(&state, user.id, &user.email).await?;

    let mut out = start_session(&state, user.id, label.as_deref()).await?;
    out["user"] = json!({ "id": user.id, "email": user.email, "username": user.username, "email_verified": false });
    Ok((StatusCode::CREATED, Json(out)))
}

#[derive(Deserialize)]
pub struct LoginBody {
    email: String,
    password: String,
    device_label: Option<String>,
}

pub async fn login(
    State(state): State<AppState>,
    Json(b): Json<LoginBody>,
) -> Result<Json<Value>, ApiError> {
    let label = device_label(b.device_label)?;
    let password = b.password;
    // An unusable email is treated like an unknown one, and takes the same time.
    let Ok(email) = normalize_email(&b.email) else {
        blocking(move || spend_the_same_time(&password)).await?;
        return Err(ApiError::InvalidCredentials);
    };
    let Some(found) = identity::credentials_by_email(&state.pool, &email)
        .await
        .map_err(ApiError::internal)?
    else {
        blocking(move || spend_the_same_time(&password)).await?;
        return Err(ApiError::InvalidCredentials);
    };

    let stored = found.password_hash.clone();
    let correct = blocking(move || verify_password(&password, &stored)).await?;
    // A locked or suspended account is refused even with the right password, and says nothing more.
    if found.locked || !found.active {
        return Err(ApiError::InvalidCredentials);
    }
    if !correct {
        identity::record_failed_login(
            &state.pool,
            found.user_id,
            state.config.max_failed_logins,
            state.config.lockout_minutes,
        )
        .await
        .map_err(ApiError::internal)?;
        return Err(ApiError::InvalidCredentials);
    }
    identity::clear_failed_logins(&state.pool, found.user_id)
        .await
        .map_err(ApiError::internal)?;
    Ok(Json(
        start_session(&state, found.user_id, label.as_deref()).await?,
    ))
}

#[derive(Deserialize)]
pub struct RefreshBody {
    refresh_token: String,
}

/// Trades a refresh token for a new pair. Each refresh token works once; presenting a spent one ends the session.
pub async fn refresh(
    State(state): State<AppState>,
    Json(b): Json<RefreshBody>,
) -> Result<Json<Value>, ApiError> {
    let (access, refresh) = (new_token(), new_token());
    let outcome = identity::rotate(
        &state.pool,
        &hash_token(b.refresh_token.trim()),
        &hash_token(&access),
        &hash_token(&refresh),
        state.config.lifetimes(),
    )
    .await
    .map_err(ApiError::internal)?;
    match outcome {
        Rotation::Rotated(_) => Ok(Json(token_json(
            &access,
            &refresh,
            state.config.access_secs,
        ))),
        Rotation::Reused => {
            tracing::warn!("a spent refresh token was presented again; the session was revoked");
            Err(ApiError::Unauthorized)
        }
        Rotation::Invalid => Err(ApiError::Unauthorized),
    }
}

pub async fn logout(State(state): State<AppState>, user: AuthUser) -> Result<StatusCode, ApiError> {
    identity::revoke_session(&state.pool, user.user_id, user.session_id, "logout")
        .await
        .map_err(ApiError::internal)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_sessions(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Value>, ApiError> {
    let sessions = identity::active_sessions(&state.pool, user.user_id)
        .await
        .map_err(ApiError::internal)?;
    let items: Vec<Value> = sessions.iter().map(|s| json!({ "id": s.id, "device_label": s.device_label, "created_at": s.created_at, "last_seen_at": s.last_seen_at, "current": s.id == user.session_id })).collect();
    Ok(Json(json!({ "sessions": items })))
}

/// Signs out another of your own devices. Someone else's session id looks exactly like one that does not exist.
pub async fn revoke_session(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    if identity::revoke_session(&state.pool, user.user_id, id, "revoked_by_user")
        .await
        .map_err(ApiError::internal)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}

// ---------------------------------------------------------------- email confirmation and password reset

/// Sends a message in the background, so that how long a request takes does not depend on whether we sent one
/// (which would otherwise reveal, for example, whether an email has an account).
fn deliver(state: &AppState, message: Message) {
    let mailer = state.mailer.clone();
    tokio::spawn(async move {
        if let Err(e) = mailer.send(message).await {
            tracing::error!("could not send email: {e}");
        }
    });
}

/// Makes a fresh confirmation token for the player and emails it. Earlier unused ones stop working.
async fn send_verification(state: &AppState, user_id: Uuid, email: &str) -> Result<(), ApiError> {
    let token = new_token();
    identity::create_email_token(
        &state.pool,
        user_id,
        email,
        TokenPurpose::VerifyEmail,
        &hash_token(&token),
        state.config.verify_ttl_secs,
    )
    .await
    .map_err(ApiError::internal)?;
    deliver(
        state,
        Message::VerifyEmail {
            to: email.into(),
            token,
            ttl_minutes: state.config.verify_ttl_secs / 60,
        },
    );
    Ok(())
}

#[derive(Deserialize)]
pub struct TokenBody {
    token: String,
}

/// Confirms an email address. Whoever holds the emailed token can do this, signed in or not: holding it is the proof.
pub async fn verify_email(
    State(state): State<AppState>,
    Json(b): Json<TokenBody>,
) -> Result<Json<Value>, ApiError> {
    identity::verify_email(&state.pool, &hash_token(b.token.trim())).await?;
    Ok(Json(json!({ "email_verified": true })))
}

/// Sends the confirmation email again (for a signed-in player who has not confirmed yet), at most once a minute.
pub async fn resend_verification(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let player = identity::user_by_id(&state.pool, user.user_id)
        .await
        .map_err(ApiError::internal)?
        .ok_or(ApiError::Unauthorized)?;
    if player.email_verified {
        return Err(ApiError::AlreadyVerified);
    }
    if !identity::allow_email(
        &state.pool,
        &player.email,
        "verify_email",
        state.config.email_cooldown_secs,
    )
    .await
    .map_err(ApiError::internal)?
    {
        return Err(ApiError::RateLimited);
    }
    send_verification(&state, player.id, &player.email).await?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "status": "sent" }))))
}

#[derive(Deserialize)]
pub struct ForgotBody {
    email: String,
}

/// Asks for a password-reset email. The answer is the same whether or not the address has an account, so this
/// cannot be used to find out who is registered.
pub async fn forgot_password(
    State(state): State<AppState>,
    Json(b): Json<ForgotBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let email = normalize_email(&b.email)?;
    if let Some(found) = identity::credentials_by_email(&state.pool, &email)
        .await
        .map_err(ApiError::internal)?
    {
        if found.active
            && identity::allow_email(
                &state.pool,
                &email,
                "reset_password",
                state.config.email_cooldown_secs,
            )
            .await
            .map_err(ApiError::internal)?
        {
            let token = new_token();
            identity::create_email_token(
                &state.pool,
                found.user_id,
                &email,
                TokenPurpose::ResetPassword,
                &hash_token(&token),
                state.config.reset_ttl_secs,
            )
            .await
            .map_err(ApiError::internal)?;
            deliver(
                &state,
                Message::PasswordReset {
                    to: email,
                    token,
                    ttl_minutes: state.config.reset_ttl_secs / 60,
                },
            );
        }
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(
            json!({ "status": "check_your_email", "message": "If that email has an account, we have sent instructions to reset the password." }),
        ),
    ))
}

#[derive(Deserialize)]
pub struct ResetBody {
    token: String,
    new_password: String,
}

/// Chooses a new password with the emailed token. Every signed-in device is signed out, and the token works once.
pub async fn reset_password(
    State(state): State<AppState>,
    Json(b): Json<ResetBody>,
) -> Result<Json<Value>, ApiError> {
    check_password(&b.new_password, "", "")?; // a weak password is refused before the token is touched
    let password = b.new_password;
    let hash = blocking(move || hash_password(&password)).await??;
    identity::reset_password(&state.pool, &hash_token(b.token.trim()), &hash).await?;
    Ok(Json(
        json!({ "status": "password_changed", "message": "Sign in with your new password." }),
    ))
}
