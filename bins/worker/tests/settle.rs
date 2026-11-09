//! The settle job's round against a real Postgres: it settles what fair play has finished with, leaves the rest, and is
//! safe to run again (`make dev`, then `make test-integration`).

use eschess_domain::money::{Amount, AssetId};
use eschess_ledger::{Entry, IdempotencyKey, Posting, Side};
use eschess_storage::games;
use eschess_storage::ledger::{balance, post};
use eschess_storage::stakes::Stake;
use eschess_storage::stellar::{system_account, user_wallet, CUSTODY_KIND};
use eschess_worker::settle::{tick, SettleConfig};
use sqlx::PgPool;
use uuid::Uuid;

const ISSUER: &str = "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5";
fn usdc() -> AssetId {
    AssetId::credit("USDC", ISSUER).unwrap()
}

async fn player(pool: &PgPool) -> Uuid {
    let id = Uuid::now_v7();
    let name = format!("u{}", &id.simple().to_string()[20..]);
    sqlx::query("INSERT INTO users (id, email, username, password_hash, email_verified_at) VALUES ($1, $2, $3, 'x', now())")
        .bind(id).bind(format!("{name}@example.com")).bind(&name).execute(pool).await.unwrap();
    let custody = system_account(pool, CUSTODY_KIND, &usdc(), Side::Debit)
        .await
        .unwrap();
    let wallet = user_wallet(pool, id, &usdc()).await.unwrap();
    let amount = Amount::new(1_000, usdc()).unwrap();
    post(
        pool,
        &Posting::new(
            IdempotencyKey::new(&format!("dep:{id}")).unwrap(),
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

async fn finished_game(pool: &PgPool) -> (Uuid, Uuid) {
    let (w, b) = (player(pool).await, player(pool).await);
    let stake = Stake {
        asset: usdc(),
        minor: 400,
    };
    let game = games::create_game(pool, w, 300_000, 0, "white", true, Some(&stake))
        .await
        .unwrap();
    games::join_game(pool, game, b, true).await.unwrap();
    sqlx::query("UPDATE games SET status = 'finished', result = 'white', ply = 40, ended_at = now() WHERE id = $1")
        .bind(game).execute(pool).await.unwrap();
    (game, w)
}

fn config() -> SettleConfig {
    SettleConfig {
        scorer: "baseline-1".into(),
        fee_bps: 0,
        batch: 50,
    }
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_round_settles_scored_games_leaves_the_rest_and_can_run_again(pool: PgPool) {
    let (scored, winner) = finished_game(&pool).await;
    let (unscored, _) = finished_game(&pool).await;
    sqlx::query(
        "INSERT INTO fairplay_runs (game_id, scorer, outcome) VALUES ($1, 'baseline-1', 'scored')",
    )
    .bind(scored)
    .execute(&pool)
    .await
    .unwrap();
    // A run by some other scorer is not the one this job waits for.
    sqlx::query(
        "INSERT INTO fairplay_runs (game_id, scorer, outcome) VALUES ($1, 'noop-1', 'scored')",
    )
    .bind(unscored)
    .execute(&pool)
    .await
    .unwrap();

    let r = tick(&pool, &config()).await.unwrap();
    assert_eq!((r.paid, r.waiting, r.failed), (1, 1, 0), "{r:?}");
    let wallet = user_wallet(&pool, winner, &usdc()).await.unwrap();
    assert_eq!(balance(&pool, wallet).await.unwrap().minor, 600 + 800);

    // Again: nothing more is paid; the unscored game still waits.
    let r = tick(&pool, &config()).await.unwrap();
    assert_eq!((r.paid, r.waiting), (0, 1), "{r:?}");
    assert_eq!(balance(&pool, wallet).await.unwrap().minor, 1_400);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_round_stops_and_says_so_when_escrow_does_not_match(pool: PgPool) {
    let (game, winner) = finished_game(&pool).await;
    sqlx::query(
        "INSERT INTO fairplay_runs (game_id, scorer, outcome) VALUES ($1, 'baseline-1', 'scored')",
    )
    .bind(game)
    .execute(&pool)
    .await
    .unwrap();
    // 5 units appear in escrow that no game accounts for.
    let escrow = system_account(
        &pool,
        eschess_storage::stakes::ESCROW_KIND,
        &usdc(),
        Side::Credit,
    )
    .await
    .unwrap();
    let wallet = user_wallet(&pool, winner, &usdc()).await.unwrap();
    let amount = Amount::new(5, usdc()).unwrap();
    post(
        &pool,
        &Posting::new(
            IdempotencyKey::new("stray-test-posting").unwrap(),
            "stake_hold",
            None,
            vec![
                Entry::debit(wallet, amount.clone()),
                Entry::credit(escrow, amount),
            ],
        )
        .unwrap(),
    )
    .await
    .unwrap();

    let r = tick(&pool, &config()).await.unwrap();
    assert_eq!((r.escrow_mismatches, r.paid), (1, 0), "{r:?}");
    let settled: bool =
        sqlx::query_scalar("SELECT settled_at IS NOT NULL FROM games WHERE id = $1")
            .bind(game)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!settled);
}
