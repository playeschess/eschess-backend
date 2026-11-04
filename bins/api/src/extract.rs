use axum::extract::FromRequestParts;
use axum::http::{header, request::Parts};
use eschess_auth::token::hash_token;
use uuid::Uuid;

use crate::error::ApiError;
use crate::AppState;

/// The signed-in player, from `Authorization: Bearer <access token>`. The server decides who this is from its
/// own records; nothing the client sends about identity is believed.
#[derive(Debug, Clone, Copy)]
pub struct AuthUser {
    pub user_id: Uuid,
    pub session_id: Uuid,
    pub email_verified: bool,
}

impl AuthUser {
    /// For anything involving money: the player must have confirmed their email address first.
    pub fn require_verified(&self) -> Result<(), ApiError> {
        if self.email_verified {
            Ok(())
        } else {
            Err(ApiError::EmailNotVerified)
        }
    }
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or(ApiError::Unauthorized)?;
        let found = eschess_storage::identity::authenticate(&state.pool, &hash_token(token))
            .await
            .map_err(ApiError::internal)?
            .ok_or(ApiError::Unauthorized)?;
        Ok(AuthUser {
            user_id: found.user_id,
            session_id: found.session_id,
            email_verified: found.email_verified,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unconfirmed_email_is_refused_where_verification_is_required() {
        let mut user = AuthUser {
            user_id: Uuid::nil(),
            session_id: Uuid::nil(),
            email_verified: false,
        };
        assert!(matches!(
            user.require_verified(),
            Err(ApiError::EmailNotVerified)
        ));
        user.email_verified = true;
        assert!(user.require_verified().is_ok());
    }
}
