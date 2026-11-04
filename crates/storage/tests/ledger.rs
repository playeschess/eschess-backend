//! The ledger against a real Postgres (`make dev`, then `make test-integration`).
//!
//! The first group posts through the Rust API. The second goes around it with raw SQL, to show the
//! database refuses bad money on its own, whatever the application code does.

use eschess_domain::money::{Amount, AssetId};
use eschess_ledger::{AccountId, Entry, IdempotencyKey, Posting, Side};
use eschess_storage::ledger::{
    balance, create_account, post, reconcile, LedgerStoreError, NewAccount, PostOutcome,
};
use sqlx::PgPool;
use uuid::Uuid;

const ISSUER: &str = "GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZVN";

fn usdc() -> AssetId {
    AssetId::credit("USDC", ISSUER).unwrap()
}
fn amount(minor: i64) -> Amount {
    Amount::new(minor, usdc()).unwrap()
}
async fn wallet(pool: &PgPool) -> AccountId {
    create_account(
        pool,
        &NewAccount {
            kind: "user_wallet".into(),
            asset: usdc(),
            normal_side: Side::Credit,
            allow_negative: false,
            owner_id: None,
        },
    )
    .await
    .unwrap()
}
async fn treasury(pool: &PgPool) -> AccountId {
    create_account(
        pool,
        &NewAccount {
            kind: "treasury".into(),
            asset: usdc(),
            normal_side: Side::Debit,
            allow_negative: false,
            owner_id: None,
        },
    )
    .await
    .unwrap()
}
fn key(s: &str) -> IdempotencyKey {
    IdempotencyKey::new(s).unwrap()
}
/// Debit `from`, credit `to`.
fn transfer(k: &str, from: AccountId, to: AccountId, minor: i64) -> Posting {
    Posting::new(
        key(k),
        "transfer",
        None,
        vec![
            Entry::debit(from, amount(minor)),
            Entry::credit(to, amount(minor)),
        ],
    )
    .unwrap()
}
async fn minor(pool: &PgPool, a: AccountId) -> i64 {
    balance(pool, a).await.unwrap().minor
}
async fn count(pool: &PgPool, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
        .fetch_one(pool)
        .await
        .unwrap()
}
/// Puts `minor` into a user's wallet the way a deposit would: debit the treasury, credit the wallet.
async fn fund(pool: &PgPool, treasury: AccountId, to: AccountId, minor: i64) {
    post(
        pool,
        &transfer(&format!("fund-{}-{minor}", to), treasury, to, minor),
    )
    .await
    .unwrap();
}

// ---------------------------------------------------------------- through the Rust API

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn deposits_transfers_and_withdrawals_move_balances(pool: PgPool) {
    let (t, alice, bob) = (
        treasury(&pool).await,
        wallet(&pool).await,
        wallet(&pool).await,
    );
    fund(&pool, t, alice, 1_000).await;
    assert_eq!(
        (minor(&pool, t).await, minor(&pool, alice).await),
        (1_000, 1_000),
        "treasury holds what customers are owed"
    );

    assert!(matches!(
        post(&pool, &transfer("alice-to-bob-1", alice, bob, 300))
            .await
            .unwrap(),
        PostOutcome::Posted(_)
    ));
    assert_eq!(
        (minor(&pool, alice).await, minor(&pool, bob).await),
        (700, 300)
    );

    // Withdrawal: the customer's balance falls, so does the treasury's.
    post(&pool, &transfer("bob-withdraws-1", bob, t, 300))
        .await
        .unwrap();
    assert_eq!((minor(&pool, bob).await, minor(&pool, t).await), (0, 700));
    assert_eq!(balance(&pool, alice).await.unwrap().asset, usdc());
    assert!(reconcile(&pool).await.unwrap().is_empty());
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn an_overdraft_is_refused_and_leaves_no_trace(pool: PgPool) {
    let (t, alice, bob) = (
        treasury(&pool).await,
        wallet(&pool).await,
        wallet(&pool).await,
    );
    fund(&pool, t, alice, 100).await;
    let (txs, entries) = (
        count(&pool, "ledger_transactions").await,
        count(&pool, "ledger_entries").await,
    );

    let result = post(&pool, &transfer("too-much-0001", alice, bob, 101)).await;
    assert!(
        matches!(result, Err(LedgerStoreError::InsufficientFunds)),
        "{result:?}"
    );
    assert_eq!(
        (minor(&pool, alice).await, minor(&pool, bob).await),
        (100, 0)
    );
    assert_eq!(
        (
            count(&pool, "ledger_transactions").await,
            count(&pool, "ledger_entries").await
        ),
        (txs, entries),
        "the failed posting was rolled back whole"
    );
    // Spending exactly everything is fine.
    post(&pool, &transfer("exactly-all-01", alice, bob, 100))
        .await
        .unwrap();
    assert_eq!(minor(&pool, alice).await, 0);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn racing_withdrawals_cannot_overdraw_an_account(pool: PgPool) {
    let (t, alice, bob) = (
        treasury(&pool).await,
        wallet(&pool).await,
        wallet(&pool).await,
    );
    fund(&pool, t, alice, 100).await;
    // Eight requests each try to take 30 of Alice's 100 at the same moment: three can win.
    let attempts = (0..8).map(|i| {
        let pool = pool.clone();
        tokio::spawn(async move {
            post(
                &pool,
                &transfer(&format!("race-take-{i:03}"), alice, bob, 30),
            )
            .await
        })
    });
    let results: Vec<_> = futures::future::join_all(attempts)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();
    let won = results
        .iter()
        .filter(|r| matches!(r, Ok(PostOutcome::Posted(_))))
        .count();
    let refused = results
        .iter()
        .filter(|r| matches!(r, Err(LedgerStoreError::InsufficientFunds)))
        .count();
    assert_eq!((won, refused), (3, 5), "{results:?}");
    assert_eq!(
        (minor(&pool, alice).await, minor(&pool, bob).await),
        (10, 90)
    );
    assert!(reconcile(&pool).await.unwrap().is_empty());
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn transfers_in_opposite_directions_do_not_deadlock(pool: PgPool) {
    let (t, a, b) = (
        treasury(&pool).await,
        wallet(&pool).await,
        wallet(&pool).await,
    );
    fund(&pool, t, a, 10_000).await;
    fund(&pool, t, b, 10_000).await;
    let jobs = (0..40).map(|i| {
        let pool = pool.clone();
        let (from, to) = if i % 2 == 0 { (a, b) } else { (b, a) };
        tokio::spawn(async move {
            post(&pool, &transfer(&format!("cross-{i:04}-xx"), from, to, 10)).await
        })
    });
    for r in futures::future::join_all(jobs).await {
        r.unwrap().expect("no deadlock, no failure");
    }
    assert_eq!(
        minor(&pool, a).await + minor(&pool, b).await,
        20_000,
        "money moved but none appeared or vanished"
    );
    assert!(reconcile(&pool).await.unwrap().is_empty());
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn the_same_key_records_once(pool: PgPool) {
    let (t, alice, bob) = (
        treasury(&pool).await,
        wallet(&pool).await,
        wallet(&pool).await,
    );
    fund(&pool, t, alice, 1_000).await;
    let p = transfer("settle-game-001", alice, bob, 400);
    let PostOutcome::Posted(first) = post(&pool, &p).await.unwrap() else {
        panic!("first send posts")
    };
    assert_eq!(
        post(&pool, &p).await.unwrap(),
        PostOutcome::Replayed(first),
        "a retry returns the original"
    );
    assert_eq!(minor(&pool, bob).await, 400, "and moves no money");

    // Same key, different money: refused, not silently accepted or applied.
    let different = transfer("settle-game-001", alice, bob, 500);
    assert!(matches!(
        post(&pool, &different).await,
        Err(LedgerStoreError::IdempotencyConflict)
    ));
    assert_eq!(minor(&pool, bob).await, 400);

    // Ten copies sent at once: exactly one is recorded.
    let q = transfer("burst-key-00001", alice, bob, 50);
    let sends = (0..10).map(|_| {
        let (pool, q) = (pool.clone(), q.clone());
        tokio::spawn(async move { post(&pool, &q).await.unwrap() })
    });
    let outcomes: Vec<_> = futures::future::join_all(sends)
        .await
        .into_iter()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| matches!(o, PostOutcome::Posted(_)))
            .count(),
        1
    );
    assert_eq!(minor(&pool, bob).await, 450);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn unknown_accounts_and_wrong_assets_are_refused(pool: PgPool) {
    let (t, alice) = (treasury(&pool).await, wallet(&pool).await);
    fund(&pool, t, alice, 100).await;
    let ghost = AccountId(Uuid::now_v7());
    assert!(matches!(
        post(&pool, &transfer("to-a-ghost-01", alice, ghost, 5)).await,
        Err(LedgerStoreError::UnknownAccount(_))
    ));

    // 5 XLM "paid" into a USDC account balances on paper but is the wrong asset for the account.
    let xlm = Amount::new(5, AssetId::native()).unwrap();
    let wrong = Posting::new(
        key("wrong-asset-01"),
        "x",
        None,
        vec![Entry::debit(alice, amount(5)), Entry::credit(t, xlm)],
    )
    .unwrap_err();
    assert!(
        matches!(wrong, eschess_ledger::LedgerError::Unbalanced(_)),
        "unequal across assets is caught before the database"
    );
    let xlm_both = Posting::new(
        key("wrong-asset-02"),
        "x",
        None,
        vec![
            Entry::debit(alice, Amount::new(5, AssetId::native()).unwrap()),
            Entry::credit(t, Amount::new(5, AssetId::native()).unwrap()),
        ],
    )
    .unwrap();
    assert!(matches!(
        post(&pool, &xlm_both).await,
        Err(LedgerStoreError::AssetMismatch(_))
    ));
    assert_eq!(minor(&pool, alice).await, 100);
}

// ---------------------------------------------------------------- around the Rust API, straight at the database

async fn insert_transaction(tx: &mut sqlx::PgConnection, key: &str) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO ledger_transactions (id, idempotency_key, fingerprint, kind) VALUES ($1, $2, 'raw', 'raw')").bind(id).bind(key).execute(&mut *tx).await.unwrap();
    id
}
async fn insert_entry(
    tx: &mut sqlx::PgConnection,
    transaction: Uuid,
    account: AccountId,
    asset: &str,
    side: &str,
    minor: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO ledger_entries (transaction_id, account_id, asset, side, amount_minor) VALUES ($1,$2,$3,$4,$5)")
        .bind(transaction)
        .bind(account.0)
        .bind(asset)
        .bind(side)
        .bind(minor)
        .execute(&mut *tx)
        .await
        .map(|_| ())
}
fn usdc_text() -> String {
    usdc().to_string()
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn the_database_refuses_an_unbalanced_transaction_at_commit(pool: PgPool) {
    let (t, alice) = (treasury(&pool).await, wallet(&pool).await);
    let mut tx = pool.begin().await.unwrap();
    let id = insert_transaction(&mut tx, "raw-unbalanced-1").await;
    insert_entry(&mut tx, id, t, &usdc_text(), "debit", 100)
        .await
        .unwrap();
    insert_entry(&mut tx, id, alice, &usdc_text(), "credit", 99)
        .await
        .unwrap();
    let err = tx
        .commit()
        .await
        .expect_err("one unit of money would have been created");
    assert!(err.to_string().contains("is not balanced"), "{err}");
    assert_eq!(
        (
            count(&pool, "ledger_transactions").await,
            count(&pool, "ledger_entries").await
        ),
        (0, 0)
    );
    assert_eq!(minor(&pool, alice).await, 0);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn the_database_refuses_a_transaction_with_no_entries_or_one_sided_entries(pool: PgPool) {
    let alice = wallet(&pool).await;
    let mut tx = pool.begin().await.unwrap();
    insert_transaction(&mut tx, "raw-empty-0001").await;
    assert!(tx
        .commit()
        .await
        .unwrap_err()
        .to_string()
        .contains("has no entries"));

    let mut tx = pool.begin().await.unwrap();
    let id = insert_transaction(&mut tx, "raw-onesided-1").await;
    insert_entry(&mut tx, id, alice, &usdc_text(), "credit", 50)
        .await
        .unwrap();
    assert!(tx
        .commit()
        .await
        .unwrap_err()
        .to_string()
        .contains("is not balanced"));
    assert_eq!(count(&pool, "ledger_transactions").await, 0);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn the_database_refuses_overdrafts_zero_amounts_and_wrong_assets(pool: PgPool) {
    let (t, alice) = (treasury(&pool).await, wallet(&pool).await);
    // Overdraw a customer wallet by hand: debit it with nothing in it.
    let mut tx = pool.begin().await.unwrap();
    let id = insert_transaction(&mut tx, "raw-overdraft-1").await;
    let err = insert_entry(&mut tx, id, alice, &usdc_text(), "debit", 10)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("ledger_balances_not_overdrawn"),
        "{err}"
    );
    drop(tx);

    for (what, asset, minor) in [
        ("zero", usdc_text(), 0),
        ("negative", usdc_text(), -5),
        ("wrong asset", "XLM".to_string(), 5),
    ] {
        let mut tx = pool.begin().await.unwrap();
        let id = insert_transaction(&mut tx, &format!("raw-bad-{}", what.len())).await;
        assert!(
            insert_entry(&mut tx, id, t, &asset, "debit", minor)
                .await
                .is_err(),
            "{what} should be refused"
        );
    }
    assert_eq!(count(&pool, "ledger_entries").await, 0);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn ledger_history_cannot_be_edited_or_deleted(pool: PgPool) {
    let (t, alice) = (treasury(&pool).await, wallet(&pool).await);
    fund(&pool, t, alice, 100).await;
    for sql in [
        "UPDATE ledger_entries SET amount_minor = 1",
        "DELETE FROM ledger_entries",
        "TRUNCATE ledger_entries",
        "UPDATE ledger_transactions SET kind = 'x'",
        "DELETE FROM ledger_transactions",
        "UPDATE ledger_accounts SET allow_negative = TRUE",
        "DELETE FROM ledger_accounts",
    ] {
        let err = sqlx::query(sql).execute(&pool).await.expect_err(sql);
        assert!(err.to_string().contains("append-only"), "{sql}: {err}");
    }
    assert_eq!(minor(&pool, alice).await, 100);
    assert!(reconcile(&pool).await.unwrap().is_empty());
}
