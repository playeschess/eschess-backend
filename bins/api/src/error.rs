use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// Every error leaves as `{"error": {"code": ..., "message": ...}}`. The codes are stable: clients switch on them.
#[derive(Debug)]
pub enum ApiError {
    Validation(String),
    /// An error with its own status and stable code, for the many specific conflicts games can have.
    Coded(StatusCode, &'static str, String),
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

impl ApiError {
    /// The HTTP status, stable error code and message for this error. REST replies with all three; the WebSocket
    /// route reports the code and message.
    pub fn parts(self) -> (StatusCode, &'static str, String) {
        match self {
            ApiError::Validation(m) => (StatusCode::BAD_REQUEST, "validation_failed", m),
            ApiError::Coded(status, code, message) => (status, code, message),
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
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = self.parts();
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

impl From<eschess_storage::games::GameError> for ApiError {
    fn from(e: eschess_storage::games::GameError) -> Self {
        use eschess_game::Refusal;
        use eschess_storage::games::GameError::*;
        let conflict = |code: &'static str, msg: &str| {
            ApiError::Coded(StatusCode::CONFLICT, code, msg.to_string())
        };
        match e {
            NotFound => ApiError::NotFound,
            NotOpen => conflict("game_not_open", "that game is not open for joining"),
            OwnGame => conflict("own_game", "you cannot join your own game"),
            TooManyOpenGames => conflict(
                "too_many_open_games",
                "you already have the most open games allowed; cancel one or wait for an opponent",
            ),
            NotStarted => conflict(
                "game_not_started",
                "the game has not started: it is waiting for an opponent",
            ),
            MoveIdReused => conflict(
                "client_move_id_reused",
                "that client_move_id was already used for a different move",
            ),
            InvalidTimeControl(why) => ApiError::Validation(why.to_string()),
            Refused(Refusal::NotAPlayer) => ApiError::NotFound,
            Refused(Refusal::NotYourTurn) => conflict("not_your_turn", "it is not your turn"),
            Refused(Refusal::GameOver) => conflict("game_over", "the game is over"),
            Refused(Refusal::NothingToClaim) => conflict(
                "nothing_to_claim",
                "there is no draw to claim in this position",
            ),
            Refused(Refusal::InvalidNotation) => ApiError::Coded(
                StatusCode::BAD_REQUEST,
                "invalid_move_notation",
                "that is not a move in UCI notation, such as e2e4 or e7e8q".into(),
            ),
            Refused(Refusal::Illegal) => ApiError::Coded(
                StatusCode::UNPROCESSABLE_ENTITY,
                "illegal_move",
                "that move is not legal in this position".into(),
            ),
            Refused(other) => ApiError::internal(other),
            Database(e) => ApiError::internal(e),
        }
    }
}
