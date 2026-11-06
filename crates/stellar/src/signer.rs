//! Who signs a transaction.
//!
//! The code that builds and sends payments only sees the [`Signer`] trait: it asks for a signature over a transaction
//! hash and never touches a key. That is the seam where production signing (a KMS or HSM, and later multisig) replaces
//! [`LocalSigner`] without changing anything else (ADR 0001, architecture §9).
//!
//! [`LocalSigner`] keeps an ed25519 key in memory. It exists for the **testnet only** and refuses to be created for the
//! public network: a real-money key must never be a file on a server or a developer's machine.

use std::fmt;
use std::path::Path;

use ed25519_dalek::{Signer as _, SigningKey};
use eschess_domain::stellar::{decode_secret_seed, AccountKey};
use zeroize::Zeroize;

use crate::tx::Network;

/// Signs transaction hashes for one account.
#[async_trait::async_trait]
pub trait Signer: Send + Sync {
    /// The account whose key this is.
    fn public_key(&self) -> AccountKey;

    /// The ed25519 signature of a 32-byte transaction hash.
    async fn sign(&self, hash: &[u8; 32]) -> Result<[u8; 64], SignError>;
}

/// Signing could not be done right now (a remote signer was unreachable, refused, ...). Nothing was signed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignError(pub String);

impl fmt::Display for SignError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SignError {}

/// Why a [`LocalSigner`] could not be set up. No variant ever contains any part of the secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignerSetupError {
    /// Local keys are for the testnet only.
    PublicNetworkRefused,
    /// The key file could not be read.
    Unreadable(String),
    /// The key file can be read by other users of the machine (Unix permissions must be 0600 or 0400).
    TooOpen { mode: u32 },
    /// The file does not hold a valid `S...` secret seed.
    InvalidSecret,
}

impl fmt::Display for SignerSetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SignerSetupError::PublicNetworkRefused => write!(
                f,
                "a local signing key is for the testnet only; the public network needs a managed key (KMS or HSM)"
            ),
            SignerSetupError::Unreadable(e) => write!(f, "the signing key file cannot be read: {e}"),
            SignerSetupError::TooOpen { mode } => write!(
                f,
                "the signing key file is readable by other users (mode {mode:o}); run chmod 600 on it"
            ),
            SignerSetupError::InvalidSecret => {
                write!(f, "the signing key file does not hold a valid S... secret seed")
            }
        }
    }
}

impl std::error::Error for SignerSetupError {}

/// An in-memory ed25519 key, for the testnet. The key is wiped from memory when this is dropped.
pub struct LocalSigner {
    key: SigningKey,
    public: AccountKey,
}

impl LocalSigner {
    /// From an `S...` secret seed.
    pub fn from_secret(secret: &str, network: Network) -> Result<LocalSigner, SignerSetupError> {
        if network != Network::Testnet {
            return Err(SignerSetupError::PublicNetworkRefused);
        }
        let mut seed = decode_secret_seed(secret).map_err(|_| SignerSetupError::InvalidSecret)?;
        let key = SigningKey::from_bytes(&seed);
        seed.zeroize();
        let public = AccountKey(key.verifying_key().to_bytes());
        Ok(LocalSigner { key, public })
    }

    /// From a file holding an `S...` secret seed (surrounding whitespace is ignored). On Unix the file must not be
    /// readable by group or others. A file rather than an environment variable keeps the secret out of process
    /// listings and crash reports.
    pub fn from_file(path: &Path, network: Network) -> Result<LocalSigner, SignerSetupError> {
        if network != Network::Testnet {
            return Err(SignerSetupError::PublicNetworkRefused);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path)
                .map_err(|e| SignerSetupError::Unreadable(e.to_string()))?
                .permissions()
                .mode()
                & 0o777;
            if mode & 0o077 != 0 {
                return Err(SignerSetupError::TooOpen { mode });
            }
        }
        let mut text = std::fs::read_to_string(path)
            .map_err(|e| SignerSetupError::Unreadable(e.to_string()))?;
        let signer = LocalSigner::from_secret(text.trim(), network);
        text.zeroize();
        signer
    }
}

impl fmt::Debug for LocalSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalSigner")
            .field("public", &self.public.to_g())
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl Signer for LocalSigner {
    fn public_key(&self) -> AccountKey {
        self.public
    }

    async fn sign(&self, hash: &[u8; 32]) -> Result<[u8; 64], SignError> {
        Ok(self.key.sign(hash).to_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Throwaway test key (tests/data/generate_payment_vectors.py); it holds nothing on any network.
    const SECRET: &str = "SA3PXIPSNIG2RVLSIREL6KV6PQJTFI7PA4ZBYVP772LORMETDV5FQTNT";
    const PUBLIC: &str = "GCLSLSBBGOP2GTENWWKA2DXRTNWMRBE6KGVV4UNZK7LMIGMCOQ6EJT2J";

    #[test]
    fn the_public_key_comes_from_the_secret() {
        let s = LocalSigner::from_secret(SECRET, Network::Testnet).unwrap();
        assert_eq!(s.public_key().to_g(), PUBLIC);
    }

    #[test]
    fn a_local_key_is_refused_for_the_public_network() {
        assert_eq!(
            LocalSigner::from_secret(SECRET, Network::Public).unwrap_err(),
            SignerSetupError::PublicNetworkRefused
        );
    }

    #[test]
    fn a_bad_secret_is_refused_without_repeating_it() {
        let e = LocalSigner::from_secret(PUBLIC, Network::Testnet).unwrap_err();
        assert_eq!(e, SignerSetupError::InvalidSecret);
        assert!(!e.to_string().contains(PUBLIC));
    }

    #[test]
    fn debug_output_shows_the_public_key_only() {
        let s = LocalSigner::from_secret(SECRET, Network::Testnet).unwrap();
        let shown = format!("{s:?}");
        assert!(shown.contains(PUBLIC));
        assert!(!shown.contains(SECRET));
    }

    #[cfg(unix)]
    #[test]
    fn a_key_file_must_be_private_to_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("eschess-signer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hot.secret");
        std::fs::write(&path, format!("  {SECRET}\n")).unwrap();

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            LocalSigner::from_file(&path, Network::Testnet).unwrap_err(),
            SignerSetupError::TooOpen { mode: 0o644 }
        );

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let s = LocalSigner::from_file(&path, Network::Testnet).unwrap();
        assert_eq!(s.public_key().to_g(), PUBLIC);

        assert!(matches!(
            LocalSigner::from_file(&dir.join("missing"), Network::Testnet),
            Err(SignerSetupError::Unreadable(_))
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
