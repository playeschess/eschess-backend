//! Staked games played on Lichess (ADR 0005), with a fake Lichess and a real Postgres (`make dev`, then
//! `make test-integration`): the challenge is made for the right two players, a result is recorded once, a no-show returns
//! the stakes, and anything unexpected is left alone for a person.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use eschess_domain::money::{Amount, AssetId};
use eschess_ledger::{Entry, IdempotencyKey, Posting, Side};
use eschess_lichess::{
    AccessToken, GameStatus, Lichess, LichessAccount, LichessError, LichessGame, NewChallenge,
    OpenChallenge, Side as Winner,
};
use eschess_storage::ledger::{balance, post};
use eschess_storage::stakes::Stake;
use eschess_storage::stellar::{system_account, user_wallet, CUSTODY_KIND};
use eschess_storage::{fairplay, games};
use eschess_worker::lichess_games::{tick, Config, Report};
use sqlx::PgPool;
use uuid::Uuid;

const ISSUER: &str = "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5";
/// A real game (Morphy's Opera Game, 33 plies) to give the fake Lichess legal moves.
const OPERA: &str = "e4 e5 Nf3 d6 d4 Bg4 dxe5 Bxf3 Qxf3 dxe5 Bc4 Nf6 Qb3 Qe7 Nc3 c6 Bg5 b5 Nxb5 cxb5 Bxb5+ Nbd7 O-O-O Rd8 Rxd7 Rxd7 Rd1 Qe6 Bxd7+ Nxd7 Qb8+ Nxb8 Rd8#";
const NOW: i64 = 1_800_000_000_000;

fn usdc() -> AssetId {
    AssetId::credit("USDC", ISSUER).unwrap()
}

/// Lichess as a test sets it up: challenges get the next id from `ids`; `games` is what the export says per id.
#[derive(Default)]
struct Fake {
    ids: Mutex<Vec<String>>,
    made: Mutex<Vec<NewChallenge>>,
    games: Mutex<HashMap<String, LichessGame>>,
    down: Mutex<bool>,
    refuse: Mutex<bool>,
}

impl Fake {
    fn with_ids(ids: &[&str]) -> Fake {
        let f = Fake::default();
        *f.ids.lock().unwrap() = ids.iter().rev().map(|s| s.to_string()).collect();
        f
    }
    /// What Lichess exports for `id`, played by the players `white` and `black`.
    fn set(
        &self,
        id: &str,
        white: &str,
        black: &str,
        status: GameStatus,
        winner: Option<Winner>,
        ply: i32,
    ) {
        let moves: Vec<String> = OPERA
            .split_whitespace()
            .take(ply as usize)
            .map(String::from)
            .collect();
        assert_eq!(moves.len(), ply as usize, "the fixture game is shorter");
        self.games.lock().unwrap().insert(
            id.to_string(),
            LichessGame {
                id: id.to_string(),
                status,
                winner,
                ply,
                clocks_cs: vec![30_000; moves.len()],
                moves,
                white: Some(white.into()),
                black: Some(black.into()),
                variant: "standard".into(),
                rated: false,
            },
        );
    }
}

#[async_trait]
impl Lichess for Fake {
    async fn exchange_code(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<AccessToken, LichessError> {
        unreachable!()
    }
    async fn account(&self, _: &AccessToken) -> Result<LichessAccount, LichessError> {
        unreachable!()
    }
    async fn revoke(&self, _: &AccessToken) -> Result<(), LichessError> {
        unreachable!()
    }
    async fn create_open_challenge(&self, c: &NewChallenge) -> Result<OpenChallenge, LichessError> {
        if *self.down.lock().unwrap() {
            return Err(LichessError::Unavailable("down".into()));
        }
        if *self.refuse.lock().unwrap() {
            return Err(LichessError::Rejected("no such user".into()));
        }
        self.made.lock().unwrap().push(c.clone());
        let id = self.ids.lock().unwrap().pop().expect("a test id");
        Ok(OpenChallenge {
            url: format!("https://lichess.org/{id}"),
            url_white: format!("https://lichess.org/{id}?color=white"),
            url_black: format!("https://lichess.org/{id}?color=black"),
            id,
        })
    }
    async fn game(&self, id: &str) -> Result<Option<LichessGame>, LichessError> {
        Ok(self.games.lock().unwrap().get(id).cloned())
    }
}

async fn player(pool: &PgPool, name: &str, minor: i64) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO users (id, email, username, password_hash, email_verified_at) VALUES ($1, $2, $3, 'x', now())")
        .bind(id).bind(format!("{name}@example.com")).bind(name).execute(pool).await.unwrap();
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
    eschess_storage::lichess::save(pool, id, name, name)
        .await
        .unwrap();
    id
}

async fn wallet(pool: &PgPool, u: Uuid) -> i64 {
    let a = user_wallet(pool, u, &usdc()).await.unwrap();
    balance(pool, a).await.unwrap().minor
}

/// Two linked players staked 400 each (White first) and the staked game started on Lichess. Lichess ids are the names.
async fn started(pool: &PgPool, white: &str, black: &str) -> (Uuid, Uuid, Uuid) {
    sqlx::query("UPDATE platform_settings SET value = 'lichess' WHERE name = 'game_venue'")
        .execute(pool)
        .await
        .unwrap();
    let (w, b) = (
        player(pool, white, 1_000).await,
        player(pool, black, 1_000).await,
    );
    let stake = Stake {
        asset: usdc(),
        minor: 400,
    };
    let game = games::create_game(pool, w, 600_000, 5_000, "white", true, Some(&stake))
        .await
        .unwrap();
    games::join_game(pool, game, b, true).await.unwrap();
    (game, w, b)
}

async fn state(pool: &PgPool, game: Uuid) -> (String, Option<String>, Option<String>, i32) {
    sqlx::query_as("SELECT status, result, lichess_game_id, ply FROM games WHERE id = $1")
        .bind(game)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn run(pool: &PgPool, f: &Fake, now_ms: i64) -> Report {
    tick(pool, f, now_ms, Config::default()).await.unwrap()
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn the_challenge_is_made_once_for_the_two_players_with_the_stakes_clock(pool: PgPool) {
    let (game, _, _) = started(&pool, "ada", "grace").await;
    let f = Fake::with_ids(&["AbCd1234", "Second12"]);
    assert_eq!(run(&pool, &f, NOW).await.created, 1);
    assert_eq!(run(&pool, &f, NOW).await.created, 0, "not made twice");
    let made = f.made.lock().unwrap().clone();
    assert_eq!(made.len(), 1);
    assert_eq!(
        (
            made[0].white.as_str(),
            made[0].black.as_str(),
            made[0].clock_limit_secs,
            made[0].clock_increment_secs,
            made[0].rated,
            made[0].expires_at_ms
        ),
        ("ada", "grace", 600, 5, false, NOW + 600_000)
    );
    assert_eq!(state(&pool, game).await.2.as_deref(), Some("AbCd1234"));
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_finished_game_is_recorded_once_and_a_game_still_on_is_left_alone(pool: PgPool) {
    let (game, _, _) = started(&pool, "ada", "grace").await;
    let f = Fake::with_ids(&["AbCd1234"]);
    run(&pool, &f, NOW).await;

    f.set("AbCd1234", "ada", "grace", GameStatus::Started, None, 10);
    assert_eq!(run(&pool, &f, NOW).await, Report::default());
    assert_eq!(state(&pool, game).await.0, "active");

    f.set(
        "AbCd1234",
        "ada",
        "grace",
        GameStatus::Resign,
        Some(Winner::Black),
        30,
    );
    assert_eq!(run(&pool, &f, NOW).await.finished, 1);
    assert_eq!(
        state(&pool, game).await,
        (
            "finished".into(),
            Some("black".into()),
            Some("AbCd1234".into()),
            30
        )
    );
    assert_eq!(
        run(&pool, &f, NOW).await,
        Report::default(),
        "a finished game is not looked at again"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn every_ending_maps_to_the_right_result(pool: PgPool) {
    let cases = [
        (GameStatus::Draw, None, "draw"),
        (GameStatus::Stalemate, None, "draw"),
        (GameStatus::InsufficientMaterialClaim, None, "draw"),
        (GameStatus::Aborted, None, "draw"),
        (GameStatus::OutOfTime, Some(Winner::White), "white"),
        (GameStatus::Timeout, Some(Winner::Black), "black"),
        (GameStatus::Mate, Some(Winner::White), "white"),
    ];
    for (i, (status, winner, want)) in cases.into_iter().enumerate() {
        let (w, b) = (format!("white{i}"), format!("black{i}"));
        let (game, _, _) = started(&pool, &w, &b).await;
        let id = format!("Game000{i}");
        let f = Fake::with_ids(&[id.as_str()]);
        run(&pool, &f, NOW).await;
        f.set(&id, &w, &b, status.clone(), winner, 30);
        run(&pool, &f, NOW).await;
        assert_eq!(
            state(&pool, game).await.1.as_deref(),
            Some(want),
            "{status:?}"
        );
    }
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn nobody_showing_up_is_a_draw_after_the_challenge_expires_and_the_stakes_can_come_back(
    pool: PgPool,
) {
    let (game, ada, grace) = started(&pool, "ada", "grace").await;
    let f = Fake::with_ids(&["AbCd1234"]);
    run(&pool, &f, NOW).await;

    // Before the challenge expires (and a minute of grace) an unstarted game is simply waiting.
    assert_eq!(run(&pool, &f, NOW + 600_000 + 30_000).await.no_shows, 0);
    assert_eq!(state(&pool, game).await.0, "active");
    // Lichess has no such game, long after expiry: a no-show. The database clock decides what is long enough.
    sqlx::query("UPDATE games SET lichess_challenge_expires_at = now() - interval '2 minutes' WHERE id = $1").bind(game).execute(&pool).await.unwrap();
    assert_eq!(run(&pool, &f, NOW).await.no_shows, 1);
    assert_eq!(
        state(&pool, game).await,
        (
            "finished".into(),
            Some("draw".into()),
            Some("AbCd1234".into()),
            0
        )
    );
    // Nothing moved in the ledger yet: settlement returns the stakes once fair play has looked at the game.
    assert_eq!(
        (wallet(&pool, ada).await, wallet(&pool, grace).await),
        (600, 600)
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_game_that_cannot_be_made_on_lichess_is_retried_then_given_up(pool: PgPool) {
    let (game, _, _) = started(&pool, "ada", "grace").await;
    let f = Fake::with_ids(&["AbCd1234"]);
    *f.down.lock().unwrap() = true;
    assert_eq!(
        run(&pool, &f, NOW).await,
        Report::default(),
        "Lichess down: try again later"
    );
    *f.down.lock().unwrap() = false;
    *f.refuse.lock().unwrap() = true;
    assert_eq!(
        run(&pool, &f, NOW).await,
        Report::default(),
        "refused: keep trying, a person can see the log"
    );
    assert_eq!(state(&pool, game).await.0, "active");
    // Twice the start window later, the stakes are given back.
    sqlx::query("UPDATE games SET started_at = now() - interval '21 minutes' WHERE id = $1")
        .bind(game)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(run(&pool, &f, NOW).await.no_shows, 1);
    assert_eq!(state(&pool, game).await.1.as_deref(), Some("draw"));
    // Lichess recovering later does not revive it.
    *f.refuse.lock().unwrap() = false;
    assert_eq!(run(&pool, &f, NOW).await.created, 0);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn unknown_endings_and_the_wrong_game_are_left_for_a_person(pool: PgPool) {
    let (game, _, _) = started(&pool, "ada", "grace").await;
    let f = Fake::with_ids(&["AbCd1234"]);
    run(&pool, &f, NOW).await;

    for (status, winner) in [
        (GameStatus::UnknownFinish, None),
        (GameStatus::Other("brandNew".into()), None),
        (GameStatus::Resign, None), // a win with no winner is not believable
    ] {
        f.set("AbCd1234", "ada", "grace", status.clone(), winner, 30);
        let r = run(&pool, &f, NOW).await;
        assert_eq!(state(&pool, game).await.0, "active", "{status:?}");
        assert_eq!(r.finished, 0, "{status:?}");
    }
    // Different players on the Lichess game than the ones we asked for.
    f.set(
        "AbCd1234",
        "ada",
        "mallory",
        GameStatus::Resign,
        Some(Winner::White),
        30,
    );
    assert_eq!(run(&pool, &f, NOW).await.held, 1);
    assert_eq!(state(&pool, game).await.0, "active");
    // Colours the wrong way round are not the game we asked for either.
    f.set(
        "AbCd1234",
        "grace",
        "ada",
        GameStatus::Resign,
        Some(Winner::White),
        30,
    );
    assert_eq!(run(&pool, &f, NOW).await.held, 1);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_second_worker_at_the_same_time_does_nothing(pool: PgPool) {
    started(&pool, "ada", "grace").await;
    let f = Fake::with_ids(&["AbCd1234", "Second12"]);
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext(current_database() || ':' || $1))")
        .bind("lichess-games")
        .execute(&mut *lock)
        .await
        .unwrap();
    assert!(run(&pool, &f, NOW).await.skipped);
    assert!(f.made.lock().unwrap().is_empty());
}

async fn count(pool: &PgPool, sql: &str, game: Uuid) -> i64 {
    sqlx::query_scalar(sql)
        .bind(game)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_finished_games_moves_are_stored_so_fair_play_can_score_them(pool: PgPool) {
    let (game, _, _) = started(&pool, "ada", "grace").await;
    let f = Fake::with_ids(&["AbCd1234"]);
    run(&pool, &f, NOW).await;
    f.set(
        "AbCd1234",
        "ada",
        "grace",
        GameStatus::Mate,
        Some(Winner::White),
        33,
    );
    assert_eq!(run(&pool, &f, NOW).await.finished, 1);

    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM game_moves WHERE game_id = $1",
            game
        )
        .await,
        33
    );
    // The scorer's own reader accepts it: consecutive plies, the right player on each, and positions that follow.
    let record = fairplay::game_record(&pool, game)
        .await
        .unwrap()
        .expect("a finished game");
    assert_eq!(record.moves.len(), 33);
    assert_eq!(record.moves[0].uci, "e2e4");
    assert_eq!(record.moves[32].uci, "d1d8");
    // The result is only paid once the scorer has run, as for any game.
    assert_eq!(
        eschess_storage::stakes::settle(&pool, game, "baseline-1", 0)
            .await
            .unwrap(),
        Err(eschess_storage::stakes::Waiting::NotScored)
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn moves_that_cannot_be_followed_leave_the_game_for_a_person(pool: PgPool) {
    let (game, _, _) = started(&pool, "ada", "grace").await;
    let f = Fake::with_ids(&["AbCd1234"]);
    run(&pool, &f, NOW).await;
    f.set(
        "AbCd1234",
        "ada",
        "grace",
        GameStatus::Mate,
        Some(Winner::White),
        33,
    );
    f.games.lock().unwrap().get_mut("AbCd1234").unwrap().moves[5] = "Qh7".into(); // not legal there
    let r = run(&pool, &f, NOW).await;
    assert_eq!((r.finished, r.held), (0, 1));
    assert_eq!(state(&pool, game).await.0, "active");
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM game_moves WHERE game_id = $1",
            game
        )
        .await,
        0,
        "nothing half-stored"
    );
    // Clock readings missing is the same.
    f.set(
        "AbCd1234",
        "ada",
        "grace",
        GameStatus::Mate,
        Some(Winner::White),
        33,
    );
    f.games
        .lock()
        .unwrap()
        .get_mut("AbCd1234")
        .unwrap()
        .clocks_cs
        .clear();
    assert_eq!(run(&pool, &f, NOW).await.held, 1);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_game_lichess_ended_for_cheating_opens_a_case_and_a_reviewer_decides_the_pot(
    pool: PgPool,
) {
    let (game, ada, grace) = started(&pool, "ada", "grace").await;
    let f = Fake::with_ids(&["AbCd1234"]);
    run(&pool, &f, NOW).await;
    // Lichess says White won because Black was found cheating.
    f.set(
        "AbCd1234",
        "ada",
        "grace",
        GameStatus::Cheat,
        Some(Winner::White),
        30,
    );
    assert_eq!(run(&pool, &f, NOW).await.finished, 1);
    assert_eq!(run(&pool, &f, NOW).await, Report::default(), "once");
    // Recorded as a draw, with a case open against the loser, and nothing paid.
    assert_eq!(state(&pool, game).await.1.as_deref(), Some("draw"));
    let open: Vec<Uuid> =
        sqlx::query_scalar("SELECT player_id FROM fairplay_cases WHERE status = 'open'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(open, vec![grace]);
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM game_moves WHERE game_id = $1",
            game
        )
        .await,
        30
    );
    // The scorer looks at the game (as for any), the open case holds the pot.
    sqlx::query(
        "INSERT INTO fairplay_runs (game_id, scorer, outcome) VALUES ($1, 'baseline-1', 'scored')",
    )
    .bind(game)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        eschess_storage::stakes::settle(&pool, game, "baseline-1", 0)
            .await
            .unwrap(),
        Err(eschess_storage::stakes::Waiting::OpenCase)
    );
    // A reviewer confirms: the honest player gets the whole pot.
    let reviewer = Uuid::now_v7();
    sqlx::query("INSERT INTO users (id, email, username, password_hash, email_verified_at) VALUES ($1, 'rev@example.com', 'reviewer', 'x', now())")
        .bind(reviewer).execute(&pool).await.unwrap();
    sqlx::query("UPDATE fairplay_cases SET status = 'confirmed', decided_by = $1, decided_at = now(), reason = 'Lichess and review agree' WHERE player_id = $2 AND status = 'open'")
        .bind(reviewer).bind(grace).execute(&pool).await.unwrap();
    let settled = eschess_storage::stakes::settle(&pool, game, "baseline-1", 0)
        .await
        .unwrap();
    assert_eq!(
        settled,
        Ok(eschess_storage::stakes::Settled::Confiscated { to: ada })
    );
    assert_eq!(
        (wallet(&pool, ada).await, wallet(&pool, grace).await),
        (1_400, 600)
    );
}
