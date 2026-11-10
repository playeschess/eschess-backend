//! Each user's own Stellar wallet (ADR 0006): the address, and the key sealed by the key vault.
//!
//! [`ensure_wallet`] makes one if the user has none and is safe to call at any time and from many places at once: the
//! first stored wins and every caller gets that address. The secret key is never read back except to sign, through
//! [`signer_for`].

use std::sync::Arc;

use eschess_domain::stellar::AccountKey;
use eschess_stellar::custody::{self, KeyVault, Sealed, VaultError, VaultSigner};
use sqlx::{PgPool, Row};
use uuid::Uuid;

#[derive(Debug)]
pub enum WalletError {
    Vault(VaultError),
    /// The stored row is not something this code wrote (a bad address): a bug or tampering.
    Corrupt(String),
    Database(sqlx::Error),
}

impl std::fmt::Display for WalletError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WalletError::Vault(e) => write!(f, "{e}"),
            WalletError::Corrupt(m) => write!(f, "a stored wallet is damaged: {m}"),
            WalletError::Database(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for WalletError {}

impl From<sqlx::Error> for WalletError {
    fn from(e: sqlx::Error) -> Self {
        WalletError::Database(e)
    }
}

impl From<VaultError> for WalletError {
    fn from(e: VaultError) -> Self {
        WalletError::Vault(e)
    }
}

/// The user's wallet address, if they have one.
pub async fn address_of(pool: &PgPool, user: Uuid) -> Result<Option<AccountKey>, WalletError> {
    let text: Option<String> =
        sqlx::query_scalar("SELECT address FROM user_wallet_keys WHERE user_id = $1")
            .bind(user)
            .fetch_optional(pool)
            .await?;
    text.map(|t| AccountKey::parse(&t).map_err(|e| WalletError::Corrupt(e.to_string())))
        .transpose()
}

/// The user's wallet address, creating the wallet first if they have none.
pub async fn ensure_wallet(
    pool: &PgPool,
    vault: &dyn KeyVault,
    user: Uuid,
) -> Result<AccountKey, WalletError> {
    if let Some(a) = address_of(pool, user).await? {
        return Ok(a);
    }
    let fresh = custody::generate(vault, &user.to_string()).await?;
    let inserted = sqlx::query(
        "INSERT INTO user_wallet_keys (user_id, address, key_version, nonce, ciphertext) \
         VALUES ($1, $2, $3, $4, $5) ON CONFLICT (user_id) DO NOTHING",
    )
    .bind(user)
    .bind(fresh.address.to_g())
    .bind(fresh.sealed.key_version)
    .bind(fresh.sealed.nonce.as_slice())
    .bind(&fresh.sealed.ciphertext)
    .execute(pool)
    .await?
    .rows_affected();
    // If `inserted` is 0, another request made one first: ours is discarded unused and theirs is the wallet.
    let _ = inserted;
    address_of(pool, user)
        .await?
        .ok_or_else(|| WalletError::Corrupt("the wallet was not stored".into()))
}

/// A signer for the user's wallet, or `None` if they have none. The key is opened only while a signature is made.
pub async fn signer_for(
    pool: &PgPool,
    vault: Arc<dyn KeyVault>,
    user: Uuid,
) -> Result<Option<VaultSigner>, WalletError> {
    let Some(row) = sqlx::query(
        "SELECT address, key_version, nonce, ciphertext FROM user_wallet_keys WHERE user_id = $1",
    )
    .bind(user)
    .fetch_optional(pool)
    .await?
    else {
        return Ok(None);
    };
    let address = AccountKey::parse(&row.get::<String, _>("address"))
        .map_err(|e| WalletError::Corrupt(e.to_string()))?;
    let nonce: Vec<u8> = row.get("nonce");
    let sealed = Sealed {
        key_version: row.get("key_version"),
        nonce: nonce
            .try_into()
            .map_err(|_| WalletError::Corrupt("bad nonce".into()))?,
        ciphertext: row.get("ciphertext"),
    };
    Ok(Some(VaultSigner::new(
        vault,
        &user.to_string(),
        address,
        sealed,
    )))
}

// ---------------------------------------------------------------------------------------------------------------
// Setting the account up on the network

/// Asks for the user's account to be set up on the network. Does nothing if they have no wallet key, or already asked.
/// Returns whether a new request was made.
pub async fn request_setup(pool: &PgPool, user: Uuid) -> Result<bool, WalletError> {
    Ok(sqlx::query(
        "INSERT INTO wallet_setup (user_id) SELECT user_id FROM user_wallet_keys WHERE user_id = $1 \
         ON CONFLICT (user_id) DO NOTHING",
    )
    .bind(user)
    .execute(pool)
    .await?
    .rows_affected()
        > 0)
}

/// Where the user's account setup stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupState {
    /// Nobody asked yet (or the user has no wallet key).
    NotRequested,
    /// Asked for; the worker has not finished.
    SettingUp,
    Ready,
}

pub async fn setup_state(pool: &PgPool, user: Uuid) -> Result<SetupState, WalletError> {
    let row: Option<bool> =
        sqlx::query_scalar("SELECT ready_at IS NOT NULL FROM wallet_setup WHERE user_id = $1")
            .bind(user)
            .fetch_optional(pool)
            .await?;
    Ok(match row {
        None => SetupState::NotRequested,
        Some(false) => SetupState::SettingUp,
        Some(true) => SetupState::Ready,
    })
}

/// An account waiting to be set up.
#[derive(Debug, Clone)]
pub struct PendingSetup {
    pub user_id: Uuid,
    pub address: AccountKey,
    /// The last transaction sent for it and the time after which it can never land, if one was sent.
    pub last_tx: Option<(String, i64)>,
    pub attempts: i32,
}

/// Accounts asked for and not ready, oldest request first.
pub async fn pending_setups(pool: &PgPool, limit: i64) -> Result<Vec<PendingSetup>, WalletError> {
    let rows = sqlx::query(
        "SELECT s.user_id, k.address, s.last_tx_hash, s.last_tx_max_time, s.attempts \
         FROM wallet_setup s JOIN user_wallet_keys k ON k.user_id = s.user_id \
         WHERE s.ready_at IS NULL ORDER BY s.requested_at, s.user_id LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(PendingSetup {
                user_id: r.get("user_id"),
                address: AccountKey::parse(&r.get::<String, _>("address"))
                    .map_err(|e| WalletError::Corrupt(e.to_string()))?,
                last_tx: r
                    .get::<Option<String>, _>("last_tx_hash")
                    .zip(r.get::<Option<i64>, _>("last_tx_max_time")),
                attempts: r.get("attempts"),
            })
        })
        .collect()
}

/// Records a transaction about to be sent, before it is sent, so that if the worker dies it is looked up, not rebuilt.
pub async fn record_setup_tx(
    pool: &PgPool,
    user: Uuid,
    hash_hex: &str,
    max_time: i64,
) -> Result<(), WalletError> {
    sqlx::query(
        "UPDATE wallet_setup SET last_tx_hash = $2, last_tx_max_time = $3, attempts = attempts + 1, \
         updated_at = now() WHERE user_id = $1 AND ready_at IS NULL",
    )
    .bind(user)
    .bind(hash_hex)
    .bind(max_time)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn record_setup_error(pool: &PgPool, user: Uuid, error: &str) -> Result<(), WalletError> {
    sqlx::query(
        "UPDATE wallet_setup SET last_error = left($2, 500), updated_at = now() WHERE user_id = $1",
    )
    .bind(user)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

/// The account exists on the network and can hold USDC.
pub async fn mark_setup_ready(pool: &PgPool, user: Uuid) -> Result<(), WalletError> {
    sqlx::query(
        "UPDATE wallet_setup SET ready_at = now(), last_error = NULL, updated_at = now() \
         WHERE user_id = $1 AND ready_at IS NULL",
    )
    .bind(user)
    .execute(pool)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------
// Watching players' own addresses for deposits

/// How often an address is looked at.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WatchTiming {
    /// While the owner is likely to be depositing: this often.
    pub hot_secs: f64,
    /// How long after the last activity (asking how to deposit, money arriving) an address stays hot.
    pub hot_window_secs: f64,
    /// Otherwise: this often. A safety net, not the normal path.
    pub cold_secs: f64,
}

impl Default for WatchTiming {
    fn default() -> Self {
        WatchTiming {
            hot_secs: 5.0,
            hot_window_secs: 3_600.0,
            cold_secs: 300.0,
        }
    }
}

/// The owner is about to deposit (they asked how): look at their address soon and often for a while.
pub async fn touch_watch(pool: &PgPool, user: Uuid) -> Result<(), WalletError> {
    sqlx::query(
        "INSERT INTO wallet_watch (user_id) SELECT user_id FROM user_wallet_keys WHERE user_id = $1 \
         ON CONFLICT (user_id) DO UPDATE SET last_activity_at = now(), next_check_at = now()",
    )
    .bind(user)
    .execute(pool)
    .await?;
    Ok(())
}

/// Addresses that are ready to receive money and due to be looked at, the most overdue first.
pub async fn due_for_watch(
    pool: &PgPool,
    limit: i64,
) -> Result<Vec<(Uuid, AccountKey)>, WalletError> {
    let rows = sqlx::query(
        "SELECT k.user_id, k.address FROM wallet_setup s \
         JOIN user_wallet_keys k ON k.user_id = s.user_id \
         LEFT JOIN wallet_watch w ON w.user_id = s.user_id \
         WHERE s.ready_at IS NOT NULL AND COALESCE(w.next_check_at, '-infinity') <= now() \
         ORDER BY COALESCE(w.next_check_at, '-infinity'), k.user_id LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.iter()
        .map(|r| {
            Ok((
                r.get("user_id"),
                AccountKey::parse(&r.get::<String, _>("address"))
                    .map_err(|e| WalletError::Corrupt(e.to_string()))?,
            ))
        })
        .collect()
}

/// An address was looked at. `found_money` keeps it hot; otherwise it cools down once its window has passed.
pub async fn record_watched(
    pool: &PgPool,
    user: Uuid,
    found_money: bool,
    timing: WatchTiming,
) -> Result<(), WalletError> {
    sqlx::query(
        "INSERT INTO wallet_watch (user_id, last_checked_at, next_check_at) \
         VALUES ($1, now(), now() + make_interval(secs => $3::float8)) \
         ON CONFLICT (user_id) DO UPDATE SET \
           last_checked_at = now(), \
           last_activity_at = CASE WHEN $2 THEN now() ELSE wallet_watch.last_activity_at END, \
           next_check_at = now() + make_interval(secs => CASE \
             WHEN (CASE WHEN $2 THEN now() ELSE wallet_watch.last_activity_at END) > now() - make_interval(secs => $4::float8) \
             THEN $3::float8 ELSE $5::float8 END)",
    )
    .bind(user)
    .bind(found_money)
    .bind(timing.hot_secs)
    .bind(timing.hot_window_secs)
    .bind(timing.cold_secs)
    .execute(pool)
    .await?;
    Ok(())
}
