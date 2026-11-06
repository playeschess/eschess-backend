//! Withdrawals against a real Postgres (`make dev`, then `make test-integration`).
//!
//! The first group goes through the storage API. The second goes around it with raw SQL, to show the database refuses
//! a double payout, a changed amount or a backwards step on its own. The last is randomised: whatever order things
//! happen in, money is never created or lost, and no withdrawal is both paid out and refunded.

use eschess_domain::money::{Amount, AssetId};
use eschess_ledger::{AccountId, Entry, IdempotencyKey, Posting, Side};
use eschess_stellar::tx::PaymentMemo;
use eschess_storage::ledger::{balance, post, reconcile};
use eschess_storage::stellar::{system_account, user_wallet, CUSTODY_KIND};
use eschess_storage::withdrawals::{
    self, approve, attempt_expired, attempt_failed, attempt_landed, cancel, give_up, grant_role,
    record_attempt, reject, request, AttemptStatus, NewAttempt, NewWithdrawal, Policy, Status,
    WithdrawalError, APPROVER_ROLE, PENDING_KIND,
};
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::{Config, TestRunner};
use sqlx::PgPool;
use uuid::Uuid;

const ISSUER: &str = "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5";
const DEST: &str = "GBUUQEXJEVAN2FLTOEBQ2OUJREAQOAEYYNW5R2X7O7RE4LZ2DNMBUEAV";
const HOT: &str = "GCLSLSBBGOP2GTENWWKA2DXRTNWMRBE6KGVV4UNZK7LMIGMCOQ6EJT2J";
/// Above 1,000 a person must approve; a high daily limit, so only the daily-limit test meets it.
const AUTO: i64 = 1_000;
const POLICY: Policy = Policy {
    auto_approve_up_to_minor: AUTO,
    daily_limit_minor: 1_000_000,
};

fn usdc() -> AssetId {
    AssetId::credit("USDC", ISSUER).unwrap()
}
fn amount(minor: i64) -> Amount {
    Amount::new(minor, usdc()).unwrap()
}

async fn user(pool: &PgPool) -> Uuid {
    let id = Uuid::now_v7();
    let name = format!("u{}", &id.simple().to_string()[20..]);
    sqlx::query("INSERT INTO users (id, email, username, password_hash) VALUES ($1, $2, $3, 'x')")
        .bind(id)
        .bind(format!("{name}@example.com"))
        .bind(&name)
        .execute(pool)
        .await
        .unwrap();
    id
}

/// A player with `minor` in their wallet, deposited the way real deposits are (custody -> wallet).
async fn funded(pool: &PgPool, minor: i64) -> Uuid {
    let u = user(pool).await;
    let custody = system_account(pool, CUSTODY_KIND, &usdc(), Side::Debit)
        .await
        .unwrap();
    let wallet = user_wallet(pool, u, &usdc()).await.unwrap();
    let key = IdempotencyKey::new(&format!("test-deposit:{u}")).unwrap();
    post(
        pool,
        &Posting::new(
            key,
            "deposit",
            None,
            vec![
                Entry::debit(custody, amount(minor)),
                Entry::credit(wallet, amount(minor)),
            ],
        )
        .unwrap(),
    )
    .await
    .unwrap();
    u
}

async fn minor(pool: &PgPool, a: AccountId) -> i64 {
    balance(pool, a).await.unwrap().minor
}
async fn wallet_of(pool: &PgPool, u: Uuid) -> i64 {
    minor(pool, user_wallet(pool, u, &usdc()).await.unwrap()).await
}
async fn pending(pool: &PgPool) -> i64 {
    minor(
        pool,
        system_account(pool, PENDING_KIND, &usdc(), Side::Credit)
            .await
            .unwrap(),
    )
    .await
}
async fn custody(pool: &PgPool) -> i64 {
    minor(
        pool,
        system_account(pool, CUSTODY_KIND, &usdc(), Side::Debit)
            .await
            .unwrap(),
    )
    .await
}

fn new(u: Uuid, minor: i64) -> NewWithdrawal {
    NewWithdrawal {
        user_id: u,
        client_request_id: Uuid::now_v7(),
        amount: amount(minor),
        destination: DEST.into(),
        memo: PaymentMemo::Id(42),
    }
}

/// A recorded attempt; `n` makes the hash and sequence distinct.
fn attempt(n: i64) -> NewAttempt {
    NewAttempt {
        tx_hash: format!("{n:064x}"),
        source_account: HOT.into(),
        sequence: 1_000 + n,
        fee_stroops: 100,
        max_time: 1_700_000_000 + n,
        envelope_xdr: format!("AAAA{n}"),
    }
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_request_holds_the_money_at_once(pool: PgPool) {
    let u = funded(&pool, 5_000).await;
    let (w, created) = request(&pool, &new(u, 700), POLICY).await.unwrap();
    assert!(created);
    assert_eq!(w.status, Status::Approved, "at or below the threshold");
    assert_eq!(w.memo, PaymentMemo::Id(42));
    assert_eq!(wallet_of(&pool, u).await, 4_300);
    assert_eq!(pending(&pool).await, 700);

    let (big, _) = request(&pool, &new(u, AUTO + 1), POLICY).await.unwrap();
    assert_eq!(big.status, Status::AwaitingApproval, "above the threshold");
    assert_eq!(wallet_of(&pool, u).await, 4_300 - AUTO - 1);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn more_than_the_balance_is_refused_and_leaves_nothing_behind(pool: PgPool) {
    let u = funded(&pool, 500).await;
    let err = request(&pool, &new(u, 501), POLICY).await.unwrap_err();
    assert!(matches!(err, WithdrawalError::InsufficientFunds), "{err:?}");
    assert!(withdrawals::of_user(&pool, u, 10).await.unwrap().is_empty());
    assert_eq!(wallet_of(&pool, u).await, 500);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_retried_request_is_the_same_withdrawal_and_holds_once(pool: PgPool) {
    let u = funded(&pool, 1_000).await;
    let req = new(u, 300);
    let (first, _) = request(&pool, &req, POLICY).await.unwrap();
    let (again, created) = request(&pool, &req, POLICY).await.unwrap();
    assert!(!created);
    assert_eq!(again.id, first.id);
    assert_eq!(wallet_of(&pool, u).await, 700);

    let mut changed = req.clone();
    changed.amount = amount(301);
    assert!(matches!(
        request(&pool, &changed, POLICY).await.unwrap_err(),
        WithdrawalError::RequestIdReused
    ));
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn racing_requests_cannot_both_spend_the_same_money(pool: PgPool) {
    let u = funded(&pool, 1_000).await;
    let tries = (0..8).map(|_| {
        let pool = pool.clone();
        async move { request(&pool, &new(u, 600), POLICY).await }
    });
    let results = futures::future::join_all(tries).await;
    let ok = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(ok, 1, "{results:?}");
    assert!(
        results
            .iter()
            .filter_map(|r| r.as_ref().err())
            .all(|e| matches!(e, WithdrawalError::InsufficientFunds)),
        "{results:?}"
    );
    assert_eq!(wallet_of(&pool, u).await, 400);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_player_can_cancel_only_their_own_and_only_before_signing(pool: PgPool) {
    let u = funded(&pool, 1_000).await;
    let stranger = user(&pool).await;
    let (w, _) = request(&pool, &new(u, 400), POLICY).await.unwrap();
    assert!(matches!(
        cancel(&pool, w.id, stranger).await.unwrap_err(),
        WithdrawalError::NotFound
    ));
    let done = cancel(&pool, w.id, u).await.unwrap();
    assert_eq!(done.status, Status::Cancelled);
    assert!(done.finished_at_ms.is_some());
    assert_eq!(wallet_of(&pool, u).await, 1_000);
    assert_eq!(pending(&pool).await, 0);
    assert!(matches!(
        cancel(&pool, w.id, u).await.unwrap_err(),
        WithdrawalError::WrongState(Status::Cancelled)
    ));

    let (sent, _) = request(&pool, &new(u, 400), POLICY).await.unwrap();
    record_attempt(&pool, sent.id, &attempt(1)).await.unwrap();
    assert!(matches!(
        cancel(&pool, sent.id, u).await.unwrap_err(),
        WithdrawalError::WrongState(Status::Submitting)
    ));
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn only_an_approver_who_is_not_the_player_decides(pool: PgPool) {
    let u = funded(&pool, 10_000).await;
    let approver = user(&pool).await;
    grant_role(&pool, approver, APPROVER_ROLE).await.unwrap();
    grant_role(&pool, u, APPROVER_ROLE).await.unwrap(); // even an approver may not approve their own
    let nobody = user(&pool).await;

    let (w, _) = request(&pool, &new(u, 5_000), POLICY).await.unwrap();
    assert!(matches!(
        approve(&pool, w.id, nobody).await.unwrap_err(),
        WithdrawalError::NotApprover
    ));
    assert!(matches!(
        approve(&pool, w.id, u).await.unwrap_err(),
        WithdrawalError::OwnWithdrawal
    ));
    assert!(matches!(
        record_attempt(&pool, w.id, &attempt(1)).await.unwrap_err(),
        WithdrawalError::WrongState(Status::AwaitingApproval)
    ));
    let ok = approve(&pool, w.id, approver).await.unwrap();
    assert_eq!(
        (ok.status, ok.decided_by),
        (Status::Approved, Some(approver))
    );

    let (w2, _) = request(&pool, &new(u, 4_000), POLICY).await.unwrap();
    let no = reject(&pool, w2.id, approver, "destination flagged")
        .await
        .unwrap();
    assert_eq!(no.status, Status::Rejected);
    assert_eq!(no.failure_reason.as_deref(), Some("destination flagged"));
    assert_eq!(wallet_of(&pool, u).await, 10_000 - 5_000);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_new_attempt_waits_until_the_last_one_provably_cannot_land(pool: PgPool) {
    let u = funded(&pool, 1_000).await;
    let custody_before = custody(&pool).await;
    let (w, _) = request(&pool, &new(u, 600), POLICY).await.unwrap();

    let first = record_attempt(&pool, w.id, &attempt(1)).await.unwrap();
    assert_eq!(first.attempt_no, 1);
    assert_eq!(
        withdrawals::get(&pool, w.id).await.unwrap().status,
        Status::Submitting
    );
    assert!(matches!(
        record_attempt(&pool, w.id, &attempt(2)).await.unwrap_err(),
        WithdrawalError::AttemptStillOpen
    ));
    assert!(matches!(
        give_up(&pool, w.id, "x").await.unwrap_err(),
        WithdrawalError::CannotGiveUp
    ));

    attempt_expired(&pool, first.id).await.unwrap();
    // Reporting it again changes nothing; reporting a different outcome is refused.
    attempt_expired(&pool, first.id).await.unwrap();
    assert!(matches!(
        attempt_landed(&pool, first.id, 100).await.unwrap_err(),
        WithdrawalError::AttemptResolved(AttemptStatus::Expired)
    ));

    // An expired transaction used up nothing, so the retry may reuse its sequence number.
    let mut retry = attempt(2);
    retry.sequence = attempt(1).sequence;
    let second = record_attempt(&pool, w.id, &retry).await.unwrap();
    assert_eq!(second.attempt_no, 2);

    let done = attempt_landed(&pool, second.id, 100).await.unwrap();
    assert_eq!(done.status, Status::Confirmed);
    assert_eq!(pending(&pool).await, 0);
    assert_eq!(
        custody(&pool).await,
        custody_before - 600,
        "the money left the wallet"
    );
    assert_eq!(wallet_of(&pool, u).await, 400);

    // A repeated report of the same success does not settle twice.
    attempt_landed(&pool, second.id, 100).await.unwrap();
    assert_eq!(custody(&pool).await, custody_before - 600);
    assert!(matches!(
        give_up(&pool, w.id, "x").await.unwrap_err(),
        WithdrawalError::WrongState(Status::Confirmed)
    ));
    assert!(reconcile(&pool).await.unwrap().is_empty());
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_failed_payment_is_refunded_when_we_give_up(pool: PgPool) {
    let u = funded(&pool, 1_000).await;
    let (w, _) = request(&pool, &new(u, 600), POLICY).await.unwrap();
    let a = record_attempt(&pool, w.id, &attempt(1)).await.unwrap();
    let failed = attempt_failed(&pool, a.id, "op_no_trust", 100)
        .await
        .unwrap();
    assert_eq!(failed.status, AttemptStatus::Failed);
    assert_eq!(failed.result_code.as_deref(), Some("op_no_trust"));

    // A failed transaction used its sequence number; another one on it is refused.
    let mut same_seq = attempt(2);
    same_seq.sequence = attempt(1).sequence;
    assert!(record_attempt(&pool, w.id, &same_seq).await.is_err());

    let done = give_up(&pool, w.id, "the destination has no USDC trustline")
        .await
        .unwrap();
    assert_eq!(done.status, Status::Failed);
    assert_eq!(wallet_of(&pool, u).await, 1_000);
    assert_eq!(pending(&pool).await, 0);
    assert!(matches!(
        record_attempt(&pool, w.id, &attempt(3)).await.unwrap_err(),
        WithdrawalError::WrongState(Status::Failed)
    ));
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn the_daily_total_counts_only_money_that_may_leave(pool: PgPool) {
    let u = funded(&pool, 10_000).await;
    let policy = Policy {
        daily_limit_minor: 1_000,
        ..POLICY
    };
    request(&pool, &new(u, 300), policy).await.unwrap();
    let (c, _) = request(&pool, &new(u, 500), policy).await.unwrap();
    cancel(&pool, c.id, u).await.unwrap();
    assert_eq!(
        withdrawals::withdrawn_recently(&pool, u, &usdc(), 24)
            .await
            .unwrap(),
        300
    );
    // 700 is left today: the cancelled 500 does not count.
    let over = request(&pool, &new(u, 701), policy).await.unwrap_err();
    assert!(
        matches!(
            over,
            WithdrawalError::DailyLimit {
                remaining_minor: 700
            }
        ),
        "{over:?}"
    );
    let req = new(u, 700);
    request(&pool, &req, policy).await.unwrap();
    // The limit is now reached, but retrying a request that was already accepted still answers with it.
    let (again, created) = request(&pool, &req, policy).await.unwrap();
    assert!(!created);
    assert_eq!(again.amount, amount(700));
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn requests_at_the_same_moment_cannot_together_pass_the_daily_limit(pool: PgPool) {
    let u = funded(&pool, 100_000).await;
    let policy = Policy {
        daily_limit_minor: 1_000,
        ..POLICY
    };
    let tries = (0..6).map(|_| {
        let pool = pool.clone();
        async move { request(&pool, &new(u, 400), policy).await }
    });
    let ok = futures::future::join_all(tries)
        .await
        .iter()
        .filter(|r| r.is_ok())
        .count();
    assert_eq!(ok, 2, "2 x 400 fit under 1,000; a third does not");
}

// The database on its own, going around the storage API.

async fn confirmed(pool: &PgPool) -> (Uuid, Uuid) {
    let u = funded(pool, 1_000).await;
    let (w, _) = request(pool, &new(u, 600), POLICY).await.unwrap();
    let a = record_attempt(pool, w.id, &attempt(1)).await.unwrap();
    attempt_landed(pool, a.id, 100).await.unwrap();
    (u, w.id)
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn the_database_refuses_a_refund_after_a_payout(pool: PgPool) {
    let (u, id) = confirmed(&pool).await;
    let pending = system_account(&pool, PENDING_KIND, &usdc(), Side::Credit)
        .await
        .unwrap();
    let wallet = user_wallet(&pool, u, &usdc()).await.unwrap();
    // A refund posted under a different key than the code would use: the outcome index still refuses it.
    let sneaky = Posting::new(
        IdempotencyKey::new(&format!("other:{id}")).unwrap(),
        "withdrawal_refund",
        Some(("withdrawal", &id.to_string())),
        vec![
            Entry::debit(pending, amount(600)),
            Entry::credit(wallet, amount(600)),
        ],
    )
    .unwrap();
    assert!(post(&pool, &sneaky).await.is_err());
    assert_eq!(wallet_of(&pool, u).await, 400);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn the_database_refuses_going_backwards_or_changing_what_was_asked(pool: PgPool) {
    let (_, id) = confirmed(&pool).await;
    for sql in [
        "UPDATE withdrawals SET status = 'failed', failure_reason = 'x', finished_at = now() WHERE id = $1",
        "UPDATE withdrawals SET status = 'submitting', finished_at = NULL WHERE id = $1",
        "UPDATE withdrawals SET amount_minor = 1 WHERE id = $1",
        "UPDATE withdrawals SET destination = 'GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZVN' WHERE id = $1",
        "UPDATE withdrawal_attempts SET status = 'expired' WHERE withdrawal_id = $1",
        "UPDATE withdrawal_attempts SET envelope_xdr = 'BBBB' WHERE withdrawal_id = $1",
        "DELETE FROM withdrawal_attempts WHERE withdrawal_id = $1",
        "DELETE FROM withdrawals WHERE id = $1",
    ] {
        let r = sqlx::query(sql).bind(id).execute(&pool).await;
        assert!(r.is_err(), "the database allowed: {sql}");
    }
    assert_eq!(
        withdrawals::get(&pool, id).await.unwrap().status,
        Status::Confirmed
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn the_database_refuses_a_second_open_attempt_and_self_approval(pool: PgPool) {
    let u = funded(&pool, 1_000).await;
    let (w, _) = request(&pool, &new(u, 600), POLICY).await.unwrap();
    record_attempt(&pool, w.id, &attempt(1)).await.unwrap();
    let raw = sqlx::query(
        "INSERT INTO withdrawal_attempts (id, withdrawal_id, attempt_no, tx_hash, source_account, sequence, fee_stroops, max_time, envelope_xdr) \
         VALUES ($1, $2, 2, $3, $4, 99, 100, 1, 'AAAA')",
    )
    .bind(Uuid::now_v7())
    .bind(w.id)
    .bind(format!("{:064x}", 77))
    .bind(HOT)
    .execute(&pool)
    .await;
    assert!(raw.is_err(), "two unresolved attempts for one withdrawal");

    let manual = Policy {
        auto_approve_up_to_minor: 0,
        ..POLICY
    };
    let (w2, _) = request(&pool, &new(u, 300), manual).await.unwrap();
    let own = sqlx::query(
        "UPDATE withdrawals SET status = 'approved', decided_by = user_id WHERE id = $1",
    )
    .bind(w2.id)
    .execute(&pool)
    .await;
    assert!(own.is_err(), "approved by its own requester");
}

// Randomised.

#[derive(Debug, Clone)]
enum Op {
    Request { player: usize, minor: i64 },
    Cancel(usize),
    Approve(usize),
    Reject(usize),
    Attempt(usize),
    Landed(usize),
    Failed(usize),
    Expired(usize),
    GiveUp(usize),
}

const PLAYERS: usize = 3;
const START: i64 = 2_000;

fn op() -> impl Strategy<Value = Op> {
    // Indexes pick among the withdrawals made so far (modulo their number).
    prop_oneof![
        3 => (0..PLAYERS, 1..1_500i64).prop_map(|(player, minor)| Op::Request { player, minor }),
        1 => any::<usize>().prop_map(Op::Cancel),
        1 => any::<usize>().prop_map(Op::Approve),
        1 => any::<usize>().prop_map(Op::Reject),
        3 => any::<usize>().prop_map(Op::Attempt),
        2 => any::<usize>().prop_map(Op::Landed),
        1 => any::<usize>().prop_map(Op::Failed),
        2 => any::<usize>().prop_map(Op::Expired),
        1 => any::<usize>().prop_map(Op::GiveUp),
    ]
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn whatever_happens_money_is_conserved_and_nothing_is_paid_twice(pool: PgPool) {
    let approver = user(&pool).await;
    grant_role(&pool, approver, APPROVER_ROLE).await.unwrap();
    let mut runner = TestRunner::new(Config {
        cases: 25,
        ..Config::default()
    });
    let strategy = proptest::collection::vec(op(), 1..40);
    let mut n = 0i64; // makes every attempt's hash and sequence unique across cases
    for case in 0..25 {
        let ops = strategy.new_tree(&mut runner).unwrap().current();
        let mut players = Vec::new();
        for _ in 0..PLAYERS {
            players.push(funded(&pool, START).await);
        }
        let custody_start = custody(&pool).await;
        let pending_start = pending(&pool).await;
        let mut made: Vec<Uuid> = Vec::new();
        let pick = |i: usize, made: &Vec<Uuid>| (!made.is_empty()).then(|| made[i % made.len()]);
        let open = |pool: PgPool, id: Uuid| async move {
            withdrawals::attempts(&pool, id)
                .await
                .unwrap()
                .into_iter()
                .find(|a| a.status == AttemptStatus::Pending)
        };

        for op in &ops {
            // Every operation either succeeds or is refused; neither may break the invariants below.
            match op.clone() {
                Op::Request { player, minor } => {
                    if let Ok((w, _)) = request(&pool, &new(players[player], minor), POLICY).await {
                        made.push(w.id);
                    }
                }
                Op::Cancel(i) => {
                    if let Some(id) = pick(i, &made) {
                        let owner = withdrawals::get(&pool, id).await.unwrap().user_id;
                        let _ = cancel(&pool, id, owner).await;
                    }
                }
                Op::Approve(i) => {
                    if let Some(id) = pick(i, &made) {
                        let _ = approve(&pool, id, approver).await;
                    }
                }
                Op::Reject(i) => {
                    if let Some(id) = pick(i, &made) {
                        let _ = reject(&pool, id, approver, "no").await;
                    }
                }
                Op::Attempt(i) => {
                    if let Some(id) = pick(i, &made) {
                        n += 1;
                        let _ = record_attempt(&pool, id, &attempt(n)).await;
                    }
                }
                Op::Landed(i) | Op::Failed(i) | Op::Expired(i) => {
                    if let Some(id) = pick(i, &made) {
                        if let Some(a) = open(pool.clone(), id).await {
                            let _ = match op {
                                Op::Landed(_) => attempt_landed(&pool, a.id, 100).await.map(|_| ()),
                                Op::Failed(_) => attempt_failed(&pool, a.id, "tx_failed", 100)
                                    .await
                                    .map(|_| ()),
                                _ => attempt_expired(&pool, a.id).await.map(|_| ()),
                            };
                        }
                    }
                }
                Op::GiveUp(i) => {
                    if let Some(id) = pick(i, &made) {
                        let _ = give_up(&pool, id, "gave up").await;
                    }
                }
            }
        }

        // Conservation: what players hold + what is held for them + what was paid out = what they started with.
        let mut held = 0;
        let mut paid = 0;
        for &id in &made {
            let w = withdrawals::get(&pool, id).await.unwrap();
            let attempts = withdrawals::attempts(&pool, id).await.unwrap();
            let landed = attempts
                .iter()
                .filter(|a| a.status == AttemptStatus::Success)
                .count();
            let outcomes: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM ledger_transactions WHERE reference_type = 'withdrawal' AND reference_id = $1 \
                 AND kind IN ('withdrawal_settle', 'withdrawal_refund')",
            )
            .bind(id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
            match w.status {
                Status::Confirmed => {
                    assert_eq!((landed, outcomes), (1, 1), "case {case}: {w:?}");
                    paid += w.amount.minor();
                }
                s if s.is_final() => assert_eq!((landed, outcomes), (0, 1), "case {case}: {w:?}"),
                _ => {
                    assert_eq!((landed, outcomes), (0, 0), "case {case}: {w:?}");
                    held += w.amount.minor();
                }
            }
            assert!(
                attempts
                    .iter()
                    .filter(|a| a.status == AttemptStatus::Pending)
                    .count()
                    <= 1,
                "case {case}: two open attempts"
            );
        }
        let mut in_wallets = 0;
        for &p in &players {
            let w = wallet_of(&pool, p).await;
            assert!(w >= 0);
            in_wallets += w;
        }
        assert_eq!(
            in_wallets + held + paid,
            START * PLAYERS as i64,
            "case {case}: {ops:?}"
        );
        assert_eq!(pending(&pool).await - pending_start, held, "case {case}");
        assert_eq!(custody_start - custody(&pool).await, paid, "case {case}");
    }
    assert!(reconcile(&pool).await.unwrap().is_empty());
}
