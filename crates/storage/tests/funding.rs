//! The `funding` state of a staked game on the contract path (ADR 0007, build step 1), against a real Postgres.

use eschess_domain::money::{Amount, AssetId};
use eschess_ledger::{Entry, IdempotencyKey, Posting, Side};
use eschess_storage::games::{self, GameError};
use eschess_storage::ledger::{balance, post};
use eschess_storage::stakes::{self, Stake, ESCROW_KIND};
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
async fn set_path(pool: &PgPool, path: &str) {
    sqlx::query("UPDATE platform_settings SET value = $1 WHERE name = 'stake_path'")
        .bind(path)
        .execute(pool)
        .await
        .unwrap();
}
async fn status(pool: &PgPool, game: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM games WHERE id = $1")
        .bind(game)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// A staked game made and joined on whatever path the setting says; the creator plays White.
async fn made(pool: &PgPool) -> (Uuid, Uuid, Uuid) {
    let (w, b) = (player(pool, 1_000).await, player(pool, 1_000).await);
    let game = games::create_game(pool, w, 300_000, 0, "white", true, Some(&stake(400)))
        .await
        .unwrap();
    games::join_game(pool, game, b, true).await.unwrap();
    (game, w, b)
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn the_ledger_path_is_the_default_and_starts_games_at_once(pool: PgPool) {
    let (game, _, _) = made(&pool).await;
    assert_eq!(status(&pool, game).await, "active");
    let path: String = sqlx::query_scalar("SELECT stake_path FROM games WHERE id = $1")
        .bind(game)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(path, "ledger");
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_contract_game_waits_in_funding_with_both_stakes_held_and_no_clock(pool: PgPool) {
    set_path(&pool, "contract").await;
    let (game, w, b) = made(&pool).await;
    assert_eq!(status(&pool, game).await, "funding");
    assert_eq!(
        (
            wallet(&pool, w).await,
            wallet(&pool, b).await,
            escrow(&pool).await
        ),
        (600, 600, 800)
    );
    let (started, turn): (Option<i64>, bool) =
        sqlx::query_as("SELECT turn_started_at_ms, started_at IS NULL FROM games WHERE id = $1")
            .bind(game)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((started, turn), (None, true), "no clock is running");

    // Nobody can move yet, and the ledger and the books agree.
    let err = games::play_move(
        &pool,
        game,
        w,
        "e2e4",
        Uuid::now_v7(),
        games::Transport::Http,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, GameError::NotStarted), "{err}");
    assert!(stakes::escrow_mismatches(&pool).await.unwrap().is_empty());
    let listed = games::funding_games(&pool, 10).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(
        (
            listed[0].id,
            listed[0].white,
            listed[0].black,
            listed[0].stake.minor
        ),
        (game, w, b, 400)
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn activating_starts_the_clock_once(pool: PgPool) {
    set_path(&pool, "contract").await;
    let (game, w, _) = made(&pool).await;
    assert!(games::activate_funded(&pool, game).await.unwrap());
    assert_eq!(status(&pool, game).await, "active");
    assert!(
        !games::activate_funded(&pool, game).await.unwrap(),
        "a second activation does nothing"
    );
    // The game can be played now.
    games::play_move(
        &pool,
        game,
        w,
        "e2e4",
        Uuid::now_v7(),
        games::Transport::Http,
    )
    .await
    .unwrap();
    assert!(games::funding_games(&pool, 10).await.unwrap().is_empty());
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn cancelling_a_funding_game_returns_both_stakes_once(pool: PgPool) {
    set_path(&pool, "contract").await;
    let (game, w, b) = made(&pool).await;
    assert!(games::cancel_funding(&pool, game).await.unwrap());
    assert_eq!(status(&pool, game).await, "cancelled");
    assert_eq!(
        (
            wallet(&pool, w).await,
            wallet(&pool, b).await,
            escrow(&pool).await
        ),
        (1_000, 1_000, 0)
    );
    assert!(
        !games::cancel_funding(&pool, game).await.unwrap(),
        "nothing is returned twice"
    );
    assert!(
        !games::activate_funded(&pool, game).await.unwrap(),
        "a cancelled game cannot start"
    );
    assert_eq!(wallet(&pool, w).await, 1_000);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_game_keeps_the_path_it_was_made_on_when_the_setting_changes(pool: PgPool) {
    // Made on the ledger path, joined after the switch: still the ledger path, so it starts at once.
    let (w, b) = (player(&pool, 1_000).await, player(&pool, 1_000).await);
    let ledger_game = games::create_game(&pool, w, 300_000, 0, "white", true, Some(&stake(400)))
        .await
        .unwrap();
    set_path(&pool, "contract").await;
    games::join_game(&pool, ledger_game, b, true).await.unwrap();
    assert_eq!(status(&pool, ledger_game).await, "active");

    // Made on the contract path, joined after the switch back: still funding.
    let (c, d) = (player(&pool, 1_000).await, player(&pool, 1_000).await);
    let contract_game = games::create_game(&pool, c, 300_000, 0, "white", true, Some(&stake(400)))
        .await
        .unwrap();
    set_path(&pool, "ledger").await;
    games::join_game(&pool, contract_game, d, true)
        .await
        .unwrap();
    assert_eq!(status(&pool, contract_game).await, "funding");

    // The database refuses to change it afterwards, and an unstaked game is always the ledger path.
    assert!(
        sqlx::query("UPDATE games SET stake_path = 'ledger' WHERE id = $1")
            .bind(contract_game)
            .execute(&pool)
            .await
            .is_err()
    );
    let free = games::create_game(&pool, c, 300_000, 0, "white", true, None)
        .await
        .unwrap();
    let path: String = sqlx::query_scalar("SELECT stake_path FROM games WHERE id = $1")
        .bind(free)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(path, "ledger");
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn the_ledger_settle_job_leaves_contract_games_alone(pool: PgPool) {
    set_path(&pool, "contract").await;
    let (game, _, _) = made(&pool).await;
    games::activate_funded(&pool, game).await.unwrap();
    sqlx::query("UPDATE games SET status = 'finished', result = 'white', ply = 40, ended_at = now() WHERE id = $1")
        .bind(game).execute(&pool).await.unwrap();
    assert!(stakes::unsettled(&pool, 10).await.unwrap().is_empty());
    let r = stakes::settle(&pool, game, "baseline-1", 0).await.unwrap();
    assert_eq!(r, Err(stakes::Waiting::NothingToDo));
    assert_eq!(escrow(&pool).await, 800, "nothing moved");
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn two_players_paired_by_the_queue_on_the_contract_path_wait_in_funding(pool: PgPool) {
    set_path(&pool, "contract").await;
    let (a, b) = (player(&pool, 1_000).await, player(&pool, 1_000).await);
    let s = stake(400);
    let mut coin = || true;
    eschess_storage::matchmaking::enqueue(&pool, a, 300_000, 0, true, Some(&s), &mut coin)
        .await
        .unwrap();
    let st = eschess_storage::matchmaking::enqueue(&pool, b, 300_000, 0, true, Some(&s), &mut coin)
        .await
        .unwrap();
    let eschess_storage::matchmaking::Status::Matched { game_id } = st else {
        panic!("not paired: {st:?}")
    };
    assert_eq!(status(&pool, game_id).await, "funding");
    assert_eq!(
        (
            wallet(&pool, a).await,
            wallet(&pool, b).await,
            escrow(&pool).await
        ),
        (600, 600, 800)
    );
    assert!(stakes::escrow_mismatches(&pool).await.unwrap().is_empty());
}
