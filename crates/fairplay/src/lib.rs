//! The public side of fair play (ADR 0002): what a scorer is given, what it returns, and the rules a score must follow.
//!
//! A scorer looks at one finished game ([`GameRecord`]) and says, per player, how much the game deserves a person's look
//! ([`RiskScore`]). **A score is not a verdict**: it orders a review queue, and only a reviewer decides anything.
//!
//! The real scorer lives in the private `eschess-fairplay` repository, because published thresholds are an evasion guide.
//! It depends on this crate, not the other way round, so this repository builds and runs without it. Here there is only
//! [`NoopScorer`], which scores everything 0.
//!
//! The backend does not trust what a scorer returns: [`check`] refuses scores for someone who did not play, two scores
//! for one player, numbers out of range, and evidence too large to store.

use eschess_domain::chess::Color;
use eschess_game::{EndReason, GameResult};
use serde_json::Value;
use uuid::Uuid;

/// How a move reached the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Over the WebSocket, where the server measures the player's lag.
    WebSocket,
    /// Over plain HTTP, where it cannot.
    Http,
    /// Played before the transport was recorded.
    Unknown,
}

impl Transport {
    /// The stored name: `ws`, `http`, or `None` for [`Transport::Unknown`].
    pub fn as_db(self) -> Option<&'static str> {
        match self {
            Transport::WebSocket => Some("ws"),
            Transport::Http => Some("http"),
            Transport::Unknown => None,
        }
    }

    pub fn from_db(value: Option<&str>) -> Transport {
        match value {
            Some("ws") => Transport::WebSocket,
            Some("http") => Transport::Http,
            _ => Transport::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeControl {
    pub base_ms: u64,
    pub increment_ms: u64,
}

/// One move, with everything the server knew when it accepted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveRecord {
    /// 1 for White's first move.
    pub ply: u32,
    pub uci: String,
    pub fen_before: String,
    pub fen_after: String,
    pub mover: Color,
    /// The time charged to the mover's clock for this move (after any lag refund).
    pub think_ms: u64,
    /// How much of the measured lag was refunded.
    pub lag_credited_ms: u64,
    /// The round-trip lag the server measured for the move; `None` over HTTP or for moves recorded before it was kept.
    pub lag_measured_ms: Option<u64>,
    pub transport: Transport,
    /// The mover's clock just after the move.
    pub remaining_ms_after: u64,
    /// Server time, in Unix milliseconds.
    pub played_at_ms: i64,
}

/// Everything the server knows about one finished game.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameRecord {
    pub game_id: Uuid,
    pub white_id: Uuid,
    pub black_id: Uuid,
    pub time_control: TimeControl,
    pub rated: bool,
    pub result: GameResult,
    /// `None` if the stored reason is one this build does not know (a newer server wrote it).
    pub end_reason: Option<EndReason>,
    /// In order: `moves[0]` is ply 1.
    pub moves: Vec<MoveRecord>,
}

impl GameRecord {
    pub fn player(&self, color: Color) -> Uuid {
        match color {
            Color::White => self.white_id,
            Color::Black => self.black_id,
        }
    }

    pub fn color_of(&self, player: Uuid) -> Option<Color> {
        if player == self.white_id {
            Some(Color::White)
        } else if player == self.black_id {
            Some(Color::Black)
        } else {
            None
        }
    }

    /// One side's moves, in order.
    pub fn moves_by(&self, color: Color) -> impl Iterator<Item = &MoveRecord> {
        self.moves.iter().filter(move |m| m.mover == color)
    }
}

/// The highest score: "a person should certainly look at this".
pub const MAX_SCORE: u8 = 100;
/// Most bytes of evidence (as JSON) one score may carry.
pub const MAX_EVIDENCE_BYTES: usize = 64 * 1024;
/// Most characters in a summary.
pub const MAX_SUMMARY_CHARS: usize = 2_000;

/// How much one game deserves a person's look, for one player.
#[derive(Debug, Clone, PartialEq)]
pub struct RiskScore {
    pub player_id: Uuid,
    /// 0 to [`MAX_SCORE`].
    pub score: u8,
    /// The scorer's name and version, so scores from different versions are never mixed up.
    pub scorer: String,
    /// For the reviewer: why, in words.
    pub summary: String,
    /// For the reviewer: whatever the scorer wants to show. The backend stores it and never reads it.
    pub evidence: Value,
}

/// Scores finished games. Pure and synchronous: a scorer that needs an engine runs it in its own worker, around this
/// call. The backend never runs an engine.
pub trait Scorer: Send + Sync {
    /// The name and version, such as `baseline-3`: 1 to 64 lowercase letters, digits or `. _ -`.
    fn name(&self) -> &str;

    /// One score per player who should be looked at (possibly none).
    fn score(&self, game: &GameRecord) -> Vec<RiskScore>;
}

/// Scores every player 0. What the public backend runs, so the pipeline works end to end without the private scorer.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopScorer;

impl Scorer for NoopScorer {
    fn name(&self) -> &str {
        "noop-1"
    }

    fn score(&self, game: &GameRecord) -> Vec<RiskScore> {
        [game.white_id, game.black_id]
            .into_iter()
            .map(|player_id| RiskScore {
                player_id,
                score: 0,
                scorer: self.name().to_string(),
                summary: "no scorer is configured".into(),
                evidence: Value::Null,
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScoreError {
    /// The scorer's name is empty, too long, or has characters other than lowercase letters, digits and `. _ -`.
    InvalidScorerName,
    /// A score names a different scorer than the one that produced it.
    WrongScorer {
        expected: String,
        got: String,
    },
    /// A score for someone who did not play this game.
    NotAPlayer(Uuid),
    /// Two scores for one player.
    Duplicate(Uuid),
    OutOfRange(u8),
    SummaryTooLong,
    EvidenceTooLarge,
}

impl std::fmt::Display for ScoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScoreError::InvalidScorerName => write!(
                f,
                "a scorer name is 1 to 64 lowercase letters, digits or . _ -"
            ),
            ScoreError::WrongScorer { expected, got } => {
                write!(
                    f,
                    "a score from {got:?} was returned by scorer {expected:?}"
                )
            }
            ScoreError::NotAPlayer(p) => write!(f, "{p} did not play this game"),
            ScoreError::Duplicate(p) => write!(f, "two scores for {p}"),
            ScoreError::OutOfRange(s) => write!(f, "score {s} is above {MAX_SCORE}"),
            ScoreError::SummaryTooLong => {
                write!(f, "a summary is at most {MAX_SUMMARY_CHARS} characters")
            }
            ScoreError::EvidenceTooLarge => {
                write!(f, "evidence is at most {MAX_EVIDENCE_BYTES} bytes of JSON")
            }
        }
    }
}

impl std::error::Error for ScoreError {}

fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}

/// Checks what `scorer` returned for `game` before anything is stored. All or nothing: one bad score refuses the lot,
/// so a buggy scorer is noticed rather than half-recorded.
pub fn check(scorer: &str, game: &GameRecord, scores: &[RiskScore]) -> Result<(), ScoreError> {
    if !valid_name(scorer) {
        return Err(ScoreError::InvalidScorerName);
    }
    let mut seen = Vec::with_capacity(2);
    for s in scores {
        if s.scorer != scorer {
            return Err(ScoreError::WrongScorer {
                expected: scorer.to_string(),
                got: s.scorer.clone(),
            });
        }
        if game.color_of(s.player_id).is_none() {
            return Err(ScoreError::NotAPlayer(s.player_id));
        }
        if seen.contains(&s.player_id) {
            return Err(ScoreError::Duplicate(s.player_id));
        }
        seen.push(s.player_id);
        if s.score > MAX_SCORE {
            return Err(ScoreError::OutOfRange(s.score));
        }
        if s.summary.chars().count() > MAX_SUMMARY_CHARS {
            return Err(ScoreError::SummaryTooLong);
        }
        if s.evidence.to_string().len() > MAX_EVIDENCE_BYTES {
            return Err(ScoreError::EvidenceTooLarge);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game() -> GameRecord {
        let mv = |ply: u32, uci: &str, mover: Color| MoveRecord {
            ply,
            uci: uci.into(),
            fen_before: String::new(),
            fen_after: String::new(),
            mover,
            think_ms: 1_000,
            lag_credited_ms: 0,
            lag_measured_ms: Some(40),
            transport: Transport::WebSocket,
            remaining_ms_after: 60_000,
            played_at_ms: 0,
        };
        GameRecord {
            game_id: Uuid::now_v7(),
            white_id: Uuid::now_v7(),
            black_id: Uuid::now_v7(),
            time_control: TimeControl {
                base_ms: 60_000,
                increment_ms: 0,
            },
            rated: true,
            result: GameResult::White,
            end_reason: Some(EndReason::Resignation),
            moves: vec![
                mv(1, "e2e4", Color::White),
                mv(2, "e7e5", Color::Black),
                mv(3, "g1f3", Color::White),
            ],
        }
    }

    fn score(player: Uuid, value: u8) -> RiskScore {
        RiskScore {
            player_id: player,
            score: value,
            scorer: "test-1".into(),
            summary: "s".into(),
            evidence: Value::Null,
        }
    }

    #[test]
    fn the_noop_scorer_scores_both_players_zero_and_passes_its_own_check() {
        let g = game();
        let scores = NoopScorer.score(&g);
        assert_eq!(scores.len(), 2);
        assert!(scores.iter().all(|s| s.score == 0));
        assert_eq!(check(NoopScorer.name(), &g, &scores), Ok(()));
    }

    #[test]
    fn a_game_record_answers_who_played_what() {
        let g = game();
        assert_eq!(g.color_of(g.black_id), Some(Color::Black));
        assert_eq!(g.color_of(Uuid::now_v7()), None);
        assert_eq!(g.player(Color::White), g.white_id);
        let white: Vec<&str> = g.moves_by(Color::White).map(|m| m.uci.as_str()).collect();
        assert_eq!(white, ["e2e4", "g1f3"]);
    }

    #[test]
    fn scores_that_break_the_rules_are_refused() {
        let g = game();
        let stranger = Uuid::now_v7();
        assert_eq!(check("test-1", &g, &[]), Ok(()), "no scores is fine");
        assert_eq!(
            check("test-1", &g, &[score(stranger, 1)]),
            Err(ScoreError::NotAPlayer(stranger))
        );
        assert_eq!(
            check("test-1", &g, &[score(g.white_id, 1), score(g.white_id, 2)]),
            Err(ScoreError::Duplicate(g.white_id))
        );
        assert_eq!(
            check("test-1", &g, &[score(g.white_id, 101)]),
            Err(ScoreError::OutOfRange(101))
        );
        assert_eq!(check("test-1", &g, &[score(g.white_id, 100)]), Ok(()));
        assert!(matches!(
            check("other-2", &g, &[score(g.white_id, 1)]),
            Err(ScoreError::WrongScorer { .. })
        ));
        for bad in ["", "Baseline-3", "baseline 3", &"x".repeat(65)] {
            assert_eq!(
                check(bad, &g, &[]),
                Err(ScoreError::InvalidScorerName),
                "{bad:?}"
            );
        }
        let mut long = score(g.white_id, 1);
        long.summary = "é".repeat(MAX_SUMMARY_CHARS + 1);
        assert_eq!(
            check("test-1", &g, &[long]),
            Err(ScoreError::SummaryTooLong)
        );
        let mut big = score(g.white_id, 1);
        big.evidence = Value::String("x".repeat(MAX_EVIDENCE_BYTES));
        assert_eq!(
            check("test-1", &g, &[big]),
            Err(ScoreError::EvidenceTooLarge)
        );
    }

    #[test]
    fn transports_round_trip_and_old_moves_are_unknown() {
        for t in [Transport::WebSocket, Transport::Http, Transport::Unknown] {
            assert_eq!(Transport::from_db(t.as_db()), t);
        }
        assert_eq!(Transport::from_db(None), Transport::Unknown);
    }
}
