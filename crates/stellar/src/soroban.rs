//! Calling a Soroban smart contract: building, simulating, signing and sending one `InvokeHostFunction` transaction.
//!
//! The same rules as for payments (ADR 0001) apply, because this is money too:
//!   - **Simulate first.** The network's simulation says whether the call would succeed and what resources it needs; a
//!     call that would fail is never signed.
//!   - **Every transaction expires** (`max_time`), so "did it land?" can be answered by looking the hash up.
//!   - **The hash is known before anything is sent.** [`prepare`] returns it with the envelope; the caller records both
//!     before [`RpcSend::send`]. An unresolved submission is looked up by hash, never rebuilt blindly.
//!
//! Only calls where the transaction's own source account is the account that must authorize them are supported (the
//! escrow's admin calls are signed by the admin, a deposit by the depositing player). A simulation that asks for
//! authorization from any other address is refused rather than sent half-signed.

use std::fmt;

use eschess_domain::stellar::{AccountKey, ContractId};
use sha2::{Digest, Sha256};
use stellar_xdr::{
    ContractId as XdrContractId, DecoratedSignature, Hash, HostFunction, Int128Parts,
    InvokeContractArgs, InvokeHostFunctionOp, Limits, Memo as XdrMemo, MuxedAccount as XdrMuxed,
    Operation, OperationBody, Preconditions, ReadXdr, ScAddress, ScBytes, ScSymbol, ScVal, ScVec,
    SequenceNumber, Signature, SignatureHint, SorobanAuthorizationEntry, SorobanCredentials,
    SorobanTransactionData, StringM, TimeBounds, TimePoint, Transaction, TransactionEnvelope,
    TransactionExt, TransactionV1Envelope, Uint256, VecM, WriteXdr,
};

use crate::signer::Signer;
use crate::tx::{Network, SignedTransaction, MIN_FEE_STROOPS};

/// How much extra resource fee to offer on top of the simulation's minimum: simulations are estimates, and a
/// transaction that runs out of fee fails after being included (and still costs the fee).
pub const RESOURCE_FEE_MARGIN_PERCENT: i64 = 15;

/// One call of one function of one contract.
#[derive(Debug, Clone, PartialEq)]
pub struct ContractCall {
    pub contract: ContractId,
    pub function: String,
    pub args: Vec<ScVal>,
}

/// Building ScVal arguments.
pub mod val {
    use super::*;

    pub fn account(key: &AccountKey) -> ScVal {
        ScVal::Address(ScAddress::Account(stellar_xdr::AccountId(
            stellar_xdr::PublicKey::PublicKeyTypeEd25519(Uint256(key.0)),
        )))
    }

    pub fn contract(id: &ContractId) -> ScVal {
        ScVal::Address(ScAddress::Contract(XdrContractId(Hash(id.0))))
    }

    /// A `BytesN<N>` (or `Bytes`) argument.
    pub fn bytes(b: &[u8]) -> ScVal {
        ScVal::Bytes(ScBytes(b.to_vec().try_into().expect("short enough")))
    }

    pub fn i128(v: i128) -> ScVal {
        ScVal::I128(Int128Parts {
            hi: (v >> 64) as i64,
            lo: v as u64,
        })
    }

    pub fn u64(v: u64) -> ScVal {
        ScVal::U64(v)
    }

    /// A unit variant of a `#[contracttype]` enum, such as `Outcome::WinA`: a vector holding the variant's name.
    pub fn unit_variant(name: &str) -> ScVal {
        let sym = ScVal::Symbol(ScSymbol(
            StringM::<32>::try_from(name.as_bytes().to_vec()).expect("a short name"),
        ));
        ScVal::Vec(Some(ScVec(vec![sym].try_into().expect("one element"))))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SorobanError {
    /// The call would fail on the network (the simulation said so). Nothing was signed.
    Simulation(String),
    /// The simulation wants an address other than the source account to authorize the call.
    NeedsOtherAuthorization,
    /// The network or the server could not be reached or answered with something unexpected.
    Rpc(String),
    /// The transaction could not be built or encoded (a bug).
    Build(String),
    /// The signer refused or failed.
    Sign(String),
    /// The signer's key is not the source account.
    WrongSigner,
}

impl fmt::Display for SorobanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SorobanError::Simulation(m) => write!(f, "the call would fail: {m}"),
            SorobanError::NeedsOtherAuthorization => {
                write!(f, "the call needs another account's authorization")
            }
            SorobanError::Rpc(m) => write!(f, "Soroban RPC: {m}"),
            SorobanError::Build(m) => write!(f, "could not build the transaction: {m}"),
            SorobanError::Sign(m) => write!(f, "signing failed: {m}"),
            SorobanError::WrongSigner => write!(f, "the signer is not the source account"),
        }
    }
}

impl std::error::Error for SorobanError {}

/// What a simulation said.
#[derive(Debug, Clone, PartialEq)]
pub struct Simulation {
    pub transaction_data: SorobanTransactionData,
    pub min_resource_fee: i64,
    pub auth: Vec<SorobanAuthorizationEntry>,
    /// What the function returned (for a read-only call, the answer).
    pub return_value: Option<ScVal>,
}

/// The network's answer to a submission. A hint only: acceptance is confirmed by [`RpcSend::transaction`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendStatus {
    Pending,
    /// The same transaction was already submitted.
    Duplicate,
    TryAgainLater,
    Error(String),
}

/// A transaction that is in a ledger.
#[derive(Debug, Clone, PartialEq)]
pub enum SorobanOutcome {
    Success {
        return_value: Option<ScVal>,
    },
    /// In a ledger but failed; the fee was still charged.
    Failed(String),
}

/// Simulating a call. Separate from sending so that reading contract state needs no signing key.
#[async_trait::async_trait]
pub trait RpcSimulate: Send + Sync {
    async fn simulate(&self, envelope_xdr: &str) -> Result<Simulation, SorobanError>;
}

#[async_trait::async_trait]
pub trait RpcSend: RpcSimulate {
    /// Submits a signed envelope. Submitting the same envelope again is safe (same hash and sequence number).
    async fn send(&self, envelope_xdr: &str) -> Result<SendStatus, SorobanError>;

    /// What happened to a transaction, by hash, or `None` if no ledger the server has seen contains it.
    async fn transaction(&self, hash_hex: &str) -> Result<Option<SorobanOutcome>, SorobanError>;
}

/// A call turned into a signed transaction, ready to send. Record `signed` before sending it.
#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    pub signed: SignedTransaction,
    /// What the simulation said the function returns.
    pub simulated_return: Option<ScVal>,
    /// The fee offered, in stroops.
    pub fee_stroops: u32,
    /// The transaction can no longer land after this time (unix seconds).
    pub max_time: u64,
}

fn xdr_err<E: fmt::Display>(e: E) -> SorobanError {
    SorobanError::Build(e.to_string())
}

fn transaction_for(
    source: &AccountKey,
    sequence: i64,
    max_time: u64,
    call: &ContractCall,
    fee: u32,
    auth: Vec<SorobanAuthorizationEntry>,
    ext: TransactionExt,
) -> Result<Transaction, SorobanError> {
    let function =
        ScSymbol(StringM::<32>::try_from(call.function.as_bytes().to_vec()).map_err(xdr_err)?);
    let op = InvokeHostFunctionOp {
        host_function: HostFunction::InvokeContract(InvokeContractArgs {
            contract_address: ScAddress::Contract(XdrContractId(Hash(call.contract.0))),
            function_name: function,
            args: call.args.clone().try_into().map_err(xdr_err)?,
        }),
        auth: auth.try_into().map_err(xdr_err)?,
    };
    Ok(Transaction {
        source_account: XdrMuxed::Ed25519(Uint256(source.0)),
        fee,
        seq_num: SequenceNumber(sequence),
        cond: Preconditions::Time(TimeBounds {
            min_time: TimePoint(0),
            max_time: TimePoint(max_time),
        }),
        memo: XdrMemo::None,
        operations: vec![Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(op),
        }]
        .try_into()
        .map_err(xdr_err)?,
        ext,
    })
}

fn unsigned_envelope(tx: Transaction) -> Result<String, SorobanError> {
    TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: VecM::default(),
    })
    .to_xdr_base64(Limits::none())
    .map_err(xdr_err)
}

/// Simulates `call` as if sent by `source`, without signing anything. For reading contract state.
pub async fn simulate(
    rpc: &dyn RpcSimulate,
    source: &AccountKey,
    sequence: i64,
    call: &ContractCall,
) -> Result<Simulation, SorobanError> {
    let tx = transaction_for(
        source,
        sequence,
        0,
        call,
        MIN_FEE_STROOPS,
        vec![],
        TransactionExt::V0,
    )?;
    rpc.simulate(&unsigned_envelope(tx)?).await
}

/// Simulates `call`, then builds and signs the real transaction with the resources the simulation asked for.
///
/// `sequence` is the source account's next sequence number (its current one plus one). `max_time` is the unix time
/// after which the transaction can never land: set it a couple of minutes ahead.
pub async fn prepare(
    rpc: &dyn RpcSimulate,
    network: Network,
    signer: &dyn Signer,
    call: &ContractCall,
    sequence: i64,
    max_time: u64,
) -> Result<Prepared, SorobanError> {
    if max_time == 0 {
        return Err(SorobanError::Build(
            "every transaction must have a max_time (an expiry)".into(),
        ));
    }
    if sequence <= 0 {
        return Err(SorobanError::Build(
            "the sequence number must be positive".into(),
        ));
    }
    let source = signer.public_key();
    let sim = simulate(rpc, &source, sequence, call).await?;
    // Only authorization by the source account itself can be satisfied by signing the transaction.
    if sim
        .auth
        .iter()
        .any(|a| !matches!(a.credentials, SorobanCredentials::SourceAccount))
    {
        return Err(SorobanError::NeedsOtherAuthorization);
    }
    let resource_fee = sim
        .min_resource_fee
        .max(0)
        .saturating_mul(100 + RESOURCE_FEE_MARGIN_PERCENT)
        / 100;
    let fee = u32::try_from((MIN_FEE_STROOPS as i64).saturating_add(resource_fee))
        .map_err(|_| SorobanError::Build("the fee does not fit".into()))?;
    let tx = transaction_for(
        &source,
        sequence,
        max_time,
        call,
        fee,
        sim.auth.clone(),
        TransactionExt::V1(sim.transaction_data.clone()),
    )?;
    let hash = tx.hash(network.id()).map_err(xdr_err)?;
    let signature = signer
        .sign(&hash)
        .await
        .map_err(|e| SorobanError::Sign(e.0))?;
    let hint: [u8; 4] = source.0[28..].try_into().unwrap();
    let envelope = TransactionEnvelope::Tx(TransactionV1Envelope {
        tx,
        signatures: vec![DecoratedSignature {
            hint: SignatureHint(hint),
            signature: Signature(signature.to_vec().try_into().map_err(xdr_err)?),
        }]
        .try_into()
        .map_err(xdr_err)?,
    });
    let envelope_xdr = envelope.to_xdr_base64(Limits::none()).map_err(xdr_err)?;
    Ok(Prepared {
        signed: SignedTransaction { hash, envelope_xdr },
        simulated_return: sim.return_value,
        fee_stroops: fee,
        max_time,
    })
}

/// How a submission ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolved {
    Success {
        return_value: Option<ScVal>,
    },
    Failed(String),
    /// Not in a ledger yet when we stopped looking. It may still land until `max_time`: look the hash up again; do not
    /// build a new transaction.
    Unresolved,
}

/// Sends a prepared transaction and watches for it for up to `wait`, polling every `poll`.
pub async fn send_and_wait(
    rpc: &dyn RpcSend,
    prepared: &Prepared,
    wait: std::time::Duration,
    poll: std::time::Duration,
) -> Result<Resolved, SorobanError> {
    match rpc.send(&prepared.signed.envelope_xdr).await? {
        SendStatus::Pending | SendStatus::Duplicate => {}
        SendStatus::TryAgainLater => return Ok(Resolved::Unresolved),
        SendStatus::Error(e) => return Ok(Resolved::Failed(e)),
    }
    let hash = prepared.signed.hash_hex();
    let end = std::time::Instant::now() + wait;
    loop {
        match rpc.transaction(&hash).await? {
            Some(SorobanOutcome::Success { return_value }) => {
                return Ok(Resolved::Success { return_value })
            }
            Some(SorobanOutcome::Failed(e)) => return Ok(Resolved::Failed(e)),
            None => {}
        }
        if std::time::Instant::now() >= end {
            return Ok(Resolved::Unresolved);
        }
        tokio::time::sleep(poll).await;
    }
}

// ---------------------------------------------------------------------------------------------------------------
// The JSON-RPC server

/// A Soroban RPC server over HTTP (the testnet one for now).
pub struct SorobanHttp {
    client: reqwest::Client,
    url: String,
}

impl SorobanHttp {
    pub fn new(url: &str) -> Result<SorobanHttp, SorobanError> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .map_err(|e| SorobanError::Rpc(e.to_string()))?;
        Ok(SorobanHttp {
            client,
            url: url.to_string(),
        })
    }

    async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, SorobanError> {
        let body =
            serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let answer: serde_json::Value = self
            .client
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .map_err(|e| SorobanError::Rpc(e.to_string()))?
            .json()
            .await
            .map_err(|e| SorobanError::Rpc(e.to_string()))?;
        if let Some(err) = answer.get("error") {
            return Err(SorobanError::Rpc(err.to_string()));
        }
        answer
            .get("result")
            .cloned()
            .ok_or_else(|| SorobanError::Rpc("no result in the answer".into()))
    }
}

/// Turns the server's `simulateTransaction` result into a [`Simulation`]. Pure, so it is tested on recorded JSON.
pub fn parse_simulation(result: &serde_json::Value) -> Result<Simulation, SorobanError> {
    if let Some(error) = result.get("error").and_then(|e| e.as_str()) {
        return Err(SorobanError::Simulation(error.to_string()));
    }
    let text = |v: &serde_json::Value, what: &str| -> Result<String, SorobanError> {
        v.as_str()
            .map(str::to_string)
            .ok_or_else(|| SorobanError::Rpc(format!("the simulation has no {what}")))
    };
    let data = SorobanTransactionData::from_xdr_base64(
        text(&result["transactionData"], "transactionData")?,
        Limits::none(),
    )
    .map_err(|e| SorobanError::Rpc(format!("transactionData: {e}")))?;
    let min_resource_fee: i64 = text(&result["minResourceFee"], "minResourceFee")?
        .parse()
        .map_err(|_| SorobanError::Rpc("minResourceFee is not a number".into()))?;
    let first = result["results"].get(0);
    let mut auth = Vec::new();
    if let Some(list) = first.and_then(|r| r["auth"].as_array()) {
        for a in list {
            let b64 = text(a, "auth entry")?;
            auth.push(
                SorobanAuthorizationEntry::from_xdr_base64(b64, Limits::none())
                    .map_err(|e| SorobanError::Rpc(format!("auth entry: {e}")))?,
            );
        }
    }
    let return_value = match first.and_then(|r| r["xdr"].as_str()) {
        Some(b64) => Some(
            ScVal::from_xdr_base64(b64, Limits::none())
                .map_err(|e| SorobanError::Rpc(format!("return value: {e}")))?,
        ),
        None => None,
    };
    Ok(Simulation {
        transaction_data: data,
        min_resource_fee,
        auth,
        return_value,
    })
}

/// Turns `getTransaction`'s result into an outcome. Pure.
pub fn parse_transaction(
    result: &serde_json::Value,
) -> Result<Option<SorobanOutcome>, SorobanError> {
    match result["status"].as_str() {
        Some("NOT_FOUND") => Ok(None),
        Some("SUCCESS") => {
            // Newer servers give the return value directly; otherwise it stays unknown here.
            let return_value = result["returnValue"]
                .as_str()
                .and_then(|b64| ScVal::from_xdr_base64(b64, Limits::none()).ok());
            Ok(Some(SorobanOutcome::Success { return_value }))
        }
        Some("FAILED") => Ok(Some(SorobanOutcome::Failed(
            result["resultXdr"].as_str().unwrap_or("failed").to_string(),
        ))),
        other => Err(SorobanError::Rpc(format!(
            "unexpected transaction status {other:?}"
        ))),
    }
}

#[async_trait::async_trait]
impl RpcSimulate for SorobanHttp {
    async fn simulate(&self, envelope_xdr: &str) -> Result<Simulation, SorobanError> {
        let result = self
            .call(
                "simulateTransaction",
                serde_json::json!({ "transaction": envelope_xdr }),
            )
            .await?;
        parse_simulation(&result)
    }
}

#[async_trait::async_trait]
impl RpcSend for SorobanHttp {
    async fn send(&self, envelope_xdr: &str) -> Result<SendStatus, SorobanError> {
        let result = self
            .call(
                "sendTransaction",
                serde_json::json!({ "transaction": envelope_xdr }),
            )
            .await?;
        Ok(match result["status"].as_str() {
            Some("PENDING") => SendStatus::Pending,
            Some("DUPLICATE") => SendStatus::Duplicate,
            Some("TRY_AGAIN_LATER") => SendStatus::TryAgainLater,
            Some("ERROR") => SendStatus::Error(
                result["errorResultXdr"]
                    .as_str()
                    .unwrap_or("error")
                    .to_string(),
            ),
            other => {
                return Err(SorobanError::Rpc(format!(
                    "unexpected send status {other:?}"
                )))
            }
        })
    }

    async fn transaction(&self, hash_hex: &str) -> Result<Option<SorobanOutcome>, SorobanError> {
        let result = self
            .call("getTransaction", serde_json::json!({ "hash": hash_hex }))
            .await?;
        parse_transaction(&result)
    }
}

/// The SHA-256 of a byte string as lower-case hex (used for the contract code hash in the deployment log).
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer as _, SigningKey, Verifier as _};
    use std::sync::Mutex;

    struct Key(SigningKey);

    impl Key {
        fn new(seed: u8) -> Key {
            Key(SigningKey::from_bytes(&[seed; 32]))
        }
    }

    #[async_trait::async_trait]
    impl Signer for Key {
        fn public_key(&self) -> AccountKey {
            AccountKey(self.0.verifying_key().to_bytes())
        }
        async fn sign(&self, hash: &[u8; 32]) -> Result<[u8; 64], crate::signer::SignError> {
            Ok(self.0.sign(hash).to_bytes())
        }
    }

    /// A network that answers every simulation the same way, and remembers what it was asked.
    struct FakeRpc {
        answer: Mutex<Result<Simulation, SorobanError>>,
        asked: Mutex<Vec<String>>,
    }

    impl FakeRpc {
        fn answering(sim: Simulation) -> FakeRpc {
            FakeRpc {
                answer: Mutex::new(Ok(sim)),
                asked: Mutex::new(vec![]),
            }
        }
    }

    #[async_trait::async_trait]
    impl RpcSimulate for FakeRpc {
        async fn simulate(&self, envelope_xdr: &str) -> Result<Simulation, SorobanError> {
            self.asked.lock().unwrap().push(envelope_xdr.to_string());
            self.answer.lock().unwrap().clone()
        }
    }

    fn simulation(min_fee: i64) -> Simulation {
        Simulation {
            transaction_data: SorobanTransactionData::default(),
            min_resource_fee: min_fee,
            auth: vec![],
            return_value: Some(ScVal::Void),
        }
    }

    fn escrow_call() -> ContractCall {
        let contract =
            ContractId::parse("CAJLKHWOKJPUBLRG6GNZZQXYWSCFZE3VLMAVUOEDI3MC7GV4GVVUN6BT").unwrap();
        ContractCall {
            contract,
            function: "settle".into(),
            args: vec![val::bytes(&[7u8; 16]), val::unit_variant("WinA")],
        }
    }

    fn decode(b64: &str) -> TransactionEnvelope {
        TransactionEnvelope::from_xdr_base64(b64, Limits::none()).unwrap()
    }

    #[tokio::test]
    async fn a_call_becomes_a_signed_transaction_with_the_resources_the_simulation_asked_for() {
        let key = Key::new(1);
        let rpc = FakeRpc::answering(simulation(10_000));
        let p = prepare(
            &rpc,
            Network::Testnet,
            &key,
            &escrow_call(),
            42,
            1_800_000_120,
        )
        .await
        .unwrap();
        // 100 base + 10 000 resource fee + 15% margin on the resource fee.
        assert_eq!(p.fee_stroops, 100 + 11_500);
        let TransactionEnvelope::Tx(env) = decode(&p.signed.envelope_xdr) else {
            panic!("expected a v1 envelope")
        };
        let tx = env.tx;
        assert_eq!(tx.seq_num.0, 42);
        assert_eq!(tx.fee, 11_600);
        assert!(matches!(tx.ext, TransactionExt::V1(_)));
        let Preconditions::Time(t) = &tx.cond else {
            panic!("expected time bounds")
        };
        assert_eq!((t.min_time.0, t.max_time.0), (0, 1_800_000_120));
        let OperationBody::InvokeHostFunction(op) = &tx.operations[0].body else {
            panic!("expected an invoke")
        };
        let HostFunction::InvokeContract(args) = &op.host_function else {
            panic!("expected a contract call")
        };
        assert_eq!(args.function_name.0.as_slice(), b"settle");
        assert_eq!(args.args.len(), 2);
        assert_eq!(args.args[0], val::bytes(&[7u8; 16]));
        // The signature is by the source key over the hash we were given back.
        assert_eq!(env.signatures.len(), 1);
        let sig = ed25519_dalek::Signature::from_bytes(
            &env.signatures[0].signature.0.to_vec().try_into().unwrap(),
        );
        key.0
            .verifying_key()
            .verify(&p.signed.hash, &sig)
            .expect("the signature is valid for the reported hash");
        assert_eq!(p.signed.hash, tx.hash(Network::Testnet.id()).unwrap());
        // The network was asked about an unsigned copy, never the signed one.
        let asked = rpc.asked.lock().unwrap();
        assert_eq!(asked.len(), 1);
        let TransactionEnvelope::Tx(asked) = decode(&asked[0]) else {
            panic!()
        };
        assert!(asked.signatures.is_empty());
    }

    #[tokio::test]
    async fn a_call_that_would_fail_is_never_signed() {
        let rpc = FakeRpc {
            answer: Mutex::new(Err(SorobanError::Simulation(
                "HostError: Error(Contract, #13)".into(),
            ))),
            asked: Mutex::new(vec![]),
        };
        let err = prepare(&rpc, Network::Testnet, &Key::new(1), &escrow_call(), 1, 100)
            .await
            .unwrap_err();
        assert!(matches!(err, SorobanError::Simulation(m) if m.contains("#13")));
    }

    #[tokio::test]
    async fn a_call_that_needs_someone_elses_signature_is_refused() {
        let mut sim = simulation(1);
        sim.auth = vec![SorobanAuthorizationEntry {
            credentials: SorobanCredentials::Address(stellar_xdr::SorobanAddressCredentials {
                address: ScAddress::Contract(XdrContractId(Hash([1; 32]))),
                nonce: 0,
                signature_expiration_ledger: 0,
                signature: ScVal::Void,
            }),
            root_invocation: Default::default(),
        }];
        let err = prepare(
            &FakeRpc::answering(sim),
            Network::Testnet,
            &Key::new(1),
            &escrow_call(),
            1,
            100,
        )
        .await
        .unwrap_err();
        assert_eq!(err, SorobanError::NeedsOtherAuthorization);
    }

    #[tokio::test]
    async fn a_transaction_without_an_expiry_or_a_sequence_is_not_built() {
        let rpc = FakeRpc::answering(simulation(1));
        let k = Key::new(1);
        assert!(prepare(&rpc, Network::Testnet, &k, &escrow_call(), 1, 0)
            .await
            .is_err());
        assert!(prepare(&rpc, Network::Testnet, &k, &escrow_call(), 0, 100)
            .await
            .is_err());
        assert!(
            rpc.asked.lock().unwrap().is_empty(),
            "nothing was even simulated"
        );
    }

    #[test]
    fn the_network_is_part_of_what_is_signed() {
        let key = Key::new(1);
        let tx = transaction_for(
            &key.public_key(),
            1,
            100,
            &escrow_call(),
            100,
            vec![],
            TransactionExt::V0,
        )
        .unwrap();
        assert_ne!(
            tx.hash(Network::Testnet.id()).unwrap(),
            tx.hash(Network::Public.id()).unwrap()
        );
    }

    #[test]
    fn arguments_are_encoded_the_way_the_contract_reads_them() {
        // i128: a small value, a negative one, and one that needs the high half.
        assert_eq!(
            val::i128(10_000_000),
            ScVal::I128(Int128Parts {
                hi: 0,
                lo: 10_000_000
            })
        );
        assert_eq!(
            val::i128(-1),
            ScVal::I128(Int128Parts {
                hi: -1,
                lo: u64::MAX
            })
        );
        assert_eq!(
            val::i128(1i128 << 64),
            ScVal::I128(Int128Parts { hi: 1, lo: 0 })
        );
        // A unit variant of a contract enum is a one-element vector holding its name (as the network showed for WinA).
        let ScVal::Vec(Some(items)) = val::unit_variant("WinA") else {
            panic!()
        };
        assert_eq!(items.0.len(), 1);
        assert!(matches!(&items.0[0], ScVal::Symbol(s) if s.0.as_slice() == b"WinA"));
        // Account and contract addresses.
        let k = AccountKey([9; 32]);
        assert!(matches!(
            val::account(&k),
            ScVal::Address(ScAddress::Account(_))
        ));
        assert!(matches!(
            val::contract(&ContractId([9; 32])),
            ScVal::Address(ScAddress::Contract(_))
        ));
    }

    #[tokio::test]
    async fn a_submission_that_never_lands_is_reported_as_unresolved_not_failed() {
        struct Never;
        #[async_trait::async_trait]
        impl RpcSimulate for Never {
            async fn simulate(&self, _: &str) -> Result<Simulation, SorobanError> {
                unreachable!()
            }
        }
        #[async_trait::async_trait]
        impl RpcSend for Never {
            async fn send(&self, _: &str) -> Result<SendStatus, SorobanError> {
                Ok(SendStatus::Pending)
            }
            async fn transaction(&self, _: &str) -> Result<Option<SorobanOutcome>, SorobanError> {
                Ok(None)
            }
        }
        let prepared = Prepared {
            signed: SignedTransaction {
                hash: [0; 32],
                envelope_xdr: String::new(),
            },
            simulated_return: None,
            fee_stroops: 100,
            max_time: 1,
        };
        let r = send_and_wait(
            &Never,
            &prepared,
            std::time::Duration::from_millis(30),
            std::time::Duration::from_millis(10),
        )
        .await
        .unwrap();
        assert_eq!(r, Resolved::Unresolved);
    }

    #[test]
    fn the_servers_answers_are_read_carefully() {
        // A simulation error is the call failing, with the network's own words.
        let err =
            parse_simulation(&serde_json::json!({ "error": "HostError: Error(Contract, #8)" }))
                .unwrap_err();
        assert!(matches!(err, SorobanError::Simulation(m) if m.contains("#8")));
        // A malformed one is the server's fault, not the call's.
        assert!(matches!(
            parse_simulation(&serde_json::json!({ "minResourceFee": "100" })).unwrap_err(),
            SorobanError::Rpc(_)
        ));
        assert_eq!(
            parse_transaction(&serde_json::json!({ "status": "NOT_FOUND" })).unwrap(),
            None
        );
        assert!(matches!(
            parse_transaction(&serde_json::json!({ "status": "FAILED", "resultXdr": "AAAA" }))
                .unwrap(),
            Some(SorobanOutcome::Failed(_))
        ));
        assert!(parse_transaction(&serde_json::json!({ "status": "WHAT" })).is_err());
    }
}
