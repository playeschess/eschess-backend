//! Lichess login, to prove who owns a Lichess account (ADR 0005).
//!
//! Lichess supports the OAuth authorization-code flow with PKCE for public clients: no registration and no client
//! secret, any unique `client_id`, and `S256` as the only challenge method. We ask for **no scopes**, so the token can do
//! nothing but read the public profile, and we revoke it as soon as we have read who the player is. Nothing from Lichess
//! is kept except the account id and username.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::fmt;

/// A one-time secret for one login attempt, and the public value derived from it (RFC 7636).
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// The challenge Lichess is given for `verifier`: `BASE64URL(SHA256(verifier))`.
pub fn challenge_of(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// A fresh verifier (86 characters, within the 43 to 128 the RFC allows) and its challenge.
pub fn new_pkce() -> Pkce {
    let mut bytes = [0u8; 64];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = challenge_of(&verifier);
    Pkce {
        verifier,
        challenge,
    }
}

/// A fresh random `state`, which ties the way back from Lichess to the attempt that started it.
pub fn new_state() -> String {
    let mut bytes = [0u8; 24];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Where to send the player to log in. No scope is requested.
pub fn authorize_url(
    base: &str,
    client_id: &str,
    redirect_uri: &str,
    challenge: &str,
    state: &str,
) -> String {
    let params = [
        ("response_type", "code"),
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("code_challenge_method", "S256"),
        ("code_challenge", challenge),
        ("state", state),
    ];
    reqwest::Url::parse_with_params(&format!("{}/oauth", base.trim_end_matches('/')), params)
        .map(|u| u.to_string())
        .unwrap_or_default()
}

/// A token from Lichess. It is never printed.
pub struct AccessToken(String);

impl AccessToken {
    pub fn new(token: String) -> AccessToken {
        AccessToken(token)
    }
    /// The token text, for sending to Lichess. Take care to send it nowhere else.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AccessToken(..)")
    }
}

/// Who the token belongs to, as Lichess says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LichessAccount {
    /// The lowercase id, the stable name of the account.
    pub id: String,
    pub username: String,
    /// The account is closed.
    pub disabled: bool,
    /// Lichess has marked the account for breaking its terms of service (cheating, boosting and so on).
    pub tos_violation: bool,
}

#[derive(Debug)]
pub enum LichessError {
    /// Lichess refused: a wrong, used or expired code, or a token it does not know.
    Rejected(String),
    /// Lichess could not be reached or answered something unusable. Worth retrying later.
    Unavailable(String),
}

impl fmt::Display for LichessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LichessError::Rejected(m) => write!(f, "Lichess refused the request: {m}"),
            LichessError::Unavailable(m) => write!(f, "Lichess is not reachable: {m}"),
        }
    }
}
impl std::error::Error for LichessError {}

/// What we need from Lichess. A trait so tests (and the API) never depend on the real site.
#[async_trait::async_trait]
pub trait Lichess: Send + Sync {
    /// Trades the code Lichess sent to the player's browser for an access token.
    async fn exchange_code(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
        client_id: &str,
    ) -> Result<AccessToken, LichessError>;
    /// The account the token belongs to.
    async fn account(&self, token: &AccessToken) -> Result<LichessAccount, LichessError>;
    /// Ends the token. Called as soon as the account is known, so no usable token is left anywhere.
    async fn revoke(&self, token: &AccessToken) -> Result<(), LichessError>;
}

/// The real lichess.org (or a stand-in server in tests, by `base`).
pub struct HttpLichess {
    base: String,
    http: reqwest::Client,
}

impl HttpLichess {
    pub fn new(base: &str) -> HttpLichess {
        HttpLichess {
            base: base.trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("an HTTP client"),
        }
    }
}

fn unavailable(e: reqwest::Error) -> LichessError {
    // The error text can contain the URL but never the token (it travels in a header or the body).
    LichessError::Unavailable(e.without_url().to_string())
}

#[async_trait::async_trait]
impl Lichess for HttpLichess {
    async fn exchange_code(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
        client_id: &str,
    ) -> Result<AccessToken, LichessError> {
        let res = self
            .http
            .post(format!("{}/api/token", self.base))
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("code_verifier", verifier),
                ("redirect_uri", redirect_uri),
                ("client_id", client_id),
            ])
            .send()
            .await
            .map_err(unavailable)?;
        let status = res.status();
        let body: serde_json::Value = res.json().await.map_err(unavailable)?;
        if status.is_client_error() {
            let why = body
                .get("error_description")
                .or_else(|| body.get("error"))
                .and_then(|v| v.as_str())
                .unwrap_or("the code was not accepted");
            return Err(LichessError::Rejected(why.to_string()));
        }
        if !status.is_success() {
            return Err(LichessError::Unavailable(format!("status {status}")));
        }
        match (
            body.get("token_type").and_then(|v| v.as_str()),
            body.get("access_token").and_then(|v| v.as_str()),
        ) {
            (Some(t), Some(token)) if t.eq_ignore_ascii_case("bearer") && !token.is_empty() => {
                Ok(AccessToken::new(token.to_string()))
            }
            _ => Err(LichessError::Unavailable(
                "no access token in the answer".into(),
            )),
        }
    }

    async fn account(&self, token: &AccessToken) -> Result<LichessAccount, LichessError> {
        let res = self
            .http
            .get(format!("{}/api/account", self.base))
            .bearer_auth(token.expose())
            .send()
            .await
            .map_err(unavailable)?;
        let status = res.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(LichessError::Rejected("the token was not accepted".into()));
        }
        if !status.is_success() {
            return Err(LichessError::Unavailable(format!("status {status}")));
        }
        let body: serde_json::Value = res.json().await.map_err(unavailable)?;
        let text = |k: &str| body.get(k).and_then(|v| v.as_str()).map(str::to_string);
        let flag = |k: &str| body.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
        match (text("id"), text("username")) {
            (Some(id), Some(username)) if !id.is_empty() => Ok(LichessAccount {
                id: id.to_ascii_lowercase(),
                username,
                disabled: flag("disabled"),
                tos_violation: flag("tosViolation"),
            }),
            _ => Err(LichessError::Unavailable(
                "the profile had no id or username".into(),
            )),
        }
    }

    async fn revoke(&self, token: &AccessToken) -> Result<(), LichessError> {
        let res = self
            .http
            .delete(format!("{}/api/token", self.base))
            .bearer_auth(token.expose())
            .send()
            .await
            .map_err(unavailable)?;
        if res.status().is_success() {
            Ok(())
        } else {
            Err(LichessError::Unavailable(format!(
                "status {}",
                res.status()
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_challenge_is_the_one_in_the_pkce_rfc() {
        // RFC 7636, appendix B.
        assert_eq!(
            challenge_of("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn verifiers_are_long_unique_and_url_safe() {
        let (a, b) = (new_pkce(), new_pkce());
        assert_ne!(a.verifier, b.verifier);
        assert!((43..=128).contains(&a.verifier.len()));
        assert!(a
            .verifier
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_eq!(a.challenge, challenge_of(&a.verifier));
        assert_ne!(new_state(), new_state());
    }

    #[test]
    fn the_login_url_asks_for_no_scope_and_escapes_the_redirect() {
        let u = authorize_url(
            "https://lichess.org/",
            "eschess",
            "https://app.example/lichess?x=1&y=2",
            "CHAL",
            "STATE",
        );
        assert!(u.starts_with("https://lichess.org/oauth?"));
        assert!(u.contains("response_type=code"));
        assert!(u.contains("code_challenge_method=S256"));
        assert!(u.contains("code_challenge=CHAL"));
        assert!(u.contains("state=STATE"));
        assert!(u.contains("redirect_uri=https%3A%2F%2Fapp.example%2Flichess%3Fx%3D1%26y%3D2"));
        assert!(!u.contains("scope"));
    }

    #[test]
    fn a_token_is_never_printed() {
        let t = AccessToken::new("lio_secret".into());
        assert!(!format!("{t:?}").contains("lio_secret"));
    }
}
