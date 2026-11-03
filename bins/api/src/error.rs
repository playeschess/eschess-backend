use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// Every error leaves as `{"error": {"code": ..., "message": ...}}`. The codes are stable: clients switch on them.
#[derive(Debug)]
pub enum ApiError {
    Validation(String),
    InvalidToken,
    AlreadyVerified,
    EmailNotVerified,
    RateLimited,
    EmailTaken,
    UsernameTaken,
    InvalidCredentials,
    Unauthorized,
    NotFound,
    Internal(String),
}

impl ApiError {
    pub fn internal(e: impl std::fmt::Display) -> Self {
        ApiError::Internal(e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            ApiError::Validation(m) => (StatusCode::BAD_REQUEST, "validation_failed", m),
            ApiError::InvalidToken => (
                StatusCode::BAD_REQUEST,
                "invalid_token",
                "that link is invalid or has expired".into(),
            ),
            ApiError::AlreadyVerified => (
                StatusCode::CONFLICT,
                "already_verified",
                "your email is already confirmed".into(),
            ),
            ApiError::EmailNotVerified => (
                StatusCode::FORBIDDEN,
                "email_not_verified",
                "confirm your email address first".into(),
            ),
            ApiError::RateLimited => (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "please wait a minute before asking again".into(),
            ),
            ApiError::EmailTaken => (
                StatusCode::CONFLICT,
                "email_taken",
                "that email is already registered".into(),
            ),
            ApiError::UsernameTaken => (
                StatusCode::CONFLICT,
                "username_taken",
                "that username is taken".into(),
            ),
            ApiError::InvalidCredentials => (
                StatusCode::UNAUTHORIZED,
                "invalid_credentials",
                "invalid email or password".into(),
            ),
            ApiError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "sign in again".into(),
            ),
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not_found", "not found".into()),
            ApiError::Internal(detail) => {
                // The detail goes to the log, never to the player.
                tracing::error!("internal error: {detail}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "something went wrong".into(),
                )
            }
        };
        (
            status,
            Json(json!({ "error": { "code": code, "message": message } })),
        )
            .into_response()
    }
}

impl From<eschess_auth::AuthError> for ApiError {
    fn from(e: eschess_auth::AuthError) -> Self {
        match e {
            eschess_auth::AuthError::Invalid(why) => ApiError::Validation(why.to_string()),
            eschess_auth::AuthError::Internal => ApiError::internal("auth"),
        }
    }
}

impl From<eschess_storage::identity::IdentityError> for ApiError {
    fn from(e: eschess_storage::identity::IdentityError) -> Self {
        use eschess_storage::identity::IdentityError::*;
        match e {
            InvalidToken => ApiError::InvalidToken,
            EmailTaken => ApiError::EmailTaken,
            UsernameTaken => ApiError::UsernameTaken,
            other => ApiError::internal(other),
        }
    }
}
