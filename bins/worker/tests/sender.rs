//! The withdrawal sender against a simulated Stellar network and a real Postgres (`make dev`, then
//! `make test-integration`).
//!
//! The simulated network checks what the real one checks (the sequence number, the expiry) and can be told to land a
//! payment, fail it, lose it, or answer "timeout" although it landed. Whatever it does, a player is paid exactly once
//! or refunded, never both.

use std::collections::HashMap;
use std::sync::Mutex;

use eschess_domain::money::{Amount, AssetId};
use eschess_domain::stellar::{AccountKey, Address};
use eschess_ledger::{Entry, IdempotencyKey, Posting, Side};
use eschess_stellar::signer::LocalSigner;
use eschess_stellar::tx::{Network, PaymentMemo};
use eschess_stellar::{AccountState, Chain, ChainError, Page, Submitted, TxOutcome};
use eschess_storage::ledger::{balance, post, reconcile};
use eschess_storage::stellar::{system_account, user_wallet, CUSTODY_KIND};
use eschess_storage::withdrawals::{
    self, AttemptStatus, NewWithdrawal, Policy, Status, PENDING_KIND,
};
use eschess_worker::sender::{tick, SenderConfig};
use sqlx::PgPool;
use stellar_xdr::{
    Limits, Memo, MuxedAccount, OperationBody, Preconditions, ReadXdr, TransactionEnvelope,
};
use uuid::Uuid;

// Throwaway test key (crates/stellar/tests/data/generate_payment_vectors.py); it holds nothing on any network.
const SECRET: &str = "SA3PXIPSNIG2RVLSIREL6KV6PQJTFI7PA4ZBYVP772LORMETDV5FQTNT";
const HOT: &str = "GCLSLSBBGOP2GTENWWKA2DXRTNWMRBE6KGVV4UNZK7LMIGMCOQ6EJT2J";
const DEST: &str = "GBUUQEXJEVAN2FLTOEBQ2OUJREAQOAEYYNW5R2X7O7RE4LZ2DNMBUEAV";
const ISSUER: &str = "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5";
const NOW: i64 = 1_800_000_000;
const POLICY: Policy = Policy {
    auto_approve_up_to_minor: i64::MAX,
    daily_limit_minor: i64::MAX,
};

fn usdc() -> AssetId {
    AssetId::credit("USDC", ISSUER).unwrap()
}

/// What the simulated network does with the next new transaction submitted to it.
#[derive(Clone, Copy, Debug)]
enum Next {
    /// It lands and pays.
    Land,
    /// It lands and fails with this payment result (e.g. "no_trust").
    Fail(&'static str),
    /// It is lost: the answer is a timeout, and it never lands.
    Lose,
    /// It lands, but the answer is a timeout.
    LandButTimeOut,
}

struct Net {
    sequence: Mutex<i64>,
    closed_at: Mutex<i64>,
    next: Mutex<Vec<Next>>,
    ledger: Mutex<HashMap<String, TxOutcome>>,
    submitted: Mutex<Vec<String>>,
    down: Mutex<bool>,
}

impl Net {
    fn new() -> Net {
        Net {
            sequence: Mutex::new(5_000),
            closed_at: Mutex::new(NOW),
            next: Mutex::new(vec![]),
            ledger: Mutex::new(HashMap::new()),
            submitted: Mutex::new(vec![]),
            down: Mutex::new(false),
        }
    }
    fn will(&self, behaviours: &[Next]) {
        *self.next.lock().unwrap() = behaviours.to_vec();
    }
    fn close_ledger_at(&self, t: i64) {
        *self.closed_at.lock().unwrap() = t;
    }
    fn hashes_submitted(&self) -> Vec<String> {
        let mut h: Vec<String> = self
            .submitted
            .lock()
            .unwrap()
            .iter()
            .map(|e| hash_of(e))
            .collect();
        h.dedup();
        h
    }
}

fn decode(envelope: &str) -> stellar_xdr::Transaction {
    match TransactionEnvelope::from_xdr_base64(envelope, Limits::none()).unwrap() {
        TransactionEnvelope::Tx(e) => e.tx,
        other => panic!("unexpected envelope {other:?}"),
    }
}

fn hash_of(envelope: &str) -> String {
    let h = TransactionEnvelope::from_xdr_base64(envelope, Limits::none())
        .unwrap()
        .hash(Network::Testnet.id())
        .unwrap();
    h.iter().map(|b| format!("{b:02x}")).collect()
}

#[async_trait::async_trait]
impl Chain for Net {
    async fn payments(&self, _: &str, _: Option<&str>, _: u32) -> Result<Page, ChainError> {
        Ok(Page {
            records: vec![],
            next_cursor: None,
        })
    }

    async fn account(&self, account: &str) -> Result<Option<AccountState>, ChainError> {
        if *self.down.lock().unwrap() {
            return Err(ChainError::Unavailable("down".into()));
        }
        assert_eq!(account, HOT);
        Ok(Some(AccountState {
            sequence: *self.sequence.lock().unwrap(),
            can_receive: vec![usdc()],
        }))
    }

    async fn submit(&self, envelope: &str) -> Result<Submitted, ChainError> {
        if *self.down.lock().unwrap() {
            return Err(ChainError::Unavailable("down".into()));
        }
        self.submitted.lock().unwrap().push(envelope.to_string());
        let hash = hash_of(envelope);
        if self.ledger.lock().unwrap().contains_key(&hash) {
            return Ok(Submitted::Refused("tx_bad_seq".into())); // already applied: its sequence is used up
        }
        let tx = decode(envelope);
        let Preconditions::Time(bounds) = tx.cond else {
            panic!("no timebounds")
        };
        if bounds.max_time.0 as i64 <= *self.closed_at.lock().unwrap() {
            return Ok(Submitted::Refused("tx_too_late".into()));
        }
        let mut sequence = self.sequence.lock().unwrap();
        if tx.seq_num.0 != *sequence + 1 {
            return Ok(Submitted::Refused("tx_bad_seq".into()));
        }
        let behaviour = {
            let mut next = self.next.lock().unwrap();
            if next.is_empty() {
                Next::Land
            } else {
                next.remove(0)
            }
        };
        let land = |ok: bool, code: String| {
            self.ledger.lock().unwrap().insert(
                hash.clone(),
                TxOutcome {
                    successful: ok,
                    fee_charged: 100,
                    result_code: code,
                },
            );
        };
        match behaviour {
            Next::Lose => Err(ChainError::Unavailable("504 timeout".into())),
            Next::Land => {
                *sequence += 1;
                land(true, "tx_success".into());
                Ok(Submitted::Accepted)
            }
            Next::LandButTimeOut => {
                *sequence += 1;
                land(true, "tx_success".into());
                Err(ChainError::Unavailable("504 timeout".into()))
            }
            Next::Fail(op) => {
                *sequence += 1;
                land(false, format!("tx_failed: payment_{op}"));
                Ok(Submitted::Refused(format!("tx_failed: op_{op}")))
            }
        }
    }

    async fn transaction(&self, hash: &str) -> Result<Option<TxOutcome>, ChainError> {
        if *self.down.lock().unwrap() {
            return Err(ChainError::Unavailable("down".into()));
        }
        Ok(self.ledger.lock().unwrap().get(hash).cloned())
    }

    async fn latest_ledger_close_time(&self) -> Result<i64, ChainError> {
        if *self.down.lock().unwrap() {
            return Err(ChainError::Unavailable("down".into()));
        }
        Ok(*self.closed_at.lock().unwrap())
    }
}

fn config() -> SenderConfig {
    SenderConfig {
        network: Network::Testnet,
        hot_account: AccountKey::parse(HOT).unwrap(),
        fee_stroops: 1_000,
        tx_timeout_secs: 120,
        expiry_margin_secs: 60,
    }
}

fn signer() -> LocalSigner {
    LocalSigner::from_secret(SECRET, Network::Testnet).unwrap()
}

async fn round(pool: &PgPool, net: &Net, now: i64) -> eschess_worker::TickReport {
    tick(pool, net, &signer(), &config(), now).await.unwrap()
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
    let custody = system_account(pool, CUSTODY_KIND, &usdc(), Side::Debit)
        .await
        .unwrap();
    let wallet = user_wallet(pool, id, &usdc()).await.unwrap();
    let amount = Amount::new(1_000_000_000, usdc()).unwrap();
    post(
        pool,
        &Posting::new(
            IdempotencyKey::new(&format!("test-deposit:{id}")).unwrap(),
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
    id
}

/// An approved withdrawal of `minor` to `destination`.
async fn approved(pool: &PgPool, minor: i64, destination: &str, memo: PaymentMemo) -> (Uuid, Uuid) {
    let u = user(pool).await;
    let (w, _) = withdrawals::request(
        pool,
        &NewWithdrawal {
            user_id: u,
            client_request_id: Uuid::now_v7(),
            amount: Amount::new(minor, usdc()).unwrap(),
            destination: destination.into(),
            memo,
        },
        POLICY,
    )
    .await
    .unwrap();
    assert_eq!(w.status, Status::Approved);
    (u, w.id)
}

async fn status(pool: &PgPool, id: Uuid) -> Status {
    withdrawals::get(pool, id).await.unwrap().status
}

async fn wallet(pool: &PgPool, u: Uuid) -> i64 {
    balance(pool, user_wallet(pool, u, &usdc()).await.unwrap())
        .await
        .unwrap()
        .minor
}

async fn pending_total(pool: &PgPool) -> i64 {
    let a = system_account(pool, PENDING_KIND, &usdc(), Side::Credit)
        .await
        .unwrap();
    balance(pool, a).await.unwrap().minor
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn an_approved_withdrawal_is_signed_recorded_sent_and_settled(pool: PgPool) {
    let net = Net::new();
    let muxed = AccountKey::parse(DEST).unwrap().muxed(77).to_m();
    let (u, id) = approved(&pool, 125_000_000, &muxed, PaymentMemo::Id(9)).await;

    let r = round(&pool, &net, NOW).await;
    assert_eq!((r.sent, r.confirmed.clone()), (Some(id), vec![id]));
    assert_eq!(status(&pool, id).await, Status::Confirmed);
    assert_eq!(wallet(&pool, u).await, 1_000_000_000 - 125_000_000);
    assert_eq!(pending_total(&pool).await, 0);

    // What went out is exactly what was asked for.
    let sent = net.submitted.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    let tx = decode(&sent[0]);
    assert_eq!(tx.seq_num.0, 5_001, "the hot wallet's sequence plus one");
    assert_eq!(tx.fee, 1_000);
    let Preconditions::Time(bounds) = tx.cond else {
        panic!()
    };
    assert_eq!(
        bounds.max_time.0 as i64,
        NOW + 120,
        "every transaction expires"
    );
    assert_eq!(tx.memo, Memo::Id(9));
    let OperationBody::Payment(p) = &tx.operations[0].body else {
        panic!()
    };
    assert_eq!(p.amount, 125_000_000);
    let MuxedAccount::MuxedEd25519(m) = &p.destination else {
        panic!("an M address stays muxed")
    };
    assert_eq!(
        (m.id, Address::parse(&muxed).unwrap().account().0),
        (77, m.ed25519.0)
    );

    let attempts = withdrawals::attempts(&pool, id).await.unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(
        attempts[0].tx_hash,
        hash_of(&sent[0]),
        "the recorded hash is the one sent"
    );
    assert!(reconcile(&pool).await.unwrap().is_empty());
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_timeout_after_landing_is_found_by_hash_and_never_paid_twice(pool: PgPool) {
    let net = Net::new();
    let (_, id) = approved(&pool, 50_000_000, DEST, PaymentMemo::None).await;
    net.will(&[Next::LandButTimeOut]);
    let r = round(&pool, &net, NOW).await;
    assert_eq!(
        (r.sent, r.confirmed.len()),
        (Some(id), 0),
        "unknown: not counted as paid yet"
    );
    assert_eq!(status(&pool, id).await, Status::Submitting);

    let r = round(&pool, &net, NOW + 5).await;
    assert_eq!(r.confirmed, vec![id]);
    assert_eq!(status(&pool, id).await, Status::Confirmed);
    assert_eq!(
        net.hashes_submitted().len(),
        1,
        "one transaction, however many submissions"
    );
    assert_eq!(*net.sequence.lock().unwrap(), 5_001, "applied once");
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_lost_transaction_is_resent_unchanged_until_it_provably_expires_then_rebuilt(
    pool: PgPool,
) {
    let net = Net::new();
    let (u, id) = approved(&pool, 50_000_000, DEST, PaymentMemo::None).await;
    let other = approved(&pool, 70_000_000, DEST, PaymentMemo::None).await.1;
    // Lost on the first try. (Re-submitting it later is refused as too late, before anything else happens to it.)
    net.will(&[Next::Lose]);
    round(&pool, &net, NOW).await;
    let first_hash = withdrawals::attempts(&pool, id).await.unwrap()[0]
        .tx_hash
        .clone();

    // Still within max_time (+ margin): the same envelope is re-sent, nothing new is built, nothing else is sent.
    net.close_ledger_at(NOW + 120 + 60);
    let r = round(&pool, &net, NOW + 180).await;
    assert_eq!((r.still_waiting, r.sent, r.expired), (1, None, 0));
    assert_eq!(net.hashes_submitted(), vec![first_hash.clone()]);
    assert_eq!(
        status(&pool, other).await,
        Status::Approved,
        "one payment in flight at a time"
    );

    // The network has closed a ledger past max_time + margin and the hash is not there: it can never land.
    net.close_ledger_at(NOW + 120 + 61);
    let r = round(&pool, &net, NOW + 181).await;
    assert_eq!(
        (r.expired, r.sent, r.confirmed.clone()),
        (1, Some(id), vec![id])
    );
    let attempts = withdrawals::attempts(&pool, id).await.unwrap();
    assert_eq!(
        attempts
            .iter()
            .map(|a| a.status.clone())
            .collect::<Vec<_>>(),
        vec![AttemptStatus::Expired, AttemptStatus::Success]
    );
    assert_ne!(attempts[1].tx_hash, first_hash);
    assert_eq!(
        wallet(&pool, u).await,
        1_000_000_000 - 50_000_000,
        "paid once"
    );

    // Now the next one goes.
    let r = round(&pool, &net, NOW + 190).await;
    assert_eq!(r.confirmed, vec![other]);
    assert!(reconcile(&pool).await.unwrap().is_empty());
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_payment_that_fails_on_the_network_is_refunded_with_the_reason(pool: PgPool) {
    let net = Net::new();
    let (u, id) = approved(&pool, 50_000_000, DEST, PaymentMemo::None).await;
    net.will(&[Next::Fail("no_trust")]);
    let r = round(&pool, &net, NOW).await;
    assert_eq!(r.sent, Some(id));
    assert_eq!(status(&pool, id).await, Status::Submitting);

    let r = round(&pool, &net, NOW + 5).await;
    assert_eq!(r.failed, vec![id]);
    let w = withdrawals::get(&pool, id).await.unwrap();
    assert_eq!(w.status, Status::Failed);
    assert!(w.failure_reason.unwrap().contains("trustline"));
    assert_eq!(wallet(&pool, u).await, 1_000_000_000, "refunded");
    assert_eq!(pending_total(&pool).await, 0);
    let a = &withdrawals::attempts(&pool, id).await.unwrap()[0];
    assert_eq!(
        a.result_code.as_deref(),
        Some("tx_failed: payment_no_trust")
    );
    assert_eq!(a.fee_charged_stroops, Some(100));
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_crash_between_recording_a_failure_and_refunding_is_repaired(pool: PgPool) {
    let net = Net::new();
    let (u, id) = approved(&pool, 50_000_000, DEST, PaymentMemo::None).await;
    net.will(&[Next::Fail("underfunded")]);
    round(&pool, &net, NOW).await;
    // The failure is recorded, then the worker dies before refunding.
    let a = withdrawals::attempts(&pool, id).await.unwrap()[0].clone();
    withdrawals::attempt_failed(&pool, a.id, "tx_failed: payment_underfunded", 100)
        .await
        .unwrap();
    assert_eq!(status(&pool, id).await, Status::Submitting);

    let r = round(&pool, &net, NOW + 5).await;
    assert_eq!(
        r.sent, None,
        "a failed withdrawal is never retried with a new transaction"
    );
    assert_eq!(status(&pool, id).await, Status::Failed);
    assert_eq!(wallet(&pool, u).await, 1_000_000_000);
    assert_eq!(net.hashes_submitted().len(), 1);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn when_the_network_cannot_be_reached_nothing_is_decided(pool: PgPool) {
    let net = Net::new();
    let (_, id) = approved(&pool, 50_000_000, DEST, PaymentMemo::None).await;
    net.will(&[Next::Lose]);
    round(&pool, &net, NOW).await;
    *net.down.lock().unwrap() = true;
    net.close_ledger_at(NOW + 10_000);
    assert!(tick(&pool, &net, &signer(), &config(), NOW + 10_000)
        .await
        .is_err());
    let a = &withdrawals::attempts(&pool, id).await.unwrap();
    assert_eq!(
        (a.len(), a[0].status.clone()),
        (1, AttemptStatus::Pending),
        "not expired on a guess"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn withdrawals_waiting_for_approval_are_not_sent(pool: PgPool) {
    let net = Net::new();
    let u = user(&pool).await;
    let (w, _) = withdrawals::request(
        &pool,
        &NewWithdrawal {
            user_id: u,
            client_request_id: Uuid::now_v7(),
            amount: Amount::new(50_000_000, usdc()).unwrap(),
            destination: DEST.into(),
            memo: PaymentMemo::None,
        },
        Policy {
            auto_approve_up_to_minor: 0,
            ..POLICY
        },
    )
    .await
    .unwrap();
    let r = round(&pool, &net, NOW).await;
    assert_eq!(r.sent, None);
    assert_eq!(status(&pool, w.id).await, Status::AwaitingApproval);
    assert!(net.submitted.lock().unwrap().is_empty());
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn workers_running_at_once_send_each_withdrawal_once(pool: PgPool) {
    let net = Net::new();
    let mut ids = vec![];
    for _ in 0..4 {
        ids.push(approved(&pool, 10_000_000, DEST, PaymentMemo::None).await.1);
    }
    // Many workers, many rounds, all at once.
    let (signer, config) = (signer(), config());
    for step in 0..6 {
        let rounds = (0..5).map(|_| tick(&pool, &net, &signer, &config, NOW + step));
        for r in futures::future::join_all(rounds).await {
            r.unwrap();
        }
    }
    for id in &ids {
        assert_eq!(status(&pool, *id).await, Status::Confirmed);
        let a = withdrawals::attempts(&pool, *id).await.unwrap();
        assert_eq!(a.len(), 1, "one transaction per withdrawal");
    }
    assert_eq!(
        *net.sequence.lock().unwrap(),
        5_004,
        "four payments applied, no more"
    );
    assert!(reconcile(&pool).await.unwrap().is_empty());
}

#[test]
fn sender_settings_that_would_break_sending_are_refused() {
    let hot = AccountKey::parse(HOT).unwrap();
    let load = |pairs: &[(&str, &str)]| {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        SenderConfig::from_lookup(&|n| map.get(n).cloned(), Network::Testnet, hot)
    };
    let d = load(&[]).unwrap();
    assert_eq!(
        (d.fee_stroops, d.tx_timeout_secs, d.expiry_margin_secs),
        (1_000, 120, 60)
    );
    assert!(load(&[("WITHDRAW_FEE_STROOPS", "99")]).is_err());
    assert!(load(&[("WITHDRAW_FEE_STROOPS", "lots")]).is_err());
    assert!(load(&[("WITHDRAW_TX_TIMEOUT_SECS", "0")]).is_err());
    assert!(load(&[("WITHDRAW_EXPIRY_MARGIN_SECS", "-1")]).is_err());
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_player_with_an_open_fair_play_case_waits_and_does_not_block_others(pool: PgPool) {
    let net = Net::new();
    let (held_user, held) = approved(&pool, 10_000_000, DEST, PaymentMemo::None).await;
    let (_, other) = approved(&pool, 20_000_000, DEST, PaymentMemo::None).await;
    let case = Uuid::now_v7();
    sqlx::query("INSERT INTO fairplay_cases (id, player_id) VALUES ($1, $2)")
        .bind(case)
        .bind(held_user)
        .execute(&pool)
        .await
        .unwrap();

    // The older, held withdrawal is skipped; the other player's is sent.
    let r = round(&pool, &net, NOW).await;
    assert_eq!(r.sent, Some(other));
    assert_eq!(status(&pool, other).await, Status::Confirmed);
    assert_eq!(status(&pool, held).await, Status::Approved);

    // Nothing else goes out while the case is open.
    let r = round(&pool, &net, NOW + 10).await;
    assert_eq!(r.sent, None);
    assert_eq!(status(&pool, held).await, Status::Approved);

    // Cleared: it is sent, unchanged.
    let reviewer = user(&pool).await;
    sqlx::query("UPDATE fairplay_cases SET status = 'cleared', decided_by = $2, decided_at = now(), reason = 'looked fine' WHERE id = $1")
        .bind(case)
        .bind(reviewer)
        .execute(&pool)
        .await
        .unwrap();
    let r = round(&pool, &net, NOW + 20).await;
    assert_eq!(r.sent, Some(held));
    assert_eq!(status(&pool, held).await, Status::Confirmed);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_closed_accounts_withdrawal_always_waits_for_a_person(pool: PgPool) {
    let net = Net::new();
    let closed = user(&pool).await;
    let (proposer, second) = (user(&pool).await, user(&pool).await);
    let (case, sanction) = (Uuid::now_v7(), Uuid::now_v7());
    sqlx::query("INSERT INTO fairplay_cases (id, player_id, status, decided_by, decided_at, reason) VALUES ($1, $2, 'confirmed', $3, now(), 'x')")
        .bind(case).bind(closed).bind(proposer).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO fairplay_sanctions (id, case_id, player_id, kind) VALUES ($1, $2, $3, 'closure')")
        .bind(sanction).bind(case).bind(closed).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO fairplay_closure_reviews (sanction_id, reviewer_id, decision, reason) VALUES ($1, $2, 'approved', 'x')")
        .bind(sanction).bind(second).execute(&pool).await.unwrap();

    // Small enough to be approved at once for anyone else (see `approved`), but not for a closed account.
    let (w, _) = withdrawals::request(
        &pool,
        &NewWithdrawal {
            user_id: closed,
            client_request_id: Uuid::now_v7(),
            amount: Amount::new(1_000_000, usdc()).unwrap(),
            destination: DEST.into(),
            memo: PaymentMemo::None,
        },
        POLICY,
    )
    .await
    .unwrap();
    assert_eq!(w.status, Status::AwaitingApproval);
    assert_eq!(round(&pool, &net, NOW).await.sent, None);
}
