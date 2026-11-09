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
