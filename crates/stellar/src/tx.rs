//! Building and signing an outgoing payment: one `Payment` operation in one transaction.
//!
//! This is the only place eSchess makes a transaction, so it holds the rules that make "did it land?" answerable
//! (ADR 0001):
//!   - **Every transaction expires.** `max_time` is required and must be after `min_time`. Once the network has closed a
//!     ledger past `max_time`, a transaction that has not landed never can, and only then may a new one be built.
//!   - **The hash is known before anything is sent.** [`sign`] returns it with the envelope, so the caller records both
//!     before submitting, and a timeout is resolved by looking the hash up instead of guessing.
//!
//! Everything here is pure except the [`Signer`] call. The output is checked byte for byte against transactions built by
//! the official Python SDK (`tests/data/payment_vectors.json`).

use std::fmt;

use eschess_domain::money::{Amount, AssetId};
use eschess_domain::stellar::{AccountKey, Address};
use sha2::{Digest, Sha256};
use stellar_xdr::{
    AccountId, AlphaNum12, AlphaNum4, Asset, AssetCode12, AssetCode4, DecoratedSignature, Hash,
    Limits, Memo as XdrMemo, MuxedAccount as XdrMuxed, MuxedAccountMed25519, Operation,
    OperationBody, PaymentOp, Preconditions, PublicKey, SequenceNumber, Signature, SignatureHint,
    TimeBounds, TimePoint, Transaction, TransactionEnvelope, TransactionExt, TransactionV1Envelope,
    Uint256, WriteXdr,
};

use crate::signer::{SignError, Signer};

/// The lowest fee the network accepts for one operation, in stroops. A lower fee can only fail.
pub const MIN_FEE_STROOPS: u32 = 100;

/// Which Stellar network a transaction is for. The network is part of what is signed, so a testnet transaction can
/// never be replayed on the public network or the other way round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    Testnet,
    Public,
}

impl Network {
    pub fn passphrase(self) -> &'static str {
        match self {
            Network::Testnet => "Test SDF Network ; September 2015",
            Network::Public => "Public Global Stellar Network ; September 2015",
        }
    }

    /// The network id: SHA-256 of the passphrase.
    pub fn id(self) -> [u8; 32] {
        Sha256::digest(self.passphrase().as_bytes()).into()
    }

    /// Reads `testnet` or `public`.
    pub fn parse(s: &str) -> Option<Network> {
        match s {
            "testnet" => Some(Network::Testnet),
            "public" => Some(Network::Public),
            _ => None,
        }
    }
}

/// The memo on an outgoing payment. Exchanges usually need an id or text memo to know which customer a deposit is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaymentMemo {
    None,
    Id(u64),
    /// At most 28 bytes of UTF-8.
    Text(String),
    Hash([u8; 32]),
    Return([u8; 32]),
}

/// Most bytes a text memo may hold.
pub const MAX_TEXT_MEMO_BYTES: usize = 28;

/// Everything that goes into one outgoing payment transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payment {
    /// The account the money leaves from, which also pays the fee and must sign.
    pub source: AccountKey,
    /// This transaction's sequence number: the source account's current sequence number plus one.
    pub sequence: i64,
    /// The fee in stroops (of XLM), at least [`MIN_FEE_STROOPS`].
    pub fee_stroops: u32,
    /// Unix seconds. Zero means "valid at once".
    pub min_time: u64,
    /// Unix seconds after which the transaction can never be applied. Required.
    pub max_time: u64,
    /// A `G...` account, or an `M...` muxed address (the account is paid and the id travels with the payment).
    pub destination: Address,
    /// How much of which asset. Seven decimals, as on Stellar, so the minor units are stroops.
    pub amount: Amount,
    pub memo: PaymentMemo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxError {
    /// The amount is zero or negative.
    NonPositiveAmount,
    /// The sequence number is zero or negative.
    InvalidSequence,
    /// The fee is below [`MIN_FEE_STROOPS`].
    FeeTooLow,
    /// `max_time` is missing (zero): the transaction would never expire, so a lost submission could never be resolved.
    NoExpiry,
    /// `max_time` is not after `min_time`.
    InvalidTimeBounds,
    /// A text memo longer than [`MAX_TEXT_MEMO_BYTES`].
    MemoTooLong,
    /// The asset's issuer is not a valid account (cannot happen for an [`AssetId`] built through its constructors).
    InvalidAsset,
    /// The signer's key is not the source account's: the network would refuse the transaction.
    WrongSigner,
    Sign(SignError),
    /// The transaction could not be encoded (a bug).
    Encoding(String),
}

impl fmt::Display for TxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TxError::NonPositiveAmount => write!(f, "a payment must be for more than zero"),
            TxError::InvalidSequence => write!(f, "the sequence number must be positive"),
            TxError::FeeTooLow => write!(f, "the fee must be at least {MIN_FEE_STROOPS} stroops"),
            TxError::NoExpiry => write!(f, "every transaction must have a max_time (an expiry)"),
            TxError::InvalidTimeBounds => write!(f, "max_time must be after min_time"),
            TxError::MemoTooLong => {
                write!(f, "a text memo can be at most {MAX_TEXT_MEMO_BYTES} bytes")
            }
            TxError::InvalidAsset => write!(f, "the asset's issuer is not a valid Stellar account"),
            TxError::WrongSigner => {
                write!(
                    f,
                    "the signer's key is not the transaction's source account"
                )
            }
            TxError::Sign(e) => write!(f, "signing failed: {e}"),
            TxError::Encoding(e) => write!(f, "the transaction could not be encoded: {e}"),
        }
    }
}

impl std::error::Error for TxError {}

/// A signed transaction, ready to submit. Record both fields before submitting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedTransaction {
    /// The transaction hash: what Horizon is asked about when the outcome of a submission is unknown.
    pub hash: [u8; 32],
    /// The base64 XDR envelope, which is what is submitted. Re-submitting this exact envelope is always safe: it has
    /// the same hash and sequence number, so the network applies it at most once.
    pub envelope_xdr: String,
}

impl SignedTransaction {
    /// The hash as lower-case hex, as Horizon shows it.
    pub fn hash_hex(&self) -> String {
        hex(&self.hash)
    }
}

impl Payment {
    /// The transaction, checked. Fails rather than build anything that could not be resolved or could only fail.
    pub fn transaction(&self) -> Result<Transaction, TxError> {
        if self.amount.minor() <= 0 {
            return Err(TxError::NonPositiveAmount);
        }
        if self.sequence <= 0 {
            return Err(TxError::InvalidSequence);
        }
        if self.fee_stroops < MIN_FEE_STROOPS {
            return Err(TxError::FeeTooLow);
        }
        if self.max_time == 0 {
            return Err(TxError::NoExpiry);
        }
        if self.max_time <= self.min_time {
            return Err(TxError::InvalidTimeBounds);
        }
        let payment = PaymentOp {
            destination: muxed(&self.destination),
            asset: asset(self.amount.asset())?,
            amount: self.amount.minor(),
        };
        Ok(Transaction {
            source_account: XdrMuxed::Ed25519(Uint256(self.source.0)),
            fee: self.fee_stroops,
            seq_num: SequenceNumber(self.sequence),
            cond: Preconditions::Time(TimeBounds {
                min_time: TimePoint(self.min_time),
                max_time: TimePoint(self.max_time),
            }),
            memo: memo(&self.memo)?,
            operations: vec![Operation {
                source_account: None,
                body: OperationBody::Payment(payment),
            }]
            .try_into()
            .map_err(|_| TxError::Encoding("operations".into()))?,
            ext: TransactionExt::V0,
        })
    }

    /// The transaction hash on `network`, known before signing or sending.
    pub fn hash(&self, network: Network) -> Result<[u8; 32], TxError> {
        self.transaction()?
            .hash(network.id())
            .map_err(|e| TxError::Encoding(e.to_string()))
    }
}

/// Builds, hashes and signs `payment` for `network`.
pub async fn sign(
    payment: &Payment,
    network: Network,
    signer: &dyn Signer,
) -> Result<SignedTransaction, TxError> {
    if signer.public_key() != payment.source {
        return Err(TxError::WrongSigner);
    }
    let tx = payment.transaction()?;
    let hash = tx
        .hash(network.id())
        .map_err(|e| TxError::Encoding(e.to_string()))?;
    let signature = signer.sign(&hash).await.map_err(TxError::Sign)?;
    let hint: [u8; 4] = payment.source.0[28..].try_into().unwrap();
    let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: vec![DecoratedSignature {
            hint: SignatureHint(hint),
            signature: Signature(
                signature
                    .to_vec()
                    .try_into()
                    .map_err(|_| TxError::Encoding("signature".into()))?,
            ),
        }]
        .try_into()
        .map_err(|_| TxError::Encoding("signatures".into()))?,
    });
    let envelope_xdr = envelope
        .to_xdr_base64(Limits::none())
        .map_err(|e| TxError::Encoding(e.to_string()))?;
    Ok(SignedTransaction { hash, envelope_xdr })
}

fn muxed(address: &Address) -> XdrMuxed {
    match address {
        Address::Account(k) => XdrMuxed::Ed25519(Uint256(k.0)),
        Address::Muxed(m) => XdrMuxed::MuxedEd25519(MuxedAccountMed25519 {
            id: m.id,
            ed25519: Uint256(m.key.0),
        }),
    }
}

pub(crate) fn asset(asset: &AssetId) -> Result<Asset, TxError> {
    let Some(issuer) = asset.issuer() else {
        return Ok(Asset::Native);
    };
    let issuer = AccountKey::parse(issuer).map_err(|_| TxError::InvalidAsset)?;
    let issuer = AccountId(PublicKey::PublicKeyTypeEd25519(Uint256(issuer.0)));
    let code = asset.code().as_bytes();
    // Codes of 1 to 4 characters are "alphanum4", 5 to 12 "alphanum12", padded with zero bytes.
    Ok(if code.len() <= 4 {
        let mut c = [0u8; 4];
        c[..code.len()].copy_from_slice(code);
        Asset::CreditAlphanum4(AlphaNum4 {
            asset_code: AssetCode4(c),
            issuer,
        })
    } else {
        let mut c = [0u8; 12];
        c[..code.len()].copy_from_slice(code);
        Asset::CreditAlphanum12(AlphaNum12 {
            asset_code: AssetCode12(c),
            issuer,
        })
    })
}

fn memo(memo: &PaymentMemo) -> Result<XdrMemo, TxError> {
    Ok(match memo {
        PaymentMemo::None => XdrMemo::None,
        PaymentMemo::Id(id) => XdrMemo::Id(*id),
        PaymentMemo::Text(t) => {
            if t.len() > MAX_TEXT_MEMO_BYTES {
                return Err(TxError::MemoTooLong);
            }
            XdrMemo::Text(
                t.as_bytes()
                    .to_vec()
                    .try_into()
                    .map_err(|_| TxError::MemoTooLong)?,
            )
        }
        PaymentMemo::Hash(h) => XdrMemo::Hash(Hash(*h)),
        PaymentMemo::Return(h) => XdrMemo::Return(Hash(*h)),
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = "GCLSLSBBGOP2GTENWWKA2DXRTNWMRBE6KGVV4UNZK7LMIGMCOQ6EJT2J";
    const DEST: &str = "GBUUQEXJEVAN2FLTOEBQ2OUJREAQOAEYYNW5R2X7O7RE4LZ2DNMBUEAV";

    fn payment() -> Payment {
        Payment {
            source: AccountKey::parse(SOURCE).unwrap(),
            sequence: 2,
            fee_stroops: 100,
            min_time: 0,
            max_time: 1_700_000_120,
            destination: Address::parse(DEST).unwrap(),
            amount: Amount::new(10, AssetId::native()).unwrap(),
            memo: PaymentMemo::None,
        }
    }

    #[test]
    fn a_transaction_that_could_never_be_resolved_or_could_only_fail_is_not_built() {
        let p = payment();
        assert!(p.transaction().is_ok());
        let with = |f: fn(&mut Payment)| {
            let mut p = payment();
            f(&mut p);
            p.transaction().unwrap_err()
        };
        assert_eq!(with(|p| p.max_time = 0), TxError::NoExpiry);
        assert_eq!(
            with(|p| p.min_time = p.max_time),
            TxError::InvalidTimeBounds
        );
        assert_eq!(with(|p| p.sequence = 0), TxError::InvalidSequence);
        assert_eq!(with(|p| p.sequence = -5), TxError::InvalidSequence);
        assert_eq!(with(|p| p.fee_stroops = 99), TxError::FeeTooLow);
        assert_eq!(
            with(|p| p.amount = Amount::zero(AssetId::native())),
            TxError::NonPositiveAmount
        );
        assert_eq!(
            with(|p| p.memo = PaymentMemo::Text("x".repeat(29))),
            TxError::MemoTooLong
        );
    }

    #[test]
    fn a_text_memo_of_exactly_28_bytes_fits() {
        let mut p = payment();
        p.memo = PaymentMemo::Text("é".repeat(14)); // 28 bytes, 14 characters
        assert!(p.transaction().is_ok());
    }

    #[test]
    fn the_network_is_part_of_the_hash() {
        let p = payment();
        assert_ne!(p.hash(Network::Testnet), p.hash(Network::Public));
        assert_eq!(Network::parse("testnet"), Some(Network::Testnet));
        assert_eq!(Network::parse("public"), Some(Network::Public));
        assert_eq!(Network::parse("mainnet"), None);
    }
}
