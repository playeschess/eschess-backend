//! Fiat sessions at a Stellar anchor, with a fake anchor and a real Postgres (`make dev`, then `make test-integration`):
//! deposits are only followed; a cash-out becomes an ordinary withdrawal, once, and only to an account the anchor lists.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use eschess_domain::money::{Amount, AssetId};
use eschess_domain::stellar::AccountKey;
use eschess_ledger::{Entry, IdempotencyKey, Posting, Side};
use eschess_stellar::custody::{KeyVault, LocalVault};
use eschess_stellar::sep::{
    Anchor, AnchorStatus, AnchorToken, AnchorTx, Interactive, Kind, SepError,
};
use eschess_stellar::signer::Signer;
use eschess_stellar::tx::{Network, PaymentMemo};
use eschess_storage::fiat::{self, Status};
use eschess_storage::ledger::{balance, post};
use eschess_storage::stellar::{system_account, user_wallet, CUSTODY_KIND};
use eschess_storage::wallets;
use eschess_storage::withdrawals::{self, Policy};
use eschess_worker::fiat::{tick, Config, Report};
use sqlx::PgPool;
use uuid::Uuid;

const ISSUER: &str = "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5";
const HOT: &str = "GBL4BGHUU6TAX7WXPSXAES7XBKEPJWLOO3HBIOYHNJH4KMLQZIQVZIF5";
const ANCHOR_ACCOUNT: &str = "GCSGSR6KQQ5BP2FXVPWRL6SWPUSFWLVONLIBJZUKTVQB5FYJFVL6XOXE";
const USDC: i64 = 10_000_000;

fn usdc() -> AssetId {
    AssetId::credit("USDC", ISSUER).unwrap()
}

#[derive(Default)]
struct FakeAnchor {
    txs: Mutex<HashMap<String, AnchorTx>>,
    accounts: Mutex<Vec<AccountKey>>,
    down: Mutex<bool>,
    logins: Mutex<u32>,
}

impl FakeAnchor {
    fn new() -> Arc<FakeAnchor> {
        let a = FakeAnchor::default();
        *a.accounts.lock().unwrap() = vec![AccountKey::parse(ANCHOR_ACCOUNT).unwrap()];
        Arc::new(a)
    }
    fn set(&self, id: &str, kind: &str, status: AnchorStatus) {
        self.txs.lock().unwrap().insert(
            id.into(),
            AnchorTx {
                id: id.into(),
                kind: kind.into(),
                status,
                amount_in: None,
                amount_out: None,
                withdraw_anchor_account: None,
                withdraw_memo: None,
                withdraw_memo_type: None,
                more_info_url: None,
                message: None,
                stellar_transaction_id: None,
            },
        );
    }
    /// The anchor asks for `amount` USDC to be sent to `account` with an id memo.
    fn ask(&self, id: &str, amount: &str, account: &str) {
        self.set(id, "withdrawal", AnchorStatus::PendingUserTransferStart);
        let mut txs = self.txs.lock().unwrap();
        let t = txs.get_mut(id).unwrap();
        t.amount_in = Some(amount.into());
        t.withdraw_anchor_account = Some(account.into());
        t.withdraw_memo = Some("424242".into());
        t.withdraw_memo_type = Some("id".into());
    }
    fn edit(&self, id: &str, f: impl FnOnce(&mut AnchorTx)) {
        f(self.txs.lock().unwrap().get_mut(id).unwrap());
    }
}

#[async_trait]
impl Anchor for FakeAnchor {
    fn home_domain(&self) -> &str {
        "anchor.example"
    }
    async fn accounts(&self) -> Result<Vec<AccountKey>, SepError> {
        Ok(self.accounts.lock().unwrap().clone())
    }
    async fn authenticate(&self, _: &dyn Signer) -> Result<AnchorToken, SepError> {
        if *self.down.lock().unwrap() {
            return Err(SepError::Unavailable("down".into()));
        }
        *self.logins.lock().unwrap() += 1;
        Ok(AnchorToken::new("jwt".into()))
    }
    async fn start(
        &self,
        _: &AnchorToken,
        _: Kind,
        _: &str,
        _: &AccountKey,
    ) -> Result<Interactive, SepError> {
        unreachable!()
    }
    async fn transaction(&self, _: &AnchorToken, id: &str) -> Result<AnchorTx, SepError> {
        self.txs
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| SepError::Rejected("no such transaction".into()))
    }
}

fn vault() -> Arc<dyn KeyVault> {
    Arc::new(LocalVault::from_bytes([9; 32], 1, Network::Testnet).unwrap())
}

fn cfg() -> Config {
    Config {
        usdc: usdc(),
        hot_account: AccountKey::parse(HOT).unwrap(),
        policy: Policy {
            auto_approve_up_to_minor: 100 * USDC,
            daily_limit_minor: 1_000 * USDC,
        },
        min_minor: USDC,
        stale_hours: 48,
        batch: 50,
    }
}

/// A player with a wallet key and `usdc_units` USDC in the ledger.
async fn player(pool: &PgPool, name: &str, usdc_units: i64) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO users (id, email, username, password_hash, email_verified_at) VALUES ($1, $2, $3, 'x', now())")
        .bind(id).bind(format!("{name}@example.com")).bind(name).execute(pool).await.unwrap();
    wallets::ensure_wallet(pool, &*vault(), id).await.unwrap();
    if usdc_units > 0 {
        let custody = system_account(pool, CUSTODY_KIND, &usdc(), Side::Debit)
            .await
            .unwrap();
        let wallet = user_wallet(pool, id, &usdc()).await.unwrap();
        let amount = Amount::new(usdc_units * USDC, usdc()).unwrap();
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
    }
    id
}

async fn funds(pool: &PgPool, u: Uuid) -> i64 {
    let a = user_wallet(pool, u, &usdc()).await.unwrap();
    balance(pool, a).await.unwrap().minor
}

async fn session(pool: &PgPool, user: Uuid, kind: fiat::Kind, id: &str) -> Uuid {
    fiat::create(pool, user, kind, "anchor.example", id)
        .await
        .unwrap()
        .id
}

async fn state(pool: &PgPool, id: Uuid) -> (String, Option<String>, Option<String>) {
    sqlx::query_as("SELECT status, anchor_status, message FROM fiat_sessions WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn run(pool: &PgPool, a: &Arc<FakeAnchor>) -> Report {
    tick(pool, &**a, vault(), &cfg()).await.unwrap()
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_deposit_is_only_followed_to_its_end(pool: PgPool) {
    let a = FakeAnchor::new();
    let ada = player(&pool, "ada", 0).await;
    let s = session(&pool, ada, fiat::Kind::Deposit, "d1").await;

    a.set("d1", "deposit", AnchorStatus::Incomplete);
    run(&pool, &a).await;
    assert_eq!(state(&pool, s).await.0, "in_progress");
    a.set("d1", "deposit", AnchorStatus::PendingExternal);
    run(&pool, &a).await;
    assert_eq!(
        state(&pool, s).await,
        ("in_progress".into(), Some("pendingexternal".into()), None)
    );
    a.set("d1", "deposit", AnchorStatus::Completed);
    assert_eq!(run(&pool, &a).await.completed, 1);
    assert_eq!(state(&pool, s).await.0, "completed");
    assert_eq!(
        run(&pool, &a).await.checked,
        0,
        "a finished session is not looked at again"
    );
    assert_eq!(
        withdrawals::of_user(&pool, ada, 10).await.unwrap().len(),
        0,
        "a deposit never makes a withdrawal"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_cash_out_becomes_one_ordinary_withdrawal_to_the_anchor(pool: PgPool) {
    let a = FakeAnchor::new();
    let ada = player(&pool, "ada", 50).await;
    let s = session(&pool, ada, fiat::Kind::Withdraw, "w1").await;

    // The player is still in the anchor's screens: nothing to send yet.
    a.set("w1", "withdrawal", AnchorStatus::Incomplete);
    assert_eq!(run(&pool, &a).await.withdrawals_made, 0);
    assert_eq!(funds(&pool, ada).await, 50 * USDC);

    // The anchor asks for 10 USDC to its account with memo 424242.
    a.ask("w1", "10.0000000", ANCHOR_ACCOUNT);
    assert_eq!(run(&pool, &a).await.withdrawals_made, 1);
    assert_eq!(
        funds(&pool, ada).await,
        40 * USDC,
        "the money is held for the payment"
    );
    let w = withdrawals::of_user(&pool, ada, 10).await.unwrap();
    assert_eq!(w.len(), 1);
    assert_eq!(
        (
            w[0].destination.as_str(),
            w[0].amount.minor(),
            w[0].memo.clone()
        ),
        (ANCHOR_ACCOUNT, 10 * USDC, PaymentMemo::Id(424242))
    );
    assert_eq!(
        w[0].status,
        withdrawals::Status::Approved,
        "small, so approved at once and sent by the withdrawal sender"
    );
    assert_eq!(state(&pool, s).await.0, "sending");

    // Rounds later the anchor still asks: no second withdrawal, no second hold.
    assert_eq!(run(&pool, &a).await.withdrawals_made, 0);
    assert_eq!(withdrawals::of_user(&pool, ada, 10).await.unwrap().len(), 1);
    assert_eq!(funds(&pool, ada).await, 40 * USDC);

    // The anchor pays the player in their currency and closes it.
    a.edit("w1", |t| t.status = AnchorStatus::Completed);
    assert_eq!(run(&pool, &a).await.completed, 1);
    assert_eq!(state(&pool, s).await.0, "completed");
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_cash_out_request_we_cannot_check_is_refused_and_nothing_is_sent(pool: PgPool) {
    let a = FakeAnchor::new();
    let ada = player(&pool, "ada", 50).await;
    let other = "GAAZI4TCR3TY5OJHCTJC2A4QSY6CJWJH5IAJTGKIN2ER7LBNVKOCCWN7";
    type Case = (&'static str, Box<dyn Fn(&FakeAnchor)>);
    let cases: Vec<Case> = vec![
        (
            "an account the anchor does not list",
            Box::new(move |a| a.ask("x", "10", other)),
        ),
        (
            "our own wallet",
            Box::new(|a| {
                a.accounts
                    .lock()
                    .unwrap()
                    .push(AccountKey::parse(HOT).unwrap());
                a.ask("x", "10", HOT)
            }),
        ),
        (
            "the USDC issuer",
            Box::new(|a| {
                a.accounts
                    .lock()
                    .unwrap()
                    .push(AccountKey::parse(ISSUER).unwrap());
                a.ask("x", "10", ISSUER)
            }),
        ),
        (
            "no account",
            Box::new(|a| {
                a.ask("x", "10", ANCHOR_ACCOUNT);
                a.edit("x", |t| t.withdraw_anchor_account = None)
            }),
        ),
        (
            "no memo",
            Box::new(|a| {
                a.ask("x", "10", ANCHOR_ACCOUNT);
                a.edit("x", |t| t.withdraw_memo = None)
            }),
        ),
        (
            "a memo type we do not send",
            Box::new(|a| {
                a.ask("x", "10", ANCHOR_ACCOUNT);
                a.edit("x", |t| t.withdraw_memo_type = Some("hash".into()))
            }),
        ),
        (
            "a text memo that is too long",
            Box::new(|a| {
                a.ask("x", "10", ANCHOR_ACCOUNT);
                a.edit("x", |t| {
                    t.withdraw_memo_type = Some("text".into());
                    t.withdraw_memo = Some("x".repeat(40));
                })
            }),
        ),
        (
            "no amount",
            Box::new(|a| {
                a.ask("x", "10", ANCHOR_ACCOUNT);
                a.edit("x", |t| t.amount_in = None)
            }),
        ),
        (
            "an amount that is not a number",
            Box::new(|a| a.ask("x", "ten", ANCHOR_ACCOUNT)),
        ),
        ("zero", Box::new(|a| a.ask("x", "0", ANCHOR_ACCOUNT))),
        (
            "below the smallest we send",
            Box::new(|a| a.ask("x", "0.5", ANCHOR_ACCOUNT)),
        ),
    ];
    for (i, (why, setup)) in cases.iter().enumerate() {
        *a.accounts.lock().unwrap() = vec![AccountKey::parse(ANCHOR_ACCOUNT).unwrap()];
        a.txs.lock().unwrap().clear();
        setup(&a);
        let id = format!("case{i}");
        let t = a.txs.lock().unwrap().remove("x").unwrap();
        a.txs.lock().unwrap().insert(
            id.clone(),
            AnchorTx {
                id: id.clone(),
                ..t
            },
        );
        let s = session(&pool, ada, fiat::Kind::Withdraw, &id).await;
        let r = run(&pool, &a).await;
        assert_eq!((r.failed, r.withdrawals_made), (1, 0), "{why}");
        assert_eq!(state(&pool, s).await.0, "failed", "{why}");
        assert_eq!(
            funds(&pool, ada).await,
            50 * USDC,
            "nothing was held for: {why}"
        );
    }
    assert_eq!(withdrawals::of_user(&pool, ada, 10).await.unwrap().len(), 0);

    // An anchor that lists no accounts at all cannot be paid.
    a.accounts.lock().unwrap().clear();
    a.ask("blind", "10", ANCHOR_ACCOUNT);
    let s = session(&pool, ada, fiat::Kind::Withdraw, "blind").await;
    assert_eq!(run(&pool, &a).await.failed, 1);
    assert_eq!(state(&pool, s).await.0, "failed");
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn too_little_money_waits_and_is_sent_once_the_player_has_it(pool: PgPool) {
    let a = FakeAnchor::new();
    let ada = player(&pool, "ada", 5).await;
    let s = session(&pool, ada, fiat::Kind::Withdraw, "w1").await;
    a.ask("w1", "10", ANCHOR_ACCOUNT);
    assert_eq!(run(&pool, &a).await.withdrawals_made, 0);
    let (status, _, message) = state(&pool, s).await;
    assert_eq!(status, "in_progress");
    assert!(
        message.unwrap().contains("waiting"),
        "the player is told why"
    );

    // They add money (from a card, say) and the next round sends it.
    let custody = system_account(&pool, CUSTODY_KIND, &usdc(), Side::Debit)
        .await
        .unwrap();
    let wallet = user_wallet(&pool, ada, &usdc()).await.unwrap();
    let amount = Amount::new(20 * USDC, usdc()).unwrap();
    post(
        &pool,
        &Posting::new(
            IdempotencyKey::new(&format!("test-topup:{ada}")).unwrap(),
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
    assert_eq!(run(&pool, &a).await.withdrawals_made, 1);
    assert_eq!(funds(&pool, ada).await, 15 * USDC);
    assert_eq!(state(&pool, s).await.0, "sending");
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn when_the_anchor_gives_up_a_waiting_payment_is_cancelled_and_the_money_comes_back(
    pool: PgPool,
) {
    let a = FakeAnchor::new();
    let ada = player(&pool, "ada", 500).await;
    let s = session(&pool, ada, fiat::Kind::Withdraw, "w1").await;
    // Over the auto-approve limit, so a person has to approve it: it is still waiting when the anchor expires.
    a.ask("w1", "200", ANCHOR_ACCOUNT);
    run(&pool, &a).await;
    assert_eq!(funds(&pool, ada).await, 300 * USDC);
    assert_eq!(
        withdrawals::of_user(&pool, ada, 10).await.unwrap()[0].status,
        withdrawals::Status::AwaitingApproval
    );

    a.edit("w1", |t| {
        t.status = AnchorStatus::Expired;
        t.message = Some("the customer never finished".into());
    });
    assert_eq!(run(&pool, &a).await.failed, 1);
    assert_eq!(state(&pool, s).await.0, "failed");
    assert_eq!(
        funds(&pool, ada).await,
        500 * USDC,
        "the held money went back"
    );
    assert_eq!(
        withdrawals::of_user(&pool, ada, 10).await.unwrap()[0].status,
        withdrawals::Status::Cancelled
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn an_anchor_that_is_down_changes_nothing_and_one_login_serves_a_players_sessions(
    pool: PgPool,
) {
    let a = FakeAnchor::new();
    let ada = player(&pool, "ada", 0).await;
    let (s1, s2) = (
        session(&pool, ada, fiat::Kind::Deposit, "d1").await,
        session(&pool, ada, fiat::Kind::Deposit, "d2").await,
    );
    a.set("d1", "deposit", AnchorStatus::PendingAnchor);
    a.set("d2", "deposit", AnchorStatus::PendingAnchor);

    *a.down.lock().unwrap() = true;
    assert_eq!(
        run(&pool, &a).await,
        Report {
            checked: 2,
            ..Report::default()
        }
    );
    assert_eq!(
        (state(&pool, s1).await.1, state(&pool, s2).await.1),
        (None, None)
    );

    *a.down.lock().unwrap() = false;
    run(&pool, &a).await;
    assert_eq!(
        *a.logins.lock().unwrap(),
        1,
        "one login for both of the player's sessions"
    );
    assert_eq!(state(&pool, s1).await.1.as_deref(), Some("pendinganchor"));
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn sessions_the_player_never_finished_are_given_up_and_a_second_worker_does_nothing(
    pool: PgPool,
) {
    let a = FakeAnchor::new();
    let ada = player(&pool, "ada", 0).await;
    let s = session(&pool, ada, fiat::Kind::Deposit, "d1").await;
    a.set("d1", "deposit", AnchorStatus::Incomplete);
    sqlx::query("UPDATE fiat_sessions SET created_at = now() - interval '3 days' WHERE id = $1")
        .bind(s)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(run(&pool, &a).await.expired, 1);
    assert_eq!(state(&pool, s).await.0, "expired");

    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext(current_database() || ':' || $1))")
        .bind("fiat-sessions")
        .execute(&mut *lock)
        .await
        .unwrap();
    assert!(run(&pool, &a).await.skipped);
    let _ = Status::Failed;
}
