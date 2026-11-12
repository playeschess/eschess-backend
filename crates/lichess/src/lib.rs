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

/// A game we ask Lichess to host for exactly two players.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewChallenge {
    /// Lichess usernames. Only these two can join; the first plays White.
    pub white: String,
    pub black: String,
    pub clock_limit_secs: u32,
    pub clock_increment_secs: u32,
    pub rated: bool,
    /// The challenge disappears by itself at this time (Unix milliseconds), so a game nobody starts leaves nothing behind.
    pub expires_at_ms: i64,
}

/// The challenge Lichess made. Its id becomes the id of the game when both players have joined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenChallenge {
    pub id: String,
    pub url: String,
    pub url_white: String,
    pub url_black: String,
}

/// How a Lichess game stands (`GameStatusName` in Lichess's API description).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GameStatus {
    Created,
    Started,
    Aborted,
    NoStart,
    Mate,
    Resign,
    Stalemate,
    Timeout,
    Draw,
    OutOfTime,
    /// Lichess ended the game because a player was found to be cheating.
    Cheat,
    UnknownFinish,
    InsufficientMaterialClaim,
    VariantEnd,
    /// A status this code does not know. Never treated as a result.
    Other(String),
}

impl GameStatus {
    fn parse(s: &str) -> GameStatus {
        match s {
            "created" => GameStatus::Created,
            "started" => GameStatus::Started,
            "aborted" => GameStatus::Aborted,
            "noStart" => GameStatus::NoStart,
            "mate" => GameStatus::Mate,
            "resign" => GameStatus::Resign,
            "stalemate" => GameStatus::Stalemate,
            "timeout" => GameStatus::Timeout,
            "draw" => GameStatus::Draw,
            "outoftime" => GameStatus::OutOfTime,
            "cheat" => GameStatus::Cheat,
            "unknownFinish" => GameStatus::UnknownFinish,
            "insufficientMaterialClaim" => GameStatus::InsufficientMaterialClaim,
            "variantEnd" => GameStatus::VariantEnd,
            other => GameStatus::Other(other.to_string()),
        }
    }
    /// The game is over, whatever the result.
    pub fn is_over(&self) -> bool {
        !matches!(
            self,
            GameStatus::Created | GameStatus::Started | GameStatus::Other(_)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    White,
    Black,
}

/// A game as Lichess exports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LichessGame {
    pub id: String,
    pub status: GameStatus,
    /// Absent for a draw, an abort, and a game still on.
    pub winner: Option<Side>,
    /// Half-moves played.
    pub ply: i32,
    /// The moves in standard algebraic notation (`e4`, `Nbd7`, `O-O`), in order.
    pub moves: Vec<String>,
    /// What each mover's clock showed after each move, in centiseconds (`clocks[i]` is after `moves[i]`). Empty when Lichess
    /// gave none.
    pub clocks_cs: Vec<i64>,
    /// Lowercase Lichess ids of the two players; `None` for an anonymous player or the computer.
    pub white: Option<String>,
    pub black: Option<String>,
    pub variant: String,
    pub rated: bool,
}

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
    /// Creates a game for exactly two named players. Needs no login of ours and no token.
    async fn create_open_challenge(
        &self,
        challenge: &NewChallenge,
    ) -> Result<OpenChallenge, LichessError>;
    /// The game with this id, or `None` if Lichess has no such game (yet: a challenge nobody has joined is not a game).
    async fn game(&self, id: &str) -> Result<Option<LichessGame>, LichessError>;
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

    async fn create_open_challenge(&self, c: &NewChallenge) -> Result<OpenChallenge, LichessError> {
        let users = format!("{},{}", c.white, c.black);
        let res = self
            .http
            .post(format!("{}/api/challenge/open", self.base))
            .form(&[
                ("variant", "standard".to_string()),
                ("rated", c.rated.to_string()),
                ("clock.limit", c.clock_limit_secs.to_string()),
                ("clock.increment", c.clock_increment_secs.to_string()),
                ("users", users),
                ("expiresAt", c.expires_at_ms.to_string()),
                ("name", "eschess staked game".to_string()),
                ("rules", "noRematch".to_string()),
            ])
            .send()
            .await
            .map_err(unavailable)?;
        let status = res.status();
        let body: serde_json::Value = res.json().await.map_err(unavailable)?;
        if status.is_client_error() {
            let why = body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("the challenge was not accepted");
            return Err(LichessError::Rejected(why.to_string()));
        }
        if !status.is_success() {
            return Err(LichessError::Unavailable(format!("status {status}")));
        }
        // The answer is the challenge itself, or wraps it as `challenge`.
        let ch = body.get("challenge").unwrap_or(&body);
        let text = |k: &str| ch.get(k).and_then(|x| x.as_str()).map(str::to_string);
        match (text("id"), text("url")) {
            (Some(id), Some(url)) => Ok(OpenChallenge {
                url_white: text("urlWhite").unwrap_or_else(|| url.clone()),
                url_black: text("urlBlack").unwrap_or_else(|| url.clone()),
                id,
                url,
            }),
            _ => Err(LichessError::Unavailable(
                "the challenge had no id or url".into(),
            )),
        }
    }

    async fn game(&self, id: &str) -> Result<Option<LichessGame>, LichessError> {
        if id.len() != 8 || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(LichessError::Rejected("not a Lichess game id".into()));
        }
        let res = self
            .http
            .get(format!("{}/game/export/{id}", self.base))
            .query(&[
                ("moves", "true"),
                ("tags", "false"),
                ("clocks", "true"),
                ("evals", "false"),
            ])
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(unavailable)?;
        let status = res.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(LichessError::Unavailable(format!("status {status}")));
        }
        let g: serde_json::Value = res.json().await.map_err(unavailable)?;
        let text = |k: &str| g.get(k).and_then(|x| x.as_str());
        let (Some(gid), Some(st)) = (text("id"), text("status")) else {
            return Err(LichessError::Unavailable(
                "the game had no id or status".into(),
            ));
        };
        let moves: Vec<String> = text("moves")
            .unwrap_or("")
            .split_whitespace()
            .map(str::to_string)
            .collect();
        Ok(Some(LichessGame {
            id: gid.to_string(),
            status: GameStatus::parse(st),
            winner: g.get("winner").and_then(side_of),
            ply: moves.len() as i32,
            moves,
            clocks_cs: clocks_of(&g),
            white: player_id(&g, "white"),
            black: player_id(&g, "black"),
            variant: text("variant").unwrap_or("").to_string(),
            rated: g.get("rated").and_then(|v| v.as_bool()).unwrap_or(false),
        }))
    }
}

fn clocks_of(game: &serde_json::Value) -> Vec<i64> {
    game.get("clocks")
        .and_then(|c| c.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_i64()).collect())
        .unwrap_or_default()
}

fn side_of(v: &serde_json::Value) -> Option<Side> {
    match v.as_str()? {
        "white" => Some(Side::White),
        "black" => Some(Side::Black),
        _ => None,
    }
}

fn player_id(game: &serde_json::Value, color: &str) -> Option<String> {
    game.get("players")?
        .get(color)?
        .get("user")?
        .get("id")?
        .as_str()
        .map(|s| s.to_ascii_lowercase())
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
