//! The anchor login and screens (SEP-10 and SEP-24), against challenges we build ourselves and a stand-in anchor on this
//! machine. The point of most of these tests: a hostile or sloppy challenge must never get the player's key to sign.

use std::sync::{Arc, Mutex};

use ed25519_dalek::{Signer as _, SigningKey};
use eschess_domain::stellar::AccountKey;
use eschess_stellar::sep::{
    parse_toml, sign_challenge, Anchor, AnchorStatus, ChallengeContext, HttpAnchor, Kind, SepError,
};
use eschess_stellar::signer::{SignError, Signer};
use eschess_stellar::tx::Network;
use stellar_xdr::{
    Asset, DataValue, DecoratedSignature, Limits, ManageDataOp, Memo, MuxedAccount, Operation,
    OperationBody, PaymentOp, Preconditions, ReadXdr, SequenceNumber, Signature, SignatureHint,
    String64, TimeBounds, TimePoint, Transaction, TransactionEnvelope, TransactionExt,
    TransactionV1Envelope, Uint256, WriteXdr,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const DOMAIN: &str = "anchor.example";
const WEB_AUTH_DOMAIN: &str = "auth.anchor.example";
const ISSUER: &str = "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5";

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}
fn account(k: &SigningKey) -> AccountKey {
    AccountKey(k.verifying_key().to_bytes())
}

/// A player's key that counts how many times it was asked to sign.
struct Player {
    key: SigningKey,
    signed: Mutex<u32>,
}
impl Player {
    fn new(seed: u8) -> Player {
        Player {
            key: key(seed),
            signed: Mutex::new(0),
        }
    }
    fn signatures(&self) -> u32 {
        *self.signed.lock().unwrap()
    }
}
#[async_trait::async_trait]
impl Signer for Player {
    fn public_key(&self) -> AccountKey {
        account(&self.key)
    }
    async fn sign(&self, hash: &[u8; 32]) -> Result<[u8; 64], SignError> {
        *self.signed.lock().unwrap() += 1;
        Ok(self.key.sign(hash).to_bytes())
    }
}

/// How a challenge is made; the default is a correct one, and each test breaks one thing.
#[derive(Clone)]
struct Make {
    anchor: SigningKey,
    signer: SigningKey,
    client: AccountKey,
    source: AccountKey,
    seq: i64,
    first_source: AccountKey,
    first_name: String,
    nonce: usize,
    min: u64,
    max: u64,
    web_auth_domain: String,
    extra: Option<(String, AccountKey)>,
    payment: bool,
    no_signature: bool,
}

impl Make {
    fn valid(client: AccountKey) -> Make {
        let anchor = key(1);
        Make {
            signer: anchor.clone(),
            source: account(&anchor),
            anchor,
            client,
            seq: 0,
            first_source: client,
            first_name: format!("{DOMAIN} auth"),
            nonce: 64,
            min: now() - 10,
            max: now() + 300,
            web_auth_domain: WEB_AUTH_DOMAIN.into(),
            extra: None,
            payment: false,
            no_signature: false,
        }
    }

    fn data(source: AccountKey, name: &str, value: &[u8]) -> Operation {
        Operation {
            source_account: Some(MuxedAccount::Ed25519(Uint256(source.0))),
            body: OperationBody::ManageData(ManageDataOp {
                data_name: String64::try_from(name.as_bytes().to_vec()).unwrap(),
                data_value: Some(DataValue::try_from(value.to_vec()).unwrap()),
            }),
        }
    }

    fn xdr(&self) -> String {
        let mut ops = vec![
            Make::data(self.first_source, &self.first_name, &vec![b'x'; self.nonce]),
            Make::data(
                account(&self.anchor),
                "web_auth_domain",
                self.web_auth_domain.as_bytes(),
            ),
        ];
        if let Some((name, src)) = &self.extra {
            ops.push(Make::data(*src, name, b"v"));
        }
        if self.payment {
            ops.push(Operation {
                source_account: Some(MuxedAccount::Ed25519(Uint256(self.client.0))),
                body: OperationBody::Payment(PaymentOp {
                    destination: MuxedAccount::Ed25519(Uint256(account(&key(9)).0)),
                    asset: Asset::Native,
                    amount: 10_000_000,
                }),
            });
        }
        let tx = Transaction {
            source_account: MuxedAccount::Ed25519(Uint256(self.source.0)),
            fee: 100,
            seq_num: SequenceNumber(self.seq),
            cond: Preconditions::Time(TimeBounds {
                min_time: TimePoint(self.min),
                max_time: TimePoint(self.max),
            }),
            memo: Memo::None,
            operations: ops.try_into().unwrap(),
            ext: TransactionExt::V0,
        };
        let hash = tx.hash(Network::Testnet.id()).unwrap();
        let signatures = if self.no_signature {
            vec![]
        } else {
            let pk = account(&self.signer).0;
            vec![DecoratedSignature {
                hint: SignatureHint(pk[28..].try_into().unwrap()),
                signature: Signature(
                    self.signer
                        .sign(&hash)
                        .to_bytes()
                        .to_vec()
                        .try_into()
                        .unwrap(),
                ),
            }]
        };
        TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: signatures.try_into().unwrap(),
        })
        .to_xdr_base64(Limits::none())
        .unwrap()
    }
}

fn ctx() -> ChallengeContext<'static> {
    ChallengeContext {
        home_domain: DOMAIN,
        web_auth_domain: WEB_AUTH_DOMAIN,
        anchor_key: account(&key(1)),
        network: Network::Testnet,
        now: now(),
    }
}

async fn refused(player: &Player, m: Make) -> String {
    match sign_challenge(&m.xdr(), &ctx(), player).await {
        Err(SepError::Challenge(why)) => {
            assert_eq!(
                player.signatures(),
                0,
                "the key was touched for a challenge that was refused ({why})"
            );
            why
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn a_correct_challenge_is_signed_once_and_the_signature_is_the_players() {
    let player = Player::new(5);
    let m = Make::valid(player.public_key());
    let signed = sign_challenge(&m.xdr(), &ctx(), &player).await.unwrap();
    assert_eq!(player.signatures(), 1);
    let TransactionEnvelope::Tx(env) =
        TransactionEnvelope::from_xdr_base64(&signed, Limits::none()).unwrap()
    else {
        panic!()
    };
    assert_eq!(env.signatures.len(), 2, "the anchor's and the player's");
    let hash = env.tx.hash(Network::Testnet.id()).unwrap();
    let sig = env
        .signatures
        .iter()
        .find(|s| s.hint.0 == player.public_key().0[28..])
        .expect("the player's signature");
    let bytes: [u8; 64] = sig.signature.0.as_slice().try_into().unwrap();
    key(5)
        .verifying_key()
        .verify_strict(&hash, &ed25519_dalek::Signature::from_bytes(&bytes))
        .expect("a valid signature");
}

#[tokio::test]
async fn a_challenge_that_could_move_money_or_is_not_the_anchors_is_never_signed() {
    let player = Player::new(5);
    let base = Make::valid(player.public_key());

    // It could be submitted: a real sequence number.
    refused(
        &player,
        Make {
            seq: 12345,
            ..base.clone()
        },
    )
    .await;
    // It does something other than write data.
    refused(
        &player,
        Make {
            payment: true,
            ..base.clone()
        },
    )
    .await;
    // Not made by the anchor's key.
    let imposter = key(2);
    refused(
        &player,
        Make {
            source: account(&imposter),
            signer: imposter.clone(),
            ..base.clone()
        },
    )
    .await;
    // Made by the anchor's account but not signed by it, or signed by someone else.
    refused(
        &player,
        Make {
            no_signature: true,
            ..base.clone()
        },
    )
    .await;
    refused(
        &player,
        Make {
            signer: key(3),
            ..base.clone()
        },
    )
    .await;
    // The first operation is for another account, or another anchor's domain, or has no real nonce.
    refused(
        &player,
        Make {
            first_source: account(&key(7)),
            ..base.clone()
        },
    )
    .await;
    refused(
        &player,
        Make {
            first_name: "evil.example auth".into(),
            ..base.clone()
        },
    )
    .await;
    refused(
        &player,
        Make {
            nonce: 10,
            ..base.clone()
        },
    )
    .await;
    // Expired, not yet valid, or no end.
    refused(
        &player,
        Make {
            min: now() - 1000,
            max: now() - 500,
            ..base.clone()
        },
    )
    .await;
    refused(
        &player,
        Make {
            min: now() + 500,
            max: now() + 800,
            ..base.clone()
        },
    )
    .await;
    refused(
        &player,
        Make {
            min: 0,
            max: 0,
            ..base.clone()
        },
    )
    .await;
    // A web auth domain that is not the anchor's, a client domain, and a later operation from the player's own account.
    refused(
        &player,
        Make {
            web_auth_domain: "other.example".into(),
            ..base.clone()
        },
    )
    .await;
    refused(
        &player,
        Make {
            extra: Some(("client_domain".into(), account(&base.anchor))),
            ..base.clone()
        },
    )
    .await;
    refused(
        &player,
        Make {
            extra: Some(("something".into(), player.public_key())),
            ..base.clone()
        },
    )
    .await;
    // Not a transaction at all.
    assert!(matches!(
        sign_challenge("not xdr", &ctx(), &player).await,
        Err(SepError::Challenge(_))
    ));
    assert_eq!(player.signatures(), 0);
}

#[tokio::test]
async fn a_fee_bump_wrapper_is_refused() {
    let player = Player::new(5);
    let m = Make::valid(player.public_key());
    let TransactionEnvelope::Tx(inner) =
        TransactionEnvelope::from_xdr_base64(&m.xdr(), Limits::none()).unwrap()
    else {
        panic!()
    };
    let bump = TransactionEnvelope::TxFeeBump(stellar_xdr::FeeBumpTransactionEnvelope {
        tx: stellar_xdr::FeeBumpTransaction {
            fee_source: MuxedAccount::Ed25519(Uint256(player.public_key().0)),
            fee: 1000,
            inner_tx: stellar_xdr::FeeBumpTransactionInnerTx::Tx(inner),
            ext: stellar_xdr::FeeBumpTransactionExt::V0,
        },
        signatures: vec![].try_into().unwrap(),
    });
    let xdr = bump.to_xdr_base64(Limits::none()).unwrap();
    assert!(matches!(
        sign_challenge(&xdr, &ctx(), &player).await,
        Err(SepError::Challenge(_))
    ));
    assert_eq!(player.signatures(), 0);
}

fn toml(extra: &str) -> String {
    format!(
        r#"SIGNING_KEY = "{}"
NETWORK_PASSPHRASE = "Test SDF Network ; September 2015"
WEB_AUTH_ENDPOINT = "https://{WEB_AUTH_DOMAIN}/auth"
TRANSFER_SERVER_SEP0024 = "https://{DOMAIN}/sep24"
[[CURRENCIES]]
code = "USDC"
issuer = "{ISSUER}"
{extra}"#,
        account(&key(1)).to_g()
    )
}

#[test]
fn the_toml_is_read_and_a_wrong_one_is_refused() {
    let info = parse_toml(&toml(""), Network::Testnet).unwrap();
    assert_eq!(info.signing_key, account(&key(1)));
    assert_eq!(
        info.currencies,
        vec![("USDC".to_string(), Some(ISSUER.to_string()))]
    );
    assert!(parse_toml(
        &toml("").replace(
            "Test SDF Network ; September 2015",
            "Public Global Stellar Network ; September 2015"
        ),
        Network::Testnet
    )
    .is_err());
    assert!(
        parse_toml(
            &toml("").replace("https://auth", "http://auth"),
            Network::Testnet
        )
        .is_err(),
        "endpoints must be https"
    );
    assert!(parse_toml(
        &toml("").replace("SIGNING_KEY", "SIGNING"),
        Network::Testnet
    )
    .is_err());
    assert!(parse_toml("= not toml", Network::Testnet).is_err());
}

type Answer = Box<dyn Fn(&str) -> (u16, String) + Send + Sync>;

/// Serves answers made by the closure `make(base)` builds, once the server knows its own address; records every request.
async fn serve(make: impl FnOnce(&str) -> Answer) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let answer = make(&base);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                let n = sock.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(end) = text.find("\r\n\r\n") {
                    let want = text[..end]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if buf.len() >= end + 4 + want {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&buf).to_string();
            log.lock().unwrap().push(text.clone());
            let (status, body) = answer(&text);
            let reply = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(reply.as_bytes()).await.unwrap();
        }
    });
    (base, seen)
}

fn body_of(req: &str) -> serde_json::Value {
    serde_json::from_str(req.split("\r\n\r\n").nth(1).unwrap_or("{}")).unwrap_or_default()
}

/// What a stand-in anchor does wrong, if anything.
#[derive(Clone, Copy, Default)]
struct Fault {
    /// Its login challenge also asks the player to pay someone.
    hostile_challenge: bool,
    /// Its screen is on plain http.
    insecure_url: bool,
}

/// A stand-in anchor: its toml (pointing at itself), a login that makes a real challenge for the account asked about and
/// checks the answer, and SEP-24.
fn anchor(base: &str, fault: Fault) -> Answer {
    let host = base.trim_start_matches("http://").to_string();
    let toml_text = toml("")
        .replace(&format!("https://{WEB_AUTH_DOMAIN}"), base)
        .replace(&format!("https://{DOMAIN}"), base);
    let url = if fault.insecure_url {
        "http://anchor.example/interactive/1"
    } else {
        "https://anchor.example/interactive/1"
    };
    Box::new(move |req: &str| {
        let line = req.lines().next().unwrap_or("");
        if line.starts_with("GET /.well-known/stellar.toml") {
            (200, toml_text.clone())
        } else if line.starts_with("GET /auth") {
            let acct = line
                .split("account=")
                .nth(1)
                .unwrap()
                .split(['&', ' '])
                .next()
                .unwrap();
            assert!(line.contains(&format!("home_domain={DOMAIN}")), "{line}");
            let m = Make {
                web_auth_domain: host.clone(),
                payment: fault.hostile_challenge,
                ..Make::valid(AccountKey::parse(acct).unwrap())
            };
            (200, serde_json::json!({ "transaction": m.xdr(), "network_passphrase": Network::Testnet.passphrase() }).to_string())
        } else if line.starts_with("POST /auth") {
            let signed = body_of(req)["transaction"].as_str().unwrap().to_string();
            let TransactionEnvelope::Tx(env) =
                TransactionEnvelope::from_xdr_base64(&signed, Limits::none()).unwrap()
            else {
                panic!()
            };
            assert_eq!(
                env.signatures.len(),
                2,
                "the anchor checks it came back signed by the player"
            );
            (200, r#"{"token":"jwt-secret-value"}"#.into())
        } else if line.starts_with("POST /sep24/transactions/deposit/interactive")
            || line.starts_with("POST /sep24/transactions/withdraw/interactive")
        {
            assert!(req
                .to_ascii_lowercase()
                .contains("authorization: bearer jwt-secret-value"));
            let b = body_of(req);
            if b["asset_code"] != "USDC" {
                return (400, r#"{"error":"asset not supported"}"#.into());
            }
            (200, serde_json::json!({ "type": "interactive_customer_info_needed", "url": url, "id": "tx-1" }).to_string())
        } else if line.starts_with("GET /sep24/transaction?id=tx-1") {
            (200, r#"{"transaction":{"id":"tx-1","kind":"withdrawal","status":"pending_user_transfer_start","amount_in":"10.0000000","amount_out":"9.50","withdraw_anchor_account":"GANCHOR","withdraw_memo":"12345","withdraw_memo_type":"id","more_info_url":"https://anchor.example/tx/1"}}"#.into())
        } else if line.starts_with("GET /sep24/transaction?id=tx-2") {
            (
                200,
                r#"{"transaction":{"id":"someone-else","kind":"deposit","status":"completed"}}"#
                    .into(),
            )
        } else {
            (404, "{}".into())
        }
    })
}

async fn connect(base: &str) -> Result<HttpAnchor, SepError> {
    HttpAnchor::connect(DOMAIN, base, Network::Testnet, ("USDC", ISSUER)).await
}

#[tokio::test]
async fn the_whole_conversation_with_an_anchor() {
    let (base, seen) = serve(|b| anchor(b, Fault::default())).await;
    let a = connect(&base).await.unwrap();
    let player = Player::new(5);
    let token = a.authenticate(&player).await.unwrap();
    assert_eq!(player.signatures(), 1);
    assert!(
        !format!("{token:?}").contains("jwt-secret-value"),
        "the token is never printed"
    );

    for kind in [Kind::Deposit, Kind::Withdraw] {
        let i = a
            .start(&token, kind, "USDC", &player.public_key())
            .await
            .unwrap();
        assert_eq!(
            (i.id.as_str(), i.url.as_str()),
            ("tx-1", "https://anchor.example/interactive/1")
        );
    }
    let t = a.transaction(&token, "tx-1").await.unwrap();
    assert_eq!(t.status, AnchorStatus::PendingUserTransferStart);
    assert!(!t.status.is_final());
    assert_eq!(
        (
            t.amount_in.as_deref(),
            t.withdraw_anchor_account.as_deref(),
            t.withdraw_memo.as_deref(),
            t.withdraw_memo_type.as_deref()
        ),
        (
            Some("10.0000000"),
            Some("GANCHOR"),
            Some("12345"),
            Some("id")
        )
    );
    let sent = seen
        .lock()
        .unwrap()
        .iter()
        .find(|r| r.contains("interactive"))
        .cloned()
        .unwrap();
    assert!(
        sent.contains(&player.public_key().to_g()),
        "the player's own account is the one asked about"
    );
}

#[tokio::test]
async fn a_hostile_anchor_gets_no_signature_and_a_wrong_one_is_not_used() {
    // An anchor whose challenge also asks the player to pay someone.
    let (base, _) = serve(|b| {
        anchor(
            b,
            Fault {
                hostile_challenge: true,
                ..Fault::default()
            },
        )
    })
    .await;
    let a = connect(&base).await.unwrap();
    let player = Player::new(5);
    assert!(matches!(
        a.authenticate(&player).await,
        Err(SepError::Challenge(_))
    ));
    assert_eq!(player.signatures(), 0, "the player's key was never used");

    // A screen on plain http is refused.
    let (base, _) = serve(|b| {
        anchor(
            b,
            Fault {
                insecure_url: true,
                ..Fault::default()
            },
        )
    })
    .await;
    let a = connect(&base).await.unwrap();
    let token = a.authenticate(&player).await.unwrap();
    assert!(matches!(
        a.start(&token, Kind::Deposit, "USDC", &player.public_key())
            .await,
        Err(SepError::Unavailable(_))
    ));

    // The anchor refusing, and answering about another transaction.
    let (base, _) = serve(|b| anchor(b, Fault::default())).await;
    let a = connect(&base).await.unwrap();
    let token = a.authenticate(&player).await.unwrap();
    assert!(
        matches!(a.start(&token, Kind::Deposit, "EURC", &player.public_key()).await, Err(SepError::Rejected(m)) if m == "asset not supported")
    );
    assert!(matches!(
        a.transaction(&token, "tx-2").await,
        Err(SepError::Unavailable(_))
    ));
}

#[tokio::test]
async fn an_anchor_for_another_issuers_usdc_is_not_connected() {
    let (base, _) = serve(|b| anchor(b, Fault::default())).await;
    let other = "GCDNJUBQSX7AJWLJACMJ7I4BC3Z47BQUTMHEICZLE6MU4KQBRYG5JY6B";
    assert!(matches!(
        HttpAnchor::connect(DOMAIN, &base, Network::Testnet, ("USDC", other)).await,
        Err(SepError::Toml(_))
    ));
    assert!(matches!(
        HttpAnchor::connect(DOMAIN, &base, Network::Testnet, ("EURC", ISSUER)).await,
        Err(SepError::Toml(_))
    ));
}
