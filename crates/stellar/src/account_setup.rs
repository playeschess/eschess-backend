//! Setting up a user's own Stellar account on the network (ADR 0006): create it, fund it, and let it hold USDC.
//!
//! A new key is only an address until someone pays it. Two things turn it into an account that can receive USDC:
//!   1. **Create and fund** it with enough XLM for the account reserve, the USDC trustline's reserve, and fees. The
//!      platform's funding account pays, and signs.
//!   2. **Add the USDC trustline.** Only the account itself can do that, so the user's key signs, through the vault.
//!
//! Both go in **one transaction** (two operations, two signatures) so the account is never left half set up. If the
//! account already exists but lacks the trustline (a payment of XLM reached it first, say), only the trustline is added,
//! paid for by the account's own XLM.
//!
//! The same rules as every other transaction: it expires (`max_time`), its hash is known before it is sent, and a
//! submission that did not land is looked up, never rebuilt blindly.

use eschess_domain::money::AssetId;
use eschess_domain::stellar::AccountKey;
use stellar_xdr::{
    AccountId, AlphaNum12, AlphaNum4, AssetCode12, AssetCode4, ChangeTrustAsset, ChangeTrustOp,
    CreateAccountOp, DecoratedSignature, Limits, Memo as XdrMemo, MuxedAccount as XdrMuxed,
    Operation, OperationBody, Preconditions, PublicKey, SequenceNumber, Signature, SignatureHint,
    TimeBounds, TimePoint, Transaction, TransactionEnvelope, TransactionExt, TransactionV1Envelope,
    Uint256, WriteXdr,
};

use crate::signer::Signer;
use crate::tx::{Network, SignedTransaction};

/// Stroops in one XLM.
pub const STROOPS_PER_XLM: i64 = 10_000_000;

/// The least that can start a new account that will hold one trustline: two base reserves for the account (0.5 XLM
/// each) and one for the trustline. The network can change its reserve; check it before launch.
pub const MIN_STARTING_BALANCE_STROOPS: i64 = 3 * STROOPS_PER_XLM / 2;

/// The fee offered per operation, in stroops. The network's minimum is 100; offering more costs nothing unless the
/// network is busy, and a transaction that is too cheap is the one that sits unconfirmed.
pub const FEE_PER_OP_STROOPS: u32 = 1_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetupError {
    /// The starting balance is below [`MIN_STARTING_BALANCE_STROOPS`]: the account would not be created.
    StartingBalanceTooLow,
    InvalidSequence,
    NoExpiry,
    /// The asset is native or has an invalid issuer: there is nothing to trust.
    NotAnIssuedAsset,
    /// A signer's key is not one of the accounts the transaction needs.
    WrongSigner,
    Sign(String),
    Encoding(String),
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetupError::StartingBalanceTooLow => write!(
                f,
                "the starting balance must be at least {} XLM",
                MIN_STARTING_BALANCE_STROOPS as f64 / STROOPS_PER_XLM as f64
            ),
            SetupError::InvalidSequence => write!(f, "the sequence number must be positive"),
            SetupError::NoExpiry => write!(f, "every transaction must have a max_time (an expiry)"),
            SetupError::NotAnIssuedAsset => write!(f, "only an issued asset can have a trustline"),
            SetupError::WrongSigner => {
                write!(f, "a signer's key is not one of the accounts involved")
            }
            SetupError::Sign(m) => write!(f, "signing failed: {m}"),
            SetupError::Encoding(m) => write!(f, "could not encode the transaction: {m}"),
        }
    }
}

impl std::error::Error for SetupError {}

fn account_id(k: &AccountKey) -> AccountId {
    AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(k.0)))
}

fn muxed(k: &AccountKey) -> XdrMuxed {
    XdrMuxed::Ed25519(Uint256(k.0))
}

fn trust_line(asset: &AssetId) -> Result<ChangeTrustAsset, SetupError> {
    let issuer = asset.issuer().ok_or(SetupError::NotAnIssuedAsset)?;
    let issuer = AccountKey::parse(issuer).map_err(|_| SetupError::NotAnIssuedAsset)?;
    let issuer = account_id(&issuer);
    let code = asset.code().as_bytes();
    Ok(if code.len() <= 4 {
        let mut c = [0u8; 4];
        c[..code.len()].copy_from_slice(code);
        ChangeTrustAsset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(c),
            issuer,
        })
    } else {
        let mut c = [0u8; 12];
        c[..code.len()].copy_from_slice(code);
        ChangeTrustAsset::CreditAlphanum12(AlphaNum12 {
            asset_code: AssetCode12(c),
            issuer,
        })
    })
}

fn transaction(
    source: &AccountKey,
    sequence: i64,
    max_time: u64,
    operations: Vec<Operation>,
) -> Result<Transaction, SetupError> {
    if sequence <= 0 {
        return Err(SetupError::InvalidSequence);
    }
    if max_time == 0 {
        return Err(SetupError::NoExpiry);
    }
    let fee = FEE_PER_OP_STROOPS * operations.len() as u32;
    Ok(Transaction {
        source_account: muxed(source),
        fee,
        seq_num: SequenceNumber(sequence),
        cond: Preconditions::Time(TimeBounds {
            min_time: TimePoint(0),
            max_time: TimePoint(max_time),
        }),
        memo: XdrMemo::None,
        operations: operations
            .try_into()
            .map_err(|_| SetupError::Encoding("operations".into()))?,
        ext: TransactionExt::V0,
    })
}

fn trust_op(user: &AccountKey, asset: &AssetId) -> Result<Operation, SetupError> {
    Ok(Operation {
        // The trustline belongs to the user's account, so the operation is the user's, not the funder's.
        source_account: Some(muxed(user)),
        body: OperationBody::ChangeTrust(ChangeTrustOp {
            line: trust_line(asset)?,
            limit: i64::MAX,
        }),
    })
}

/// Creates `user` with `starting_balance_stroops` of XLM and gives it a trustline for `asset`, in one transaction whose
/// source is the funding account. Needs the signatures of the funder and of the user.
pub fn create_and_trust(
    funder: &AccountKey,
    funder_sequence: i64,
    user: &AccountKey,
    starting_balance_stroops: i64,
    asset: &AssetId,
    max_time: u64,
) -> Result<Transaction, SetupError> {
    if starting_balance_stroops < MIN_STARTING_BALANCE_STROOPS {
        return Err(SetupError::StartingBalanceTooLow);
    }
    transaction(
        funder,
        funder_sequence,
        max_time,
        vec![
            Operation {
                source_account: None,
                body: OperationBody::CreateAccount(CreateAccountOp {
                    destination: account_id(user),
                    starting_balance: starting_balance_stroops,
                }),
            },
            trust_op(user, asset)?,
        ],
    )
}

/// Adds a trustline for `asset` to an account that already exists. Source and only signer: the user.
pub fn trust_only(
    user: &AccountKey,
    user_sequence: i64,
    asset: &AssetId,
    max_time: u64,
) -> Result<Transaction, SetupError> {
    transaction(
        user,
        user_sequence,
        max_time,
        vec![Operation {
            source_account: None,
            body: trust_op(user, asset)?.body,
        }],
    )
}

/// Signs `tx` with every signer given (each must be one of the accounts the transaction involves: its source, or the
/// source of an operation) and returns it with its hash, ready to submit. Record both before submitting.
pub async fn sign(
    tx: Transaction,
    network: Network,
    signers: &[&dyn Signer],
) -> Result<SignedTransaction, SetupError> {
    let involved = |k: &AccountKey| {
        let is = |m: &XdrMuxed| matches!(m, XdrMuxed::Ed25519(u) if u.0 == k.0);
        is(&tx.source_account)
            || tx
                .operations
                .iter()
                .any(|op| op.source_account.as_ref().is_some_and(is))
    };
    let hash = tx
        .hash(network.id())
        .map_err(|e| SetupError::Encoding(e.to_string()))?;
    let mut signatures = Vec::new();
    for s in signers {
        let key = s.public_key();
        if !involved(&key) {
            return Err(SetupError::WrongSigner);
        }
        let sig = s.sign(&hash).await.map_err(|e| SetupError::Sign(e.0))?;
        signatures.push(DecoratedSignature {
            hint: SignatureHint(key.0[28..].try_into().unwrap()),
            signature: Signature(
                sig.to_vec()
                    .try_into()
                    .map_err(|_| SetupError::Encoding("signature".into()))?,
            ),
        });
    }
    let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: signatures
            .try_into()
            .map_err(|_| SetupError::Encoding("signatures".into()))?,
    });
    let envelope_xdr = envelope
        .to_xdr_base64(Limits::none())
        .map_err(|e| SetupError::Encoding(e.to_string()))?;
    Ok(SignedTransaction { hash, envelope_xdr })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer as _, SigningKey, Verifier as _};
    use stellar_xdr::ReadXdr;

    struct Key(SigningKey);
    impl Key {
        fn new(seed: u8) -> Key {
            Key(SigningKey::from_bytes(&[seed; 32]))
        }
        fn account(&self) -> AccountKey {
            AccountKey(self.0.verifying_key().to_bytes())
        }
    }
    #[async_trait::async_trait]
    impl Signer for Key {
        fn public_key(&self) -> AccountKey {
            self.account()
        }
        async fn sign(&self, hash: &[u8; 32]) -> Result<[u8; 64], crate::signer::SignError> {
            Ok(self.0.sign(hash).to_bytes())
        }
    }

    fn usdc() -> AssetId {
        AssetId::credit(
            "USDC",
            "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5",
        )
        .unwrap()
    }

    #[tokio::test]
    async fn a_new_account_is_created_funded_and_given_a_trustline_in_one_signed_transaction() {
        let (funder, user) = (Key::new(1), Key::new(2));
        let tx = create_and_trust(
            &funder.account(),
            10,
            &user.account(),
            2 * STROOPS_PER_XLM,
            &usdc(),
            1_800_000_120,
        )
        .unwrap();
        assert_eq!(tx.fee, 2_000, "1000 stroops for each of the two operations");
        assert_eq!(tx.seq_num.0, 10);
        let ops: Vec<_> = tx.operations.iter().collect();
        assert!(
            matches!(&ops[0].body, OperationBody::CreateAccount(c) if c.starting_balance == 20_000_000)
        );
        assert!(
            ops[0].source_account.is_none(),
            "the funder pays for the account"
        );
        assert!(matches!(&ops[1].body, OperationBody::ChangeTrust(c) if c.limit == i64::MAX));
        assert_eq!(
            ops[1].source_account,
            Some(XdrMuxed::Ed25519(Uint256(user.account().0))),
            "the trustline is the user's own"
        );

        let signed = sign(tx, Network::Testnet, &[&funder, &user]).await.unwrap();
        let TransactionEnvelope::Tx(env) =
            TransactionEnvelope::from_xdr_base64(&signed.envelope_xdr, Limits::none()).unwrap()
        else {
            panic!()
        };
        assert_eq!(env.signatures.len(), 2);
        for (sig, key) in env.signatures.iter().zip([&funder, &user]) {
            let s =
                ed25519_dalek::Signature::from_bytes(&sig.signature.0.to_vec().try_into().unwrap());
            key.0
                .verifying_key()
                .verify(&signed.hash, &s)
                .expect("both signatures are over the reported hash");
        }
    }

    #[tokio::test]
    async fn the_trustline_alone_is_the_users_own_transaction() {
        let user = Key::new(2);
        let tx = trust_only(&user.account(), 5, &usdc(), 100).unwrap();
        assert_eq!(tx.operations.len(), 1);
        assert_eq!(tx.fee, 1_000);
        let signed = sign(tx, Network::Testnet, &[&user]).await.unwrap();
        assert_eq!(signed.hash_hex().len(), 64);
    }

    #[tokio::test]
    async fn nothing_is_built_that_could_only_fail_and_nobody_unrelated_may_sign() {
        let (funder, user, stranger) = (Key::new(1), Key::new(2), Key::new(3));
        let ok = |bal, seq, max| {
            create_and_trust(&funder.account(), seq, &user.account(), bal, &usdc(), max)
        };
        assert_eq!(
            ok(14_999_999, 1, 100).unwrap_err(),
            SetupError::StartingBalanceTooLow
        );
        assert!(ok(15_000_000, 1, 100).is_ok(), "1.5 XLM is the least");
        assert_eq!(
            ok(20_000_000, 0, 100).unwrap_err(),
            SetupError::InvalidSequence
        );
        assert_eq!(ok(20_000_000, 1, 0).unwrap_err(), SetupError::NoExpiry);
        assert_eq!(
            create_and_trust(
                &funder.account(),
                1,
                &user.account(),
                20_000_000,
                &AssetId::native(),
                100
            )
            .unwrap_err(),
            SetupError::NotAnIssuedAsset
        );
        let tx = ok(20_000_000, 1, 100).unwrap();
        assert_eq!(
            sign(tx, Network::Testnet, &[&funder, &stranger])
                .await
                .unwrap_err(),
            SetupError::WrongSigner
        );
    }

    #[test]
    fn the_network_is_part_of_what_is_signed() {
        let (funder, user) = (Key::new(1), Key::new(2));
        let tx = create_and_trust(
            &funder.account(),
            1,
            &user.account(),
            20_000_000,
            &usdc(),
            100,
        )
        .unwrap();
        assert_ne!(
            tx.hash(Network::Testnet.id()).unwrap(),
            tx.hash(Network::Public.id()).unwrap()
        );
    }
}
