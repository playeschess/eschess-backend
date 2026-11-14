//! Fiat in and out through a Stellar anchor: SEP-1 (the anchor's `stellar.toml`), SEP-10 (proving we own a Stellar account)
//! and SEP-24 (the anchor's own screens for ID checks and payment).
//!
//! The anchor holds the player's money in their own currency and the platform never does. A deposit is the anchor sending
//! USDC to the player's own address, which the deposit watcher already credits. A withdrawal is the platform sending USDC
//! from the player's own account to the anchor, which pays the player in their currency.
//!
//! **The login is the sensitive part.** SEP-10 has the anchor hand us a transaction to sign with the player's key. That key
//! can move the player's money, so [`sign_challenge`] signs only a challenge of exactly the shape the standard allows: sequence
//! number zero, only data-entry operations, the first one naming the player's account and the anchor's domain, signed by
//! the anchor's own key, and not expired. Anything else is refused before the key is touched.

use std::fmt;
use std::time::Duration;

use ed25519_dalek::{Signature as DalekSignature, Verifier as _, VerifyingKey};
use eschess_domain::stellar::AccountKey;
use stellar_xdr::{
    DecoratedSignature, Limits, MuxedAccount, OperationBody, Preconditions, ReadXdr, Signature,
    SignatureHint, TransactionEnvelope, WriteXdr,
};

use crate::signer::Signer;
use crate::tx::Network;

/// Largest `stellar.toml` we will read.
const MAX_TOML_BYTES: usize = 100 * 1024;

#[derive(Debug)]
pub enum SepError {
    /// The anchor's login challenge was not one we are willing to sign. Nothing was signed.
    Challenge(String),
    /// The anchor's `stellar.toml` is missing, unreadable, or lacks what we need.
    Toml(String),
    /// The anchor refused the request.
    Rejected(String),
    /// The anchor could not be reached or answered something unusable. Worth retrying later.
    Unavailable(String),
}

impl fmt::Display for SepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SepError::Challenge(m) => write!(f, "the login challenge was refused: {m}"),
            SepError::Toml(m) => write!(f, "the anchor's stellar.toml: {m}"),
            SepError::Rejected(m) => write!(f, "the anchor refused the request: {m}"),
            SepError::Unavailable(m) => write!(f, "the anchor is not reachable: {m}"),
        }
    }
}
impl std::error::Error for SepError {}

fn refuse<T>(why: &str) -> Result<T, SepError> {
    Err(SepError::Challenge(why.to_string()))
}

/// What the challenge must say for us to sign it.
pub struct ChallengeContext<'a> {
    /// The anchor's domain, as we were told to talk to it (not as the anchor claims).
    pub home_domain: &'a str,
    /// The host of the anchor's web-auth endpoint, which its `web_auth_domain` operation must carry.
    pub web_auth_domain: &'a str,
    /// The anchor's `SIGNING_KEY` from its `stellar.toml`: the account that made and signed the challenge.
    pub anchor_key: AccountKey,
    pub network: Network,
    /// Unix seconds.
    pub now: u64,
}

/// Checks a SEP-10 challenge and, only if it is exactly what the standard allows, signs it with `client` and returns the
/// signed envelope (base64 XDR) to send back. Refuses, signing nothing, on any difference.
pub async fn sign_challenge(
    challenge_xdr: &str,
    ctx: &ChallengeContext<'_>,
    client: &dyn Signer,
) -> Result<String, SepError> {
    let envelope = TransactionEnvelope::from_xdr_base64(challenge_xdr, Limits::none())
        .map_err(|_| SepError::Challenge("not a transaction".into()))?;
    let TransactionEnvelope::Tx(env) = envelope else {
        return refuse("not an ordinary transaction (fee bumps and old formats are refused)");
    };
    let tx = &env.tx;
    let client_key = client.public_key();

    // Made by the anchor, from the anchor's account, with sequence number zero: it can never be submitted.
    match &tx.source_account {
        MuxedAccount::Ed25519(k) if k.0 == ctx.anchor_key.0 => {}
        _ => return refuse("not made by the anchor's signing key"),
    }
    if tx.seq_num.0 != 0 {
        return refuse("the sequence number is not zero, so it could be submitted");
    }
    // Alive now.
    match &tx.cond {
        Preconditions::Time(t) => {
            let (min, max) = (t.min_time.0, t.max_time.0);
            if max == 0 || ctx.now < min || ctx.now > max {
                return refuse("outside its time bounds");
            }
        }
        _ => return refuse("no time bounds"),
    }

    // Only data entries. The first is the client's, "<home domain> auth"; the rest are the anchor's.
    let ops = tx.operations.as_slice();
    if ops.is_empty() {
        return refuse("no operations");
    }
    for (i, op) in ops.iter().enumerate() {
        let OperationBody::ManageData(data) = &op.body else {
            return refuse("an operation other than a data entry");
        };
        let source = match &op.source_account {
            Some(MuxedAccount::Ed25519(k)) => k.0,
            _ => return refuse("an operation without an ordinary source account"),
        };
        let name = String::from_utf8(data.data_name.as_vec().clone())
            .map_err(|_| SepError::Challenge("a data name that is not text".into()))?;
        let value = data.data_value.as_ref().map(|v| v.as_vec().clone());
        if i == 0 {
            if source != client_key.0 {
                return refuse("the first operation is not for this account");
            }
            if name != format!("{} auth", ctx.home_domain) {
                return refuse("the first operation does not name the anchor's domain");
            }
            if value.as_ref().is_none_or(|v| v.len() != 64) {
                return refuse("the first operation has no 64-byte nonce");
            }
        } else {
            if source != ctx.anchor_key.0 {
                return refuse("a later operation not from the anchor");
            }
            if name == "web_auth_domain" {
                if value.as_deref() != Some(ctx.web_auth_domain.as_bytes()) {
                    return refuse("the web auth domain is not the anchor's");
                }
            } else if name == "client_domain" {
                return refuse("a client domain, which this integration does not use");
            }
        }
    }

    // It must carry the anchor's own signature: proof the challenge came from the anchor.
    let hash = tx
        .hash(ctx.network.id())
        .map_err(|_| SepError::Challenge("cannot hash the transaction".into()))?;
    let anchor_verifying = VerifyingKey::from_bytes(&ctx.anchor_key.0)
        .map_err(|_| SepError::Challenge("the anchor's key is not valid".into()))?;
    let signed_by_anchor = env.signatures.iter().any(|s| {
        s.hint.0 == ctx.anchor_key.0[28..]
            && <[u8; 64]>::try_from(s.signature.0.as_slice())
                .map(|b| {
                    anchor_verifying
                        .verify(&hash, &DalekSignature::from_bytes(&b))
                        .is_ok()
                })
                .unwrap_or(false)
    });
    if !signed_by_anchor {
        return refuse("not signed by the anchor");
    }

    // Everything checks out: sign, once, with the player's key.
    let signature = client
        .sign(&hash)
        .await
        .map_err(|e| SepError::Challenge(format!("signing failed: {e}")))?;
    let mut env = env;
    let mut sigs = env.signatures.to_vec();
    sigs.push(DecoratedSignature {
        hint: SignatureHint(client_key.0[28..].try_into().expect("four bytes")),
        signature: Signature(
            signature
                .to_vec()
                .try_into()
                .map_err(|_| SepError::Challenge("signature size".into()))?,
        ),
    });
    env.signatures = sigs
        .try_into()
        .map_err(|_| SepError::Challenge("too many signatures".into()))?;
    TransactionEnvelope::Tx(env)
        .to_xdr_base64(Limits::none())
        .map_err(|_| SepError::Challenge("cannot encode the signed transaction".into()))
}

/// A SEP-10 token. It is never printed.
pub struct AnchorToken(String);

impl AnchorToken {
    pub fn new(token: String) -> AnchorToken {
        AnchorToken(token)
    }
    /// The token text, for the anchor's `Authorization` header. Send it nowhere else.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AnchorToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AnchorToken(..)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Deposit,
    Withdraw,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Deposit => "deposit",
            Kind::Withdraw => "withdraw",
        }
    }
}

/// The anchor's own screen for the player, and the id of the transaction it opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interactive {
    pub id: String,
    /// Where to send the player (a page of the anchor's, over https).
    pub url: String,
}

/// How an anchor transaction stands (SEP-24).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnchorStatus {
    Incomplete,
    PendingUserTransferStart,
    PendingUserTransferComplete,
    PendingExternal,
    PendingAnchor,
    PendingStellar,
    PendingTrust,
    PendingUser,
    OnHold,
    Completed,
    Refunded,
    Expired,
    NoMarket,
    TooSmall,
    TooLarge,
    Error,
    /// A status this code does not know. Treated as still in progress and never acted on.
    Other(String),
}

impl AnchorStatus {
    fn parse(s: &str) -> AnchorStatus {
        match s {
            "incomplete" => AnchorStatus::Incomplete,
            "pending_user_transfer_start" => AnchorStatus::PendingUserTransferStart,
            "pending_user_transfer_complete" => AnchorStatus::PendingUserTransferComplete,
            "pending_external" => AnchorStatus::PendingExternal,
            "pending_anchor" => AnchorStatus::PendingAnchor,
            "pending_stellar" => AnchorStatus::PendingStellar,
            "pending_trust" => AnchorStatus::PendingTrust,
            "pending_user" => AnchorStatus::PendingUser,
            "on_hold" => AnchorStatus::OnHold,
            "completed" => AnchorStatus::Completed,
            "refunded" => AnchorStatus::Refunded,
            "expired" => AnchorStatus::Expired,
            "no_market" => AnchorStatus::NoMarket,
            "too_small" => AnchorStatus::TooSmall,
            "too_large" => AnchorStatus::TooLarge,
            "error" => AnchorStatus::Error,
            other => AnchorStatus::Other(other.to_string()),
        }
    }
    /// Nothing more will happen to this transaction.
    pub fn is_final(&self) -> bool {
        matches!(
            self,
            AnchorStatus::Completed
                | AnchorStatus::Refunded
                | AnchorStatus::Expired
                | AnchorStatus::NoMarket
                | AnchorStatus::TooSmall
                | AnchorStatus::TooLarge
                | AnchorStatus::Error
        )
    }
}

/// What the anchor says about one of its transactions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorTx {
    pub id: String,
    pub kind: String,
    pub status: AnchorStatus,
    /// What the anchor expects us to send (a withdrawal), as the decimal text it gave.
    pub amount_in: Option<String>,
    pub amount_out: Option<String>,
    /// Where to send a withdrawal's USDC, and with which memo.
    pub withdraw_anchor_account: Option<String>,
    pub withdraw_memo: Option<String>,
    pub withdraw_memo_type: Option<String>,
    pub more_info_url: Option<String>,
    pub message: Option<String>,
    /// The Stellar transaction that carried the anchor's side, once there is one.
    pub stellar_transaction_id: Option<String>,
}

/// What we need from an anchor. A trait so tests (and the API) never depend on a real one.
#[async_trait::async_trait]
pub trait Anchor: Send + Sync {
    /// The domain this anchor is reached at.
    fn home_domain(&self) -> &str;
    /// The anchor's own Stellar accounts, from its `stellar.toml`. A withdrawal's USDC is sent only to one of these.
    async fn accounts(&self) -> Result<Vec<AccountKey>, SepError>;
    /// Proves we own the player's account (SEP-10) and returns a token for it. Signs the player's key only for a challenge
    /// that passes [`sign_challenge`].
    async fn authenticate(&self, user: &dyn Signer) -> Result<AnchorToken, SepError>;
    /// Opens the anchor's screens for a deposit or withdrawal of `asset_code` for `account`.
    async fn start(
        &self,
        token: &AnchorToken,
        kind: Kind,
        asset_code: &str,
        account: &AccountKey,
    ) -> Result<Interactive, SepError>;
    /// The anchor's record of one of its transactions.
    async fn transaction(&self, token: &AnchorToken, id: &str) -> Result<AnchorTx, SepError>;
}

/// The anchor's endpoints and key, from its `stellar.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorInfo {
    pub signing_key: AccountKey,
    pub web_auth_endpoint: String,
    pub transfer_server_sep24: String,
    /// The anchor's own accounts (`ACCOUNTS`): the only places a withdrawal's USDC may be sent.
    pub accounts: Vec<AccountKey>,
    /// `(code, issuer)` of the assets the anchor lists.
    pub currencies: Vec<(String, Option<String>)>,
}

fn secure(url: &str) -> bool {
    url.starts_with("https://")
        || url.starts_with("http://127.0.0.1")
        || url.starts_with("http://localhost")
}

/// Reads an anchor's `stellar.toml`. `network` is checked against the passphrase it declares.
pub fn parse_toml(text: &str, network: Network) -> Result<AnchorInfo, SepError> {
    let bad = |m: &str| SepError::Toml(m.to_string());
    let doc: toml::Table = text.parse().map_err(|_| bad("not valid TOML"))?;
    let get = |k: &str| doc.get(k).and_then(|v| v.as_str()).map(str::to_string);
    if let Some(p) = get("NETWORK_PASSPHRASE") {
        if p != network.passphrase() {
            return Err(bad("it is for a different Stellar network"));
        }
    }
    let signing_key = AccountKey::parse(&get("SIGNING_KEY").ok_or_else(|| bad("no SIGNING_KEY"))?)
        .map_err(|_| bad("SIGNING_KEY is not a Stellar account"))?;
    let web_auth_endpoint = get("WEB_AUTH_ENDPOINT").ok_or_else(|| bad("no WEB_AUTH_ENDPOINT"))?;
    let transfer_server_sep24 =
        get("TRANSFER_SERVER_SEP0024").ok_or_else(|| bad("no TRANSFER_SERVER_SEP0024"))?;
    if !secure(&web_auth_endpoint) || !secure(&transfer_server_sep24) {
        return Err(bad("its endpoints are not https"));
    }
    let accounts = doc
        .get("ACCOUNTS")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .filter_map(|g| AccountKey::parse(g).ok())
                .collect()
        })
        .unwrap_or_default();
    let currencies = doc
        .get("CURRENCIES")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|c| {
                    let code = c.get("code")?.as_str()?.to_string();
                    Some((
                        code,
                        c.get("issuer").and_then(|i| i.as_str()).map(str::to_string),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(AnchorInfo {
        signing_key,
        web_auth_endpoint,
        transfer_server_sep24,
        accounts,
        currencies,
    })
}

/// A real anchor, reached over HTTP.
pub struct HttpAnchor {
    home_domain: String,
    web_auth_domain: String,
    info: AnchorInfo,
    network: Network,
    http: reqwest::Client,
    clock: fn() -> u64,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn host_of(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.host_str()
                .map(|h| u.port().map_or(h.to_string(), |p| format!("{h}:{p}")))
        })
        .unwrap_or_default()
}

fn unavailable(e: reqwest::Error) -> SepError {
    SepError::Unavailable(e.without_url().to_string())
}

impl HttpAnchor {
    /// Reads the anchor's `stellar.toml` at `toml_base` (`https://<home_domain>`, or a stand-in server in tests) and checks
    /// that it lists `asset` from `issuer`: an anchor for some other asset, or another issuer's USDC, is refused here.
    pub async fn connect(
        home_domain: &str,
        toml_base: &str,
        network: Network,
        asset: (&str, &str),
    ) -> Result<HttpAnchor, SepError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .expect("an HTTP client");
        let res = http
            .get(format!(
                "{}/.well-known/stellar.toml",
                toml_base.trim_end_matches('/')
            ))
            .send()
            .await
            .map_err(|e| SepError::Toml(format!("cannot be read: {}", e.without_url())))?;
        if !res.status().is_success() {
            return Err(SepError::Toml(format!("status {}", res.status())));
        }
        let bytes = res
            .bytes()
            .await
            .map_err(|e| SepError::Toml(format!("cannot be read: {}", e.without_url())))?;
        if bytes.len() > MAX_TOML_BYTES {
            return Err(SepError::Toml("it is too large".into()));
        }
        let text =
            String::from_utf8(bytes.to_vec()).map_err(|_| SepError::Toml("not text".into()))?;
        let info = parse_toml(&text, network)?;
        if !info
            .currencies
            .iter()
            .any(|(code, issuer)| code == asset.0 && issuer.as_deref() == Some(asset.1))
        {
            return Err(SepError::Toml(format!(
                "it does not list {} from the issuer we use",
                asset.0
            )));
        }
        Ok(HttpAnchor {
            home_domain: home_domain.to_string(),
            web_auth_domain: host_of(&info.web_auth_endpoint),
            info,
            network,
            http,
            clock: now_secs,
        })
    }

    fn sep24(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.info.transfer_server_sep24.trim_end_matches('/'),
            path
        )
    }
}

async fn json_of(
    res: reqwest::Response,
) -> Result<(reqwest::StatusCode, serde_json::Value), SepError> {
    let status = res.status();
    let body = res.json().await.unwrap_or(serde_json::Value::Null);
    Ok((status, body))
}

fn refused(body: &serde_json::Value) -> SepError {
    let why = body
        .get("error")
        .and_then(|v| v.as_str())
        .or_else(|| body.get("message").and_then(|v| v.as_str()))
        .unwrap_or("no reason given");
    SepError::Rejected(why.chars().take(300).collect())
}

#[async_trait::async_trait]
impl Anchor for HttpAnchor {
    fn home_domain(&self) -> &str {
        &self.home_domain
    }

    async fn accounts(&self) -> Result<Vec<AccountKey>, SepError> {
        Ok(self.info.accounts.clone())
    }

    async fn authenticate(&self, user: &dyn Signer) -> Result<AnchorToken, SepError> {
        let account = user.public_key().to_g();
        let res = self
            .http
            .get(&self.info.web_auth_endpoint)
            .query(&[
                ("account", account.as_str()),
                ("home_domain", self.home_domain.as_str()),
            ])
            .send()
            .await
            .map_err(unavailable)?;
        let (status, body) = json_of(res).await?;
        if !status.is_success() {
            return Err(if status.is_client_error() {
                refused(&body)
            } else {
                SepError::Unavailable(format!("status {status}"))
            });
        }
        let challenge = body
            .get("transaction")
            .and_then(|v| v.as_str())
            .ok_or_else(|| SepError::Unavailable("no challenge in the answer".into()))?;
        if let Some(p) = body.get("network_passphrase").and_then(|v| v.as_str()) {
            if p != self.network.passphrase() {
                return Err(SepError::Challenge(
                    "the challenge is for another network".into(),
                ));
            }
        }
        let ctx = ChallengeContext {
            home_domain: &self.home_domain,
            web_auth_domain: &self.web_auth_domain,
            anchor_key: self.info.signing_key,
            network: self.network,
            now: (self.clock)(),
        };
        let signed = sign_challenge(challenge, &ctx, user).await?;
        let res = self
            .http
            .post(&self.info.web_auth_endpoint)
            .json(&serde_json::json!({ "transaction": signed }))
            .send()
            .await
            .map_err(unavailable)?;
        let (status, body) = json_of(res).await?;
        if !status.is_success() {
            return Err(if status.is_client_error() {
                refused(&body)
            } else {
                SepError::Unavailable(format!("status {status}"))
            });
        }
        body.get("token")
            .and_then(|v| v.as_str())
            .filter(|t| !t.is_empty())
            .map(|t| AnchorToken::new(t.to_string()))
            .ok_or_else(|| SepError::Unavailable("no token in the answer".into()))
    }

    async fn start(
        &self,
        token: &AnchorToken,
        kind: Kind,
        asset_code: &str,
        account: &AccountKey,
    ) -> Result<Interactive, SepError> {
        let res = self
            .http
            .post(self.sep24(&format!("transactions/{}/interactive", kind.as_str())))
            .bearer_auth(token.expose())
            .json(&serde_json::json!({ "asset_code": asset_code, "account": account.to_g(), "lang": "en" }))
            .send()
            .await
            .map_err(unavailable)?;
        let (status, body) = json_of(res).await?;
        if !status.is_success() {
            return Err(if status.is_client_error() {
                refused(&body)
            } else {
                SepError::Unavailable(format!("status {status}"))
            });
        }
        let text = |k: &str| body.get(k).and_then(|v| v.as_str());
        match (text("id"), text("url")) {
            (Some(id), Some(url)) if secure(url) && !id.is_empty() => Ok(Interactive {
                id: id.to_string(),
                url: url.to_string(),
            }),
            (Some(_), Some(_)) => Err(SepError::Unavailable(
                "the anchor's screen is not on https".into(),
            )),
            _ => Err(SepError::Unavailable("no id or url in the answer".into())),
        }
    }

    async fn transaction(&self, token: &AnchorToken, id: &str) -> Result<AnchorTx, SepError> {
        let res = self
            .http
            .get(self.sep24("transaction"))
            .query(&[("id", id)])
            .bearer_auth(token.expose())
            .send()
            .await
            .map_err(unavailable)?;
        let (status, body) = json_of(res).await?;
        if !status.is_success() {
            return Err(if status.is_client_error() {
                refused(&body)
            } else {
                SepError::Unavailable(format!("status {status}"))
            });
        }
        let t = body
            .get("transaction")
            .ok_or_else(|| SepError::Unavailable("no transaction in the answer".into()))?;
        let text = |k: &str| t.get(k).and_then(|v| v.as_str()).map(str::to_string);
        let (Some(got_id), Some(status_text)) = (text("id"), text("status")) else {
            return Err(SepError::Unavailable(
                "the transaction has no id or status".into(),
            ));
        };
        if got_id != id {
            return Err(SepError::Unavailable(
                "the anchor answered about another transaction".into(),
            ));
        }
        Ok(AnchorTx {
            id: got_id,
            kind: text("kind").unwrap_or_default(),
            status: AnchorStatus::parse(&status_text),
            amount_in: text("amount_in"),
            amount_out: text("amount_out"),
            withdraw_anchor_account: text("withdraw_anchor_account"),
            withdraw_memo: text("withdraw_memo"),
            withdraw_memo_type: text("withdraw_memo_type"),
            more_info_url: text("more_info_url"),
            message: text("message"),
            stellar_transaction_id: text("stellar_transaction_id"),
        })
    }
}

/// An anchor that connects the first time it is needed, and tries again next time if that failed. A provider being down when
/// the platform starts must not switch fiat off until someone restarts it.
pub struct LazyAnchor {
    home_domain: String,
    toml_base: String,
    network: Network,
    asset_code: String,
    asset_issuer: String,
    connected: tokio::sync::Mutex<Option<std::sync::Arc<HttpAnchor>>>,
}

impl LazyAnchor {
    pub fn new(
        home_domain: &str,
        toml_base: &str,
        network: Network,
        asset: (&str, &str),
    ) -> LazyAnchor {
        LazyAnchor {
            home_domain: home_domain.to_string(),
            toml_base: toml_base.to_string(),
            network,
            asset_code: asset.0.to_string(),
            asset_issuer: asset.1.to_string(),
            connected: tokio::sync::Mutex::new(None),
        }
    }

    async fn get(&self) -> Result<std::sync::Arc<HttpAnchor>, SepError> {
        let mut slot = self.connected.lock().await;
        if let Some(a) = &*slot {
            return Ok(a.clone());
        }
        let a = std::sync::Arc::new(
            HttpAnchor::connect(
                &self.home_domain,
                &self.toml_base,
                self.network,
                (&self.asset_code, &self.asset_issuer),
            )
            .await?,
        );
        *slot = Some(a.clone());
        Ok(a)
    }
}

#[async_trait::async_trait]
impl Anchor for LazyAnchor {
    fn home_domain(&self) -> &str {
        &self.home_domain
    }
    async fn accounts(&self) -> Result<Vec<AccountKey>, SepError> {
        self.get().await?.accounts().await
    }
    async fn authenticate(&self, user: &dyn Signer) -> Result<AnchorToken, SepError> {
        self.get().await?.authenticate(user).await
    }
    async fn start(
        &self,
        token: &AnchorToken,
        kind: Kind,
        asset_code: &str,
        account: &AccountKey,
    ) -> Result<Interactive, SepError> {
        self.get()
            .await?
            .start(token, kind, asset_code, account)
            .await
    }
    async fn transaction(&self, token: &AnchorToken, id: &str) -> Result<AnchorTx, SepError> {
        self.get().await?.transaction(token, id).await
    }
}
