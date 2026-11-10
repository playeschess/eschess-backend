//! Deposits to a player's own address (ADR 0006 §3), with a fake network (`make dev`, then `make test-integration`):
//! credited exactly once to the owner, looked at often while the owner is depositing and rarely otherwise.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use eschess_api::stellar_watcher::poll_wallets_once;
use eschess_domain::money::{Amount, AssetId};
use eschess_domain::stellar::AccountKey;
use eschess_stellar::custody::{KeyVault, LocalVault};
use eschess_stellar::tx::Network;
use eschess_stellar::{
    AccountState, Chain, ChainError, IncomingPayment, Memo, Page, Record, Submitted, TxOutcome,
};
use eschess_storage::ledger::balance;
use eschess_storage::stellar::user_wallet;
use eschess_storage::wallets::{self, WatchTiming};
use sqlx::PgPool;
use uuid::Uuid;

const ISSUER: &str = "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5";

fn usdc() -> AssetId {
    AssetId::credit("USDC", ISSUER).unwrap()
}

/// Payments per account, served by position like Horizon, and a count of how often each account was asked about.
#[derive(Default)]
struct FakeChain {
    records: Mutex<HashMap<String, Vec<Record>>>,
    asked: Mutex<HashMap<String, usize>>,
}

impl FakeChain {
    fn send(&self, account: &str, record: Record) {
        self.records
            .lock()
            .unwrap()
            .entry(account.to_string())
            .or_default()
            .push(record);
    }
    fn asked(&self, account: &str) -> usize {
        *self.asked.lock().unwrap().get(account).unwrap_or(&0)
    }
}

#[async_trait::async_trait]
impl Chain for FakeChain {
    async fn payments(
        &self,
        account: &str,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page, ChainError> {
        *self
            .asked
            .lock()
            .unwrap()
            .entry(account.to_string())
            .or_default() += 1;
        let all = self
            .records
            .lock()
            .unwrap()
            .get(account)
            .cloned()
            .unwrap_or_default();
        let from = cursor.map(|c| c.parse::<usize>().unwrap()).unwrap_or(0);
        let records: Vec<Record> = all
            .iter()
            .skip(from)
            .take(limit as usize)
            .cloned()
            .collect();
        let next_cursor = (!records.is_empty()).then(|| (from + records.len()).to_string());
        Ok(Page {
            records,
            next_cursor,
        })
    }
    async fn account(&self, _: &str) -> Result<Option<AccountState>, ChainError> {
        unreachable!()
    }
    async fn submit(&self, _: &str) -> Result<Submitted, ChainError> {
        unreachable!()
    }
    async fn transaction(&self, _: &str) -> Result<Option<TxOutcome>, ChainError> {
        unreachable!()
    }
    async fn latest_ledger_close_time(&self) -> Result<i64, ChainError> {
        unreachable!()
    }
}

fn payment(
    op: &str,
    to: &AccountKey,
    asset: AssetId,
    minor: i64,
    mux: Option<u64>,
    memo: Memo,
) -> Record {
    Record::Incoming(Box::new(IncomingPayment {
        op_id: op.to_string(),
        tx_hash: format!("tx-{op}"),
        amount: Amount::new(minor, asset.clone()).unwrap(),
        asset,
        from: "GDK4GYHCFWWDDFISK266LRQCRKNIOG2Y4PJLVR6FOLCBM4ECA2LXGGHQ".into(),
        to: to.to_g(),
        mux_id: mux,
        memo,
        created_at: "2026-11-10T00:00:00Z".into(),
    }))
}

fn vault() -> Arc<dyn KeyVault> {
    Arc::new(LocalVault::from_bytes([6; 32], 1, Network::Testnet).unwrap())
}

/// A player whose own account is ready to receive money.
async fn ready_player(pool: &PgPool) -> (Uuid, AccountKey) {
    let id = Uuid::now_v7();
    let name = format!("u{}", &id.simple().to_string()[20..]);
    sqlx::query("INSERT INTO users (id, email, username, password_hash) VALUES ($1, $2, $3, 'x')")
        .bind(id)
        .bind(format!("{name}@example.com"))
        .bind(&name)
        .execute(pool)
        .await
        .unwrap();
    let address = wallets::ensure_wallet(pool, &*vault(), id).await.unwrap();
    wallets::request_setup(pool, id).await.unwrap();
    wallets::mark_setup_ready(pool, id).await.unwrap();
    (id, address)
}

async fn usdc_balance(pool: &PgPool, user: Uuid) -> i64 {
    let w = user_wallet(pool, user, &usdc()).await.unwrap();
    balance(pool, w).await.unwrap().minor
}

async fn poll(pool: &PgPool, chain: &FakeChain) -> eschess_api::stellar_watcher::Stats {
    poll_wallets_once(pool, chain, &[usdc()], WatchTiming::default())
        .await
        .unwrap()
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn usdc_sent_to_a_players_own_address_is_credited_to_them_once(pool: PgPool) {
    let chain = FakeChain::default();
    let (ada, ada_addr) = ready_player(&pool).await;
    let (grace, _) = ready_player(&pool).await;
    // The account's own creation is not a payment; then 5 USDC arrives, with a memo and muxed id that mean nothing here.
    chain.send(&ada_addr.to_g(), Record::Ignored);
    chain.send(
        &ada_addr.to_g(),
        payment(
            "op-1",
            &ada_addr,
            usdc(),
            50_000_000,
            Some(99),
            Memo::Id(12345),
        ),
    );

    let s = poll(&pool, &chain).await;
    assert_eq!((s.credited, s.needs_review), (1, 0), "{s:?}");
    assert_eq!(
        (
            usdc_balance(&pool, ada).await,
            usdc_balance(&pool, grace).await
        ),
        (50_000_000, 0)
    );

    // Looking again, even from the start, credits nothing more.
    sqlx::query("DELETE FROM stellar_cursors")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE wallet_watch SET next_check_at = now()")
        .execute(&pool)
        .await
        .unwrap();
    let s = poll(&pool, &chain).await;
    assert_eq!((s.credited, s.already_handled), (0, 1), "{s:?}");
    assert_eq!(usdc_balance(&pool, ada).await, 50_000_000);
    let row: (String, String) =
        sqlx::query_as("SELECT status, to_account FROM stellar_deposits WHERE op_id = 'op-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(row, ("credited".to_string(), ada_addr.to_g()));
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn other_assets_are_set_aside_and_each_address_credits_only_its_owner(pool: PgPool) {
    let chain = FakeChain::default();
    let (ada, ada_addr) = ready_player(&pool).await;
    let (grace, grace_addr) = ready_player(&pool).await;
    chain.send(
        &ada_addr.to_g(),
        payment(
            "xlm",
            &ada_addr,
            AssetId::native(),
            10_000_000,
            None,
            Memo::None,
        ),
    );
    chain.send(
        &ada_addr.to_g(),
        payment("a", &ada_addr, usdc(), 10_000_000, None, Memo::None),
    );
    chain.send(
        &grace_addr.to_g(),
        payment("g", &grace_addr, usdc(), 30_000_000, None, Memo::None),
    );

    let s = poll(&pool, &chain).await;
    assert_eq!((s.credited, s.needs_review), (2, 1), "{s:?}");
    assert_eq!(
        (
            usdc_balance(&pool, ada).await,
            usdc_balance(&pool, grace).await
        ),
        (10_000_000, 30_000_000)
    );
    let status: String =
        sqlx::query_scalar("SELECT status FROM stellar_deposits WHERE op_id = 'xlm'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        status, "needs_review",
        "XLM is not an accepted deposit; a person decides"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn an_address_is_looked_at_often_while_hot_and_rarely_once_cold(pool: PgPool) {
    let chain = FakeChain::default();
    let (_, hot) = ready_player(&pool).await;
    let (_, cold) = ready_player(&pool).await;
    // `hot` just asked how to deposit; `cold` has been quiet for two hours.
    poll(&pool, &chain).await; // first look at both
    assert_eq!(
        (chain.asked(&hot.to_g()), chain.asked(&cold.to_g())),
        (1, 1)
    );
    sqlx::query("UPDATE wallet_watch SET last_activity_at = now() - interval '2 hours' WHERE user_id = (SELECT user_id FROM user_wallet_keys WHERE address = $1)")
        .bind(cold.to_g()).execute(&pool).await.unwrap();

    // Looking after `record_watched` has run again: cold goes to the slow tier, hot stays quick.
    sqlx::query("UPDATE wallet_watch SET next_check_at = now()")
        .execute(&pool)
        .await
        .unwrap();
    poll(&pool, &chain).await;
    let (hot_next, cold_next): (f64, f64) = (
        sqlx::query_scalar("SELECT extract(epoch FROM next_check_at - now())::float8 FROM wallet_watch w JOIN user_wallet_keys k ON k.user_id = w.user_id WHERE k.address = $1")
            .bind(hot.to_g()).fetch_one(&pool).await.unwrap(),
        sqlx::query_scalar("SELECT extract(epoch FROM next_check_at - now())::float8 FROM wallet_watch w JOIN user_wallet_keys k ON k.user_id = w.user_id WHERE k.address = $1")
            .bind(cold.to_g()).fetch_one(&pool).await.unwrap(),
    );
    assert!(hot_next <= 5.0, "hot: {hot_next}");
    assert!(cold_next > 250.0, "cold: {cold_next}");

    // Right after, nothing is due; only the hot one comes due within seconds, the cold one only after minutes.
    let before = (chain.asked(&hot.to_g()), chain.asked(&cold.to_g()));
    poll(&pool, &chain).await;
    assert_eq!(
        (chain.asked(&hot.to_g()), chain.asked(&cold.to_g())),
        before,
        "nothing is due yet"
    );
    sqlx::query("UPDATE wallet_watch SET next_check_at = now() - interval '1 second' WHERE next_check_at < now() + interval '10 seconds'")
        .execute(&pool).await.unwrap();
    poll(&pool, &chain).await;
    assert_eq!(chain.asked(&hot.to_g()), before.0 + 1);
    assert_eq!(
        chain.asked(&cold.to_g()),
        before.1,
        "the cold address was not due"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn money_that_arrives_while_cold_is_found_late_not_lost_and_makes_the_address_hot(
    pool: PgPool,
) {
    let chain = FakeChain::default();
    let (ada, addr) = ready_player(&pool).await;
    poll(&pool, &chain).await;
    sqlx::query("UPDATE wallet_watch SET last_activity_at = now() - interval '5 hours', next_check_at = now()")
        .execute(&pool).await.unwrap();
    // It arrives; the next (late) look finds it.
    chain.send(
        &addr.to_g(),
        payment("late", &addr, usdc(), 20_000_000, None, Memo::None),
    );
    let s = poll(&pool, &chain).await;
    assert_eq!(s.credited, 1, "{s:?}");
    assert_eq!(usdc_balance(&pool, ada).await, 20_000_000);
    let next: f64 = sqlx::query_scalar(
        "SELECT extract(epoch FROM next_check_at - now())::float8 FROM wallet_watch",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        next <= 5.0,
        "money just arrived, so look again soon: {next}"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn an_account_that_is_not_ready_is_not_watched(pool: PgPool) {
    let chain = FakeChain::default();
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO users (id, email, username, password_hash) VALUES ($1, 'idle@example.com', 'idleuser', 'x')")
        .bind(id).execute(&pool).await.unwrap();
    let addr = wallets::ensure_wallet(&pool, &*vault(), id).await.unwrap();
    wallets::request_setup(&pool, id).await.unwrap();
    poll(&pool, &chain).await;
    assert_eq!(chain.asked(&addr.to_g()), 0);
}
