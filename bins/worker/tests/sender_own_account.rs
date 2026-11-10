//! Withdrawals paid from a player's own account (ADR 0006), with a fake network (`make dev`, then `make test-integration`):
//! the player's account pays when it holds the money, the platform's wallet otherwise, and the books settle the same.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use eschess_domain::money::{Amount, AssetId};
use eschess_domain::stellar::AccountKey;
use eschess_ledger::{Entry, IdempotencyKey, Posting, Side};
use eschess_stellar::custody::{KeyVault, LocalVault};
use eschess_stellar::signer::{LocalSigner, Signer};
use eschess_stellar::tx::{Network, PaymentMemo};
use eschess_stellar::{AccountState, Chain, ChainError, Page, Submitted, TxOutcome};
use eschess_storage::ledger::{balance, post};
use eschess_storage::stellar::{system_account, user_wallet, CUSTODY_KIND};
use eschess_storage::wallets;
use eschess_storage::withdrawals::{self, NewWithdrawal, Policy, Status};
use eschess_worker::sender::{tick, tick_with, OwnAccounts, SenderConfig};
use sqlx::PgPool;
use stellar_xdr::{Limits, ReadXdr, TransactionEnvelope};
use uuid::Uuid;

const ISSUER: &str = "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5";
const HOT_SECRET: &str = "SA3PXIPSNIG2RVLSIREL6KV6PQJTFI7PA4ZBYVP772LORMETDV5FQTNT";
const DEST: &str = "GBUUQEXJEVAN2FLTOEBQ2OUJREAQOAEYYNW5R2X7O7RE4LZ2DNMBUEAV";
const POLICY: Policy = Policy {
    auto_approve_up_to_minor: i64::MAX,
    daily_limit_minor: i64::MAX,
};
const NOW: i64 = 1_800_000_000;

fn usdc() -> AssetId {
    AssetId::credit("USDC", ISSUER).unwrap()
}

/// Accounts with chosen balances; every transaction submitted lands and succeeds, using up one sequence number.
#[derive(Default)]
struct Net {
    accounts: Mutex<HashMap<String, AccountState>>,
    landed: Mutex<HashMap<String, TxOutcome>>,
    submitted: Mutex<Vec<String>>,
}

impl Net {
    fn set(&self, key: &str, sequence: i64, usdc_minor: i64, xlm_stroops: i64) {
        self.accounts.lock().unwrap().insert(
            key.to_string(),
            AccountState {
                sequence,
                can_receive: vec![usdc()],
                balances: vec![(usdc(), usdc_minor), (AssetId::native(), xlm_stroops)],
            },
        );
    }
}

fn envelope(b64: &str) -> stellar_xdr::TransactionV1Envelope {
    match TransactionEnvelope::from_xdr_base64(b64, Limits::none()).unwrap() {
        TransactionEnvelope::Tx(e) => e,
        other => panic!("{other:?}"),
    }
}

#[async_trait::async_trait]
impl Chain for Net {
    async fn payments(&self, _: &str, _: Option<&str>, _: u32) -> Result<Page, ChainError> {
        unreachable!()
    }
    async fn account(&self, account: &str) -> Result<Option<AccountState>, ChainError> {
        Ok(self.accounts.lock().unwrap().get(account).cloned())
    }
    async fn submit(&self, b64: &str) -> Result<Submitted, ChainError> {
        self.submitted.lock().unwrap().push(b64.to_string());
        let e = envelope(b64);
        let hash: String =
            e.tx.hash(Network::Testnet.id())
                .unwrap()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
        let source = match &e.tx.source_account {
            stellar_xdr::MuxedAccount::Ed25519(k) => AccountKey(k.0).to_g(),
            _ => panic!(),
        };
        let mut accounts = self.accounts.lock().unwrap();
        let state = accounts.get_mut(&source).expect("the source exists");
        assert_eq!(
            e.tx.seq_num.0,
            state.sequence + 1,
            "the source's own next sequence number"
        );
        state.sequence += 1;
        self.landed.lock().unwrap().insert(
            hash,
            TxOutcome {
                successful: true,
                fee_charged: 100,
                result_code: "tx_success".into(),
            },
        );
        Ok(Submitted::Accepted)
    }
    async fn transaction(&self, hash: &str) -> Result<Option<TxOutcome>, ChainError> {
        Ok(self.landed.lock().unwrap().get(hash).cloned())
    }
    async fn latest_ledger_close_time(&self) -> Result<i64, ChainError> {
        Ok(NOW)
    }
}

struct World {
    net: Net,
    hot: LocalSigner,
    config: SenderConfig,
    own: OwnAccounts,
}

fn world() -> World {
    let hot = LocalSigner::from_secret(HOT_SECRET, Network::Testnet).unwrap();
    let net = Net::default();
    net.set(&hot.public_key().to_g(), 100, 1_000_000_000, 1_000_000_000);
    let vault: Arc<dyn KeyVault> =
        Arc::new(LocalVault::from_bytes([5; 32], 1, Network::Testnet).unwrap());
    World {
        net,
        config: SenderConfig {
            network: Network::Testnet,
            hot_account: hot.public_key(),
            fee_stroops: 1_000,
            tx_timeout_secs: 120,
            expiry_margin_secs: 60,
        },
        hot,
        own: OwnAccounts { vault },
    }
}

/// A player with 100 USDC in the ledger and, optionally, an account of their own that is ready on the network.
async fn player(
    pool: &PgPool,
    w: &World,
    own_account: Option<(i64, i64)>,
) -> (Uuid, Option<AccountKey>) {
    let id = Uuid::now_v7();
    let name = format!("u{}", &id.simple().to_string()[20..]);
    sqlx::query("INSERT INTO users (id, email, username, password_hash) VALUES ($1, $2, $3, 'x')")
        .bind(id)
        .bind(format!("{name}@example.com"))
        .bind(&name)
        .execute(pool)
        .await
        .unwrap();
    let custody = system_account(pool, CUSTODY_KIND, &usdc(), Side::Debit)
        .await
        .unwrap();
    let wallet = user_wallet(pool, id, &usdc()).await.unwrap();
    let amount = Amount::new(1_000_000_000, usdc()).unwrap();
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
    let address = match own_account {
        None => None,
        Some((usdc_minor, xlm)) => {
            let a = wallets::ensure_wallet(pool, &*w.own.vault, id)
                .await
                .unwrap();
            wallets::request_setup(pool, id).await.unwrap();
            wallets::mark_setup_ready(pool, id).await.unwrap();
            w.net.set(&a.to_g(), 7, usdc_minor, xlm);
            Some(a)
        }
    };
    (id, address)
}

async fn withdraw(pool: &PgPool, user: Uuid, minor: i64) -> Uuid {
    let (wd, _) = withdrawals::request(
        pool,
        &NewWithdrawal {
            user_id: user,
            client_request_id: Uuid::now_v7(),
            amount: Amount::new(minor, usdc()).unwrap(),
            destination: DEST.into(),
            memo: PaymentMemo::None,
        },
        POLICY,
    )
    .await
    .unwrap();
    wd.id
}

async fn source_of(pool: &PgPool, id: Uuid) -> String {
    sqlx::query_scalar("SELECT source_account FROM withdrawal_attempts WHERE withdrawal_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn wallet_minor(pool: &PgPool, u: Uuid) -> i64 {
    balance(pool, user_wallet(pool, u, &usdc()).await.unwrap())
        .await
        .unwrap()
        .minor
}

async fn round(pool: &PgPool, w: &World, with_own: bool) -> eschess_worker::TickReport {
    if with_own {
        tick_with(pool, &w.net, &w.hot, &w.config, Some(&w.own), NOW)
            .await
            .unwrap()
    } else {
        tick(pool, &w.net, &w.hot, &w.config, NOW).await.unwrap()
    }
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_player_whose_own_account_holds_the_money_is_paid_from_it_and_the_books_settle(
    pool: PgPool,
) {
    let w = world();
    let (user, own) = player(&pool, &w, Some((500_000_000, 20_000_000))).await;
    let own = own.unwrap();
    let id = withdraw(&pool, user, 125_000_000).await;
    assert_eq!(
        wallet_minor(&pool, user).await,
        1_000_000_000 - 125_000_000,
        "held at once"
    );

    let r = round(&pool, &w, true).await;
    assert_eq!((r.sent, r.confirmed.len()), (Some(id), 1), "{r:?}");
    assert_eq!(source_of(&pool, id).await, own.to_g());
    let e = envelope(&w.net.submitted.lock().unwrap()[0]);
    assert_eq!(
        e.tx.seq_num.0, 8,
        "the player's own next sequence number, not the hot wallet's"
    );
    assert_eq!(e.signatures.len(), 1);
    assert_eq!(
        e.tx.source_account,
        stellar_xdr::MuxedAccount::Ed25519(stellar_xdr::Uint256(own.0))
    );
    // The signature is the player's, made through the vault.
    let hash = e.tx.hash(Network::Testnet.id()).unwrap();
    let sig = ed25519_dalek::Signature::from_bytes(
        &e.signatures[0].signature.0.to_vec().try_into().unwrap(),
    );
    ed25519_dalek::Verifier::verify(
        &ed25519_dalek::VerifyingKey::from_bytes(&own.0).unwrap(),
        &hash,
        &sig,
    )
    .unwrap();

    assert_eq!(
        withdrawals::get(&pool, id).await.unwrap().status,
        Status::Confirmed
    );
    assert_eq!(wallet_minor(&pool, user).await, 1_000_000_000 - 125_000_000);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn otherwise_the_platforms_wallet_pays_as_before(pool: PgPool) {
    let w = world();
    let hot = w.hot.public_key().to_g();
    // Own account too poor in USDC; own account short of XLM; no account of their own; an account but not asked for.
    let (poor, _) = player(&pool, &w, Some((10_000_000, 20_000_000))).await;
    let (dry, _) = player(&pool, &w, Some((900_000_000, 10_000_000))).await;
    let (none, _) = player(&pool, &w, None).await;
    for user in [poor, dry, none] {
        let id = withdraw(&pool, user, 125_000_000).await;
        let r = round(&pool, &w, true).await;
        assert_eq!(r.sent, Some(id), "{r:?}");
        assert_eq!(source_of(&pool, id).await, hot);
        assert_eq!(
            withdrawals::get(&pool, id).await.unwrap().status,
            Status::Confirmed
        );
    }
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn without_the_vault_even_a_rich_own_account_is_not_used(pool: PgPool) {
    let w = world();
    let (user, _) = player(&pool, &w, Some((900_000_000, 50_000_000))).await;
    let id = withdraw(&pool, user, 125_000_000).await;
    round(&pool, &w, false).await;
    assert_eq!(source_of(&pool, id).await, w.hot.public_key().to_g());
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn an_account_that_is_not_ready_yet_is_not_paid_from(pool: PgPool) {
    let w = world();
    let (user, addr) = player(&pool, &w, Some((900_000_000, 50_000_000))).await;
    let _ = addr;
    // Pretend setup had not finished.
    sqlx::query("DELETE FROM wallet_watch")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM wallet_setup WHERE user_id = $1")
        .bind(user)
        .execute(&pool)
        .await
        .ok();
    let id = withdraw(&pool, user, 125_000_000).await;
    round(&pool, &w, true).await;
    assert_eq!(source_of(&pool, id).await, w.hot.public_key().to_g());
}
