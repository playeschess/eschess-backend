//! The move loop: given a game as it stands, who is acting, a move and the time, say what happens.
//!
//! This is the whole of "game truth" for a live game, and it is pure: no database, no network, no reading of the
//! clock. The caller (the game service) loads the game, passes in the time, and saves the answer. A move is judged
//! by the server alone: the client says which move it wants and nothing else (not the position, not the time left,
//! not whose turn it is).

use std::fmt;

use eschess_domain::chess::{ClaimableDraw, Color, DrawReason, FlagResult, Game, Move, Outcome};
use eschess_domain::clock::{Clock, ClockError, Timed};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GameResult {
    White,
    Black,
    Draw,
}

impl GameResult {
    pub fn as_str(self) -> &'static str {
        match self {
            GameResult::White => "white",
            GameResult::Black => "black",
            GameResult::Draw => "draw",
        }
    }

    pub fn parse(text: &str) -> Option<GameResult> {
        match text {
            "white" => Some(GameResult::White),
            "black" => Some(GameResult::Black),
            "draw" => Some(GameResult::Draw),
            _ => None,
        }
    }

    pub fn win_for(color: Color) -> GameResult {
        match color {
            Color::White => GameResult::White,
            Color::Black => GameResult::Black,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    Checkmate,
    Resignation,
    /// A player ran out of time and lost.
    Timeout,
    /// A player ran out of time but the opponent could never checkmate, so the game is drawn.
    TimeoutInsufficientMaterial,
    Stalemate,
    InsufficientMaterial,
    FivefoldRepetition,
    SeventyFiveMoveRule,
    /// Draws a player claimed.
    ThreefoldRepetition,
    FiftyMoveRule,
    /// Both players agreed to a draw.
    Agreement,
}

impl EndReason {
    /// Every reason, so a name can be read back without a second list to keep in step.
    pub const ALL: [EndReason; 11] = [
        EndReason::Checkmate,
        EndReason::Resignation,
        EndReason::Timeout,
        EndReason::TimeoutInsufficientMaterial,
        EndReason::Stalemate,
        EndReason::InsufficientMaterial,
        EndReason::FivefoldRepetition,
        EndReason::SeventyFiveMoveRule,
        EndReason::ThreefoldRepetition,
        EndReason::FiftyMoveRule,
        EndReason::Agreement,
    ];

    /// Reads a name written by [`EndReason::as_str`].
    pub fn parse(text: &str) -> Option<EndReason> {
        EndReason::ALL.into_iter().find(|r| r.as_str() == text)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            EndReason::Checkmate => "checkmate",
            EndReason::Resignation => "resignation",
            EndReason::Timeout => "timeout",
            EndReason::TimeoutInsufficientMaterial => "timeout_insufficient_material",
            EndReason::Stalemate => "stalemate",
            EndReason::InsufficientMaterial => "insufficient_material",
            EndReason::FivefoldRepetition => "fivefold_repetition",
            EndReason::SeventyFiveMoveRule => "seventy_five_move_rule",
            EndReason::ThreefoldRepetition => "threefold_repetition",
            EndReason::FiftyMoveRule => "fifty_move_rule",
            EndReason::Agreement => "agreement",
        }
    }
}

/// How a game ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ended {
    pub result: GameResult,
    pub reason: EndReason,
}

impl Ended {
    fn win(color: Color, reason: EndReason) -> Ended {
        Ended {
            result: GameResult::win_for(color),
            reason,
        }
    }

    fn draw(reason: EndReason) -> Ended {
        Ended {
            result: GameResult::Draw,
            reason,
        }
    }
}

fn ended_by_rules(outcome: Outcome) -> Ended {
    match outcome {
        Outcome::Checkmate { winner } => Ended::win(winner, EndReason::Checkmate),
        Outcome::Stalemate => Ended::draw(EndReason::Stalemate),
        Outcome::Draw(DrawReason::InsufficientMaterial) => {
            Ended::draw(EndReason::InsufficientMaterial)
        }
        Outcome::Draw(DrawReason::FivefoldRepetition) => Ended::draw(EndReason::FivefoldRepetition),
        Outcome::Draw(DrawReason::SeventyFiveMoveRule) => {
            Ended::draw(EndReason::SeventyFiveMoveRule)
        }
    }
}

/// Why a request was refused. A refusal changes nothing: it costs no time and does not touch the game.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The actor is not one of the two players.
    NotAPlayer,
    NotYourTurn,
    /// Not a move in UCI notation (`e2e4`, `e7e8q`).
    InvalidNotation,
    /// A well-formed move that the rules do not allow here.
    Illegal,
    GameOver,
    /// Nothing to claim.
    NothingToClaim,
    /// A draw offer of yours is already on the table, or you already offered one in this very position.
    DrawAlreadyOffered,
    /// There is no draw offer to answer.
    NoDrawOffer,
    /// You cannot accept your own draw offer.
    OwnDrawOffer,
    /// The stored game cannot be replayed: a bug or corrupted data, never the player's fault.
    CorruptHistory,
    /// The game is played on Lichess: our server runs no clock and takes no moves, resignations or draw offers for it.
    PlayedElsewhere,
    Clock(ClockError),
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::NotAPlayer => write!(f, "you are not a player in this game"),
            Refusal::NotYourTurn => write!(f, "it is not your turn"),
            Refusal::InvalidNotation => write!(
                f,
                "that is not a move in UCI notation, such as e2e4 or e7e8q"
            ),
            Refusal::Illegal => write!(f, "that move is not legal in this position"),
            Refusal::GameOver => write!(f, "the game is over"),
            Refusal::NothingToClaim => write!(f, "there is no draw to claim in this position"),
            Refusal::DrawAlreadyOffered => {
                write!(f, "you have already offered a draw in this position")
            }
            Refusal::NoDrawOffer => write!(f, "there is no draw offer to answer"),
            Refusal::OwnDrawOffer => write!(f, "you cannot accept your own draw offer"),
            Refusal::CorruptHistory => write!(f, "the stored game history is not valid"),
            Refusal::PlayedElsewhere => write!(f, "this game is played on Lichess"),
            Refusal::Clock(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Refusal {}

/// A game that is being played: who has which colour, every move so far, and the clock.
#[derive(Debug, Clone)]
pub struct ActiveGame {
    pub white: Uuid,
    pub black: Uuid,
    pub moves: Vec<Move>,
    pub clock: Clock,
}

impl ActiveGame {
    pub fn player(&self, color: Color) -> Uuid {
        match color {
            Color::White => self.white,
            Color::Black => self.black,
        }
    }

    pub fn color_of(&self, user: Uuid) -> Option<Color> {
        if user == self.white {
            Some(Color::White)
        } else if user == self.black {
            Some(Color::Black)
        } else {
            None
        }
    }

    /// The game as the rules see it, rebuilt by replaying the moves from the start. (The repetition rules need the
    /// whole history, and replaying keeps a single source of truth: the list of moves.)
    pub fn rules(&self) -> Result<Game, Refusal> {
        let mut game = Game::new();
        for m in &self.moves {
            game.apply(*m).map_err(|_| Refusal::CorruptHistory)?;
        }
        Ok(game)
    }
}

/// What a move did.
#[derive(Debug, Clone)]
pub struct MoveApplied {
    pub mv: Move,
    pub fen_after: String,
    pub clock: Clock,
    /// Time taken off the mover's clock, and the lag refunded to them.
    pub charged_ms: u64,
    pub credited_ms: u64,
    /// Set if this move ended the game (checkmate, stalemate, an automatic draw).
    pub ended: Option<Ended>,
}

#[derive(Debug, Clone)]
pub enum MoveOutcome {
    Played(Box<MoveApplied>),
    /// The mover's time had run out when the move arrived: it does not count, and they lose (or draw, if the opponent
    /// could never mate).
    Flagged(Ended),
}

/// Plays `uci` for `actor`, with the move arriving at `received_at_ms` and `measured_lag_ms` the lag the server
/// measured for them. The order of the checks matters: identity, then turn, then the move itself, and only a legal
/// move by the right player reaches the clock, so a mistake never costs anyone time.
pub fn play_move(
    game: &ActiveGame,
    actor: Uuid,
    uci: &str,
    received_at_ms: u64,
    measured_lag_ms: u64,
) -> Result<MoveOutcome, Refusal> {
    let mover = game.color_of(actor).ok_or(Refusal::NotAPlayer)?;
    let mut rules = game.rules()?;
    if rules.outcome().is_some() {
        return Err(Refusal::GameOver);
    }
    if rules.position().side_to_move() != mover {
        return Err(Refusal::NotYourTurn);
    }
    let mv = Move::from_uci(uci).ok_or(Refusal::InvalidNotation)?;
    if !rules.position().is_legal(mv) {
        return Err(Refusal::Illegal);
    }

    let timing = match game
        .clock
        .record_move(received_at_ms, measured_lag_ms)
        .map_err(Refusal::Clock)?
    {
        Timed::Flagged { color } => return Ok(MoveOutcome::Flagged(flag_ended(&rules, color))),
        Timed::Made(timing) => timing,
    };
    rules.apply(mv).map_err(|_| Refusal::Illegal)?;
    Ok(MoveOutcome::Played(Box::new(MoveApplied {
        mv,
        fen_after: rules.position().to_fen(),
        clock: timing.clock,
        charged_ms: timing.charged_ms,
        credited_ms: timing.credited_ms,
        ended: rules.outcome().map(ended_by_rules),
    })))
}

fn flag_ended(rules: &Game, flagged: Color) -> Ended {
    match rules.flag_result(flagged) {
        FlagResult::Loses(loser) => Ended::win(loser.opposite(), EndReason::Timeout),
        FlagResult::Draw => Ended::draw(EndReason::TimeoutInsufficientMaterial),
    }
}

/// For a timer: has the player to move run out of time, with no refund left that could still save a move in flight?
pub fn check_flag(game: &ActiveGame, now_ms: u64) -> Result<Option<Ended>, Refusal> {
    let Some(flagged) = game.clock.flagged_at(now_ms) else {
        return Ok(None);
    };
    let rules = game.rules()?;
    if rules.outcome().is_some() {
        return Ok(None);
    }
    Ok(Some(flag_ended(&rules, flagged)))
}

/// A player gives up. They may do so at any time, on either side's turn.
pub fn resign(game: &ActiveGame, actor: Uuid) -> Result<Ended, Refusal> {
    let color = game.color_of(actor).ok_or(Refusal::NotAPlayer)?;
    if game.rules()?.outcome().is_some() {
        return Err(Refusal::GameOver);
    }
    Ok(Ended::win(color.opposite(), EndReason::Resignation))
}

/// The player to move claims a draw by threefold repetition or the fifty-move rule, when one is available.
pub fn claim_draw(game: &ActiveGame, actor: Uuid) -> Result<Ended, Refusal> {
    let color = game.color_of(actor).ok_or(Refusal::NotAPlayer)?;
    let rules = game.rules()?;
    if rules.outcome().is_some() {
        return Err(Refusal::GameOver);
    }
    if rules.position().side_to_move() != color {
        return Err(Refusal::NotYourTurn);
    }
    match rules.claimable_draw() {
        Some(ClaimableDraw::ThreefoldRepetition) => Ok(Ended::draw(EndReason::ThreefoldRepetition)),
        Some(ClaimableDraw::FiftyMoveRule) => Ok(Ended::draw(EndReason::FiftyMoveRule)),
        None => Err(Refusal::NothingToClaim),
    }
}

/// The draw offers in a game: who has one waiting, and when each side last made one.
///
/// An offer stays until it is accepted, declined or withdrawn, or until the player it was made to plays a move (which
/// counts as declining it). The player who made it may keep playing. To stop a player pestering their opponent, each
/// side may make only one offer per position: after an offer is turned down, a new one needs a move to have been played.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DrawOffers {
    pub pending: Option<Color>,
    /// The number of moves played when White and Black (in that order) last made an offer.
    pub last_offer_ply: [Option<u32>; 2],
}

fn slot(color: Color) -> usize {
    match color {
        Color::White => 0,
        Color::Black => 1,
    }
}

/// What answering a draw offer did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Answered {
    pub offers: DrawOffers,
    /// Set when the offer was accepted: the game is a draw.
    pub ended: Option<Ended>,
}

/// A player offers a draw. Allowed at any time in a game that is still going, whoever's turn it is.
pub fn offer_draw(
    game: &ActiveGame,
    actor: Uuid,
    offers: &DrawOffers,
) -> Result<Answered, Refusal> {
    let color = game.color_of(actor).ok_or(Refusal::NotAPlayer)?;
    if game.rules()?.outcome().is_some() {
        return Err(Refusal::GameOver);
    }
    let ply = game.moves.len() as u32;
    if offers.pending == Some(color) || offers.last_offer_ply[slot(color)] == Some(ply) {
        return Err(Refusal::DrawAlreadyOffered);
    }
    let mut next = *offers;
    next.last_offer_ply[slot(color)] = Some(ply);
    // The opponent already offered, and now this player does too: both want a draw, so it is agreed.
    if offers.pending == Some(color.opposite()) {
        next.pending = None;
        return Ok(Answered {
            offers: next,
            ended: Some(Ended::draw(EndReason::Agreement)),
        });
    }
    next.pending = Some(color);
    Ok(Answered {
        offers: next,
        ended: None,
    })
}

/// The player an offer was made to accepts it (the game is drawn) or declines it. The player who made it may also
/// decline, which withdraws it.
pub fn answer_draw_offer(
    game: &ActiveGame,
    actor: Uuid,
    offers: &DrawOffers,
    accept: bool,
) -> Result<Answered, Refusal> {
    let color = game.color_of(actor).ok_or(Refusal::NotAPlayer)?;
    if game.rules()?.outcome().is_some() {
        return Err(Refusal::GameOver);
    }
    let Some(offered_by) = offers.pending else {
        return Err(Refusal::NoDrawOffer);
    };
    if accept {
        if offered_by == color {
            return Err(Refusal::OwnDrawOffer);
        }
        return Ok(Answered {
            offers: DrawOffers {
                pending: None,
                ..*offers
            },
            ended: Some(Ended::draw(EndReason::Agreement)),
        });
    }
    Ok(Answered {
        offers: DrawOffers {
            pending: None,
            ..*offers
        },
        ended: None,
    })
}

/// A move has been played: an offer made to the player who just moved is thereby declined.
pub fn offers_after_move(offers: &DrawOffers, mover: Color) -> DrawOffers {
    match offers.pending {
        Some(offered_by) if offered_by != mover => DrawOffers {
            pending: None,
            ..*offers
        },
        _ => *offers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eschess_domain::clock::{LagPolicy, TimeControl};

    fn ids() -> (Uuid, Uuid, Uuid) {
        (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3))
    }

    fn new_game(base_ms: u64, inc_ms: u64) -> ActiveGame {
        let (white, black, _) = ids();
        ActiveGame {
            white,
            black,
            moves: vec![],
            clock: Clock::start(
                TimeControl::new(base_ms, inc_ms).unwrap(),
                LagPolicy::default(),
                0,
            ),
        }
    }

    /// Plays a move at `at` ms and returns the updated game (and what ended it, if anything).
    fn step(game: &ActiveGame, uci: &str, at: u64) -> (ActiveGame, Option<Ended>) {
        let actor = game.player(game.rules().unwrap().position().side_to_move());
        match play_move(game, actor, uci, at, 0).unwrap_or_else(|e| panic!("{uci}: {e}")) {
            MoveOutcome::Played(m) => {
                let mut next = game.clone();
                next.moves.push(m.mv);
                next.clock = m.clock;
                (next, m.ended)
            }
            MoveOutcome::Flagged(e) => panic!("{uci} flagged: {e:?}"),
        }
    }

    #[test]
    fn a_legal_move_by_the_right_player_updates_everything() {
        let g = new_game(300_000, 2_000);
        let (white, _, _) = ids();
        let MoveOutcome::Played(m) = play_move(&g, white, "e2e4", 5_000, 0).unwrap() else {
            panic!("flagged")
        };
        assert_eq!(m.mv.to_uci(), "e2e4");
        assert_eq!(
            m.fen_after,
            "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq e3 0 1"
        );
        assert_eq!((m.charged_ms, m.credited_ms), (5_000, 0));
        assert_eq!(m.clock.banked_ms(Color::White), 300_000 - 5_000 + 2_000);
        assert_eq!(m.clock.turn(), Color::Black);
        assert!(m.ended.is_none());
    }

    #[test]
    fn mistakes_are_refused_and_never_cost_time() {
        let g = new_game(300_000, 0);
        let (white, black, stranger) = ids();
        assert_eq!(
            play_move(&g, stranger, "e2e4", 1_000, 0).unwrap_err(),
            Refusal::NotAPlayer
        );
        assert_eq!(
            play_move(&g, black, "e7e5", 1_000, 0).unwrap_err(),
            Refusal::NotYourTurn
        );
        for junk in ["", "e2", "hello", "e2e9", "E2E4"] {
            assert_eq!(
                play_move(&g, white, junk, 1_000, 0).unwrap_err(),
                Refusal::InvalidNotation,
                "{junk:?}"
            );
        }
        for illegal in ["e2e5", "e7e5", "a1a3", "e1g1", "d1d4"] {
            assert_eq!(
                play_move(&g, white, illegal, 1_000, 0).unwrap_err(),
                Refusal::Illegal,
                "{illegal}"
            );
        }
        // The game is untouched, so the same player can still make a proper move and is charged only for the real one.
        let MoveOutcome::Played(m) = play_move(&g, white, "e2e4", 9_000, 0).unwrap() else {
            panic!()
        };
        assert_eq!(m.charged_ms, 9_000);
    }

    #[test]
    fn the_players_alternate_and_the_clock_follows() {
        let (mut g, _) = step(&new_game(60_000, 1_000), "e2e4", 3_000);
        (g, _) = step(&g, "e7e5", 7_000);
        assert_eq!(g.clock.banked_ms(Color::Black), 60_000 - 4_000 + 1_000);
        assert_eq!(g.clock.turn(), Color::White);
        let (white, black, _) = ids();
        assert_eq!(
            play_move(&g, black, "b8c6", 8_000, 0).unwrap_err(),
            Refusal::NotYourTurn
        );
        assert!(matches!(
            play_move(&g, white, "g1f3", 8_000, 0).unwrap(),
            MoveOutcome::Played(_)
        ));
    }

    #[test]
    fn checkmate_ends_the_game_and_nothing_more_can_be_played() {
        let mut g = new_game(600_000, 0);
        let mut at = 0;
        let mut last = None;
        for m in ["f2f3", "e7e5", "g2g4", "d8h4"] {
            at += 1_000;
            (g, last) = step(&g, m, at);
        }
        assert_eq!(
            last,
            Some(Ended {
                result: GameResult::Black,
                reason: EndReason::Checkmate
            })
        );
        let (white, black, _) = ids();
        assert_eq!(
            play_move(&g, white, "a2a3", at + 1, 0).unwrap_err(),
            Refusal::GameOver
        );
        assert_eq!(resign(&g, black).unwrap_err(), Refusal::GameOver);
    }

    #[test]
    fn automatic_draws_end_the_game_with_the_right_reason() {
        // Stalemate: the move that makes it ends the game.
        let (white, black, _) = ids();
        let mut g = ActiveGame {
            white,
            black,
            moves: vec![],
            clock: Clock::start(
                TimeControl::new(600_000, 0).unwrap(),
                LagPolicy::default(),
                0,
            ),
        };
        // Reach a known stalemate by replaying a short game: 1.e3 a5 2.Qh5 Ra6 3.Qxa5 h5 4.h4 Rah6 5.Qxc7 f6 6.Qxd7+ Kf7 7.Qxb7 Qd3 8.Qxb8 Qh7 9.Qxc8 Kg6 10.Qe6
        let mut at = 0;
        let mut last = None;
        for m in "e2e3 a7a5 d1h5 a8a6 h5a5 h7h5 h2h4 a6h6 a5c7 f7f6 c7d7 e8f7 d7b7 d8d3 b7b8 d3h7 b8c8 f7g6 c8e6".split(' ') {
            at += 1_000;
            (g, last) = step(&g, m, at);
        }
        assert_eq!(
            last,
            Some(Ended {
                result: GameResult::Draw,
                reason: EndReason::Stalemate
            })
        );
    }

    #[test]
    fn running_out_of_time_loses_and_the_late_move_does_not_count() {
        let g = new_game(10_000, 0);
        let (white, _, _) = ids();
        let MoveOutcome::Flagged(ended) = play_move(&g, white, "e2e4", 10_000, 0).unwrap() else {
            panic!("should have flagged")
        };
        assert_eq!(
            ended,
            Ended {
                result: GameResult::Black,
                reason: EndReason::Timeout
            }
        );
        // One millisecond earlier it is fine.
        assert!(matches!(
            play_move(&g, white, "e2e4", 9_999, 0).unwrap(),
            MoveOutcome::Played(_)
        ));
    }

    #[test]
    fn lag_credit_can_rescue_a_move_that_arrived_just_late() {
        let g = new_game(10_000, 0);
        let (white, _, _) = ids();
        // 10.4 s of clock time, but the server measured 600 ms of lag: charged 9.8 s, so the move stands.
        let MoveOutcome::Played(m) = play_move(&g, white, "e2e4", 10_400, 600).unwrap() else {
            panic!("flagged")
        };
        assert_eq!((m.charged_ms, m.credited_ms), (9_800, 600));
    }

    #[test]
    fn a_timer_flags_the_player_to_move_only_after_the_lag_grace() {
        let g = new_game(10_000, 0);
        assert_eq!(check_flag(&g, 10_999).unwrap(), None);
        assert_eq!(
            check_flag(&g, 11_000).unwrap(),
            Some(Ended {
                result: GameResult::Black,
                reason: EndReason::Timeout
            })
        );
        // After White moves, it is Black's clock that runs.
        let (g, _) = step(&g, "e2e4", 2_000);
        assert_eq!(check_flag(&g, 11_999).unwrap(), None);
        assert_eq!(
            check_flag(&g, 2_000 + 10_000 + 1_000).unwrap(),
            Some(Ended {
                result: GameResult::White,
                reason: EndReason::Timeout
            })
        );
    }

    /// A real 127-move game (found with the independent python-chess library, which also confirms the verdicts) that
    /// ends with White king and knight against Black king and queen, Black to move. White can never checkmate there;
    /// Black can.
    const KNIGHT_VS_QUEEN: &str = "g2g4 e7e5 d2d4 e5d4 d1d4 c7c5 d4g7 d8c7 g7h7 e8e7 h7h8 c7g3 f2g3 b7b5 a2a3 b8c6 c1d2 c6b4 h8g8 a8b8 h2h4 a7a5 e1f2 b4c2 g8f7 e7f7 d2b4 c2b4 f2g2 c8b7 g1f3 b8c8 h1h2 b7a6 a3b4 c5b4 g4g5 f8d6 g2h1 c8h8 a1a5 f7e6 a5b5 h8f8 h2f2 f8g8 b5c5 a6e2 c5c6 g8g5 c6c3 d6g3 f2h2 e2f1 f3g5 e6e5 h2h3 d7d5 c3g3 e5d4 h1h2 f1b5 b1c3 b4c3 g3c3 b5e8 h3g3 e8f7 h2g1 f7e8 c3b3 d4c5 b3f3 e8a4 b2b3 a4b3 g1g2 c5c6 g3g4 c6d7 f3f8 b3c4 g5f3 c4b5 f8f6 d7c7 g4c4 d5c4 f3h2 b5c6 f6c6 c7d8 c6a6 d8d7 h2f3 c4c3 g2h1 d7e8 a6a8 e8e7 a8h8 e7d7 h1g1 d7c6 h8b8 c6d6 g1h2 d6d7 b8b4 d7d6 b4b2 c3b2 f3g1 b2b1q g1h3 b1b6 h2g3 b6b4 g3h2 b4a5 h2g2 d6e6 g2h2 a5d8 h2g2 d8h4 h3g1";

    fn knight_vs_queen() -> ActiveGame {
        let (white, black, _) = ids();
        let mut g = ActiveGame {
            white,
            black,
            moves: vec![],
            clock: Clock::start(
                TimeControl::new(600_000, 0).unwrap(),
                LagPolicy::default(),
                0,
            ),
        };
        for (i, m) in KNIGHT_VS_QUEEN.split(' ').enumerate() {
            g = step(&g, m, (i as u64 + 1) * 10).0;
        }
        assert_eq!(
            g.rules().unwrap().position().to_fen(),
            "8/8/4k3/8/7q/8/6K1/6N1 b - - 1 64"
        );
        g
    }

    #[test]
    fn running_out_of_time_is_a_draw_when_the_opponent_could_never_mate_and_a_loss_when_they_could()
    {
        // Black (with the queen) runs out of time, but White's lone knight could never checkmate: a draw.
        let g = knight_vs_queen();
        let at = g.clock.flag_deadline_ms();
        assert_eq!(
            check_flag(&g, at).unwrap(),
            Some(Ended {
                result: GameResult::Draw,
                reason: EndReason::TimeoutInsufficientMaterial
            })
        );

        // One Black move later it is White's clock that runs, and now Black (with the queen) could mate: White loses.
        let (g, _) = step(&g, "e6d5", g.clock.state().turn_started_at_ms + 1_000);
        let at = g.clock.flag_deadline_ms();
        assert_eq!(
            check_flag(&g, at).unwrap(),
            Some(Ended {
                result: GameResult::Black,
                reason: EndReason::Timeout
            })
        );
    }

    #[test]
    fn resigning_hands_the_win_to_the_opponent_on_either_turn() {
        let g = new_game(60_000, 0);
        let (white, black, stranger) = ids();
        assert_eq!(
            resign(&g, white).unwrap(),
            Ended {
                result: GameResult::Black,
                reason: EndReason::Resignation
            }
        );
        assert_eq!(
            resign(&g, black).unwrap(),
            Ended {
                result: GameResult::White,
                reason: EndReason::Resignation
            },
            "even when it is not their turn"
        );
        assert_eq!(resign(&g, stranger).unwrap_err(), Refusal::NotAPlayer);
    }

    #[test]
    fn a_threefold_repetition_can_be_claimed_by_the_player_to_move() {
        let (white, black, _) = ids();
        let mut g = new_game(600_000, 0);
        assert_eq!(claim_draw(&g, white).unwrap_err(), Refusal::NothingToClaim);
        let mut at = 0;
        for m in "g1f3 g8f6 f3g1 f6g8 g1f3 g8f6 f3g1 f6g8".split(' ') {
            at += 1_000;
            (g, _) = step(&g, m, at);
        }
        assert_eq!(
            claim_draw(&g, white).unwrap(),
            Ended {
                result: GameResult::Draw,
                reason: EndReason::ThreefoldRepetition
            }
        );
        assert_eq!(
            claim_draw(&g, black).unwrap_err(),
            Refusal::NotYourTurn,
            "only the player to move may claim"
        );
    }

    #[test]
    fn a_broken_history_is_reported_not_papered_over() {
        let (white, _, _) = ids();
        let mut g = new_game(60_000, 0);
        g.moves.push(Move::from_uci("e2e5").unwrap());
        assert_eq!(
            play_move(&g, white, "e2e4", 1_000, 0).unwrap_err(),
            Refusal::CorruptHistory
        );
    }

    #[test]
    fn every_end_reason_reads_back_from_its_name() {
        for r in EndReason::ALL {
            assert_eq!(EndReason::parse(r.as_str()), Some(r));
        }
        assert_eq!(EndReason::parse("abandoned"), None);
    }

    #[test]
    fn end_reasons_have_stable_names() {
        let names: Vec<&str> = [
            EndReason::Checkmate,
            EndReason::Resignation,
            EndReason::Timeout,
            EndReason::TimeoutInsufficientMaterial,
            EndReason::Stalemate,
            EndReason::InsufficientMaterial,
            EndReason::FivefoldRepetition,
            EndReason::SeventyFiveMoveRule,
            EndReason::ThreefoldRepetition,
            EndReason::FiftyMoveRule,
            EndReason::Agreement,
        ]
        .iter()
        .map(|r| r.as_str())
        .collect();
        assert_eq!(
            names,
            [
                "checkmate",
                "resignation",
                "timeout",
                "timeout_insufficient_material",
                "stalemate",
                "insufficient_material",
                "fivefold_repetition",
                "seventy_five_move_rule",
                "threefold_repetition",
                "fifty_move_rule",
                "agreement"
            ]
        );
        assert_eq!(GameResult::parse("draw"), Some(GameResult::Draw));
        assert_eq!(GameResult::parse("nobody"), None);
    }

    fn offers_game_after(moves: &[&str]) -> ActiveGame {
        let mut g = new_game(300_000, 0);
        for (i, m) in moves.iter().enumerate() {
            g = step(&g, m, 1_000 * (i as u64 + 1)).0;
        }
        g
    }

    #[test]
    fn a_draw_offer_can_be_accepted_declined_or_withdrawn() {
        let (white, black, stranger) = ids();
        let g = offers_game_after(&["e2e4"]);
        let none = DrawOffers::default();

        // White offers (even though it is Black's turn); only Black may accept, and it is a draw by agreement.
        let offered = offer_draw(&g, white, &none).unwrap();
        assert_eq!(offered.offers.pending, Some(Color::White));
        assert!(offered.ended.is_none());
        assert_eq!(
            answer_draw_offer(&g, white, &offered.offers, true),
            Err(Refusal::OwnDrawOffer)
        );
        let accepted = answer_draw_offer(&g, black, &offered.offers, true).unwrap();
        assert_eq!(accepted.ended, Some(Ended::draw(EndReason::Agreement)));
        assert_eq!(accepted.offers.pending, None);

        // Declining clears it; so does the offerer withdrawing it.
        for who in [black, white] {
            let declined = answer_draw_offer(&g, who, &offered.offers, false).unwrap();
            assert_eq!((declined.offers.pending, declined.ended), (None, None));
        }
        // Nothing to answer, and strangers have no part in it.
        assert_eq!(
            answer_draw_offer(&g, black, &none, true),
            Err(Refusal::NoDrawOffer)
        );
        assert_eq!(offer_draw(&g, stranger, &none), Err(Refusal::NotAPlayer));
        assert_eq!(
            answer_draw_offer(&g, stranger, &offered.offers, true),
            Err(Refusal::NotAPlayer)
        );
    }

    #[test]
    fn nobody_can_pester_their_opponent_with_the_same_offer() {
        let (white, black, _) = ids();
        let g = offers_game_after(&["e2e4"]);
        let first = offer_draw(&g, white, &DrawOffers::default())
            .unwrap()
            .offers;
        // Already pending.
        assert_eq!(
            offer_draw(&g, white, &first),
            Err(Refusal::DrawAlreadyOffered)
        );
        // Declined, but still the same position: no second offer until a move has been played.
        let declined = answer_draw_offer(&g, black, &first, false).unwrap().offers;
        assert_eq!(
            offer_draw(&g, white, &declined),
            Err(Refusal::DrawAlreadyOffered)
        );
        let later = offers_game_after(&["e2e4", "e7e5"]);
        assert!(offer_draw(&later, white, &declined).is_ok());
        // The other side is not held back by White's offers.
        assert!(offer_draw(&g, black, &declined).is_ok());
    }

    #[test]
    fn offers_made_to_each_other_at_once_make_a_draw() {
        let (white, black, _) = ids();
        let g = offers_game_after(&["e2e4"]);
        let white_offers = offer_draw(&g, white, &DrawOffers::default())
            .unwrap()
            .offers;
        let both = offer_draw(&g, black, &white_offers).unwrap();
        assert_eq!(both.ended, Some(Ended::draw(EndReason::Agreement)));
    }

    #[test]
    fn the_recipient_moving_declines_the_offer_but_the_offerer_moving_does_not() {
        let offers = DrawOffers {
            pending: Some(Color::White),
            last_offer_ply: [Some(1), None],
        };
        assert_eq!(
            offers_after_move(&offers, Color::White),
            offers,
            "the offerer may keep playing"
        );
        let after = offers_after_move(&offers, Color::Black);
        assert_eq!(after.pending, None);
        assert_eq!(
            after.last_offer_ply,
            [Some(1), None],
            "what was offered when is remembered"
        );
        assert_eq!(
            offers_after_move(&DrawOffers::default(), Color::Black),
            DrawOffers::default()
        );
    }

    #[test]
    fn no_offers_once_the_game_is_over() {
        let (white, _, _) = ids();
        // Fool's mate.
        let g = offers_game_after(&["f2f3", "e7e5", "g2g4", "d8h4"]);
        assert_eq!(
            offer_draw(&g, white, &DrawOffers::default()),
            Err(Refusal::GameOver)
        );
    }
}
