//! Games played on Lichess (ADR 0005), against a real Postgres (`make dev`, then `make test-integration`): our server runs
//! no clock and takes no moves for them, a result from outside is recorded once, and settlement then pays it like any other.

use eschess_domain::money::{Amount, AssetId};
use eschess_game::Refusal;
use eschess_ledger::{Entry, IdempotencyKey, Posting, Side};
use eschess_storage::games;
use eschess_storage::games::GameError;
use eschess_storage::ledger::{balance, post};
use eschess_storage::stakes::{self, Settled, Stake, Waiting, ESCROW_KIND};
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

async fn settle(pool: &PgPool, game: Uuid) -> Result<Settled, Waiting> {
    stakes::settle(pool, game, SCORER, 0).await.unwrap()
}

/// Puts `n` moves into the game, as if they had been fetched from Lichess.
async fn with_moves(pool: &PgPool, game: Uuid, white: Uuid, black: Uuid, n: i32) {
    for ply in 1..=n {
        sqlx::query("INSERT INTO game_moves (game_id, ply, uci, fen_after, mover_id, client_move_id, charged_ms, credited_ms, remaining_ms_after, played_at_ms) VALUES ($1, $2, 'e2e4', 'x', $3, $4, 1000, 0, 1000, $2)")
            .bind(game).bind(ply).bind(if ply % 2 == 1 { white } else { black }).bind(Uuid::now_v7())
            .execute(pool).await.unwrap();
    }
}

const LICHESS: &str = "AbCd1234";

fn played_elsewhere<T: std::fmt::Debug>(r: Result<T, GameError>) -> bool {
    matches!(r, Err(GameError::Refused(Refusal::PlayedElsewhere)))
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn our_server_takes_no_moves_clock_or_resignation_for_a_lichess_game(pool: PgPool) {
    let (game, w, b) = started(&pool).await;
    assert!(games::attach_lichess_game(&pool, game, LICHESS)
        .await
        .unwrap());
    let moved = games::play_move(
        &pool,
        game,
        w,
        "e2e4",
        Uuid::now_v7(),
        games::Transport::Http,
    )
    .await;
    assert!(played_elsewhere(moved));
    assert!(played_elsewhere(games::resign(&pool, game, b).await));
    assert!(played_elsewhere(games::offer_draw(&pool, game, w).await));
    // Even with the clock long run out, nothing flags it and nobody loses on time.
    sqlx::query("UPDATE games SET turn_started_at_ms = 1 WHERE id = $1")
        .bind(game)
        .execute(&pool)
        .await
        .unwrap();
    assert!(!games::flag_due(&pool, game).await.unwrap());
    games::get_game(&pool, game, w).await.unwrap();
    let status: String = sqlx::query_scalar("SELECT status FROM games WHERE id = $1")
        .bind(game)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "active");
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn attaching_is_once_and_only_before_a_move_and_one_lichess_game_serves_one_pot(
    pool: PgPool,
) {
    let (game, w, _) = started(&pool).await;
    assert!(games::attach_lichess_game(&pool, game, LICHESS)
        .await
        .unwrap());
    assert!(
        !games::attach_lichess_game(&pool, game, LICHESS)
            .await
            .unwrap(),
        "a repeat changes nothing"
    );
    assert!(
        games::attach_lichess_game(&pool, game, "ZzZz9999")
            .await
            .is_err(),
        "a different Lichess game is refused"
    );
    // The same Lichess game cannot settle a second pot.
    let (other, _, _) = started(&pool).await;
    assert!(games::attach_lichess_game(&pool, other, LICHESS)
        .await
        .is_err());
    // A game already moved on our server cannot move to Lichess, and a bad id is refused by the database.
    let (moved, mw, _) = started(&pool).await;
    games::play_move(
        &pool,
        moved,
        mw,
        "e2e4",
        Uuid::now_v7(),
        games::Transport::Http,
    )
    .await
    .unwrap();
    assert!(games::attach_lichess_game(&pool, moved, "Qwer5678")
        .await
        .is_err());
    let (bad, _, _) = started(&pool).await;
    assert!(games::attach_lichess_game(&pool, bad, "no good")
        .await
        .is_err());
    // The venue cannot be rewritten by hand either.
    assert!(
        sqlx::query("UPDATE games SET venue = 'own', lichess_game_id = NULL WHERE id = $1")
            .bind(game)
            .execute(&pool)
            .await
            .is_err()
    );
    let _ = w;
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_result_from_lichess_is_recorded_once_and_then_settles_like_any_other(pool: PgPool) {
    let (game, w, b) = started(&pool).await;
    assert!(games::attach_lichess_game(&pool, game, LICHESS)
        .await
        .unwrap());

    // Our own games cannot be finished this way, and a nonsense result is refused.
    let (own, _, _) = started(&pool).await;
    assert!(games::finish_external(&pool, own, "white", "mate", 40)
        .await
        .is_err());
    assert!(games::finish_external(&pool, game, "banana", "mate", 40)
        .await
        .is_err());

    // Nothing to settle while Lichess has not said.
    assert_eq!(settle(&pool, game).await, Err(Waiting::NothingToDo));

    assert!(games::finish_external(&pool, game, "black", "resign", 44)
        .await
        .unwrap());
    assert!(
        !games::finish_external(&pool, game, "black", "resign", 44)
            .await
            .unwrap(),
        "the same result again changes nothing"
    );
    assert!(
        games::finish_external(&pool, game, "white", "mate", 44)
            .await
            .is_err(),
        "a different result is refused"
    );

    // Its moves are not here yet (they are imported for scoring in a later step), so the pot waits even if a scorer run
    // were recorded: a payout never skips the fair-play check because the data is missing.
    scored(&pool, game, &[w, b]).await;
    assert_eq!(settle(&pool, game).await, Err(Waiting::NotScored));
    with_moves(&pool, game, w, b, 44).await;
    assert_eq!(
        settle(&pool, game).await,
        Ok(Settled::Paid {
            winner: b,
            fee_minor: 0
        })
    );
    assert_eq!(
        (
            wallet(&pool, w).await,
            wallet(&pool, b).await,
            escrow(&pool).await
        ),
        (600, 1_400, 800) // 800 is the other, still unsettled game's two stakes
    );
    assert_eq!(
        settle(&pool, game).await,
        Err(Waiting::NothingToDo),
        "paid once"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_short_or_drawn_lichess_game_returns_the_stakes(pool: PgPool) {
    for (result, ply) in [("draw", 60), ("white", 2)] {
        let (game, w, b) = started(&pool).await;
        games::attach_lichess_game(&pool, game, &format!("G{:07}", ply))
            .await
            .unwrap();
        games::finish_external(&pool, game, result, "x", ply)
            .await
            .unwrap();
        with_moves(&pool, game, w, b, ply).await;
        scored(&pool, game, &[w, b]).await;
        assert_eq!(settle(&pool, game).await, Ok(Settled::Returned));
        assert_eq!(
            (wallet(&pool, w).await, wallet(&pool, b).await),
            (1_000, 1_000)
        );
    }
}

async fn lichess_on(pool: &PgPool) {
    sqlx::query("UPDATE platform_settings SET value = 'lichess' WHERE name = 'game_venue'")
        .execute(pool)
        .await
        .unwrap();
}

async fn link(pool: &PgPool, user: Uuid, name: &str) {
    eschess_storage::lichess::save(pool, user, name, name)
        .await
        .unwrap();
}

async fn venue_of(pool: &PgPool, game: Uuid) -> (String, Option<String>) {
    sqlx::query_as("SELECT venue, lichess_game_id FROM games WHERE id = $1")
        .bind(game)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn staked_games_start_on_lichess_only_for_linked_players_and_a_clock_it_accepts(
    pool: PgPool,
) {
    // By default games are played on our server and no account is needed.
    let (before, w, b) = started(&pool).await;
    assert_eq!(venue_of(&pool, before).await.0, "own");

    lichess_on(&pool).await;
    let (ada, grace) = (player(&pool, 1_000).await, player(&pool, 1_000).await);
    // Creating or joining needs a linked account, and nothing is held for a refused attempt.
    assert!(matches!(
        games::create_game(&pool, ada, 300_000, 0, "white", true, Some(&stake(400))).await,
        Err(GameError::LichessNotLinked)
    ));
    link(&pool, ada, "ada").await;
    assert!(
        matches!(
            games::create_game(&pool, ada, 300_500, 0, "white", true, Some(&stake(400))).await,
            Err(GameError::InvalidTimeControl(_))
        ),
        "whole seconds only"
    );
    assert!(
        matches!(
            games::create_game(
                &pool,
                ada,
                300_000,
                61_000,
                "white",
                true,
                Some(&stake(400))
            )
            .await,
            Err(GameError::InvalidTimeControl(_))
        ),
        "an increment over a minute"
    );
    let game = games::create_game(&pool, ada, 300_000, 2_000, "white", true, Some(&stake(400)))
        .await
        .unwrap();
    assert!(matches!(
        games::join_game(&pool, game, grace, true).await,
        Err(GameError::LichessNotLinked)
    ));
    assert_eq!(
        wallet(&pool, grace).await,
        1_000,
        "the refused join held nothing"
    );
    link(&pool, grace, "grace").await;
    games::join_game(&pool, game, grace, true).await.unwrap();
    assert_eq!(
        venue_of(&pool, before).await.0,
        "own",
        "a game that started before the switch stays where it was"
    );
    assert_eq!(venue_of(&pool, game).await, ("lichess".to_string(), None));

    // The game that started before the switch stays where it was, and the new one cannot be played here.
    assert_eq!(
        venue_of(&pool, started_before(&pool, w, b).await).await.0,
        "lichess"
    );
    let moved = games::play_move(
        &pool,
        game,
        ada,
        "e2e4",
        Uuid::now_v7(),
        games::Transport::Http,
    )
    .await;
    assert!(played_elsewhere(moved));
}

/// A game between two players made after the switch (they are linked here).
async fn started_before(pool: &PgPool, w: Uuid, b: Uuid) -> Uuid {
    link(pool, w, "whitey").await;
    link(pool, b, "blacky").await;
    let game = games::create_game(pool, w, 300_000, 0, "white", true, Some(&stake(400)))
        .await
        .unwrap();
    games::join_game(pool, game, b, true).await.unwrap();
    game
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn the_lichess_game_is_filled_in_once_and_the_venue_never_goes_back(pool: PgPool) {
    lichess_on(&pool).await;
    let (w, b) = (player(&pool, 1_000).await, player(&pool, 1_000).await);
    let game = started_before(&pool, w, b).await;

    let todo = eschess_storage::lichess_games::to_create(&pool, 10)
        .await
        .unwrap();
    assert_eq!(todo.len(), 1);
    assert_eq!(
        (todo[0].id, todo[0].white.as_str(), todo[0].black.as_str()),
        (game, "whitey", "blacky")
    );

    let expires = 4_000_000_000_000;
    assert!(
        eschess_storage::lichess_games::set_challenge(&pool, game, LICHESS, expires)
            .await
            .unwrap()
    );
    assert!(
        !eschess_storage::lichess_games::set_challenge(&pool, game, "ZzZz9999", expires)
            .await
            .unwrap(),
        "already made"
    );
    assert!(eschess_storage::lichess_games::to_create(&pool, 10)
        .await
        .unwrap()
        .is_empty());
    let playing = eschess_storage::lichess_games::in_play(&pool, 10)
        .await
        .unwrap();
    assert_eq!(
        (
            playing.len(),
            playing[0].lichess_game_id.as_str(),
            playing[0].challenge_expired
        ),
        (1, LICHESS, false)
    );

    assert!(
        sqlx::query("UPDATE games SET venue = 'own', lichess_game_id = NULL WHERE id = $1")
            .bind(game)
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE games SET lichess_game_id = 'ZzZz9999' WHERE id = $1")
            .bind(game)
            .execute(&pool)
            .await
            .is_err()
    );
}
