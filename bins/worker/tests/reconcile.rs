//! The books-against-chain check (ADR 0006 §6), with a fake network (`make dev`, then `make test-integration`): matching
//! books, money set aside, money in motion, a persistent difference that halts new outflows, and every gate the halt closes.

use std::collections::HashMap;
use std::sync::Mutex;

use eschess_domain::money::{Amount, AssetId};
use eschess_ledger::{Entry, IdempotencyKey, Posting, Side};
use eschess_stellar::custody::LocalVault;
use eschess_stellar::signer::{LocalSigner, Signer};
use eschess_stellar::tx::{Network, PaymentMemo};
use eschess_stellar::{AccountState, Chain, ChainError, Page, Submitted, TxOutcome};
use eschess_storage::games::{self, GameError};
use eschess_storage::ledger::post;
use eschess_storage::reconcile::{self, Verdict};
use eschess_storage::stakes::{self, Stake, Waiting};
use eschess_storage::stellar::{system_account, user_wallet, CUSTODY_KIND};
use eschess_storage::wallets;
use eschess_storage::withdrawals::{self, NewWithdrawal, Policy, Status};
use eschess_worker::reconcile::{tick, ReconcileConfig};
use eschess_worker::sender::{self, SenderConfig};
use sqlx::PgPool;
use uuid::Uuid;

const ISSUER: &str = "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5";
const HOT_SECRET: &str = "SA3PXIPSNIG2RVLSIREL6KV6PQJTFI7PA4ZBYVP772LORMETDV5FQTNT";
const DEST: &str = "GBUUQEXJEVAN2FLTOEBQ2OUJREAQOAEYYNW5R2X7O7RE4LZ2DNMBUEAV";

fn usdc() -> AssetId {
    AssetId::credit("USDC", ISSUER).unwrap()
}

/// A network that holds chosen USDC balances. It is never asked to send anything.
#[derive(Default)]
struct Net {
    usdc: Mutex<HashMap<String, i64>>,
}

impl Net {
    fn set(&self, account: &str, minor: i64) {
        self.usdc.lock().unwrap().insert(account.to_string(), minor);
    }
}

#[async_trait::async_trait]
impl Chain for Net {
    async fn payments(&self, _: &str, _: Option<&str>, _: u32) -> Result<Page, ChainError> {
        unreachable!()
    }
    async fn account(&self, account: &str) -> Result<Option<AccountState>, ChainError> {
        Ok(self
            .usdc
            .lock()
            .unwrap()
            .get(account)
            .map(|m| AccountState {
                sequence: 1,
                can_receive: vec![usdc()],
                balances: vec![(usdc(), *m)],
            }))
    }
    async fn submit(&self, _: &str) -> Result<Submitted, ChainError> {
        unreachable!("a halted sender sends nothing")
    }
    async fn transaction(&self, _: &str) -> Result<Option<TxOutcome>, ChainError> {
        unreachable!()
    }
    async fn latest_ledger_close_time(&self) -> Result<i64, ChainError> {
        unreachable!()
    }
}

struct World {
    net: Net,
    hot: LocalSigner,
    config: ReconcileConfig,
}

fn world() -> World {
    let hot = LocalSigner::from_secret(HOT_SECRET, Network::Testnet).unwrap();
    World {
        net: Net::default(),
        config: ReconcileConfig {
            hot_account: hot.public_key(),
            assets: vec![usdc()],
            mismatches_to_halt: 3,
        },
        hot,
    }
}

fn vault() -> LocalVault {
    LocalVault::from_bytes([8; 32], 1, Network::Testnet).unwrap()
}

async fn user(pool: &PgPool) -> Uuid {
    let id = Uuid::now_v7();
    let name = format!("u{}", &id.simple().to_string()[20..]);
    sqlx::query("INSERT INTO users (id, email, username, password_hash, email_verified_at) VALUES ($1, $2, $3, 'x', now())")
        .bind(id).bind(format!("{name}@example.com")).bind(&name).execute(pool).await.unwrap();
    id
}

/// Records a deposit of `minor` for `user` in the ledger, as the watcher would.
async fn credit(pool: &PgPool, user: Uuid, minor: i64) {
    let custody = system_account(pool, CUSTODY_KIND, &usdc(), Side::Debit)
        .await
        .unwrap();
    let wallet = user_wallet(pool, user, &usdc()).await.unwrap();
    let amount = Amount::new(minor, usdc()).unwrap();
    post(
        pool,
        &Posting::new(
            IdempotencyKey::new(&format!("dep:{user}:{minor}")).unwrap(),
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

/// A player with `minor` credited in the ledger, whose own account holds `on_chain` on the network.
async fn player(pool: &PgPool, w: &World, minor: i64, on_chain: i64) -> Uuid {
    let id = user(pool).await;
    credit(pool, id, minor).await;
    let address = wallets::ensure_wallet(pool, &vault(), id).await.unwrap();
    wallets::request_setup(pool, id).await.unwrap();
    wallets::mark_setup_ready(pool, id).await.unwrap();
    w.net.set(&address.to_g(), on_chain);
    id
}

fn hot(w: &World) -> String {
    w.hot.public_key().to_g()
}

async fn check(pool: &PgPool, w: &World) -> eschess_worker::reconcile::ReconcileReport {
    tick(pool, &w.net, &w.config).await.unwrap()
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn books_that_match_the_chain_are_ok_whether_the_money_is_in_players_accounts_or_the_platform_wallet(
    pool: PgPool,
) {
    let w = world();
    // 100 in the ledger: 25 and 15 in players' own accounts, 60 still in the platform wallet from earlier deposits.
    player(&pool, &w, 25, 25).await;
    player(&pool, &w, 15, 15).await;
    let legacy = user(&pool).await;
    credit(&pool, legacy, 60).await;
    w.net.set(&hot(&w), 60);

    let r = check(&pool, &w).await;
    let c = &r.checks[0];
    assert_eq!(
        (
            c.verdict,
            c.ledger_minor,
            c.chain_minor,
            c.hot_minor,
            c.players_minor
        ),
        (Verdict::Ok, 100, 100, 60, 40)
    );
    assert_eq!(
        (c.difference_minor, c.accounts_read, r.halted),
        (0, 3, false)
    );
    assert!(reconcile::active_halt(&pool).await.unwrap().is_none());
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn money_that_arrived_and_was_set_aside_for_review_is_explained_not_a_difference(
    pool: PgPool,
) {
    let w = world();
    player(&pool, &w, 100, 100).await;
    w.net.set(&hot(&w), 15); // 15 of someone else's money sits in the platform wallet, set aside for a person
    sqlx::query("INSERT INTO stellar_deposits (op_id, status, review_reason, asset, amount_minor) VALUES ('odd', 'needs_review', 'no such player', $1, 15)")
        .bind(usdc().to_string()).execute(&pool).await.unwrap();
    let r = check(&pool, &w).await;
    assert_eq!(
        (r.checks[0].verdict, r.checks[0].explained_minor),
        (Verdict::Ok, 15)
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_shortfall_that_persists_halts_new_outflows_and_only_then(pool: PgPool) {
    let w = world();
    player(&pool, &w, 100, 90).await; // the ledger says 100; the network holds 90
    let first = check(&pool, &w).await;
    assert_eq!(
        (
            first.checks[0].verdict,
            first.checks[0].difference_minor,
            first.halted
        ),
        (Verdict::Mismatch, -10, false)
    );
    assert!(
        !check(&pool, &w).await.halted,
        "two in a row is not yet three"
    );
    assert!(reconcile::active_halt(&pool).await.unwrap().is_none());
    let third = check(&pool, &w).await;
    assert!(third.halted);
    let halt = reconcile::active_halt(&pool)
        .await
        .unwrap()
        .expect("halted");
    assert!(halt.reason.contains("difference -10"), "{}", halt.reason);
    // Every comparison was kept.
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM money_checks WHERE verdict = 'mismatch'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 3);
    // A fourth does not start a second halt.
    assert!(!check(&pool, &w).await.halted);
    assert!(
        !reconcile::halt(&pool, "again", None).await.unwrap(),
        "only one halt at a time"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_difference_that_comes_and_goes_or_appears_while_money_moves_never_halts(pool: PgPool) {
    let w = world();
    let id = player(&pool, &w, 100, 90).await;
    check(&pool, &w).await;
    check(&pool, &w).await; // two mismatches
                            // A deposit has been seen and not yet credited: money is in motion, so the difference proves nothing.
    sqlx::query("INSERT INTO stellar_deposits (op_id, status, user_id, asset, amount_minor) VALUES ('moving', 'pending', $1, $2, 5)")
        .bind(id).bind(usdc().to_string()).execute(&pool).await.unwrap();
    let r = check(&pool, &w).await;
    assert_eq!(r.checks[0].verdict, Verdict::Inconclusive);
    assert!(!r.halted);
    // It settles; the run of mismatches starts again from one.
    sqlx::query("UPDATE stellar_deposits SET status = 'credited', credited_at = now()")
        .execute(&pool)
        .await
        .unwrap();
    assert!(!check(&pool, &w).await.halted);
    assert!(reconcile::active_halt(&pool).await.unwrap().is_none());
    // And when the network catches up, books match again.
    w.net.set(&hot(&w), 10);
    assert_eq!(check(&pool, &w).await.checks[0].verdict, Verdict::Ok);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn while_halted_nothing_new_leaves_and_when_cleared_it_resumes(pool: PgPool) {
    let w = world();
    // A funded player with a withdrawal approved and waiting, a staked game open, and a finished staked game to settle.
    let a = user(&pool).await;
    let b = user(&pool).await;
    credit(&pool, a, 1_000).await;
    credit(&pool, b, 1_000).await;
    let (wd, _) = withdrawals::request(
        &pool,
        &NewWithdrawal {
            user_id: a,
            client_request_id: Uuid::now_v7(),
            amount: Amount::new(100, usdc()).unwrap(),
            destination: DEST.into(),
            memo: PaymentMemo::None,
        },
        Policy {
            auto_approve_up_to_minor: i64::MAX,
            daily_limit_minor: i64::MAX,
        },
    )
    .await
    .unwrap();
    assert_eq!(wd.status, Status::Approved);
    let stake = Stake {
        asset: usdc(),
        minor: 50,
    };
    let game = games::create_game(&pool, a, 300_000, 0, "white", true, Some(&stake))
        .await
        .unwrap();
    games::join_game(&pool, game, b, true).await.unwrap();
    sqlx::query("UPDATE games SET status = 'finished', result = 'white', ply = 40, ended_at = now() WHERE id = $1")
        .bind(game).execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO fairplay_runs (game_id, scorer, outcome) VALUES ($1, 'baseline-1', 'scored')",
    )
    .bind(game)
    .execute(&pool)
    .await
    .unwrap();

    assert!(reconcile::halt(&pool, "test halt", None).await.unwrap());

    // The sender sends nothing new (the fake network panics if it is asked to submit).
    let config = SenderConfig {
        network: Network::Testnet,
        hot_account: w.hot.public_key(),
        fee_stroops: 1_000,
        tx_timeout_secs: 120,
        expiry_margin_secs: 60,
    };
    let r = sender::tick(&pool, &w.net, &w.hot, &config, 1_800_000_000)
        .await
        .unwrap();
    assert!(r.halted && r.sent.is_none(), "{r:?}");
    assert_eq!(
        withdrawals::get(&pool, wd.id).await.unwrap().status,
        Status::Approved
    );
    // No new stake is taken, directly or by queueing.
    let c = user(&pool).await;
    credit(&pool, c, 1_000).await;
    assert!(matches!(
        games::create_game(&pool, c, 300_000, 0, "white", true, Some(&stake)).await,
        Err(GameError::MoneyHalted)
    ));
    let queued = eschess_storage::matchmaking::enqueue(
        &pool,
        c,
        300_000,
        0,
        true,
        Some(&stake),
        &mut || true,
    )
    .await;
    assert!(matches!(queued, Err(GameError::MoneyHalted)));
    // A pot waits.
    assert_eq!(
        stakes::settle(&pool, game, "baseline-1", 0).await.unwrap(),
        Err(Waiting::Halted)
    );
    // But unstaked play and cancelling to get a stake back are not affected.
    assert!(
        games::create_game(&pool, c, 300_000, 0, "white", true, None)
            .await
            .is_ok()
    );

    // A person looks and clears it, saying why; the stuck pot settles.
    assert!(reconcile::clear_halt(
        &pool,
        c,
        "checked the explorer: the missing 10 was a test transfer"
    )
    .await
    .unwrap());
    assert!(reconcile::active_halt(&pool).await.unwrap().is_none());
    assert!(stakes::settle(&pool, game, "baseline-1", 0)
        .await
        .unwrap()
        .is_ok());
    assert!(
        games::create_game(&pool, c, 300_000, 0, "white", true, Some(&stake))
            .await
            .is_ok()
    );
    let by: Uuid = sqlx::query_scalar("SELECT cleared_by FROM money_halts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(by, c);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn history_is_kept_and_cannot_be_rewritten(pool: PgPool) {
    let w = world();
    player(&pool, &w, 100, 100).await;
    check(&pool, &w).await;
    for sql in [
        "UPDATE money_checks SET verdict = 'ok'",
        "DELETE FROM money_checks",
    ] {
        assert!(sqlx::query(sql).execute(&pool).await.is_err(), "{sql}");
    }
}
