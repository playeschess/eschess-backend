//! A fake escrow contract and a fake network, shared by the worker's contract tests (funding and settling).
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ed25519_dalek::{Signer as _, SigningKey};
use eschess_domain::money::{Amount, AssetId};
use eschess_domain::stellar::{AccountKey, ContractId};
use eschess_ledger::{Entry, IdempotencyKey, Posting, Side};
use eschess_stellar::custody::{KeyVault, LocalVault};
use eschess_stellar::escrow::Escrow;
use eschess_stellar::signer::{SignError, Signer};
use eschess_stellar::soroban::{
    val, RpcSend, RpcSimulate, SendStatus, Simulation, SorobanError, SorobanOutcome,
};
use eschess_stellar::tx::Network;
use eschess_stellar::{AccountState, Chain, ChainError, Page, Submitted, TxOutcome};
use eschess_storage::games;
use eschess_storage::ledger::{balance, post};
use eschess_storage::stakes::Stake;
use eschess_storage::stellar::{system_account, user_wallet, CUSTODY_KIND};
use eschess_storage::wallets;
use eschess_worker::funding::{tick, FundingConfig, FundingReport, Parts};
use sqlx::PgPool;
use stellar_xdr::{
    HostFunction, Limits, OperationBody, ReadXdr, ScAddress, ScMap, ScMapEntry, ScSymbol, ScVal,
    SorobanTransactionData, StringM, TransactionEnvelope,
};
use uuid::Uuid;

pub const ISSUER: &str = "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5";
pub const STAKE: i64 = 400;

pub fn usdc() -> AssetId {
    AssetId::credit("USDC", ISSUER).unwrap()
}

pub struct Key(pub SigningKey);
impl Key {
    pub fn new(seed: u8) -> Key {
        Key(SigningKey::from_bytes(&[seed; 32]))
    }
}
#[async_trait::async_trait]
impl Signer for Key {
    fn public_key(&self) -> AccountKey {
        AccountKey(self.0.verifying_key().to_bytes())
    }
    async fn sign(&self, hash: &[u8; 32]) -> Result<[u8; 64], SignError> {
        Ok(self.0.sign(hash).to_bytes())
    }
}

pub struct FakeMatch {
    pub a: AccountKey,
    pub b: AccountKey,
    pub a_funded: bool,
    pub b_funded: bool,
    pub status: &'static str,
}

/// A fake contract that reads real transactions and keeps real-looking matches.
#[derive(Default)]
pub struct FakeNet {
    /// The outcomes the contract was told, in order.
    pub told: Mutex<Vec<String>>,
    pub matches: Mutex<HashMap<[u8; 16], FakeMatch>>,
    pub sent: Mutex<Vec<String>>,
    pub lands: Mutex<bool>,
    pub fail_deposits: Mutex<bool>,
}

pub fn call_of(envelope_xdr: &str) -> (String, Vec<ScVal>) {
    let TransactionEnvelope::Tx(e) =
        TransactionEnvelope::from_xdr_base64(envelope_xdr, Limits::none()).unwrap()
    else {
        panic!()
    };
    let OperationBody::InvokeHostFunction(op) = &e.tx.operations[0].body else {
        panic!()
    };
    let HostFunction::InvokeContract(c) = &op.host_function else {
        panic!()
    };
    (
        String::from_utf8(c.function_name.0.to_vec()).unwrap(),
        c.args.to_vec(),
    )
}

pub fn id_of(v: &ScVal) -> [u8; 16] {
    let ScVal::Bytes(b) = v else { panic!() };
    b.0.to_vec().try_into().unwrap()
}

pub fn account_of(v: &ScVal) -> AccountKey {
    let ScVal::Address(ScAddress::Account(stellar_xdr::AccountId(
        stellar_xdr::PublicKey::PublicKeyTypeEd25519(k),
    ))) = v
    else {
        panic!()
    };
    AccountKey(k.0)
}

pub fn sym(s: &str) -> ScVal {
    ScVal::Symbol(ScSymbol(
        StringM::<32>::try_from(s.as_bytes().to_vec()).unwrap(),
    ))
}

impl FakeNet {
    pub fn new() -> FakeNet {
        FakeNet {
            lands: Mutex::new(true),
            ..FakeNet::default()
        }
    }
    pub fn sent_functions(&self) -> Vec<String> {
        self.sent.lock().unwrap().clone()
    }
    pub fn match_value(&self, id: [u8; 16]) -> Option<ScVal> {
        let m = self.matches.lock().unwrap();
        let m = m.get(&id)?;
        let entries = vec![
            ScMapEntry {
                key: sym("a_funded"),
                val: ScVal::Bool(m.a_funded),
            },
            ScMapEntry {
                key: sym("b_funded"),
                val: ScVal::Bool(m.b_funded),
            },
            ScMapEntry {
                key: sym("expires_at"),
                val: ScVal::U64(1_900_000_000),
            },
            ScMapEntry {
                key: sym("player_a"),
                val: val::account(&m.a),
            },
            ScMapEntry {
                key: sym("player_b"),
                val: val::account(&m.b),
            },
            ScMapEntry {
                key: sym("stake"),
                val: val::i128(STAKE as i128),
            },
            ScMapEntry {
                key: sym("status"),
                val: val::unit_variant(m.status),
            },
            ScMapEntry {
                key: sym("token"),
                val: val::contract(&ContractId([3; 32])),
            },
        ];
        Some(ScVal::Map(Some(ScMap(entries.try_into().unwrap()))))
    }
}

pub fn simulation(ret: Option<ScVal>) -> Simulation {
    Simulation {
        transaction_data: SorobanTransactionData::default(),
        min_resource_fee: 1_000,
        auth: vec![],
        return_value: ret,
    }
}

#[async_trait::async_trait]
impl RpcSimulate for FakeNet {
    async fn simulate(&self, envelope_xdr: &str) -> Result<Simulation, SorobanError> {
        let (f, args) = call_of(envelope_xdr);
        let err = |n: u32| {
            Err(SorobanError::Simulation(format!(
                "HostError: Error(Contract, #{n})"
            )))
        };
        match f.as_str() {
            "get_match" => match self.match_value(id_of(&args[0])) {
                Some(v) => Ok(simulation(Some(v))),
                None => err(7),
            },
            "create" if self.matches.lock().unwrap().contains_key(&id_of(&args[0])) => err(6),
            "deposit" | "cancel"
                if !self.matches.lock().unwrap().contains_key(&id_of(&args[0])) =>
            {
                err(7)
            }
            _ => Ok(simulation(Some(ScVal::Void))),
        }
    }
}

#[async_trait::async_trait]
impl RpcSend for FakeNet {
    async fn send(&self, envelope_xdr: &str) -> Result<SendStatus, SorobanError> {
        let (f, args) = call_of(envelope_xdr);
        if f == "deposit" && *self.fail_deposits.lock().unwrap() {
            return Ok(SendStatus::Error("txFAILED".into()));
        }
        self.sent.lock().unwrap().push(f.clone());
        if !*self.lands.lock().unwrap() {
            return Ok(SendStatus::Pending);
        }
        let mut m = self.matches.lock().unwrap();
        match f.as_str() {
            "create" => {
                m.insert(
                    id_of(&args[0]),
                    FakeMatch {
                        a: account_of(&args[1]),
                        b: account_of(&args[2]),
                        a_funded: false,
                        b_funded: false,
                        status: "Open",
                    },
                );
            }
            "deposit" => {
                let who = account_of(&args[1]);
                let mm = m.get_mut(&id_of(&args[0])).unwrap();
                if who == mm.a {
                    mm.a_funded = true
                } else {
                    mm.b_funded = true
                }
            }
            "cancel" => {
                m.get_mut(&id_of(&args[0])).unwrap().status = "Cancelled";
            }
            "settle" => {
                m.get_mut(&id_of(&args[0])).unwrap().status = "Settled";
                // What the contract was told: WinA, WinB or Draw.
                let ScVal::Vec(Some(v)) = &args[1] else {
                    panic!()
                };
                let ScVal::Symbol(name) = &v.0[0] else {
                    panic!()
                };
                self.told
                    .lock()
                    .unwrap()
                    .push(String::from_utf8(name.0.to_vec()).unwrap());
            }
            _ => {}
        }
        Ok(SendStatus::Pending)
    }
    async fn transaction(&self, _hash: &str) -> Result<Option<SorobanOutcome>, SorobanError> {
        Ok(if *self.lands.lock().unwrap() {
            Some(SorobanOutcome::Success { return_value: None })
        } else {
            None
        })
    }
}

#[derive(Default)]
pub struct FakeChain {
    pub accounts: Mutex<HashMap<String, (i64, i64, i64)>>, // sequence, xlm, usdc
    pub payments: Mutex<Vec<(String, i64)>>,
    pub now: Mutex<i64>,
}

impl FakeChain {
    pub fn set(&self, a: &AccountKey, xlm: i64, usdc: i64) {
        self.accounts
            .lock()
            .unwrap()
            .insert(a.to_g(), (10, xlm, usdc));
    }
}

#[async_trait::async_trait]
impl Chain for FakeChain {
    async fn payments(&self, _: &str, _: Option<&str>, _: u32) -> Result<Page, ChainError> {
        unreachable!()
    }
    async fn account(&self, account: &str) -> Result<Option<AccountState>, ChainError> {
        Ok(self
            .accounts
            .lock()
            .unwrap()
            .get(account)
            .map(|(seq, xlm, u)| AccountState {
                sequence: *seq,
                can_receive: vec![usdc()],
                balances: vec![(AssetId::native(), *xlm), (usdc(), *u)],
            }))
    }
    async fn submit(&self, envelope_xdr: &str) -> Result<Submitted, ChainError> {
        let TransactionEnvelope::Tx(e) =
            TransactionEnvelope::from_xdr_base64(envelope_xdr, Limits::none()).unwrap()
        else {
            panic!()
        };
        let OperationBody::Payment(p) = &e.tx.operations[0].body else {
            panic!("not a payment")
        };
        let stellar_xdr::MuxedAccount::Ed25519(k) = &p.destination else {
            panic!()
        };
        let to = AccountKey(k.0).to_g();
        self.payments.lock().unwrap().push((to.clone(), p.amount));
        if let Some(a) = self.accounts.lock().unwrap().get_mut(&to) {
            a.2 += p.amount;
        }
        Ok(Submitted::Accepted)
    }
    async fn transaction(&self, _: &str) -> Result<Option<TxOutcome>, ChainError> {
        Ok(Some(TxOutcome {
            successful: true,
            fee_charged: 100,
            result_code: "tx_success".into(),
        }))
    }
    async fn latest_ledger_close_time(&self) -> Result<i64, ChainError> {
        Ok(*self.now.lock().unwrap())
    }
}

pub struct World {
    pub net: Arc<FakeNet>,
    pub chain: Arc<FakeChain>,
    pub vault: Arc<dyn KeyVault>,
    pub admin: Key,
    pub hot: Key,
    pub config: FundingConfig,
}

pub fn world() -> World {
    let chain = Arc::new(FakeChain::default());
    *chain.now.lock().unwrap() = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let (admin, hot) = (Key::new(1), Key::new(2));
    chain.set(&admin.public_key(), 100_000_000, 0);
    chain.set(&hot.public_key(), 100_000_000, 10_000);
    World {
        net: Arc::new(FakeNet::new()),
        chain,
        vault: Arc::new(LocalVault::from_bytes([4; 32], 1, Network::Testnet).unwrap()),
        admin,
        hot,
        config: FundingConfig {
            network: Network::Testnet,
            escrow: Escrow::new(ContractId([5; 32])),
            token: ContractId([3; 32]),
            usdc: usdc(),
            timeout_secs: 120,
            match_expiry_secs: 29 * 24 * 3600,
            tx_wait: std::time::Duration::from_millis(200),
            batch: 10,
        },
    }
}

pub async fn round(pool: &PgPool, w: &World, with_hot: bool) -> FundingReport {
    let parts = Parts {
        pool,
        rpc: &*w.net,
        chain: &*w.chain,
        vault: w.vault.clone(),
        admin: &w.admin,
        hot: if with_hot { Some(&w.hot) } else { None },
    };
    tick(&parts, &w.config).await.unwrap()
}

/// A player with a wallet key, a ledger balance of 1 000, and (on the fake network) an account holding `usdc` USDC.
pub async fn player(pool: &PgPool, w: &World, on_chain_usdc: i64) -> (Uuid, AccountKey) {
    let id = Uuid::now_v7();
    let name = format!("u{}", &id.simple().to_string()[20..]);
    sqlx::query("INSERT INTO users (id, email, username, password_hash, email_verified_at) VALUES ($1, $2, $3, 'x', now())")
        .bind(id).bind(format!("{name}@example.com")).bind(&name).execute(pool).await.unwrap();
    let address = wallets::ensure_wallet(pool, &*w.vault, id).await.unwrap();
    w.chain.set(&address, 20_000_000, on_chain_usdc);
    let custody = system_account(pool, CUSTODY_KIND, &usdc(), Side::Debit)
        .await
        .unwrap();
    let wallet = user_wallet(pool, id, &usdc()).await.unwrap();
    let amount = Amount::new(1_000, usdc()).unwrap();
    post(
        pool,
        &Posting::new(
            IdempotencyKey::new(&format!("dep:{id}")).unwrap(),
            "deposit",
            None,
            vec![
                Entry::debit(custody, amount.clone()),
                Entry::credit(wallet, amount),
            ],
        )
        .unwrap(),
    )
    .await
    .unwrap();
    (id, address)
}

pub async fn funding_game(
    pool: &PgPool,
    w: &World,
    white_usdc: i64,
    black_usdc: i64,
) -> (Uuid, Uuid, Uuid, AccountKey, AccountKey) {
    sqlx::query("UPDATE platform_settings SET value = 'contract' WHERE name = 'stake_path'")
        .execute(pool)
        .await
        .unwrap();
    let (white, wa) = player(pool, w, white_usdc).await;
    let (black, ba) = player(pool, w, black_usdc).await;
    let stake = Stake {
        asset: usdc(),
        minor: STAKE,
    };
    let game = games::create_game(pool, white, 300_000, 0, "white", true, Some(&stake))
        .await
        .unwrap();
    games::join_game(pool, game, black, true).await.unwrap();
    (game, white, black, wa, ba)
}

pub async fn status(pool: &PgPool, g: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM games WHERE id = $1")
        .bind(g)
        .fetch_one(pool)
        .await
        .unwrap()
}
pub async fn ledger(pool: &PgPool, u: Uuid) -> i64 {
    balance(pool, user_wallet(pool, u, &usdc()).await.unwrap())
        .await
        .unwrap()
        .minor
}
