//! Expiry of staked games nobody joined, against a real Postgres (`make dev`, then `make test-integration`).

use eschess_domain::money::{Amount, AssetId};
use eschess_ledger::{Entry, IdempotencyKey, Posting, Side};
use eschess_storage::games;
use eschess_storage::ledger::{balance, post};
use eschess_storage::stakes::Stake;
use eschess_storage::stellar::{system_account, user_wallet, CUSTODY_KIND};
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

use eschess_worker::expire::tick as expire;

async fn open(pool: &PgPool, creator: Uuid, stake: Option<&Stake>) -> Uuid {
    games::create_game(pool, creator, 300_000, 0, "white", true, stake)
        .await
        .unwrap()
}

async fn age(pool: &PgPool, game: Uuid, minutes: i32) {
    sqlx::query("UPDATE games SET created_at = now() - make_interval(mins => $2) WHERE id = $1")
        .bind(game)
        .bind(minutes)
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

async fn wallet_of(pool: &PgPool, u: Uuid) -> i64 {
    let a = user_wallet(pool, u, &usdc()).await.unwrap();
    balance(pool, a).await.unwrap().minor
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_staked_game_nobody_joins_is_cancelled_after_ten_minutes_and_the_stake_returned(
    pool: PgPool,
) {
    let (a, b, c) = (
        player(&pool).await,
        player(&pool).await,
        player(&pool).await,
    );
    let stake = Stake {
        asset: usdc(),
        minor: 400,
    };
    let stale = open(&pool, a, Some(&stake)).await;
    let fresh = open(&pool, b, Some(&stake)).await;
    let casual = open(&pool, c, None).await;
    for g in [stale, casual] {
        age(&pool, g, 11).await;
    }
    age(&pool, fresh, 9).await;
    assert_eq!(wallet_of(&pool, a).await, 600);

    let r = expire(&pool, 600).await.unwrap();
    assert_eq!(r.cancelled, 1, "{r:?}");
    assert_eq!(
        (
            status(&pool, stale).await.as_str(),
            wallet_of(&pool, a).await
        ),
        ("cancelled", 1_000)
    );
    assert_eq!(
        status(&pool, fresh).await,
        "waiting",
        "nine minutes is not ten"
    );
    assert_eq!(wallet_of(&pool, b).await, 600, "its stake is still held");
    assert_eq!(
        status(&pool, casual).await,
        "waiting",
        "unstaked games hold no money and are left alone"
    );

    // Again: nothing more to do, and nothing is returned twice.
    assert_eq!(expire(&pool, 600).await.unwrap().cancelled, 0);
    assert_eq!(wallet_of(&pool, a).await, 1_000);

    // A game joined in time is not touched however old it is.
    let joined = open(&pool, c, Some(&stake)).await;
    games::join_game(&pool, joined, a, true).await.unwrap();
    age(&pool, joined, 60).await;
    assert_eq!(expire(&pool, 600).await.unwrap().cancelled, 0);
    assert_eq!(status(&pool, joined).await, "active");
}
