//! Players, their custodial wallets, and sessions.
//!
//! Tokens never appear here in the clear: callers pass hashes (see `eschess_auth::token`).

use std::fmt;

use eschess_domain::money::AssetId;
use eschess_ledger::{AccountId, Side};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::ledger::{self, Balance, LedgerStoreError, NewAccount};

#[derive(Debug)]
pub enum IdentityError {
    /// The emailed token is unknown, already used, expired, or for a different purpose.
    InvalidToken,
    EmailTaken,
    UsernameTaken,
    Ledger(LedgerStoreError),
    Database(sqlx::Error),
}

impl fmt::Display for IdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IdentityError::InvalidToken => write!(f, "that link is invalid or has expired"),
            IdentityError::EmailTaken => write!(f, "that email is already registered"),
            IdentityError::UsernameTaken => write!(f, "that username is taken"),
            IdentityError::Ledger(e) => write!(f, "{e}"),
            IdentityError::Database(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for IdentityError {}

impl From<sqlx::Error> for IdentityError {
    fn from(e: sqlx::Error) -> Self {
        if let sqlx::Error::Database(db) = &e {
            match db.constraint() {
                Some("users_email_unique") => return IdentityError::EmailTaken,
                Some("users_username_unique") => return IdentityError::UsernameTaken,
                _ => {}
            }
        }
        IdentityError::Database(e)
    }
}

impl From<LedgerStoreError> for IdentityError {
    fn from(e: LedgerStoreError) -> Self {
        IdentityError::Ledger(e)
    }
}

type Result<T> = std::result::Result<T, IdentityError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: Uuid,
    pub email: String,
    pub username: String,
    pub email_verified: bool,
}

/// Creates a player and one custodial wallet account per asset, in one transaction: either the player exists
/// with all their wallets, or nothing was created.
pub async fn create_user_with_wallets(
    pool: &PgPool,
    email: &str,
    username: &str,
    password_hash: &str,
    assets: &[AssetId],
) -> Result<User> {
    let id = Uuid::now_v7();
    let mut tx = pool.begin().await?;
    sqlx::query("INSERT INTO users (id, email, username, password_hash) VALUES ($1,$2,$3,$4)")
        .bind(id)
        .bind(email)
        .bind(username)
        .bind(password_hash)
        .execute(&mut *tx)
        .await?;
    for asset in assets {
        // A wallet is a liability of the platform to the player: a credit raises it, and it can never go below zero.
        let account = NewAccount {
            kind: "user_wallet".into(),
            asset: asset.clone(),
            normal_side: Side::Credit,
            allow_negative: false,
            owner_id: Some(id),
        };
        ledger::create_account(&mut *tx, &account).await?;
    }
    tx.commit().await?;
    Ok(User {
        id,
        email: email.into(),
        username: username.into(),
        email_verified: false,
    })
}

#[derive(Debug, Clone)]
pub struct Credentials {
    pub user_id: Uuid,
    pub password_hash: String,
    pub active: bool,
    pub locked: bool,
}

pub async fn credentials_by_email(pool: &PgPool, email: &str) -> Result<Option<Credentials>> {
    let row = sqlx::query("SELECT id, password_hash, status = 'active' AS active, (locked_until IS NOT NULL AND locked_until > now()) AS locked FROM users WHERE email = $1").bind(email).fetch_optional(pool).await?;
    Ok(row.map(|r| Credentials {
        user_id: r.get("id"),
        password_hash: r.get("password_hash"),
        active: r.get("active"),
        locked: r.get("locked"),
    }))
}

/// Counts a wrong password; after `max` in a row the account is locked for `lockout_minutes`. Returns whether it is now locked.
pub async fn record_failed_login(
    pool: &PgPool,
    user_id: Uuid,
    max: i32,
    lockout_minutes: i32,
) -> Result<bool> {
    Ok(sqlx::query_scalar(
        "UPDATE users SET failed_login_count = failed_login_count + 1,
             locked_until = CASE WHEN failed_login_count + 1 >= $2 THEN now() + make_interval(mins => $3) ELSE locked_until END
         WHERE id = $1 RETURNING (locked_until IS NOT NULL AND locked_until > now())",
    )
    .bind(user_id)
    .bind(max)
    .bind(lockout_minutes)
    .fetch_one(pool)
    .await?)
}

pub async fn clear_failed_logins(pool: &PgPool, user_id: Uuid) -> Result<()> {
    sqlx::query("UPDATE users SET failed_login_count = 0, locked_until = NULL WHERE id = $1")
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub struct Lifetimes {
    pub access_secs: i64,
    pub refresh_secs: i64,
    pub session_max_secs: i64,
}

/// Starts a session for a signed-in device, returning its id.
pub async fn create_session(
    pool: &PgPool,
    user_id: Uuid,
    device_label: Option<&str>,
    access_hash: &str,
    refresh_hash: &str,
    life: Lifetimes,
) -> Result<Uuid> {
    let id = Uuid::now_v7();
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO sessions (id, user_id, device_label, access_hash, access_expires_at, absolute_expires_at)
         VALUES ($1,$2,$3,$4, now() + make_interval(secs => $5), now() + make_interval(secs => $6))",
    )
    .bind(id)
    .bind(user_id)
    .bind(device_label)
    .bind(access_hash)
    .bind(life.access_secs as f64)
    .bind(life.session_max_secs as f64)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO refresh_tokens (id, session_id, token_hash, expires_at) VALUES ($1,$2,$3, now() + make_interval(secs => $4))")
        .bind(Uuid::now_v7())
        .bind(id)
        .bind(refresh_hash)
        .bind(life.refresh_secs as f64)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Authenticated {
    pub user_id: Uuid,
    pub session_id: Uuid,
    pub email_verified: bool,
}

/// Who an access token belongs to, if it is current: not expired, session not revoked or past its maximum
/// age, and the player still active.
pub async fn authenticate(pool: &PgPool, access_hash: &str) -> Result<Option<Authenticated>> {
    let row = sqlx::query(
        "SELECT s.id, s.user_id, u.email_verified_at IS NOT NULL AS verified, s.last_seen_at < now() - interval '1 minute' AS stale
         FROM sessions s JOIN users u ON u.id = s.user_id
         WHERE s.access_hash = $1 AND s.access_expires_at > now() AND s.revoked_at IS NULL AND s.absolute_expires_at > now() AND u.status = 'active'",
    )
    .bind(access_hash)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let found = Authenticated {
        user_id: row.get("user_id"),
        session_id: row.get("id"),
        email_verified: row.get("verified"),
    };
    if row.get::<bool, _>("stale") {
        sqlx::query("UPDATE sessions SET last_seen_at = now() WHERE id = $1")
            .bind(found.session_id)
            .execute(pool)
            .await?; // at most once a minute
    }
    Ok(Some(found))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rotation {
    /// The refresh token was good and has been spent; new tokens were stored.
    Rotated(Authenticated),
    /// Unknown, expired, or belongs to a revoked session.
    Invalid,
    /// A token that was already spent was presented again. The session has been revoked.
    Reused,
}

/// Spends a refresh token and issues the next pair. A token can be used exactly once; presenting a spent one
/// means it was copied, so the whole session is revoked (and the revocation is kept even though this call
/// reports failure).
pub async fn rotate(
    pool: &PgPool,
    old_refresh_hash: &str,
    new_access_hash: &str,
    new_refresh_hash: &str,
    life: Lifetimes,
) -> Result<Rotation> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "SELECT rt.id, rt.used_at IS NOT NULL AS used, rt.expires_at > now() AS fresh,
                s.id AS session_id, s.user_id, s.revoked_at IS NOT NULL AS revoked, s.absolute_expires_at > now() AS in_window, u.status = 'active' AS active
         FROM refresh_tokens rt JOIN sessions s ON s.id = rt.session_id JOIN users u ON u.id = s.user_id
         WHERE rt.token_hash = $1 FOR UPDATE OF rt, s",
    )
    .bind(old_refresh_hash)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        return Ok(Rotation::Invalid);
    };
    let (token_id, session_id, user_id): (Uuid, Uuid, Uuid) =
        (row.get("id"), row.get("session_id"), row.get("user_id"));
    if row.get::<bool, _>("revoked")
        || !row.get::<bool, _>("in_window")
        || !row.get::<bool, _>("active")
    {
        return Ok(Rotation::Invalid);
    }
    if row.get::<bool, _>("used") {
        sqlx::query("UPDATE sessions SET revoked_at = now(), revoked_reason = 'refresh_token_reuse' WHERE id = $1").bind(session_id).execute(&mut *tx).await?;
        tx.commit().await?;
        return Ok(Rotation::Reused);
    }
    if !row.get::<bool, _>("fresh") {
        return Ok(Rotation::Invalid);
    }
    sqlx::query("UPDATE refresh_tokens SET used_at = now() WHERE id = $1")
        .bind(token_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO refresh_tokens (id, session_id, token_hash, expires_at) VALUES ($1,$2,$3, now() + make_interval(secs => $4))")
        .bind(Uuid::now_v7())
        .bind(session_id)
        .bind(new_refresh_hash)
        .bind(life.refresh_secs as f64)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE sessions SET access_hash = $2, access_expires_at = now() + make_interval(secs => $3), last_seen_at = now() WHERE id = $1")
        .bind(session_id)
        .bind(new_access_hash)
        .bind(life.access_secs as f64)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Rotation::Rotated(Authenticated {
        user_id,
        session_id,
        email_verified: false,
    }))
}

/// Ends one of this player's sessions. Returns false if it is not theirs, does not exist or is already ended
/// (so a player cannot probe for other people's sessions).
pub async fn revoke_session(
    pool: &PgPool,
    user_id: Uuid,
    session_id: Uuid,
    reason: &str,
) -> Result<bool> {
    let n = sqlx::query("UPDATE sessions SET revoked_at = now(), revoked_reason = $3 WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL")
        .bind(session_id)
        .bind(user_id)
        .bind(reason)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(n == 1)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: Uuid,
    pub device_label: Option<String>,
    pub created_at: String,
    pub last_seen_at: String,
}

/// The player's live sessions, newest first.
pub async fn active_sessions(pool: &PgPool, user_id: Uuid) -> Result<Vec<SessionInfo>> {
    let rows = sqlx::query(
        "SELECT id, device_label, to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS created, to_char(last_seen_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') AS seen
         FROM sessions WHERE user_id = $1 AND revoked_at IS NULL AND absolute_expires_at > now() ORDER BY created_at DESC, id DESC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| SessionInfo {
            id: r.get("id"),
            device_label: r.get("device_label"),
            created_at: r.get("created"),
            last_seen_at: r.get("seen"),
        })
        .collect())
}

pub async fn user_by_id(pool: &PgPool, id: Uuid) -> Result<Option<User>> {
    let row = sqlx::query("SELECT id, email, username, email_verified_at IS NOT NULL AS verified FROM users WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| User {
        id: r.get("id"),
        email: r.get("email"),
        username: r.get("username"),
        email_verified: r.get("verified"),
    }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalletBalance {
    pub account: AccountId,
    pub balance: Balance,
}

/// The player's wallet balances, read from the ledger (the only place a balance exists).
pub async fn wallet(pool: &PgPool, user_id: Uuid) -> Result<Vec<WalletBalance>> {
    let ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM ledger_accounts WHERE owner_id = $1 AND kind = 'user_wallet' ORDER BY asset").bind(user_id).fetch_all(pool).await?;
    let mut out = Vec::new();
    for id in ids {
        out.push(WalletBalance {
            account: AccountId(id),
            balance: ledger::balance(pool, AccountId(id)).await?,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------- emailed one-time tokens

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenPurpose {
    VerifyEmail,
    ResetPassword,
}

impl TokenPurpose {
    fn as_str(self) -> &'static str {
        match self {
            TokenPurpose::VerifyEmail => "verify_email",
            TokenPurpose::ResetPassword => "reset_password",
        }
    }
}

/// Says whether we may send this kind of email to this address now, and records it if so. At most one of each
/// kind per `cooldown_secs`, so repeated requests cannot be used to flood someone's inbox.
pub async fn allow_email(
    pool: &PgPool,
    email: &str,
    kind: &str,
    cooldown_secs: i64,
) -> Result<bool> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
        .bind(format!("email:{email}:{kind}"))
        .execute(&mut *tx)
        .await?;
    let allowed = sqlx::query(
        "INSERT INTO email_log (email, kind) SELECT $1, $2
         WHERE NOT EXISTS (SELECT 1 FROM email_log WHERE email = $1 AND kind = $2 AND created_at > now() - make_interval(secs => $3))",
    )
    .bind(email)
    .bind(kind)
    .bind(cooldown_secs as f64)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    tx.commit().await?;
    Ok(allowed)
}

/// Stores the hash of a new one-time token, cancelling the player's earlier unused tokens of the same purpose.
pub async fn create_email_token(
    pool: &PgPool,
    user_id: Uuid,
    email: &str,
    purpose: TokenPurpose,
    token_hash: &str,
    ttl_secs: i64,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE email_tokens SET used_at = now() WHERE user_id = $1 AND purpose = $2 AND used_at IS NULL").bind(user_id).bind(purpose.as_str()).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO email_tokens (id, user_id, email, purpose, token_hash, expires_at) VALUES ($1,$2,$3,$4,$5, now() + make_interval(secs => $6))")
        .bind(Uuid::now_v7())
        .bind(user_id)
        .bind(email)
        .bind(purpose.as_str())
        .bind(token_hash)
        .bind(ttl_secs as f64)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Confirms the player's email address with a verification token, spending it. Works only while the address
/// the token was sent to is still the player's address.
pub async fn verify_email(pool: &PgPool, token_hash: &str) -> Result<()> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query("SELECT id, user_id, email FROM email_tokens WHERE token_hash = $1 AND purpose = 'verify_email' AND used_at IS NULL AND expires_at > now() FOR UPDATE")
        .bind(token_hash)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(IdentityError::InvalidToken)?;
    let (token_id, user_id, email): (Uuid, Uuid, String) =
        (row.get("id"), row.get("user_id"), row.get("email"));
    let changed = sqlx::query("UPDATE users SET email_verified_at = COALESCE(email_verified_at, now()) WHERE id = $1 AND email = $2").bind(user_id).bind(&email).execute(&mut *tx).await?.rows_affected();
    if changed == 0 {
        return Err(IdentityError::InvalidToken);
    }
    sqlx::query("UPDATE email_tokens SET used_at = now() WHERE id = $1")
        .bind(token_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Sets a new password with a reset token, spending it. Everything that depended on the old password ends:
/// every signed-in device is signed out, and any lockout is lifted. Holding the token also proves the player
/// can read their mailbox, so the address counts as confirmed. Returns the player's id.
pub async fn reset_password(
    pool: &PgPool,
    token_hash: &str,
    new_password_hash: &str,
) -> Result<Uuid> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query("SELECT id, user_id FROM email_tokens WHERE token_hash = $1 AND purpose = 'reset_password' AND used_at IS NULL AND expires_at > now() FOR UPDATE")
        .bind(token_hash)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(IdentityError::InvalidToken)?;
    let (token_id, user_id): (Uuid, Uuid) = (row.get("id"), row.get("user_id"));
    sqlx::query("UPDATE users SET password_hash = $2, failed_login_count = 0, locked_until = NULL, email_verified_at = COALESCE(email_verified_at, now()) WHERE id = $1").bind(user_id).bind(new_password_hash).execute(&mut *tx).await?;
    sqlx::query("UPDATE sessions SET revoked_at = now(), revoked_reason = 'password_reset' WHERE user_id = $1 AND revoked_at IS NULL").bind(user_id).execute(&mut *tx).await?;
    sqlx::query("UPDATE email_tokens SET used_at = now() WHERE user_id = $1 AND purpose = 'reset_password' AND used_at IS NULL").bind(user_id).execute(&mut *tx).await?;
    let _ = token_id;
    tx.commit().await?;
    Ok(user_id)
}
