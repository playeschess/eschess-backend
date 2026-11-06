//! The Stellar network: reading payments that arrive, and building and signing payments that leave ([`tx`],
//! [`signer`]).
//!
//! eSchess keeps all customer funds in one platform wallet (the "hot wallet"). For deposits this crate answers *which
//! payments have arrived at that wallet since I last looked?* It knows nothing about players or the database; it
//! turns the network's JSON into plain [`IncomingPayment`]s and says clearly when a record is something it cannot
//! understand, so the caller can set it aside for a human instead of losing it. For withdrawals it builds a payment
//! transaction, gives its hash before anything is sent, and has a [`signer::Signer`] sign it.
//!
//! Where the data comes from is behind the [`Chain`] trait. [`horizon::HorizonChain`] reads a Horizon server (the
//! testnet one for now); tests use a fake. Horizon is documented as eventually giving way to other APIs, and the trait
//! is what lets that change without touching the rest of the platform.

pub mod deposit;
pub mod horizon;
pub mod signer;
pub mod tx;

use std::fmt;

use eschess_domain::money::{Amount, AssetId};

/// The memo attached to a Stellar transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Memo {
    None,
    Text(String),
    /// A 64-bit number. eSchess deposit instructions ask senders to use this when they cannot send to an `M...` address.
    Id(u64),
    Hash(String),
    Return(String),
}

/// A payment that arrived at the platform wallet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingPayment {
    /// Horizon's id for this operation: unique and permanent, so it is what makes crediting happen only once.
    pub op_id: String,
    pub tx_hash: String,
    pub asset: AssetId,
    pub amount: Amount,
    pub from: String,
    /// The `G...` account that was paid (always the platform wallet, even when the sender used an `M...` address).
    pub to: String,
    /// The id of the `M...` address the sender used, when the network reports one.
    pub mux_id: Option<u64>,
    pub memo: Memo,
    pub created_at: String,
}

/// One record read from the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    /// A successful payment into the account we watch.
    Incoming(Box<IncomingPayment>),
    /// Something that is not an incoming payment (an outgoing one, an account creation, a failed transaction, ...).
    Ignored,
    /// It looks like money for us but could not be understood. Never dropped silently: the caller records it.
    Malformed { op_id: String, reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub records: Vec<Record>,
    /// Where to continue from: the position of the last record read, whatever it was. `None` when nothing was read.
    pub next_cursor: Option<String>,
}

#[derive(Debug)]
pub enum ChainError {
    /// The network could not be reached or answered with an error.
    Unavailable(String),
    /// The network answered with something that is not the expected shape at all.
    Unexpected(String),
}

impl fmt::Display for ChainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChainError::Unavailable(m) => write!(f, "the Stellar network is unavailable: {m}"),
            ChainError::Unexpected(m) => {
                write!(f, "unexpected answer from the Stellar network: {m}")
            }
        }
    }
}

impl std::error::Error for ChainError {}

#[async_trait::async_trait]
pub trait Chain: Send + Sync {
    /// Up to `limit` records for `account` after `cursor`, oldest first. An account that does not exist yet has no records.
    async fn payments(
        &self,
        account: &str,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page, ChainError>;

    /// The account's current state, or `None` if it does not exist on the network (it has never been funded).
    async fn account(&self, account: &str) -> Result<Option<AccountState>, ChainError>;

    /// Submits a signed envelope (base64 XDR). The answer is only a hint: a timeout or an error does **not** mean the
    /// transaction will not land, and acceptance is confirmed by [`Chain::transaction`]. Submitting the same envelope
    /// again is always safe (same hash, same sequence number: it is applied at most once).
    async fn submit(&self, envelope_xdr: &str) -> Result<Submitted, ChainError>;

    /// What happened to a transaction, by hash, or `None` if it is not in any ledger the server has seen.
    async fn transaction(&self, hash: &str) -> Result<Option<TxOutcome>, ChainError>;

    /// The close time (unix seconds) of the latest ledger the server has seen. Once it is past a transaction's
    /// `max_time`, a transaction not found by [`Chain::transaction`] can never land.
    async fn latest_ledger_close_time(&self) -> Result<i64, ChainError>;
}

/// What the network said when a transaction was submitted. A hint only (see [`Chain::submit`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Submitted {
    /// Accepted (and, with Horizon, usually already in a ledger).
    Accepted,
    /// Refused or failed, with the network's result code. It may still have been included as a failed transaction.
    Refused(String),
}

/// A transaction that is in a ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxOutcome {
    /// It paid. `false` means it is in the ledger but did nothing except charge its fee.
    pub successful: bool,
    /// What the fee really was, in stroops.
    pub fee_charged: i64,
    /// The result, such as `tx_success` or `tx_failed: payment_no_trust`.
    pub result_code: String,
}

/// What eSchess needs to know about an account on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountState {
    /// The last sequence number the account used. Its next transaction must use this plus one.
    pub sequence: i64,
    /// The credit assets it holds a trustline for and is authorized to receive. A payment of any other credit asset to
    /// it fails on chain (and still costs a fee). Every account can receive XLM.
    pub can_receive: Vec<AssetId>,
}

impl AccountState {
    pub fn can_receive(&self, asset: &AssetId) -> bool {
        asset.is_native() || self.can_receive.contains(asset)
    }
}
