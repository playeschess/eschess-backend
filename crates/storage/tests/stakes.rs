//! Staked games against a real Postgres (`make dev`, then `make test-integration`): ADR 0004, build step 1.
//!
//! A stake is held from the wallet into escrow when a game is created and when it is joined, and comes back if the
//! game is cancelled. Refusals move nothing. Settlement (paying a pot) is a later step and is not tested here.

use eschess_domain::money::{Amount, AssetId};
use eschess_ledger::{Entry, IdempotencyKey, Posting, Side};
use eschess_storage::games::{self, GameError};
use eschess_storage::ledger::{balance, post};
use eschess_storage::stakes::{Stake, ESCROW_KIND};
use eschess_storage::stellar::{system_account, user_wallet, CUSTODY_KIND};
use sqlx::PgPool;
use uuid::Uuid;

const ISSUER: &str = "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5";

fn usdc() -> AssetId {
    AssetId::credit("USDC", ISSUER).unwrap()
}
fn stake(minor: i64) -> Stake {
    Stake {
        asset: usdc(),
        minor,
    }
}

/// A player with a confirmed email and `minor` in their wallet.
async fn player(pool: &PgPool, minor: i64) -> Uuid {
    let id = Uuid::now_v7();
    let name = format!("u{}", &id.simple().to_string()[20..]);
    sqlx::query("INSERT INTO users (id, email, username, password_hash, email_verified_at) VALUES ($1, $2, $3, 'x', now())")
        .bind(id)
        .bind(format!("{name}@example.com"))
        .bind(&name)
        .execute(pool)
        .await
        .unwrap();
    if minor > 0 {
        let custody = system_account(pool, CUSTODY_KIND, &usdc(), Side::Debit)
            .await
            .unwrap();
        let wallet = user_wallet(pool, id, &usdc()).await.unwrap();
        let amount = Amount::new(minor, usdc()).unwrap();
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

async fn wallet(pool: &PgPool, u: Uuid) -> i64 {
    let a = user_wallet(pool, u, &usdc()).await.unwrap();
    balance(pool, a).await.unwrap().minor
}
async fn escrow(pool: &PgPool) -> i64 {
    let a = system_account(pool, ESCROW_KIND, &usdc(), Side::Credit)
        .await
        .unwrap();
    balance(pool, a).await.unwrap().minor
}

async fn open(
    pool: &PgPool,
    creator: Uuid,
    rated: bool,
    s: Option<&Stake>,
) -> Result<Uuid, GameError> {
    games::create_game(pool, creator, 300_000, 0, "white", rated, s).await
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn both_stakes_are_held_and_the_game_starts_only_with_both(pool: PgPool) {
    let (a, b) = (player(&pool, 1_000).await, player(&pool, 1_000).await);
    let game = open(&pool, a, true, Some(&stake(400))).await.unwrap();
    assert_eq!((wallet(&pool, a).await, escrow(&pool).await), (600, 400));

    let view = games::join_game(&pool, game, b, true).await.unwrap();
    assert_eq!(view.status, "active");
    assert_eq!(
        (view.stake_minor, view.stake_asset.as_deref()),
        (Some(400), Some(usdc().to_string().as_str()))
    );
    assert_eq!(wallet(&pool, b).await, 600);
    assert_eq!(escrow(&pool).await, 800, "escrow holds both stakes");
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_joiner_who_cannot_pay_does_not_start_the_game(pool: PgPool) {
    let (a, poor) = (player(&pool, 1_000).await, player(&pool, 100).await);
    let game = open(&pool, a, true, Some(&stake(400))).await.unwrap();
    let err = games::join_game(&pool, game, poor, true).await.unwrap_err();
    assert!(matches!(err, GameError::InsufficientFunds), "{err}");
    let view = games::get_game(&pool, game, a).await.unwrap();
    assert_eq!(view.status, "waiting", "still open for someone else");
    assert_eq!((wallet(&pool, poor).await, escrow(&pool).await), (100, 400));
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn cancelling_returns_the_creators_stake(pool: PgPool) {
    let a = player(&pool, 1_000).await;
    let game = open(&pool, a, true, Some(&stake(400))).await.unwrap();
    games::cancel_game(&pool, game, a).await.unwrap();
    assert_eq!((wallet(&pool, a).await, escrow(&pool).await), (1_000, 0));
    // Cancelling again is refused and moves nothing.
    assert!(games::cancel_game(&pool, game, a).await.is_err());
    assert_eq!((wallet(&pool, a).await, escrow(&pool).await), (1_000, 0));
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn creating_without_the_money_makes_no_game(pool: PgPool) {
    let poor = player(&pool, 100).await;
    let err = open(&pool, poor, true, Some(&stake(400)))
        .await
        .unwrap_err();
    assert!(matches!(err, GameError::InsufficientFunds), "{err}");
    let games: i64 = sqlx::query_scalar("SELECT count(*) FROM games WHERE created_by = $1")
        .bind(poor)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(games, 0, "the failed hold rolled the game back");
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn unrated_games_cannot_be_staked(pool: PgPool) {
    let a = player(&pool, 1_000).await;
    let err = open(&pool, a, false, Some(&stake(400))).await.unwrap_err();
    assert!(matches!(err, GameError::StakeNotOffered(_)), "{err}");
    assert_eq!(wallet(&pool, a).await, 1_000);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn an_unconfirmed_email_cannot_stake(pool: PgPool) {
    let a = player(&pool, 1_000).await;
    sqlx::query("UPDATE users SET email_verified_at = NULL WHERE id = $1")
        .bind(a)
        .execute(&pool)
        .await
        .unwrap();
    let err = open(&pool, a, true, Some(&stake(400))).await.unwrap_err();
    assert!(matches!(err, GameError::EmailNotConfirmed), "{err}");
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn an_open_fair_play_case_cannot_stake_but_can_still_play_unstaked(pool: PgPool) {
    let (a, b) = (player(&pool, 1_000).await, player(&pool, 1_000).await);
    sqlx::query("INSERT INTO fairplay_cases (id, player_id) VALUES ($1, $2)")
        .bind(Uuid::now_v7())
        .bind(a)
        .execute(&pool)
        .await
        .unwrap();
    let err = open(&pool, a, true, Some(&stake(400))).await.unwrap_err();
    assert!(matches!(err, GameError::StakingRestricted), "{err}");

    // As a joiner too.
    let game = open(&pool, b, true, Some(&stake(400))).await.unwrap();
    let err = games::join_game(&pool, game, a, true).await.unwrap_err();
    assert!(matches!(err, GameError::StakingRestricted), "{err}");
    assert_eq!((wallet(&pool, a).await, escrow(&pool).await), (1_000, 400));

    // Unstaked rated play is not affected by this rule.
    assert!(open(&pool, a, true, None).await.is_ok());
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn the_database_refuses_to_change_a_stake_or_stake_an_unrated_game(pool: PgPool) {
    let a = player(&pool, 1_000).await;
    let game = open(&pool, a, true, Some(&stake(400))).await.unwrap();
    let change = sqlx::query("UPDATE games SET stake_minor = 1 WHERE id = $1")
        .bind(game)
        .execute(&pool)
        .await;
    assert!(
        change.is_err(),
        "a stake cannot change after the game exists"
    );
    let unrate = sqlx::query("UPDATE games SET rated = FALSE WHERE id = $1")
        .bind(game)
        .execute(&pool)
        .await;
    assert!(unrate.is_err(), "a staked game cannot become unrated");
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn escrow_always_equals_the_stakes_of_open_and_started_games(pool: PgPool) {
    let ids = [
        player(&pool, 5_000).await,
        player(&pool, 5_000).await,
        player(&pool, 5_000).await,
    ];
    let g1 = open(&pool, ids[0], true, Some(&stake(100))).await.unwrap();
    let g2 = open(&pool, ids[1], true, Some(&stake(250))).await.unwrap();
    games::join_game(&pool, g1, ids[2], false).await.unwrap(); // g1 started: 200 in escrow
    games::cancel_game(&pool, g2, ids[1]).await.unwrap(); // g2 returned
    let held: i64 = sqlx::query_scalar(
        "SELECT COALESCE(sum(stake_minor * (CASE WHEN status = 'active' THEN 2 WHEN status = 'waiting' THEN 1 ELSE 0 END)), 0)::bigint FROM games WHERE stake_minor IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(escrow(&pool).await, held);
    assert_eq!(held, 200);
}
