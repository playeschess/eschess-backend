//! Settling staked games against a real Postgres (`make dev`, then `make test-integration`): ADR 0004 §5. A pot is paid
//! once, only after fair play has finished with the game and no case is open, and a confirmed cheat pays the opponent.

use eschess_domain::money::{Amount, AssetId};
use eschess_ledger::{Entry, IdempotencyKey, Posting, Side};
use eschess_storage::games;
use eschess_storage::ledger::{balance, post};
use eschess_storage::stakes::{self, Settled, Stake, Waiting, ESCROW_KIND, FEE_KIND};
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

const SCORER: &str = "baseline-1";

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

/// `white` and `black` each staked 400 and the game started; White is the creator.
async fn started(pool: &PgPool) -> (Uuid, Uuid, Uuid) {
    let (w, b) = (player(pool, 1_000).await, player(pool, 1_000).await);
    let game = games::create_game(pool, w, 300_000, 0, "white", true, Some(&stake(400)))
        .await
        .unwrap();
    games::join_game(pool, game, b, true).await.unwrap();
    (game, w, b)
}

/// Ends the game with `result` after `ply` plies, without playing it out.
async fn finish(pool: &PgPool, game: Uuid, result: &str, ply: i32) {
    sqlx::query("UPDATE games SET status = 'finished', result = $2, ply = $3, ended_at = now() WHERE id = $1")
        .bind(game).bind(result).bind(ply).execute(pool).await.unwrap();
}

/// The scorer has looked at the game, and scored `players` at 90.
async fn scored(pool: &PgPool, game: Uuid, players: &[Uuid]) {
    sqlx::query("INSERT INTO fairplay_runs (game_id, scorer, outcome) VALUES ($1, $2, 'scored')")
        .bind(game)
        .bind(SCORER)
        .execute(pool)
        .await
        .unwrap();
    for p in players {
        sqlx::query("INSERT INTO fairplay_scores (game_id, player_id, scorer, score, summary) VALUES ($1, $2, $3, 90, '')")
            .bind(game).bind(p).bind(SCORER).execute(pool).await.unwrap();
    }
}

/// A case about `player` containing the game; `decision` is `None` (left open), `cleared` or `confirmed`.
async fn case(pool: &PgPool, game: Uuid, player_id: Uuid, decision: Option<&str>) {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO fairplay_cases (id, player_id) VALUES ($1, $2)")
        .bind(id)
        .bind(player_id)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO fairplay_case_games (case_id, game_id, player_id, scorer) VALUES ($1, $2, $3, $4)")
        .bind(id).bind(game).bind(player_id).bind(SCORER).execute(pool).await.unwrap();
    if let Some(d) = decision {
        let reviewer = player(pool, 0).await;
        sqlx::query("UPDATE fairplay_cases SET status = $2, decided_by = $3, decided_at = now(), reason = 'x' WHERE id = $1")
            .bind(id).bind(d).bind(reviewer).execute(pool).await.unwrap();
    }
}

async fn settle(pool: &PgPool, game: Uuid) -> Result<Settled, Waiting> {
    stakes::settle(pool, game, SCORER, 0).await.unwrap()
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_pot_waits_for_fair_play_then_goes_to_the_winner_once(pool: PgPool) {
    let (game, w, b) = started(&pool).await;
    // Not finished yet: nothing to settle.
    assert_eq!(settle(&pool, game).await, Err(Waiting::NothingToDo));
    finish(&pool, game, "white", 40).await;
    // Finished but the scorer has not looked: the pot does not move.
    assert_eq!(settle(&pool, game).await, Err(Waiting::NotScored));
    assert_eq!(
        (
            wallet(&pool, w).await,
            wallet(&pool, b).await,
            escrow(&pool).await
        ),
        (600, 600, 800)
    );

    scored(&pool, game, &[]).await;
    assert_eq!(
        settle(&pool, game).await,
        Ok(Settled::Paid {
            winner: w,
            fee_minor: 0
        })
    );
    assert_eq!(
        (
            wallet(&pool, w).await,
            wallet(&pool, b).await,
            escrow(&pool).await
        ),
        (1_400, 600, 0)
    );

    // Once: a repeat changes nothing.
    assert_eq!(settle(&pool, game).await, Err(Waiting::NothingToDo));
    assert_eq!(wallet(&pool, w).await, 1_400);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn draws_and_games_that_were_never_really_played_return_the_stakes(pool: PgPool) {
    let (game, w, b) = started(&pool).await;
    finish(&pool, game, "draw", 60).await;
    scored(&pool, game, &[]).await;
    assert_eq!(settle(&pool, game).await, Ok(Settled::Returned));
    assert_eq!(
        (
            wallet(&pool, w).await,
            wallet(&pool, b).await,
            escrow(&pool).await
        ),
        (1_000, 1_000, 0)
    );

    // A resignation on move one is not a win worth a pot.
    let (game, w, b) = started(&pool).await;
    finish(&pool, game, "black", 2).await;
    scored(&pool, game, &[]).await;
    assert_eq!(settle(&pool, game).await, Ok(Settled::Returned));
    assert_eq!(
        (wallet(&pool, w).await, wallet(&pool, b).await),
        (1_000, 1_000)
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn an_open_case_holds_the_pot_and_the_decision_decides_it(pool: PgPool) {
    // Open: waits, whoever the case is about.
    let (game, w, b) = started(&pool).await;
    finish(&pool, game, "white", 40).await;
    scored(&pool, game, &[w]).await;
    case(&pool, game, w, None).await;
    assert_eq!(settle(&pool, game).await, Err(Waiting::OpenCase));
    assert_eq!(escrow(&pool).await, 800);

    // Cleared: the result stands.
    let (game, w, _b) = started(&pool).await;
    finish(&pool, game, "white", 40).await;
    scored(&pool, game, &[w]).await;
    case(&pool, game, w, Some("cleared")).await;
    assert_eq!(
        settle(&pool, game).await,
        Ok(Settled::Paid {
            winner: w,
            fee_minor: 0
        })
    );

    // Confirmed against the winner: the loser takes the whole pot (OD-14).
    let (game, w, b2) = started(&pool).await;
    finish(&pool, game, "white", 40).await;
    scored(&pool, game, &[w]).await;
    case(&pool, game, w, Some("confirmed")).await;
    assert_eq!(
        settle(&pool, game).await,
        Ok(Settled::Confiscated { to: b2 })
    );
    assert_eq!(wallet(&pool, b2).await, 1_400);
    assert_eq!(wallet(&pool, w).await, 600);
    let _ = b;
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn both_players_confirmed_return_both_stakes_and_a_cheating_loser_pays_the_winner_nothing_extra(
    pool: PgPool,
) {
    let (game, w, b) = started(&pool).await;
    finish(&pool, game, "white", 40).await;
    scored(&pool, game, &[w, b]).await;
    case(&pool, game, w, Some("confirmed")).await;
    case(&pool, game, b, Some("confirmed")).await;
    assert_eq!(settle(&pool, game).await, Ok(Settled::Returned));
    assert_eq!(
        (
            wallet(&pool, w).await,
            wallet(&pool, b).await,
            escrow(&pool).await
        ),
        (1_000, 1_000, 0)
    );

    // The loser confirmed as the cheat: the winner takes the pot, as they would have.
    let (game, w, b) = started(&pool).await;
    finish(&pool, game, "white", 40).await;
    scored(&pool, game, &[b]).await;
    case(&pool, game, b, Some("confirmed")).await;
    assert_eq!(
        settle(&pool, game).await,
        Ok(Settled::Confiscated { to: w })
    );
    assert_eq!(wallet(&pool, w).await, 1_400);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn the_fee_comes_out_of_a_decisive_pot_only(pool: PgPool) {
    let (game, w, _b) = started(&pool).await;
    finish(&pool, game, "white", 40).await;
    scored(&pool, game, &[]).await;
    // 5% of an 800 pot is 40.
    let r = stakes::settle(&pool, game, SCORER, 500).await.unwrap();
    assert_eq!(
        r,
        Ok(Settled::Paid {
            winner: w,
            fee_minor: 40
        })
    );
    assert_eq!(wallet(&pool, w).await, 600 + 760);
    let fees = system_account(&pool, FEE_KIND, &usdc(), Side::Credit)
        .await
        .unwrap();
    assert_eq!(balance(&pool, fees).await.unwrap().minor, 40);
    assert_eq!(escrow(&pool).await, 0);

    // A draw pays no fee, even if one is configured.
    let (game, w, b) = started(&pool).await;
    finish(&pool, game, "draw", 60).await;
    scored(&pool, game, &[]).await;
    assert_eq!(
        stakes::settle(&pool, game, SCORER, 500).await.unwrap(),
        Ok(Settled::Returned)
    );
    assert_eq!(
        (wallet(&pool, w).await, wallet(&pool, b).await),
        (1_000, 1_000)
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn two_workers_at_once_pay_the_pot_once(pool: PgPool) {
    let (game, w, _b) = started(&pool).await;
    finish(&pool, game, "white", 40).await;
    scored(&pool, game, &[]).await;
    let (a, b) = tokio::join!(
        stakes::settle(&pool, game, SCORER, 0),
        stakes::settle(&pool, game, SCORER, 0)
    );
    let outcomes = [a.unwrap(), b.unwrap()];
    assert_eq!(
        outcomes.iter().filter(|o| o.is_ok()).count(),
        1,
        "{outcomes:?}"
    );
    assert_eq!(wallet(&pool, w).await, 1_400);
    assert_eq!(escrow(&pool).await, 0);
}
