//! Settling finished staked games through the contract, against a real Postgres with a fake contract and network
//! (`make dev`, then `make test-integration`): the pot is decided by the same rules as the ledger path, the contract is
//! told, and the ledger moves only after the network shows the match closed.

mod common;

use common::*;
use eschess_ledger::Side;
use eschess_storage::ledger::balance;
use eschess_storage::stakes::ESCROW_KIND;
use eschess_storage::stellar::{system_account, user_wallet};
use eschess_worker::contract_settle::{tick, ContractSettleReport};
use eschess_worker::funding::Parts;
use sqlx::PgPool;
use uuid::Uuid;

const SCORER: &str = "baseline-1";

async fn settle_round(pool: &PgPool, w: &World) -> ContractSettleReport {
    let parts = Parts {
        pool,
        rpc: &*w.net,
        chain: &*w.chain,
        vault: w.vault.clone(),
        admin: &w.admin,
        hot: Some(&w.hot),
    };
    tick(&parts, &w.config, SCORER, 10).await.unwrap()
}

/// A game that was funded through the contract and has finished; White is the creator.
async fn finished(pool: &PgPool, w: &World, result: &str, ply: i32) -> (Uuid, Uuid, Uuid) {
    let (game, white, black, ..) = funding_game(pool, w, 1_000, 1_000).await;
    assert_eq!(round(pool, w, true).await.activated, 1);
    sqlx::query("UPDATE games SET status = 'finished', result = $2, ply = $3, ended_at = now() WHERE id = $1")
        .bind(game).bind(result).bind(ply).execute(pool).await.unwrap();
    (game, white, black)
}

async fn scored(pool: &PgPool, game: Uuid) {
    sqlx::query("INSERT INTO fairplay_runs (game_id, scorer, outcome) VALUES ($1, $2, 'scored')")
        .bind(game)
        .bind(SCORER)
        .execute(pool)
        .await
        .unwrap();
}

async fn settled(pool: &PgPool, game: Uuid) -> bool {
    sqlx::query_scalar("SELECT settled_at IS NOT NULL FROM games WHERE id = $1")
        .bind(game)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn escrow(pool: &PgPool) -> i64 {
    balance(
        pool,
        system_account(pool, ESCROW_KIND, &usdc(), Side::Credit)
            .await
            .unwrap(),
    )
    .await
    .unwrap()
    .minor
}

async fn wallet(pool: &PgPool, u: Uuid) -> i64 {
    balance(pool, user_wallet(pool, u, &usdc()).await.unwrap())
        .await
        .unwrap()
        .minor
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_win_waits_for_fair_play_then_the_contract_pays_and_the_ledger_follows(pool: PgPool) {
    let w = world();
    let (game, white, black) = finished(&pool, &w, "white", 40).await;
    // Fair play has not looked: nothing is sent, nothing moves.
    let r = settle_round(&pool, &w).await;
    assert_eq!((r.sent, r.settled, r.waiting), (0, 0, 1), "{r:?}");
    assert!(w.net.told.lock().unwrap().is_empty());
    scored(&pool, game).await;

    let r = settle_round(&pool, &w).await;
    assert_eq!((r.sent, r.settled, r.failed), (1, 1, 0), "{r:?}");
    assert_eq!(
        *w.net.told.lock().unwrap(),
        ["WinA"],
        "White is player A, and won"
    );
    assert!(settled(&pool, game).await);
    assert_eq!(
        (
            wallet(&pool, white).await,
            wallet(&pool, black).await,
            escrow(&pool).await
        ),
        (1_400, 600, 0)
    );
    // Done: a further round changes nothing and tells the contract nothing more.
    assert_eq!(
        settle_round(&pool, &w).await,
        ContractSettleReport::default()
    );
    assert_eq!(w.net.told.lock().unwrap().len(), 1);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn draws_and_games_that_were_never_really_played_are_told_to_the_contract_as_a_draw(
    pool: PgPool,
) {
    let w = world();
    let (draw, ..) = finished(&pool, &w, "draw", 60).await;
    let (short, ..) = finished(&pool, &w, "black", 2).await;
    scored(&pool, draw).await;
    scored(&pool, short).await;
    let r = settle_round(&pool, &w).await;
    assert_eq!((r.settled, r.failed), (2, 0), "{r:?}");
    assert_eq!(*w.net.told.lock().unwrap(), ["Draw", "Draw"]);
    assert_eq!(escrow(&pool).await, 0);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn an_open_case_holds_the_pot_and_a_confirmed_cheat_pays_the_opponent(pool: PgPool) {
    let w = world();
    let (game, white, black) = finished(&pool, &w, "white", 40).await;
    scored(&pool, game).await;
    sqlx::query("INSERT INTO fairplay_scores (game_id, player_id, scorer, score, summary) VALUES ($1, $2, $3, 90, '')")
        .bind(game).bind(white).bind(SCORER).execute(&pool).await.unwrap();
    let case = Uuid::now_v7();
    sqlx::query("INSERT INTO fairplay_cases (id, player_id) VALUES ($1, $2)")
        .bind(case)
        .bind(white)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO fairplay_case_games (case_id, game_id, player_id, scorer) VALUES ($1, $2, $3, $4)")
        .bind(case).bind(game).bind(white).bind(SCORER).execute(&pool).await.unwrap();

    let r = settle_round(&pool, &w).await;
    assert_eq!(
        (r.sent, r.waiting),
        (0, 1),
        "an open case holds the pot: {r:?}"
    );
    assert!(w.net.told.lock().unwrap().is_empty());

    // A reviewer confirms: White cheated, so Black takes the pot although White won the game (OD-14).
    let reviewer = player(&pool, &w, 0).await.0;
    sqlx::query("UPDATE fairplay_cases SET status = 'confirmed', decided_by = $2, decided_at = now(), reason = 'x' WHERE id = $1")
        .bind(case).bind(reviewer).execute(&pool).await.unwrap();
    let r = settle_round(&pool, &w).await;
    assert_eq!((r.sent, r.settled), (1, 1), "{r:?}");
    assert_eq!(*w.net.told.lock().unwrap(), ["WinB"]);
    assert_eq!(
        (wallet(&pool, black).await, wallet(&pool, white).await),
        (1_400, 600)
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn the_ledger_never_runs_ahead_of_the_network_and_follows_what_the_network_was_told(
    pool: PgPool,
) {
    let w = world();
    // The worker died after the contract paid but before the ledger moved: the network says Settled and the recorded
    // outcome says Black won, although the game's own result says White. The ledger follows the network.
    let (game, white, black) = finished(&pool, &w, "white", 40).await;
    scored(&pool, game).await;
    w.net
        .matches
        .lock()
        .unwrap()
        .get_mut(game.as_bytes())
        .unwrap()
        .status = "Settled";
    sqlx::query("INSERT INTO contract_txs (tx_hash, game_id, step, max_time, detail) VALUES ($1, $2, 'settle', 1, 'win_black')")
        .bind("a".repeat(64)).bind(game).execute(&pool).await.unwrap();
    let r = settle_round(&pool, &w).await;
    assert_eq!((r.settled, r.sent), (1, 0), "{r:?}");
    assert!(
        w.net.told.lock().unwrap().is_empty(),
        "the contract is not told again"
    );
    assert_eq!(
        (wallet(&pool, black).await, wallet(&pool, white).await),
        (1_400, 600)
    );

    // Settled on the network with no record of what it was told: nothing is guessed, a person must look.
    let (lost, white2, _) = finished(&pool, &w, "white", 40).await;
    scored(&pool, lost).await;
    w.net
        .matches
        .lock()
        .unwrap()
        .get_mut(lost.as_bytes())
        .unwrap()
        .status = "Settled";
    let r = settle_round(&pool, &w).await;
    assert_eq!((r.settled, r.failed), (0, 1), "{r:?}");
    assert!(!settled(&pool, lost).await);
    assert_eq!(wallet(&pool, white2).await, 600, "nothing was posted");
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_match_the_network_refunded_returns_both_stakes_in_the_ledger(pool: PgPool) {
    let w = world();
    let (game, white, black) = finished(&pool, &w, "white", 40).await;
    // Nobody settled it in time and the contract's expiry refunded both stakes.
    w.net
        .matches
        .lock()
        .unwrap()
        .get_mut(game.as_bytes())
        .unwrap()
        .status = "Cancelled";
    let r = settle_round(&pool, &w).await;
    assert_eq!(r.settled, 1, "{r:?}");
    assert_eq!(
        (
            wallet(&pool, white).await,
            wallet(&pool, black).await,
            escrow(&pool).await
        ),
        (1_000, 1_000, 0)
    );
    assert!(w.net.told.lock().unwrap().is_empty());
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_settle_that_has_not_landed_is_waited_for_and_the_ledger_does_not_move(pool: PgPool) {
    let w = world();
    let (game, white, _) = finished(&pool, &w, "white", 40).await;
    scored(&pool, game).await;
    // The funding transactions landed long ago; from now on nothing lands.
    *w.chain.now.lock().unwrap() += 200;
    *w.net.lands.lock().unwrap() = false;
    let r = settle_round(&pool, &w).await;
    assert_eq!((r.sent, r.settled, r.waiting), (0, 0, 1), "{r:?}");
    assert_eq!(
        wallet(&pool, white).await,
        600,
        "not paid in the ledger before the network has paid"
    );
    let r = settle_round(&pool, &w).await;
    assert_eq!(r.waiting, 1);
    assert_eq!(
        w.net
            .sent_functions()
            .iter()
            .filter(|f| *f == "settle")
            .count(),
        1,
        "no second settle beside the first"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_halt_stops_payouts_until_it_is_cleared(pool: PgPool) {
    let w = world();
    let (game, white, _) = finished(&pool, &w, "white", 40).await;
    scored(&pool, game).await;
    sqlx::query("INSERT INTO money_halts (reason) VALUES ('books do not match the chain')")
        .execute(&pool)
        .await
        .unwrap();
    let r = settle_round(&pool, &w).await;
    assert_eq!((r.sent, r.settled, r.waiting), (0, 0, 1), "{r:?}");
    assert_eq!(wallet(&pool, white).await, 600);
    let who = player(&pool, &w, 0).await.0;
    sqlx::query(
        "UPDATE money_halts SET cleared_at = now(), cleared_by = $1, clear_note = 'looked into it'",
    )
    .bind(who)
    .execute(&pool)
    .await
    .unwrap();
    let r = settle_round(&pool, &w).await;
    assert_eq!(r.settled, 1, "{r:?}");
}
