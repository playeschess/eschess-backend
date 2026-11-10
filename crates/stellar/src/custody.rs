//! Keeping users' keys (ADR 0006).
//!
//! Every user has their own Stellar key, and the platform holds it. This module is the only place those keys exist as
//! anything but ciphertext, and the only way to use one is to ask the [`KeyVault`] to open it for a single signature.
//!
//!   - A key is generated from the operating system's random source and **sealed** at once: encrypted with AES-256-GCM
//!     under the vault's master key, with the user's id and address bound in as associated data. A ciphertext copied to
//!     another user's row, or with its address changed, fails to open.
//!   - Opening happens inside [`VaultSigner::sign`]: decrypt, sign, wipe. The key is never returned, logged, or put in an
//!     error message.
//!   - The master key sits behind the [`KeyVault`] trait, the same seam [`crate::signer::Signer`] is for the platform's own
//!     key. [`LocalVault`] reads it from a file and refuses the public network: for real money the master key must live
//!     in a hardware-backed key service and never on the application server.

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use ed25519_dalek::{Signer as _, SigningKey};
use eschess_domain::stellar::AccountKey;
use rand::rngs::OsRng;
use rand::RngCore;
use zeroize::{Zeroize, Zeroizing};

use crate::signer::{SignError, Signer};
use crate::tx::Network;

/// An encrypted secret key, as stored.
#[derive(Clone, PartialEq, Eq)]
pub struct Sealed {
    /// Which master key sealed it, so keys can be moved to a new master key without guessing.
    pub key_version: i16,
    pub nonce: [u8; 12],
    /// The 32-byte secret plus the 16-byte authentication tag.
    pub ciphertext: Vec<u8>,
}

impl fmt::Debug for Sealed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sealed")
            .field("key_version", &self.key_version)
            .finish_non_exhaustive()
    }
}

/// Why a vault could not seal or open. Never contains a key, a master key, or plaintext.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultError {
    /// The sealed key does not open under this context: it was altered, belongs to someone else, or was sealed by a
    /// different master key.
    CannotOpen,
    /// The vault could not be reached or refused (a remote key service, say). Nothing was opened.
    Unavailable(String),
}

impl fmt::Display for VaultError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VaultError::CannotOpen => write!(f, "the sealed key could not be opened"),
            VaultError::Unavailable(m) => write!(f, "the key vault is unavailable: {m}"),
        }
    }
}

impl std::error::Error for VaultError {}

/// Seals and opens users' secret keys.
#[async_trait::async_trait]
pub trait KeyVault: Send + Sync {
    /// Encrypts a 32-byte secret. `context` is bound in: opening needs the same one.
    async fn seal(&self, context: &[u8], secret: &[u8; 32]) -> Result<Sealed, VaultError>;

    /// Decrypts. The caller must use the secret at once and let it drop (it is wiped when dropped).
    async fn open(
        &self,
        context: &[u8],
        sealed: &Sealed,
    ) -> Result<Zeroizing<[u8; 32]>, VaultError>;
}

/// Why a [`LocalVault`] could not be set up. Never contains any part of the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultSetupError {
    /// A master key in a file is for the testnet only.
    PublicNetworkRefused,
    Unreadable(String),
    /// The file can be read by other users of the machine (Unix permissions must be 0600 or 0400).
    TooOpen {
        mode: u32,
    },
    /// The file does not hold 64 hexadecimal characters (32 bytes).
    InvalidKey,
}

impl fmt::Display for VaultSetupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VaultSetupError::PublicNetworkRefused => write!(
                f,
                "a master key in a file is for the testnet only; the public network needs a hardware-backed key service"
            ),
            VaultSetupError::Unreadable(e) => write!(f, "the master key file could not be read: {e}"),
            VaultSetupError::TooOpen { mode } => write!(
                f,
                "the master key file is readable by others (mode {mode:o}); it must be 0600 or 0400"
            ),
            VaultSetupError::InvalidKey => {
                write!(f, "the master key file must hold 64 hexadecimal characters")
            }
        }
    }
}

impl std::error::Error for VaultSetupError {}

/// A master key kept in memory, for the testnet. Wiped when dropped.
pub struct LocalVault {
    key: Zeroizing<[u8; 32]>,
    version: i16,
}

impl LocalVault {
    /// From 32 raw bytes (tests, and callers that obtained the key themselves).
    pub fn from_bytes(
        key: [u8; 32],
        version: i16,
        network: Network,
    ) -> Result<LocalVault, VaultSetupError> {
        if network != Network::Testnet {
            return Err(VaultSetupError::PublicNetworkRefused);
        }
        Ok(LocalVault {
            key: Zeroizing::new(key),
            version,
        })
    }

    /// From a file holding the master key as 64 hexadecimal characters. On Unix the file must not be readable by group
    /// or others.
    pub fn from_file(
        path: &Path,
        version: i16,
        network: Network,
    ) -> Result<LocalVault, VaultSetupError> {
        if network != Network::Testnet {
            return Err(VaultSetupError::PublicNetworkRefused);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path)
                .map_err(|e| VaultSetupError::Unreadable(e.to_string()))?
                .permissions()
                .mode()
                & 0o777;
            if mode & 0o077 != 0 {
                return Err(VaultSetupError::TooOpen { mode });
            }
        }
        let mut text = std::fs::read_to_string(path)
            .map_err(|e| VaultSetupError::Unreadable(e.to_string()))?;
        let mut bytes = hex::decode(text.trim()).map_err(|_| VaultSetupError::InvalidKey);
        text.zeroize();
        let key: [u8; 32] = match &bytes {
            Ok(b) if b.len() == 32 => b.as_slice().try_into().unwrap(),
            _ => {
                if let Ok(b) = bytes.as_mut() {
                    b.zeroize();
                }
                return Err(VaultSetupError::InvalidKey);
            }
        };
        if let Ok(b) = bytes.as_mut() {
            b.zeroize();
        }
        Ok(LocalVault {
            key: Zeroizing::new(key),
            version,
        })
    }

    fn cipher(&self) -> Aes256Gcm {
        Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&*self.key))
    }
}

impl fmt::Debug for LocalVault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalVault")
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl KeyVault for LocalVault {
    async fn seal(&self, context: &[u8], secret: &[u8; 32]) -> Result<Sealed, VaultError> {
        let mut nonce = [0u8; 12];
        OsRng.fill_bytes(&mut nonce);
        let ciphertext = self
            .cipher()
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: secret,
                    aad: context,
                },
            )
            .map_err(|_| VaultError::Unavailable("encryption failed".into()))?;
        Ok(Sealed {
            key_version: self.version,
            nonce,
            ciphertext,
        })
    }

    async fn open(
        &self,
        context: &[u8],
        sealed: &Sealed,
    ) -> Result<Zeroizing<[u8; 32]>, VaultError> {
        if sealed.key_version != self.version {
            return Err(VaultError::CannotOpen);
        }
        let mut plain = self
            .cipher()
            .decrypt(
                Nonce::from_slice(&sealed.nonce),
                Payload {
                    msg: &sealed.ciphertext,
                    aad: context,
                },
            )
            .map_err(|_| VaultError::CannotOpen)?;
        let out: Result<[u8; 32], _> = plain.as_slice().try_into();
        plain.zeroize();
        out.map(Zeroizing::new).map_err(|_| VaultError::CannotOpen)
    }
}

/// What a sealed key is bound to: the user and the address. Opening with any other context fails.
pub fn context(user: &str, address: &AccountKey) -> Vec<u8> {
    format!("eschess-user-wallet:v1:{user}:{}", address.to_g()).into_bytes()
}

/// A new key, already sealed. Only the address and the ciphertext leave this function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewWallet {
    pub address: AccountKey,
    pub sealed: Sealed,
}

/// Generates a key from the operating system's random source and seals it for `user` at once.
pub async fn generate(vault: &dyn KeyVault, user: &str) -> Result<NewWallet, VaultError> {
    let mut seed = Zeroizing::new([0u8; 32]);
    OsRng.fill_bytes(&mut *seed);
    let key = SigningKey::from_bytes(&seed);
    let address = AccountKey(key.verifying_key().to_bytes());
    let sealed = vault.seal(&context(user, &address), &seed).await?;
    Ok(NewWallet { address, sealed })
}

/// Signs as one user, opening their key only for the length of each signature.
pub struct VaultSigner {
    vault: Arc<dyn KeyVault>,
    context: Vec<u8>,
    sealed: Sealed,
    public: AccountKey,
}

impl VaultSigner {
    pub fn new(
        vault: Arc<dyn KeyVault>,
        user: &str,
        address: AccountKey,
        sealed: Sealed,
    ) -> VaultSigner {
        VaultSigner {
            vault,
            context: context(user, &address),
            sealed,
            public: address,
        }
    }
}

impl fmt::Debug for VaultSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VaultSigner")
            .field("public", &self.public.to_g())
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl Signer for VaultSigner {
    fn public_key(&self) -> AccountKey {
        self.public
    }

    async fn sign(&self, hash: &[u8; 32]) -> Result<[u8; 64], SignError> {
        let seed = self
            .vault
            .open(&self.context, &self.sealed)
            .await
            .map_err(|e| SignError(e.to_string()))?;
        let key = SigningKey::from_bytes(&seed);
        // The key must be the one this address belongs to, or signing would produce a signature the network refuses.
        if key.verifying_key().to_bytes() != self.public.0 {
            return Err(SignError(
                "the stored key does not match the address".into(),
            ));
        }
        Ok(key.sign(hash).to_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Verifier as _;

    fn vault(seed: u8) -> LocalVault {
        LocalVault::from_bytes([seed; 32], 1, Network::Testnet).unwrap()
    }

    #[tokio::test]
    async fn a_generated_key_is_sealed_and_signs_for_its_address_only() {
        let v: Arc<dyn KeyVault> = Arc::new(vault(7));
        let w = generate(&*v, "user-1").await.unwrap();
        // 32 bytes of key and a 16-byte tag, and nothing that looks like the key.
        assert_eq!(w.sealed.ciphertext.len(), 48);
        let signer = VaultSigner::new(v.clone(), "user-1", w.address, w.sealed.clone());
        let hash = [9u8; 32];
        let sig = signer.sign(&hash).await.unwrap();
        let key = ed25519_dalek::VerifyingKey::from_bytes(&w.address.0).unwrap();
        key.verify(&hash, &ed25519_dalek::Signature::from_bytes(&sig))
            .expect("the signature verifies under the wallet's address");
        assert_eq!(signer.public_key(), w.address);
    }

    #[tokio::test]
    async fn two_wallets_differ_and_sealing_the_same_secret_twice_gives_different_ciphertext() {
        let v = vault(7);
        let a = generate(&v, "u").await.unwrap();
        let b = generate(&v, "u").await.unwrap();
        assert_ne!(a.address, b.address);
        let secret = [5u8; 32];
        let c1 = v.seal(b"ctx", &secret).await.unwrap();
        let c2 = v.seal(b"ctx", &secret).await.unwrap();
        assert_ne!(c1.nonce, c2.nonce);
        assert_ne!(c1.ciphertext, c2.ciphertext);
    }

    #[tokio::test]
    async fn a_sealed_key_will_not_open_for_another_user_another_address_a_tampered_copy_or_another_master_key(
    ) {
        let v = vault(7);
        let w = generate(&v, "user-1").await.unwrap();
        let ok = context("user-1", &w.address);
        assert!(v.open(&ok, &w.sealed).await.is_ok());
        // Another user's row.
        let other_user = context("user-2", &w.address);
        assert_eq!(
            v.open(&other_user, &w.sealed).await.unwrap_err(),
            VaultError::CannotOpen
        );
        // Another address.
        let other_addr = context("user-1", &AccountKey([1; 32]));
        assert_eq!(
            v.open(&other_addr, &w.sealed).await.unwrap_err(),
            VaultError::CannotOpen
        );
        // A flipped bit.
        let mut tampered = w.sealed.clone();
        tampered.ciphertext[0] ^= 1;
        assert_eq!(
            v.open(&ok, &tampered).await.unwrap_err(),
            VaultError::CannotOpen
        );
        // Another master key, and a different key version.
        assert_eq!(
            vault(8).open(&ok, &w.sealed).await.unwrap_err(),
            VaultError::CannotOpen
        );
        let mut newer = w.sealed.clone();
        newer.key_version = 2;
        assert_eq!(
            v.open(&ok, &newer).await.unwrap_err(),
            VaultError::CannotOpen
        );
    }

    #[tokio::test]
    async fn a_signer_given_the_wrong_address_refuses_to_sign() {
        let v: Arc<dyn KeyVault> = Arc::new(vault(7));
        let w = generate(&*v, "user-1").await.unwrap();
        // Same ciphertext, claimed address changed: the context no longer matches, so it cannot even open.
        let bad = VaultSigner::new(v.clone(), "user-1", AccountKey([2; 32]), w.sealed.clone());
        assert!(bad.sign(&[0; 32]).await.is_err());
    }

    #[test]
    fn nothing_printed_about_a_wallet_contains_a_key() {
        let sealed = Sealed {
            key_version: 1,
            nonce: [0xab; 12],
            ciphertext: vec![0xcd; 48],
        };
        // Bytes would show as decimals (171, 205, 7) in a derived Debug; only the version may appear.
        let text = format!("{sealed:?} {:?}", vault(7));
        assert!(!text.contains("171") && !text.contains("205") && !text.contains("[7"));
        assert!(text.contains("key_version") && text.contains("version"));
    }

    #[test]
    fn a_master_key_file_must_be_private_valid_and_for_the_testnet() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("eschess-vault-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("master");
        let write = |text: &str, mode: u32| {
            std::fs::write(&path, text).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        let good = "11".repeat(32);
        write(&format!("{good}\n"), 0o600);
        assert!(LocalVault::from_file(&path, 1, Network::Testnet).is_ok());
        assert_eq!(
            LocalVault::from_file(&path, 1, Network::Public).unwrap_err(),
            VaultSetupError::PublicNetworkRefused
        );
        write(&good, 0o644);
        assert!(matches!(
            LocalVault::from_file(&path, 1, Network::Testnet).unwrap_err(),
            VaultSetupError::TooOpen { .. }
        ));
        write("not hex at all", 0o600);
        assert_eq!(
            LocalVault::from_file(&path, 1, Network::Testnet).unwrap_err(),
            VaultSetupError::InvalidKey
        );
        write(&"11".repeat(31), 0o600);
        assert_eq!(
            LocalVault::from_file(&path, 1, Network::Testnet).unwrap_err(),
            VaultSetupError::InvalidKey,
            "31 bytes is too short"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
