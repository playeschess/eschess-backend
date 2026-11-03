//! Randomised testing of the ledger: whatever sequence of deposits, transfers, withdrawals and retries
//! happens, money is never created or lost. Needs Postgres (`make dev`, then `make test-integration`).

use eschess_domain::money::{Amount, AssetId};
use eschess_ledger::{AccountId, Entry, IdempotencyKey, Posting, Side};
use eschess_storage::ledger::{
    balance, create_account, post, reconcile, LedgerStoreError, NewAccount, PostOutcome,
};
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::{Config, TestRunner};
use sqlx::PgPool;

const ISSUER: &str = "GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZVN";
const USERS: usize = 4;

#[derive(Debug, Clone)]
enum Op {
    Deposit {
        to: usize,
        minor: i64,
    },
    Transfer {
        from: usize,
        to: usize,
        minor: i64,
    },
    Withdraw {
        from: usize,
        minor: i64,
    },
    /// Send the previous successful posting again under its own key.
    Replay,
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (0..USERS, 1..1_000i64).prop_map(|(to, minor)| Op::Deposit { to, minor }),
        4 => (0..USERS, 0..USERS, 1..1_500i64).prop_map(|(from, to, minor)| Op::Transfer { from, to, minor }),
        2 => (0..USERS, 1..1_500i64).prop_map(|(from, minor)| Op::Withdraw { from, minor }),
        1 => Just(Op::Replay),
    ]
}

fn usdc(minor: i64) -> Amount {
    Amount::new(minor, AssetId::credit("USDC", ISSUER).unwrap()).unwrap()
}

async fn account(pool: &PgPool, kind: &str, side: Side) -> AccountId {
    create_account(
        pool,
        &NewAccount {
            kind: kind.into(),
            asset: AssetId::credit("USDC", ISSUER).unwrap(),
            normal_side: side,
            allow_negative: false,
            owner_id: None,
        },
    )
    .await
    .unwrap()
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn random_sequences_never_create_or_lose_money(pool: PgPool) {
    let mut runner = TestRunner::new(Config {
        cases: 30,
        failure_persistence: None,
        ..Config::default()
    });
    let strategy = vec(op(), 1..30);

    for case in 0..30 {
        let ops = strategy.new_tree(&mut runner).unwrap().current();
        // A fresh set of accounts per case, so cases cannot affect each other.
        let treasury = account(&pool, "treasury", Side::Debit).await;
        let mut users = Vec::new();
        for _ in 0..USERS {
            users.push(account(&pool, "user_wallet", Side::Credit).await);
        }
        // What the balances should be, tracked independently of the database.
        let (mut model, mut model_treasury) = ([0i64; USERS], 0i64);
        let mut last: Option<Posting> = None;

        for (i, op) in ops.iter().enumerate() {
            let key = IdempotencyKey::new(&format!("prop-{case}-{i}-op")).unwrap();
            let build =
                |entries: Vec<Entry>| Posting::new(key.clone(), "random", None, entries).unwrap();
            let (posting, expect_ok) = match *op {
                Op::Deposit { to, minor } => (
                    build(vec![
                        Entry::debit(treasury, usdc(minor)),
                        Entry::credit(users[to], usdc(minor)),
                    ]),
                    true,
                ),
                Op::Transfer { from, to, minor } if from != to => (
                    build(vec![
                        Entry::debit(users[from], usdc(minor)),
                        Entry::credit(users[to], usdc(minor)),
                    ]),
                    model[from] >= minor,
                ),
                Op::Transfer { .. } => continue,
                Op::Withdraw { from, minor } => (
                    build(vec![
                        Entry::debit(users[from], usdc(minor)),
                        Entry::credit(treasury, usdc(minor)),
                    ]),
                    model[from] >= minor,
                ),
                Op::Replay => {
                    if let Some(previous) = &last {
                        assert!(
                            matches!(
                                post(&pool, previous).await.unwrap(),
                                PostOutcome::Replayed(_)
                            ),
                            "a retry must not post again"
                        );
                    }
                    continue;
                }
            };
            match post(&pool, &posting).await {
                Ok(PostOutcome::Posted(_)) => {
                    assert!(expect_ok, "case {case}: {op:?} should have been refused");
                    match *op {
                        Op::Deposit { to, minor } => {
                            (model[to], model_treasury) =
                                (model[to] + minor, model_treasury + minor)
                        }
                        Op::Transfer { from, to, minor } => {
                            (model[from], model[to]) = (model[from] - minor, model[to] + minor)
                        }
                        Op::Withdraw { from, minor } => {
                            (model[from], model_treasury) =
                                (model[from] - minor, model_treasury - minor)
                        }
                        Op::Replay => unreachable!(),
                    }
                    last = Some(posting);
                }
                Err(LedgerStoreError::InsufficientFunds) => assert!(
                    !expect_ok,
                    "case {case}: {op:?} was refused but the account had the money"
                ),
                other => panic!("case {case}: unexpected {other:?} for {op:?}"),
            }
        }

        // The invariants, checked against the database after every case.
        for (u, expected) in users.iter().zip(model) {
            let actual = balance(&pool, *u).await.unwrap().minor;
            assert_eq!(actual, expected, "case {case}: a wallet balance drifted");
            assert!(actual >= 0, "case {case}: a wallet went negative");
        }
        assert_eq!(
            balance(&pool, treasury).await.unwrap().minor,
            model_treasury
        );
        assert_eq!(
            model.iter().sum::<i64>(),
            model_treasury,
            "case {case}: the treasury must hold exactly what customers are owed"
        );
    }

    // Across every case together: debits equal credits per asset, and no stored balance disagrees with its entries.
    let (debits, credits): (i64, i64) = sqlx::query_as(
        "SELECT COALESCE(sum(amount_minor) FILTER (WHERE side = 'debit'), 0)::bigint, COALESCE(sum(amount_minor) FILTER (WHERE side = 'credit'), 0)::bigint FROM ledger_entries",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(debits, credits, "total debits must equal total credits");
    assert!(debits > 0, "the test actually moved money");
    assert!(
        reconcile(&pool).await.unwrap().is_empty(),
        "stored balances must match the entries"
    );
}
