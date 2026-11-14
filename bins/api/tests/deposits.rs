//! Stellar deposits, with a fake chain standing in for the network (`make dev`, then `make test-integration`).

mod common;

use std::sync::Mutex;

use axum::http::StatusCode;
use common::*;
use eschess_api::config::{StellarConfig, TESTNET_HORIZON_URL, TESTNET_USDC_ISSUER};
use eschess_api::stellar_watcher::{poll_once, Stats};
use eschess_api::Config;
use eschess_domain::money::{Amount, AssetId};
use eschess_domain::stellar::{AccountKey, MuxedAccount};
use eschess_ledger::AccountId;
use eschess_stellar::{
    AccountState, Chain, ChainError, IncomingPayment, Memo, Page, Record, Submitted, TxOutcome,
};
use sqlx::PgPool;
use uuid::Uuid;

const HOT: &str = "GBL4BGHUU6TAX7WXPSXAES7XBKEPJWLOO3HBIOYHNJH4KMLQZIQVZIF5";
const SENDER: &str = "GDK4GYHCFWWDDFISK266LRQCRKNIOG2Y4PJLVR6FOLCBM4ECA2LXGGHQ";

fn usdc() -> AssetId {
    AssetId::credit("USDC", TESTNET_USDC_ISSUER).unwrap()
}

fn config() -> Config {
    Config {
        stellar: Some(StellarConfig {
            network: "testnet".into(),
            horizon_url: TESTNET_HORIZON_URL.into(),
            hot_account: AccountKey::parse(HOT).unwrap(),
            usdc: usdc(),
            poll_ms: 5_000,
            withdraw: Default::default(),
        }),
        ..Config::default()
    }
}

/// A chain that serves whatever has been "sent" to it, paged by position like Horizon.
#[derive(Default)]
struct FakeChain {
    records: Mutex<Vec<Record>>,
    fail: Mutex<bool>,
}

impl FakeChain {
    fn send(&self, record: Record) {
        self.records.lock().unwrap().push(record);
    }
}

#[async_trait::async_trait]
impl Chain for FakeChain {
    async fn payments(
        &self,
        _account: &str,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page, ChainError> {
        if *self.fail.lock().unwrap() {
            return Err(ChainError::Unavailable("the network is down".into()));
        }
        let all = self.records.lock().unwrap();
        let from: usize = cursor.map(|c| c.parse().unwrap()).unwrap_or(0);
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

    async fn account(&self, _account: &str) -> Result<Option<AccountState>, ChainError> {
        Ok(None) // deposits never look accounts up
    }
    async fn submit(&self, _: &str) -> Result<Submitted, ChainError> {
        unreachable!("the API never submits")
    }

    async fn transaction(&self, _: &str) -> Result<Option<TxOutcome>, ChainError> {
        unreachable!("the API never looks transactions up")
    }

    async fn latest_ledger_close_time(&self) -> Result<i64, ChainError> {
        unreachable!("the API never reads ledgers")
    }
}

fn payment(op: u64, asset: AssetId, units: &str, mux_id: Option<u64>, memo: Memo) -> Record {
    Record::Incoming(Box::new(IncomingPayment {
        op_id: format!("{}", 5_000_000_000_000_000 + op),
        tx_hash: format!("{op:064x}"),
        amount: Amount::from_decimal_str(units, asset.clone()).unwrap(),
        asset,
        from: SENDER.into(),
        to: HOT.into(),
        mux_id,
        memo,
        created_at: "2026-11-05T10:00:00Z".into(),
    }))
}

async fn verified(api: &Api, name: &str) -> (String, Uuid) {
    let token = api.player(name).await;
    sqlx::query("UPDATE users SET email_verified_at = now() WHERE username = $1")
        .bind(name)
        .execute(&api.pool)
        .await
        .unwrap();
    let id: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE username = $1")
        .bind(name)
        .fetch_one(&api.pool)
        .await
        .unwrap();
    (token, id)
}

async fn deposit_id(api: &Api, token: &str) -> u64 {
    let (s, info) = api.get("/v1/wallet/deposit-info", token).await;
    assert_eq!(s, StatusCode::OK, "{info}");
    info["memo"]["value"].as_str().unwrap().parse().unwrap()
}

async fn poll(api: &Api, chain: &FakeChain) -> Stats {
    poll_once(&api.pool, chain, HOT, &[usdc()])
        .await
        .expect("a poll succeeds")
}

/// A round straight after one that failed. The failed round's lock is released a moment later (its dropped transaction rolls
/// back in the background), so this round can find the lock still held and skip. That is harmless in production, where the next
/// round does the work; a test that needs the work done waits a moment and asks again.
async fn poll_after_a_failed_round(api: &Api, chain: &FakeChain) -> Stats {
    for _ in 0..50 {
        let stats = poll(api, chain).await;
        if !stats.skipped {
            return stats;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("every round was skipped: something else holds the lock");
}

async fn usdc_balance(api: &Api, token: &str) -> i64 {
    let (_, me) = api.get("/v1/me", token).await;
    me["wallet"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["asset_id"] == usdc().to_string().as_str())
        .map(|w| w["amount_minor"].as_str().unwrap().parse().unwrap())
        .unwrap_or(0)
}

async fn custody_balance(api: &Api) -> i64 {
    let id: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM ledger_accounts WHERE kind = 'custody_stellar' AND asset = $1",
    )
    .bind(usdc().to_string())
    .fetch_optional(&api.pool)
    .await
    .unwrap();
    match id {
        Some(id) => {
            eschess_storage::ledger::balance(&api.pool, AccountId(id))
                .await
                .unwrap()
                .minor
        }
        None => 0,
    }
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn deposit_instructions_name_the_player_two_equivalent_ways(pool: PgPool) {
    let api = Api::with_config(pool, config());
    let unverified = api.player("newbie").await;
    let (s, b) = api.get("/v1/wallet/deposit-info", &unverified).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::FORBIDDEN, "email_not_verified"),
        "money needs a confirmed email"
    );

    let (ada, _) = verified(&api, "ada").await;
    let (grace, _) = verified(&api, "grace").await;
    let (_, info) = api.get("/v1/wallet/deposit-info", &ada).await;
    assert_eq!(info["network"], "testnet");
    assert_eq!(info["asset"]["id"], usdc().to_string().as_str());
    assert_eq!(info["account"], HOT);
    // The M-address is the platform wallet with Ada's id built in, the same id as the memo.
    let m = MuxedAccount::parse(info["muxed_address"].as_str().unwrap()).unwrap();
    assert_eq!(m.key, AccountKey::parse(HOT).unwrap());
    assert_eq!(m.id.to_string(), info["memo"]["value"].as_str().unwrap());
    assert_eq!(info["memo"]["type"], "id");
    // Stable for Ada, different for Grace.
    assert_eq!(deposit_id(&api, &ada).await, m.id);
    assert_ne!(deposit_id(&api, &grace).await, m.id);
    assert!(m.id >= 100_000);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn deposits_are_off_when_the_platform_has_no_wallet_configured(pool: PgPool) {
    let api = Api::new(pool);
    let (ada, _) = verified(&api, "ada").await;
    let (s, b) = api.get("/v1/wallet/deposit-info", &ada).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "deposits_unavailable")
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_payment_to_the_players_address_or_memo_is_credited_exactly_once(pool: PgPool) {
    let api = Api::with_config(pool, config());
    let chain = FakeChain::default();
    let ((ada, _), (grace, _)) = (verified(&api, "ada").await, verified(&api, "grace").await);
    let (ada_id, grace_id) = (deposit_id(&api, &ada).await, deposit_id(&api, &grace).await);

    chain.send(payment(1, usdc(), "12.5", Some(ada_id), Memo::None)); // to Ada's M-address
    chain.send(payment(2, usdc(), "3", None, Memo::Id(grace_id))); // to the wallet with Grace's memo
    chain.send(payment(
        3,
        usdc(),
        "0.0000001",
        Some(ada_id),
        Memo::Id(ada_id),
    )); // both given, agreeing
    let stats = poll(&api, &chain).await;
    assert_eq!((stats.credited, stats.needs_review), (3, 0));
    assert_eq!(usdc_balance(&api, &ada).await, 125_000_001);
    assert_eq!(usdc_balance(&api, &grace).await, 30_000_000);
    assert_eq!(
        custody_balance(&api).await,
        155_000_001,
        "the platform's mirror of the wallet equals what it owes players"
    );

    // Polling again, or the same payments appearing again, changes nothing.
    assert_eq!(poll(&api, &chain).await, Stats::default());
    // The network shows the first payment a second time.
    chain.send(payment(1, usdc(), "12.5", Some(ada_id), Memo::None));
    sqlx::query("DELETE FROM stellar_cursors")
        .execute(&api.pool)
        .await
        .unwrap(); // forget the cursor: re-read everything
    let again = poll(&api, &chain).await;
    assert_eq!((again.credited, again.already_handled), (0, 4));
    assert_eq!(usdc_balance(&api, &ada).await, 125_000_001);
    assert!(
        eschess_storage::ledger::reconcile(&api.pool)
            .await
            .unwrap()
            .is_empty(),
        "the books balance"
    );

    // The player sees them listed, with exact amounts.
    let (_, list) = api.get("/v1/wallet/deposits", &ada).await;
    let amounts: Vec<&str> = list["deposits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["amount"].as_str().unwrap())
        .collect();
    assert_eq!(amounts.len(), 2);
    assert!(
        amounts.contains(&"12.5") && amounts.contains(&"0.0000001"),
        "{amounts:?}"
    );
    assert!(list["deposits"]
        .as_array()
        .unwrap()
        .iter()
        .all(|d| d["status"] == "credited"));
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn money_that_cannot_be_matched_is_held_for_review_and_never_credited_to_a_guess(
    pool: PgPool,
) {
    let api = Api::with_config(pool, config());
    let chain = FakeChain::default();
    let (ada, _) = verified(&api, "ada").await;
    let ada_id = deposit_id(&api, &ada).await;
    let other_usdc = AssetId::credit("USDC", SENDER).unwrap();

    chain.send(payment(1, usdc(), "5", None, Memo::None)); // no id at all
    chain.send(payment(
        2,
        usdc(),
        "5",
        None,
        Memo::Text(ada_id.to_string()),
    )); // a text memo is not trusted
    chain.send(payment(3, usdc(), "5", Some(999_999), Memo::None)); // nobody has that id
    chain.send(payment(4, usdc(), "5", Some(ada_id), Memo::Id(ada_id + 1))); // the two disagree
    chain.send(payment(5, other_usdc, "5", Some(ada_id), Memo::None)); // same code, different issuer
    chain.send(payment(6, AssetId::native(), "5", Some(ada_id), Memo::None)); // a different asset
    chain.send(Record::Malformed {
        op_id: "5000000000000000007".into(),
        reason: "invalid amount".into(),
    });
    chain.send(Record::Ignored);
    let stats = poll(&api, &chain).await;
    assert_eq!(
        (stats.credited, stats.needs_review, stats.ignored),
        (0, 7, 1)
    );
    assert_eq!(usdc_balance(&api, &ada).await, 0);
    assert_eq!(custody_balance(&api).await, 0);

    let waiting = eschess_storage::stellar::needing_review(&api.pool)
        .await
        .unwrap();
    assert_eq!(waiting.len(), 7);
    let reasons: Vec<String> = waiting
        .iter()
        .map(|d| d.review_reason.clone().unwrap())
        .collect();
    assert!(reasons.iter().any(|r| r.contains("no deposit id")));
    assert!(reasons.iter().any(|r| r.contains("different players")));
    assert!(reasons
        .iter()
        .any(|r| r.contains("no player has deposit id 999999")));
    assert!(reasons
        .iter()
        .any(|r| r.contains("not an asset deposits are accepted in")));
    // None of it shows in the player's own list (it is not theirs, as far as we can tell).
    assert!(api.get("/v1/wallet/deposits", &ada).await.1["deposits"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_crash_between_recording_and_crediting_is_repaired_by_handling_it_again(pool: PgPool) {
    let api = Api::with_config(pool, config());
    let chain = FakeChain::default();
    let (ada, ada_uuid) = verified(&api, "ada").await;
    let ada_id = deposit_id(&api, &ada).await;
    let record = payment(1, usdc(), "10", Some(ada_id), Memo::None);
    let Record::Incoming(p) = &record else {
        unreachable!()
    };

    // The payment was recorded as pending, but the server died before posting to the ledger.
    sqlx::query("INSERT INTO stellar_deposits (op_id, tx_hash, asset, amount_minor, user_id, status) VALUES ($1, $2, $3, 100000000, $4, 'pending')")
        .bind(&p.op_id).bind(&p.tx_hash).bind(usdc().to_string()).bind(ada_uuid).execute(&api.pool).await.unwrap();
    assert_eq!(usdc_balance(&api, &ada).await, 0);

    chain.send(record.clone());
    assert_eq!(poll(&api, &chain).await.credited, 1);
    assert_eq!(usdc_balance(&api, &ada).await, 100_000_000);
    // Or the ledger posting was made but the status was never updated: handling it again must not pay twice.
    sqlx::query("UPDATE stellar_deposits SET status = 'pending', credited_at = NULL")
        .execute(&api.pool)
        .await
        .ok();
    sqlx::query("DELETE FROM stellar_cursors")
        .execute(&api.pool)
        .await
        .unwrap();
    poll(&api, &chain).await;
    assert_eq!(
        usdc_balance(&api, &ada).await,
        100_000_000,
        "the ledger's idempotency key stops a second credit"
    );
    assert_eq!(
        api.count("SELECT count(*) FROM stellar_deposits WHERE status = 'credited'")
            .await,
        1
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn the_same_payment_handled_by_many_at_once_is_credited_once(pool: PgPool) {
    let api = Api::with_config(pool, config());
    let (ada, _) = verified(&api, "ada").await;
    let ada_id = deposit_id(&api, &ada).await;
    let Record::Incoming(p) = payment(1, usdc(), "7", Some(ada_id), Memo::None) else {
        unreachable!()
    };
    let accepted = [usdc()];
    let results = futures::future::join_all(
        (0..8).map(|_| eschess_storage::stellar::handle_payment(&api.pool, &p, &accepted)),
    )
    .await;
    assert!(results.iter().all(|r| r.is_ok()), "{results:?}");
    assert_eq!(usdc_balance(&api, &ada).await, 70_000_000);
    assert_eq!(
        api.count("SELECT count(*) FROM ledger_transactions WHERE kind = 'deposit'")
            .await,
        1
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn the_cursor_survives_failures_and_servers_take_turns(pool: PgPool) {
    let api = Api::with_config(pool, config());
    let chain = FakeChain::default();
    let (ada, _) = verified(&api, "ada").await;
    let ada_id = deposit_id(&api, &ada).await;

    chain.send(payment(1, usdc(), "1", Some(ada_id), Memo::None));
    assert_eq!(poll(&api, &chain).await.credited, 1);
    let saved = eschess_storage::stellar::cursor(&api.pool, &format!("deposits:{HOT}"))
        .await
        .unwrap();
    assert_eq!(saved.as_deref(), Some("1"));

    // The network goes down: the round fails, and the cursor does not move.
    *chain.fail.lock().unwrap() = true;
    chain.send(payment(2, usdc(), "2", Some(ada_id), Memo::None));
    assert!(poll_once(&api.pool, &chain, HOT, &[usdc()]).await.is_err());
    assert_eq!(
        eschess_storage::stellar::cursor(&api.pool, &format!("deposits:{HOT}"))
            .await
            .unwrap()
            .as_deref(),
        Some("1")
    );
    *chain.fail.lock().unwrap() = false;
    assert_eq!(
        poll_after_a_failed_round(&api, &chain).await.credited,
        1,
        "the missed payment is picked up when the network returns"
    );
    assert_eq!(usdc_balance(&api, &ada).await, 30_000_000);

    // While another server is polling, this one stands aside.
    let mut other_server = api.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext(current_database() || ':' || $1))")
        .bind(format!("stellar-watch:{HOT}"))
        .execute(&mut *other_server)
        .await
        .unwrap();
    chain.send(payment(3, usdc(), "4", Some(ada_id), Memo::None));
    assert!(poll(&api, &chain).await.skipped);
    assert_eq!(usdc_balance(&api, &ada).await, 30_000_000);
    other_server.commit().await.unwrap();
    assert_eq!(poll(&api, &chain).await.credited, 1);
    assert_eq!(usdc_balance(&api, &ada).await, 70_000_000);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn the_database_refuses_inconsistent_deposit_rows(pool: PgPool) {
    let api = Api::with_config(pool, config());
    let (_, uuid) = verified(&api, "ada").await;
    let bad = [
        "INSERT INTO stellar_deposits (op_id, asset, amount_minor, status) VALUES ('a', 'XLM', 1, 'credited')", // no player, no credited_at
        "INSERT INTO stellar_deposits (op_id, asset, amount_minor, status) VALUES ('b', 'XLM', 1, 'pending')", // pending needs a player
        "INSERT INTO stellar_deposits (op_id, status) VALUES ('c', 'needs_review')", // a review needs a reason
        "INSERT INTO stellar_deposits (op_id, status, user_id) VALUES ('d', 'pending', NULL)",
        "INSERT INTO stellar_deposits (op_id, asset, amount_minor, status, review_reason) VALUES ('e', 'XLM', 0, 'needs_review', 'x')", // zero amount
        "INSERT INTO stellar_deposits (op_id, status, review_reason) VALUES ('f', 'whatever', 'x')",
    ];
    for sql in bad {
        assert!(sqlx::query(sql).execute(&api.pool).await.is_err(), "{sql}");
    }
    // A record we could not read may lack its details; one we are crediting may not.
    sqlx::query("INSERT INTO stellar_deposits (op_id, status, review_reason) VALUES ('ok', 'needs_review', 'unreadable')").execute(&api.pool).await.unwrap();
    assert!(sqlx::query(
        "INSERT INTO stellar_deposits (op_id, status, user_id) VALUES ('ok2', 'pending', $1)"
    )
    .bind(uuid)
    .execute(&api.pool)
    .await
    .is_err());
    // There is only ever one custody account per asset.
    let first = eschess_storage::stellar::system_account(
        &api.pool,
        "custody_stellar",
        &usdc(),
        eschess_ledger::Side::Debit,
    )
    .await
    .unwrap();
    let second = eschess_storage::stellar::system_account(
        &api.pool,
        "custody_stellar",
        &usdc(),
        eschess_ledger::Side::Debit,
    )
    .await
    .unwrap();
    assert_eq!(first, second);
}
