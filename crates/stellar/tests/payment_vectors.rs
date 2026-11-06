//! Our payment transactions, byte for byte against the official Python SDK (`data/payment_vectors.json`, written by
//! `data/generate_payment_vectors.py`). The hash and the whole signed envelope must match exactly: a single wrong byte
//! is a transaction the network refuses, or worse, one that pays something other than what we recorded.

use eschess_domain::money::{Amount, AssetId};
use eschess_domain::stellar::{AccountKey, Address};
use eschess_stellar::signer::{LocalSigner, Signer};
use eschess_stellar::tx::{sign, Network, Payment, PaymentMemo, TxError};
use serde_json::Value;

fn cases() -> Vec<Value> {
    let text = include_str!("data/payment_vectors.json");
    let file: Value = serde_json::from_str(text).unwrap();
    file["cases"].as_array().unwrap().clone()
}

fn bytes32(hex: &str) -> [u8; 32] {
    let v: Vec<u8> = (0..64)
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect();
    v.try_into().unwrap()
}

fn payment(c: &Value) -> Payment {
    let s = |k: &str| c[k].as_str().unwrap();
    let u = |k: &str| c[k].as_u64().unwrap();
    let asset = AssetId::parse(s("asset")).unwrap();
    let memo = match s("memo_type") {
        "none" => PaymentMemo::None,
        "id" => PaymentMemo::Id(s("memo").parse().unwrap()),
        "text" => PaymentMemo::Text(s("memo").into()),
        "hash" => PaymentMemo::Hash(bytes32(s("memo"))),
        "return" => PaymentMemo::Return(bytes32(s("memo"))),
        other => panic!("unknown memo type {other}"),
    };
    Payment {
        source: AccountKey::parse(s("source")).unwrap(),
        sequence: c["sequence"].as_i64().unwrap(),
        fee_stroops: u("fee") as u32,
        min_time: u("min_time"),
        max_time: u("max_time"),
        destination: Address::parse(s("destination")).unwrap(),
        amount: Amount::new(c["amount_minor"].as_i64().unwrap(), asset).unwrap(),
        memo,
    }
}

#[tokio::test]
async fn every_case_matches_the_official_sdk_exactly() {
    let cases = cases();
    assert!(cases.len() >= 12);
    let signer =
        LocalSigner::from_secret(cases[0]["secret"].as_str().unwrap(), Network::Testnet).unwrap();
    for c in &cases {
        let name = c["name"].as_str().unwrap();
        let p = payment(c);
        let network = Network::parse(c["network"].as_str().unwrap()).unwrap();
        let expected_hash = c["hash"].as_str().unwrap();

        // The hash is known before signing: this is what lets a lost submission be looked up later.
        let hash: String = p
            .hash(network)
            .unwrap()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(hash, expected_hash, "hash: {name}");

        // The local signer refuses the public network, so public cases are signed with the same key through a signer
        // that does not check the network. The signature itself does not depend on the network, only the hash does.
        let signed = if network == Network::Testnet {
            sign(&p, network, &signer).await.unwrap()
        } else {
            sign(&p, network, &AnyNetwork(&signer)).await.unwrap()
        };
        assert_eq!(signed.hash_hex(), expected_hash, "signed hash: {name}");
        assert_eq!(
            signed.envelope_xdr,
            c["envelope"].as_str().unwrap(),
            "envelope: {name}"
        );
    }
}

#[tokio::test]
async fn a_key_that_is_not_the_source_account_cannot_sign() {
    let c = &cases()[0];
    let signer = LocalSigner::from_secret(c["secret"].as_str().unwrap(), Network::Testnet).unwrap();
    let mut p = payment(c);
    p.source = AccountKey::parse(c["destination"].as_str().unwrap()).unwrap();
    assert_eq!(
        sign(&p, Network::Testnet, &signer).await.unwrap_err(),
        TxError::WrongSigner
    );
}

/// Signs with a testnet [`LocalSigner`]'s key whatever the network; for comparing public-network vectors only.
struct AnyNetwork<'a>(&'a LocalSigner);

#[async_trait::async_trait]
impl Signer for AnyNetwork<'_> {
    fn public_key(&self) -> AccountKey {
        self.0.public_key()
    }

    async fn sign(&self, hash: &[u8; 32]) -> Result<[u8; 64], eschess_stellar::signer::SignError> {
        self.0.sign(hash).await
    }
}
