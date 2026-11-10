//! Setting players' own accounts up on the network, with a fake network (`make dev`, then `make test-integration`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use eschess_domain::money::AssetId;
use eschess_domain::stellar::AccountKey;
use eschess_stellar::custody::{KeyVault, LocalVault};
use eschess_stellar::signer::LocalSigner;
use eschess_stellar::tx::Network;
use eschess_stellar::{AccountState, Chain, ChainError, Page, Submitted, TxOutcome};
use eschess_storage::wallets::{self, SetupState};
use eschess_worker::wallet_setup::{tick, SetupConfig};
use sqlx::PgPool;
use stellar_xdr::{Limits, OperationBody, ReadXdr, TransactionEnvelope};
use uuid::Uuid;

const ISSUER: &str = "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5";
// A throwaway funder key (never used for anything real).
const FUNDER_SECRET: &str = "SA3PXIPSNIG2RVLSIREL6KV6PQJTFI7PA4ZBYVP772LORMETDV5FQTNT";

fn usdc() -> AssetId {
    AssetId::credit("USDC", ISSUER).unwrap()
}

#[derive(Default)]
struct FakeChain {
    accounts: Mutex<HashMap<String, AccountState>>,
    submitted: Mutex<Vec<String>>,
    now: Mutex<i64>,
    refuse: Mutex<Option<String>>,
}

impl FakeChain {
    fn set_account(&self, key: &str, sequence: i64, with_usdc: bool) {
        self.accounts.lock().unwrap().insert(
            key.to_string(),
            AccountState {
                sequence,
                can_receive: if with_usdc { vec![usdc()] } else { vec![] },
                balances: vec![],
            },
        );
    }
}

#[async_trait::async_trait]
impl Chain for FakeChain {
    async fn payments(&self, _: &str, _: Option<&str>, _: u32) -> Result<Page, ChainError> {
        unreachable!()
    }
    async fn account(&self, account: &str) -> Result<Option<AccountState>, ChainError> {
        Ok(self.accounts.lock().unwrap().get(account).cloned())
    }
    async fn submit(&self, envelope_xdr: &str) -> Result<Submitted, ChainError> {
        if let Some(why) = self.refuse.lock().unwrap().clone() {
            return Ok(Submitted::Refused(why));
        }
        self.submitted
            .lock()
            .unwrap()
            .push(envelope_xdr.to_string());
        Ok(Submitted::Accepted)
    }
    async fn transaction(&self, _: &str) -> Result<Option<TxOutcome>, ChainError> {
        Ok(None) // nothing we send ever lands unless the test says the account now exists
    }
    async fn latest_ledger_close_time(&self) -> Result<i64, ChainError> {
        Ok(*self.now.lock().unwrap())
    }
}

struct World {
    chain: Arc<FakeChain>,
    vault: Arc<dyn KeyVault>,
    funder: LocalSigner,
    config: SetupConfig,
}

fn world() -> World {
    let chain = Arc::new(FakeChain::default());
    *chain.now.lock().unwrap() = 1_800_000_000;
    let funder = LocalSigner::from_secret(FUNDER_SECRET, Network::Testnet).unwrap();
    chain.set_account(
        &eschess_stellar::signer::Signer::public_key(&funder).to_g(),
        500,
        false,
    );
    World {
        chain,
        vault: Arc::new(LocalVault::from_bytes([4; 32], 1, Network::Testnet).unwrap()),
        funder,
        config: SetupConfig {
            network: Network::Testnet,
            usdc: usdc(),
            starting_balance_stroops: 20_000_000,
            batch: 20,
        },
    }
}

async fn player(pool: &PgPool, w: &World) -> (Uuid, AccountKey) {
    let id = Uuid::now_v7();
    let name = format!("u{}", &id.simple().to_string()[20..]);
    sqlx::query("INSERT INTO users (id, email, username, password_hash) VALUES ($1, $2, $3, 'x')")
        .bind(id)
        .bind(format!("{name}@example.com"))
        .bind(&name)
        .execute(pool)
        .await
        .unwrap();
    let address = wallets::ensure_wallet(pool, &*w.vault, id).await.unwrap();
    assert!(wallets::request_setup(pool, id).await.unwrap());
    (id, address)
}

async fn round(pool: &PgPool, w: &World) -> eschess_worker::wallet_setup::SetupReport {
    tick(pool, &*w.chain, w.vault.clone(), &w.funder, &w.config)
        .await
        .unwrap()
}

fn decode(b64: &str) -> stellar_xdr::TransactionV1Envelope {
    match TransactionEnvelope::from_xdr_base64(b64, Limits::none()).unwrap() {
        TransactionEnvelope::Tx(e) => e,
        other => panic!("{other:?}"),
    }
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_new_players_account_is_created_funded_and_trusts_usdc_then_is_ready_once_it_exists(
    pool: PgPool,
) {
    let w = world();
    let (user, address) = player(&pool, &w).await;
    assert_eq!(
        wallets::setup_state(&pool, user).await.unwrap(),
        SetupState::SettingUp
    );

    let r = round(&pool, &w).await;
    assert_eq!((r.sent, r.ready, r.failed), (1, 0, 0), "{r:?}");
    let sent = w.chain.submitted.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    let env = decode(&sent[0]);
    assert_eq!(env.tx.seq_num.0, 501, "the funder's next sequence number");
    assert_eq!(
        env.signatures.len(),
        2,
        "the funder and the player both sign"
    );
    assert!(
        matches!(env.tx.operations[0].body, OperationBody::CreateAccount(ref c) if c.starting_balance == 20_000_000)
    );
    assert!(matches!(
        env.tx.operations[1].body,
        OperationBody::ChangeTrust(_)
    ));

    // While that transaction could still land, nothing else is built.
    let r = round(&pool, &w).await;
    assert_eq!((r.sent, r.waiting), (0, 1), "{r:?}");
    assert_eq!(w.chain.submitted.lock().unwrap().len(), 1);

    // It lands: the account exists and holds USDC.
    w.chain.set_account(&address.to_g(), 1, true);
    let r = round(&pool, &w).await;
    assert_eq!((r.ready, r.sent), (1, 0), "{r:?}");
    assert_eq!(
        wallets::setup_state(&pool, user).await.unwrap(),
        SetupState::Ready
    );
    assert_eq!(
        round(&pool, &w).await,
        Default::default(),
        "nothing left to do"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_transaction_that_never_landed_is_replaced_only_after_it_has_expired(pool: PgPool) {
    let w = world();
    let (user, _) = player(&pool, &w).await;
    round(&pool, &w).await;
    // Just before it expires: still waiting. Just after, and not found anywhere: a new one is built.
    *w.chain.now.lock().unwrap() += 119;
    assert_eq!(round(&pool, &w).await.waiting, 1);
    *w.chain.now.lock().unwrap() += 2;
    let r = round(&pool, &w).await;
    assert_eq!((r.sent, r.waiting), (1, 0), "{r:?}");
    assert_eq!(w.chain.submitted.lock().unwrap().len(), 2);
    let attempts: i32 = sqlx::query_scalar("SELECT attempts FROM wallet_setup WHERE user_id = $1")
        .bind(user)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(attempts, 2);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn an_account_that_exists_without_the_trustline_only_gets_the_trustline(pool: PgPool) {
    let w = world();
    let (_, address) = player(&pool, &w).await;
    w.chain.set_account(&address.to_g(), 77, false);
    let r = round(&pool, &w).await;
    assert_eq!(r.sent, 1);
    let env = decode(&w.chain.submitted.lock().unwrap()[0]);
    assert_eq!(env.tx.operations.len(), 1);
    assert!(matches!(
        env.tx.operations[0].body,
        OperationBody::ChangeTrust(_)
    ));
    assert_eq!(
        env.tx.seq_num.0, 78,
        "the player's own next sequence number"
    );
    assert_eq!(
        env.signatures.len(),
        1,
        "only the player signs; the platform pays nothing here"
    );
    assert_eq!(
        env.tx.source_account,
        stellar_xdr::MuxedAccount::Ed25519(stellar_xdr::Uint256(address.0))
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_refusal_is_recorded_and_does_not_stop_the_other_players(pool: PgPool) {
    let w = world();
    let (first, _) = player(&pool, &w).await;
    let (second, second_addr) = player(&pool, &w).await;
    *w.chain.refuse.lock().unwrap() = Some("tx_insufficient_balance".into());
    let r = round(&pool, &w).await;
    assert_eq!((r.failed, r.sent), (2, 0), "{r:?}");
    let err: Option<String> =
        sqlx::query_scalar("SELECT last_error FROM wallet_setup WHERE user_id = $1")
            .bind(first)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(err.unwrap().contains("insufficient_balance"));
    // Still waiting to be set up, not lost; the network accepts later and the second player gets there.
    assert_eq!(
        wallets::setup_state(&pool, first).await.unwrap(),
        SetupState::SettingUp
    );
    *w.chain.refuse.lock().unwrap() = None;
    w.chain.set_account(&second_addr.to_g(), 1, true);
    *w.chain.now.lock().unwrap() += 200;
    let r = round(&pool, &w).await;
    assert_eq!((r.ready, r.sent), (1, 1), "{r:?}");
    assert_eq!(
        wallets::setup_state(&pool, second).await.unwrap(),
        SetupState::Ready
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn asking_for_setup_twice_or_without_a_wallet_does_nothing_extra(pool: PgPool) {
    let w = world();
    let (user, _) = player(&pool, &w).await;
    assert!(
        !wallets::request_setup(&pool, user).await.unwrap(),
        "already asked"
    );
    let nobody = Uuid::now_v7();
    assert!(
        !wallets::request_setup(&pool, nobody).await.unwrap(),
        "no wallet key, nothing to set up"
    );
    assert_eq!(
        wallets::setup_state(&pool, nobody).await.unwrap(),
        SetupState::NotRequested
    );
}
