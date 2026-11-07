//! The fair-play job against a real Postgres (`make dev`, then `make test-integration`): finished games are scored once
//! per scorer, high scores open review cases, and a game that cannot be scored is set aside with the reason instead of
//! blocking the rest. The last tests go around the code with raw SQL to show the database refuses bad scores and cases
//! on its own.

use eschess_fairplay::{GameRecord, NoopScorer, RiskScore, Scorer};
use eschess_storage::fairplay::{
    game_record, open_case, record_scores, run_of, scores_of_game, unscored_games, Recorded,
};
use eschess_worker::fairplay::{tick, FairplayConfig};
use serde_json::json;
use sqlx::{Execute, PgPool};
use uuid::Uuid;

const CONFIG: FairplayConfig = FairplayConfig {
    case_threshold: 70,
    batch: 50,
};

async fn user(pool: &PgPool) -> Uuid {
    let id = Uuid::now_v7();
    let name = format!("u{}", &id.simple().to_string()[20..]);
    sqlx::query("INSERT INTO users (id, email, username, password_hash) VALUES ($1, $2, $3, 'x')")
        .bind(id)
        .bind(format!("{name}@example.com"))
        .bind(&name)
        .execute(pool)
        .await
        .unwrap();
    id
}

/// A game between `white` and `black` with the given moves (White first), finished unless `finished` is false.
async fn game(pool: &PgPool, white: Uuid, black: Uuid, moves: &[&str], finished: bool) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO games (id, created_by, color_preference, white_id, black_id, base_ms, increment_ms, status, result, \
         end_reason, fen, ply, white_remaining_ms, black_remaining_ms, white_lag_quota_ms, black_lag_quota_ms, \
         turn_started_at_ms, started_at, ended_at) \
         VALUES ($1, $2, 'white', $2, $3, 180000, 2000, $4, $5, $6, 'x', $7, 1000, 1000, 0, 0, $8, now(), \
         CASE WHEN $4 = 'finished' THEN clock_timestamp() END)",
    )
    .bind(id)
    .bind(white)
    .bind(black)
    .bind(if finished { "finished" } else { "active" })
    .bind(finished.then_some("white"))
    .bind(finished.then_some("resignation"))
    .bind(moves.len() as i32)
    .bind((!finished).then_some(0i64))
    .execute(pool)
    .await
    .unwrap();
    for (i, uci) in moves.iter().enumerate() {
        let mover = if i % 2 == 0 { white } else { black };
        sqlx::query(
            "INSERT INTO game_moves (game_id, ply, uci, fen_after, mover_id, client_move_id, charged_ms, credited_ms, \
             remaining_ms_after, played_at_ms, transport, lag_measured_ms) \
             VALUES ($1, $2, $3, $4, $5, $6, 900, 30, 100000, $7, 'ws', 45)",
        )
        .bind(id)
        .bind(i as i32 + 1)
        .bind(uci)
        .bind(format!("after {uci}"))
        .bind(mover)
        .bind(Uuid::now_v7())
        .bind(1_000 * i as i64)
        .execute(pool)
        .await
        .unwrap();
    }
    id
}

/// Scores the player `suspect` at `score`, and nobody else.
struct Flags {
    name: &'static str,
    suspect: Uuid,
    score: u8,
}

impl Scorer for Flags {
    fn name(&self) -> &str {
        self.name
    }

    fn score(&self, game: &GameRecord) -> Vec<RiskScore> {
        if game.color_of(self.suspect).is_none() {
            return vec![];
        }
        vec![RiskScore {
            player_id: self.suspect,
            score: self.score,
            scorer: self.name.into(),
            summary: format!("{} moves looked at", game.moves.len()),
            evidence: json!({ "moves": game.moves.len() }),
        }]
    }
}

/// Returns a score for someone who was not in the game.
struct Confused;

impl Scorer for Confused {
    fn name(&self) -> &str {
        "confused-1"
    }

    fn score(&self, _: &GameRecord) -> Vec<RiskScore> {
        vec![RiskScore {
            player_id: Uuid::now_v7(),
            score: 99,
            scorer: "confused-1".into(),
            summary: String::new(),
            evidence: json!(null),
        }]
    }
}

/// Panics on games with exactly one move, scores the rest 0.
struct Fragile;

impl Scorer for Fragile {
    fn name(&self) -> &str {
        "fragile-1"
    }

    fn score(&self, game: &GameRecord) -> Vec<RiskScore> {
        assert!(game.moves.len() != 1, "a bug in the scorer");
        NoopScorer
            .score(game)
            .into_iter()
            .map(|mut s| {
                s.scorer = "fragile-1".into();
                s
            })
            .collect()
    }
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn every_finished_game_is_scored_once_and_games_in_progress_wait(pool: PgPool) {
    let (a, b) = (user(&pool).await, user(&pool).await);
    let done = game(&pool, a, b, &["e2e4", "e7e5"], true).await;
    let going = game(&pool, a, b, &["d2d4"], false).await;

    let r = tick(&pool, &NoopScorer, &CONFIG).await.unwrap();
    assert_eq!((r.scored, r.unscorable, r.cases), (1, 0, 0));
    let scores = scores_of_game(&pool, done).await.unwrap();
    assert_eq!(scores.len(), 2, "both players");
    assert!(scores.iter().all(|s| s.score == 0 && s.scorer == "noop-1"));
    assert!(scores_of_game(&pool, going).await.unwrap().is_empty());

    // Nothing new: the next round does nothing.
    let again = tick(&pool, &NoopScorer, &CONFIG).await.unwrap();
    assert_eq!((again.scored, again.unscorable), (0, 0));

    // The game in progress is scored once it finishes.
    sqlx::query("UPDATE games SET status = 'finished', result = 'draw', end_reason = 'agreement', ended_at = now() WHERE id = $1")
        .bind(going)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(tick(&pool, &NoopScorer, &CONFIG).await.unwrap().scored, 1);
    assert_eq!(scores_of_game(&pool, going).await.unwrap().len(), 2);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_high_score_opens_one_case_per_player_and_later_games_join_it(pool: PgPool) {
    let (cheat, honest, other) = (user(&pool).await, user(&pool).await, user(&pool).await);
    let g1 = game(&pool, cheat, honest, &["e2e4", "e7e5", "g1f3"], true).await;
    let g2 = game(&pool, other, cheat, &["d2d4", "d7d5"], true).await;
    let flags = Flags {
        name: "flags-1",
        suspect: cheat,
        score: 85,
    };
    let r = tick(&pool, &flags, &CONFIG).await.unwrap();
    assert_eq!((r.scored, r.cases), (2, 2));

    let case = open_case(&pool, cheat)
        .await
        .unwrap()
        .expect("a case is open");
    assert_eq!(case.status, "open");
    let mut games = case.games.clone();
    games.sort();
    let mut expected = vec![g1, g2];
    expected.sort();
    assert_eq!(games, expected, "both games, in one case");
    assert!(open_case(&pool, honest).await.unwrap().is_none());
    let opened: i64 = sqlx::query_scalar("SELECT count(*) FROM fairplay_cases")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(opened, 1);

    let s = &scores_of_game(&pool, g1).await.unwrap()[0];
    assert_eq!((s.player_id, s.score), (cheat, 85));
    assert_eq!(
        s.evidence,
        json!({ "moves": 3 }),
        "evidence is stored as given"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_score_below_the_threshold_opens_nothing(pool: PgPool) {
    let (a, b) = (user(&pool).await, user(&pool).await);
    game(&pool, a, b, &["e2e4"], true).await;
    let flags = Flags {
        name: "flags-1",
        suspect: a,
        score: 69,
    };
    let r = tick(&pool, &flags, &CONFIG).await.unwrap();
    assert_eq!((r.scored, r.cases), (1, 0));
    assert!(open_case(&pool, a).await.unwrap().is_none());
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn scores_that_break_the_rules_are_set_aside_with_the_reason_and_not_retried(pool: PgPool) {
    let (a, b) = (user(&pool).await, user(&pool).await);
    let g = game(&pool, a, b, &["e2e4"], true).await;
    let r = tick(&pool, &Confused, &CONFIG).await.unwrap();
    assert_eq!((r.scored, r.unscorable), (0, 1));
    let (outcome, reason) = run_of(&pool, g, "confused-1").await.unwrap().unwrap();
    assert_eq!(outcome, "unscorable");
    assert!(
        reason.unwrap().contains("did not play"),
        "the reason says what was wrong"
    );
    assert!(scores_of_game(&pool, g).await.unwrap().is_empty());
    assert_eq!(
        tick(&pool, &Confused, &CONFIG).await.unwrap().unscorable,
        0,
        "not retried"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_scorer_that_panics_costs_one_game_not_the_worker(pool: PgPool) {
    let (a, b) = (user(&pool).await, user(&pool).await);
    let bad = game(&pool, a, b, &["e2e4"], true).await;
    let good = game(&pool, a, b, &["e2e4", "e7e5"], true).await;
    let r = tick(&pool, &Fragile, &CONFIG).await.unwrap();
    assert_eq!((r.scored, r.unscorable), (1, 1));
    assert_eq!(
        run_of(&pool, bad, "fragile-1").await.unwrap(),
        Some(("unscorable".into(), Some("the scorer panicked".into())))
    );
    assert_eq!(scores_of_game(&pool, good).await.unwrap().len(), 2);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_game_whose_stored_moves_do_not_add_up_is_set_aside(pool: PgPool) {
    let (a, b) = (user(&pool).await, user(&pool).await);
    let g = game(&pool, a, b, &["e2e4"], true).await;
    // A second move "by White" again: the stored game contradicts itself.
    sqlx::query(
        "INSERT INTO game_moves (game_id, ply, uci, fen_after, mover_id, client_move_id, charged_ms, credited_ms, \
         remaining_ms_after, played_at_ms) VALUES ($1, 2, 'd2d4', 'x', $2, $3, 0, 0, 0, 0)",
    )
    .bind(g)
    .bind(a)
    .bind(Uuid::now_v7())
    .execute(&pool)
    .await
    .unwrap();
    let r = tick(&pool, &NoopScorer, &CONFIG).await.unwrap();
    assert_eq!((r.scored, r.unscorable), (0, 1));
    let (_, reason) = run_of(&pool, g, "noop-1").await.unwrap().unwrap();
    assert!(reason.unwrap().contains("ply 2"));
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn a_new_scorer_version_scores_everything_again_without_touching_the_old_scores(
    pool: PgPool,
) {
    let (a, b) = (user(&pool).await, user(&pool).await);
    let g = game(&pool, a, b, &["e2e4"], true).await;
    let v1 = Flags {
        name: "flags-1",
        suspect: a,
        score: 10,
    };
    let v2 = Flags {
        name: "flags-2",
        suspect: a,
        score: 20,
    };
    tick(&pool, &v1, &CONFIG).await.unwrap();
    assert_eq!(unscored_games(&pool, "flags-2", 10).await.unwrap(), vec![g]);
    tick(&pool, &v2, &CONFIG).await.unwrap();
    let scores: Vec<(String, u8)> = scores_of_game(&pool, g)
        .await
        .unwrap()
        .into_iter()
        .map(|s| (s.scorer, s.score))
        .collect();
    assert_eq!(scores, [("flags-1".into(), 10), ("flags-2".into(), 20)]);
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn workers_scoring_the_same_game_at_once_record_it_once(pool: PgPool) {
    let (cheat, b) = (user(&pool).await, user(&pool).await);
    let g = game(&pool, cheat, b, &["e2e4"], true).await;
    let record = game_record(&pool, g).await.unwrap().unwrap();
    let flags = Flags {
        name: "flags-1",
        suspect: cheat,
        score: 90,
    };
    let scores = flags.score(&record);
    let tries = (0..8).map(|_| {
        let (pool, record, scores) = (pool.clone(), record.clone(), scores.clone());
        async move {
            record_scores(&pool, &record, "flags-1", &scores, 70)
                .await
                .unwrap()
        }
    });
    let results = futures::future::join_all(tries).await;
    let stored = results
        .iter()
        .filter(|r| matches!(r, Recorded::Scored { .. }))
        .count();
    assert_eq!(stored, 1, "{results:?}");
    assert!(results
        .iter()
        .all(|r| matches!(r, Recorded::Scored { .. } | Recorded::AlreadyRun)));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM fairplay_case_games")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);

    // And whole rounds at once: one takes the lock, the others step aside.
    let (c, d) = (user(&pool).await, user(&pool).await);
    for _ in 0..5 {
        game(&pool, c, d, &["e2e4"], true).await;
    }
    let rounds = (0..4).map(|_| {
        let pool = pool.clone();
        async move { tick(&pool, &NoopScorer, &CONFIG).await.unwrap() }
    });
    let reports = futures::future::join_all(rounds).await;
    let scored: usize = reports.iter().map(|r| r.scored).sum();
    assert_eq!(
        scored, 6,
        "every game once (5 new + the one above), across all rounds"
    );
}

#[sqlx::test(migrator = "eschess_storage::MIGRATOR")]
#[ignore = "needs DATABASE_URL: run `make dev` then `make test-integration`"]
async fn the_database_refuses_bad_scores_and_case_changes_on_its_own(pool: PgPool) {
    let (cheat, b, stranger, reviewer) = (
        user(&pool).await,
        user(&pool).await,
        user(&pool).await,
        user(&pool).await,
    );
    let g = game(&pool, cheat, b, &["e2e4"], true).await;
    let flags = Flags {
        name: "flags-1",
        suspect: cheat,
        score: 90,
    };
    tick(&pool, &flags, &CONFIG).await.unwrap();
    let case = open_case(&pool, cheat).await.unwrap().unwrap().id;
    let other = game(&pool, cheat, b, &["d2d4"], true).await;
    sqlx::query(
        "INSERT INTO fairplay_runs (game_id, scorer, outcome) VALUES ($1, 'manual-1', 'scored')",
    )
    .bind(other)
    .execute(&pool)
    .await
    .unwrap();

    let refused = [
        // A score for someone who did not play.
        sqlx::query("INSERT INTO fairplay_scores (game_id, player_id, scorer, score, summary) VALUES ($1, $2, 'manual-1', 50, '')")
            .bind(other).bind(stranger),
        // A score with no run behind it.
        sqlx::query("INSERT INTO fairplay_scores (game_id, player_id, scorer, score, summary) VALUES ($1, $2, 'ghost-1', 50, '')")
            .bind(other).bind(cheat),
        // Out of range.
        sqlx::query("INSERT INTO fairplay_scores (game_id, player_id, scorer, score, summary) VALUES ($1, $2, 'manual-1', 101, '')")
            .bind(other).bind(cheat),
        // Changing or removing a score.
        sqlx::query("UPDATE fairplay_scores SET score = 0 WHERE game_id = $1 AND player_id = $2").bind(g).bind(cheat),
        sqlx::query("DELETE FROM fairplay_scores WHERE game_id = $1 AND player_id = $2").bind(g).bind(cheat),
        // A second open case for the same player.
        sqlx::query("INSERT INTO fairplay_cases (id, player_id) VALUES ($1, $2)").bind(Uuid::now_v7()).bind(cheat),
        // Deciding your own case, or deciding without a reason.
        sqlx::query("UPDATE fairplay_cases SET status = 'cleared', decided_by = player_id, decided_at = now(), reason = 'x' WHERE id = $1").bind(case),
        sqlx::query("UPDATE fairplay_cases SET status = 'cleared', decided_by = $2, decided_at = now() WHERE id = $1").bind(case).bind(reviewer),
        // Deleting a case.
        sqlx::query("DELETE FROM fairplay_cases WHERE id = $1").bind(case),
    ];
    for q in refused {
        let sql = q.sql().to_string();
        assert!(
            q.execute(&pool).await.is_err(),
            "the database allowed: {sql}"
        );
    }

    // A decided case cannot be reopened, and no game can join it.
    sqlx::query("UPDATE fairplay_cases SET status = 'cleared', decided_by = $2, decided_at = now(), reason = 'engine-like but within the player''s history' WHERE id = $1")
        .bind(case)
        .bind(reviewer)
        .execute(&pool)
        .await
        .unwrap();
    assert!(sqlx::query("UPDATE fairplay_cases SET status = 'open', decided_by = NULL, decided_at = NULL, reason = NULL WHERE id = $1")
        .bind(case)
        .execute(&pool)
        .await
        .is_err());
    sqlx::query("INSERT INTO fairplay_scores (game_id, player_id, scorer, score, summary) VALUES ($1, $2, 'manual-1', 95, '')")
        .bind(other)
        .bind(cheat)
        .execute(&pool)
        .await
        .unwrap();
    assert!(sqlx::query("INSERT INTO fairplay_case_games (case_id, game_id, player_id, scorer) VALUES ($1, $2, $3, 'manual-1')")
        .bind(case)
        .bind(other)
        .bind(cheat)
        .execute(&pool)
        .await
        .is_err());

    // A new suspicious game after the decision opens a fresh case.
    let g3 = game(&pool, b, cheat, &["c2c4"], true).await;
    tick(&pool, &flags, &CONFIG).await.unwrap();
    let fresh = open_case(&pool, cheat).await.unwrap().unwrap();
    assert_ne!(fresh.id, case);
    assert!(fresh.games.contains(&g3));
}
