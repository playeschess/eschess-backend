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
        // A closed account (ADR 0003) may use its wallet, read its own record and sign out, and nothing else. Deny by
        // default: a new route is closed to it unless it is listed here.
        if found.closed && !closed_may_use(parts.uri.path()) {
            return Err(ApiError::Coded(
                axum::http::StatusCode::FORBIDDEN,
                "account_closed",
                "this account was closed; you can still see and withdraw your balance".into(),
            ));
        }
        Ok(AuthUser {
            user_id: found.user_id,
            session_id: found.session_id,
            email_verified: found.email_verified,
        })
    }
}

/// What a closed account can still reach.
fn closed_may_use(path: &str) -> bool {
    path == "/v1/me"
        || path.starts_with("/v1/me/")
        || path.starts_with("/v1/wallet/")
        || path.starts_with("/v1/auth/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closed_account_reaches_its_wallet_and_nothing_to_do_with_play() {
        for ok in [
            "/v1/me",
            "/v1/wallet/withdrawals",
            "/v1/wallet/deposit-info",
            "/v1/auth/logout",
        ] {
            assert!(closed_may_use(ok), "{ok}");
        }
        for no in [
            "/v1/games",
            "/v1/matchmaking",
            "/v1/ws",
            "/v1/leaderboard",
            "/v1/admin/fairplay/cases",
            "/v1/wallets",
            "/v1/mex",
        ] {
            assert!(!closed_may_use(no), "{no}");
        }
    }

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
