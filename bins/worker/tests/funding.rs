//! The funding job against a real Postgres with a fake contract and a fake network (`make dev`, then
//! `make test-integration`): it does the next missing step, never repeats one, tops up, waits, gives up, and cancels.

mod common;

use common::*;
use eschess_domain::stellar::AccountKey;
use eschess_ledger::Side;
use eschess_storage::ledger::balance;
use eschess_storage::stakes::ESCROW_KIND;
use eschess_storage::stellar::{system_account, user_wallet};
use eschess_worker::funding::FundingReport;
use sqlx::PgPool;
use uuid::Uuid;

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_funding_game_gets_its_match_both_deposits_and_then_starts(pool: PgPool) {
    let w = world();
    let (game, ..) = funding_game(&pool, &w, 1_000, 1_000).await;
    let r = round(&pool, &w, true).await;
    assert_eq!((r.sent, r.activated, r.failed), (3, 1, 0), "{r:?}");
    assert_eq!(w.net.sent_functions(), ["create", "deposit", "deposit"]);
    assert_eq!(status(&pool, game).await, "active");
    // Nothing is left to do, and nothing is repeated.
    assert_eq!(round(&pool, &w, true).await, FundingReport::default());
    assert_eq!(w.net.sent_functions().len(), 3);
    let recorded: i64 = sqlx::query_scalar("SELECT count(*) FROM contract_txs WHERE game_id = $1")
        .bind(game)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        recorded, 3,
        "every transaction was recorded before it was sent"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_match_already_part_funded_is_finished_not_started_again(pool: PgPool) {
    let w = world();
    let (game, _, _, wa, ba) = funding_game(&pool, &w, 1_000, 1_000).await;
    w.net.matches.lock().unwrap().insert(
        *game.as_bytes(),
        FakeMatch {
            a: wa,
            b: ba,
            a_funded: true,
            b_funded: false,
            status: "Open",
        },
    );
    let r = round(&pool, &w, true).await;
    assert_eq!((r.sent, r.activated), (1, 1), "{r:?}");
    assert_eq!(
        w.net.sent_functions(),
        ["deposit"],
        "only the missing deposit"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_player_who_is_short_is_topped_up_from_the_platform_wallet_first(pool: PgPool) {
    let w = world();
    let (game, _, _, _, ba) = funding_game(&pool, &w, 1_000, 100).await; // Black holds 100 of 400 on the network
    let r = round(&pool, &w, true).await;
    assert_eq!(r.activated, 1, "{r:?}");
    assert_eq!(
        *w.chain.payments.lock().unwrap(),
        vec![(ba.to_g(), 300)],
        "exactly the shortfall, to Black"
    );
    assert_eq!(w.net.sent_functions(), ["create", "deposit", "deposit"]);
    assert_eq!(status(&pool, game).await, "active");
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn without_a_platform_wallet_a_short_player_means_the_game_is_cancelled_when_time_runs_out(
    pool: PgPool,
) {
    let w = world();
    let (game, white, black, ..) = funding_game(&pool, &w, 1_000, 0).await;
    let r = round(&pool, &w, false).await;
    assert_eq!((r.activated, r.failed), (0, 1), "{r:?}");
    assert_eq!(status(&pool, game).await, "funding");
    // White's stake is on the network by now; time runs out.
    *w.chain.now.lock().unwrap() += 200;
    let r = round(&pool, &w, false).await;
    assert_eq!(r.cancelled, 1, "{r:?}");
    assert_eq!(status(&pool, game).await, "cancelled");
    assert_eq!(
        w.net
            .matches
            .lock()
            .unwrap()
            .get(game.as_bytes())
            .unwrap()
            .status,
        "Cancelled",
        "the match was cancelled on the network too"
    );
    assert_eq!(
        (ledger(&pool, white).await, ledger(&pool, black).await),
        (1_000, 1_000)
    );
    let escrow = balance(
        &pool,
        system_account(&pool, ESCROW_KIND, &usdc(), Side::Credit)
            .await
            .unwrap(),
    )
    .await
    .unwrap()
    .minor;
    assert_eq!(escrow, 0);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn nothing_created_on_the_network_means_only_the_ledger_is_released(pool: PgPool) {
    let w = world();
    let (game, white, black, ..) = funding_game(&pool, &w, 1_000, 1_000).await;
    *w.chain.now.lock().unwrap() += 200;
    let r = round(&pool, &w, true).await;
    assert_eq!((r.cancelled, r.sent), (1, 0), "{r:?}");
    assert!(
        w.net.sent_functions().is_empty(),
        "no transaction for a match that never existed"
    );
    assert_eq!(
        (
            status(&pool, game).await.as_str(),
            ledger(&pool, white).await,
            ledger(&pool, black).await
        ),
        ("cancelled", 1_000, 1_000)
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_transaction_that_has_not_landed_is_waited_for_not_repeated(pool: PgPool) {
    let w = world();
    let (game, ..) = funding_game(&pool, &w, 1_000, 1_000).await;
    *w.net.lands.lock().unwrap() = false;
    let r = round(&pool, &w, true).await;
    assert_eq!((r.sent, r.waiting), (0, 1), "{r:?}");
    assert_eq!(w.net.sent_functions(), ["create"]);
    // Still within its window: another round builds nothing.
    let r = round(&pool, &w, true).await;
    assert_eq!(r.waiting, 1, "{r:?}");
    assert_eq!(
        w.net.sent_functions(),
        ["create"],
        "no second create beside the first"
    );
    // It lands: the job carries on from where the network is.
    *w.net.lands.lock().unwrap() = true;
    w.net.matches.lock().unwrap().insert(
        *game.as_bytes(),
        FakeMatch {
            a: AccountKey([0; 32]),
            b: AccountKey([0; 32]),
            a_funded: false,
            b_funded: false,
            status: "Open",
        },
    );
    let (wa, ba): (String, String) = sqlx::query_as("SELECT (SELECT address FROM user_wallet_keys WHERE user_id = white_id), (SELECT address FROM user_wallet_keys WHERE user_id = black_id) FROM games WHERE id = $1")
        .bind(game).fetch_one(&pool).await.unwrap();
    {
        let mut m = w.net.matches.lock().unwrap();
        let mm = m.get_mut(game.as_bytes()).unwrap();
        mm.a = AccountKey::parse(&wa).unwrap();
        mm.b = AccountKey::parse(&ba).unwrap();
    }
    let r = round(&pool, &w, true).await;
    assert_eq!(r.activated, 1, "{r:?}");
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_step_that_keeps_failing_stops_after_four_tries(pool: PgPool) {
    let w = world();
    let (game, ..) = funding_game(&pool, &w, 1_000, 1_000).await;
    *w.net.fail_deposits.lock().unwrap() = true;
    for _ in 0..8 {
        round(&pool, &w, true).await;
        // Time passes between rounds, so each retry is a different transaction (a different expiry).
        *w.chain.now.lock().unwrap() += 3;
    }
    let tries: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM contract_txs WHERE game_id = $1 AND step = 'deposit'",
    )
    .bind(game)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(tries, 4, "four tries and no more");
    assert_eq!(
        status(&pool, game).await,
        "funding",
        "still waiting for its time limit to cancel it"
    );
}
