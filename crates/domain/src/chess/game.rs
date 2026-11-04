use std::fmt;

use super::position::Position;
use super::types::{Color, Move};

/// How a game ended by the rules of chess alone. Resignation, a flag falling and agreed draws are not here:
/// they come from the players and the clock, which are outside the rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Checkmate { winner: Color },
    Stalemate,
    Draw(DrawReason),
}

/// Draws that happen automatically, with no player asking for them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawReason {
    /// No sequence of legal moves can end in checkmate.
    InsufficientMaterial,
    /// The same position has occurred five times.
    FivefoldRepetition,
    /// Seventy-five moves by each side without a capture or pawn move.
    SeventyFiveMoveRule,
}

/// Draws a player may claim; the game goes on until one does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimableDraw {
    ThreefoldRepetition,
    FiftyMoveRule,
}

/// What happens when a player's time runs out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagResult {
    /// The player whose time ran out loses.
    Loses(Color),
    /// The opponent could not checkmate by any series of legal moves, so the game is drawn.
    Draw,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveError {
    /// The game is already over.
    GameOver,
    /// Not valid UCI notation (`e2e4`, `e7e8q`).
    InvalidNotation,
    /// Valid notation, but not a legal move here.
    Illegal,
}

impl fmt::Display for MoveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MoveError::GameOver => write!(f, "the game is over"),
            MoveError::InvalidNotation => write!(
                f,
                "that is not a move in UCI notation, such as e2e4 or e7e8q"
            ),
            MoveError::Illegal => write!(f, "that move is not legal in this position"),
        }
    }
}

impl std::error::Error for MoveError {}

/// A game in progress: the current position, the moves played, and enough memory of earlier positions to apply
/// the repetition rules.
#[derive(Debug, Clone)]
pub struct Game {
    position: Position,
    moves: Vec<Move>,
    /// The identity of every position reached so far (including the starting one), for counting repetitions.
    history: Vec<String>,
}

impl Default for Game {
    fn default() -> Self {
        Game::new()
    }
}

/// Two positions count as the same for repetition if the pieces, the side to move, the castling rights and the
/// en passant possibilities are the same. The en passant square only matters when a capture there is actually legal.
fn repetition_key(position: &Position) -> String {
    let fen = position.to_fen();
    let mut fields = fen.split(' ').take(4);
    let (board, side, castling, ep) = (
        fields.next().unwrap_or(""),
        fields.next().unwrap_or(""),
        fields.next().unwrap_or(""),
        fields.next().unwrap_or("-"),
    );
    let ep = if ep != "-"
        && position
            .legal_moves()
            .iter()
            .any(|m| position.is_en_passant(*m))
    {
        ep
    } else {
        "-"
    };
    format!("{board} {side} {castling} {ep}")
}

impl Game {
    pub fn new() -> Game {
        Game::from_position(Position::starting())
    }

    pub fn from_position(position: Position) -> Game {
        Game {
            history: vec![repetition_key(&position)],
            position,
            moves: Vec::new(),
        }
    }

    pub fn position(&self) -> &Position {
        &self.position
    }

    pub fn moves(&self) -> &[Move] {
        &self.moves
    }

    pub fn legal_moves(&self) -> Vec<Move> {
        if self.outcome().is_some() {
            return Vec::new();
        }
        self.position.legal_moves()
    }

    /// How many times the current position has occurred in this game, counting now.
    pub fn repetitions(&self) -> usize {
        let now = self
            .history
            .last()
            .expect("history always holds the current position");
        self.history.iter().filter(|k| *k == now).count()
    }

    /// `None` while the game is going on. Checkmate takes precedence over the draw rules on the move that ends it.
    pub fn outcome(&self) -> Option<Outcome> {
        if !self.position.has_legal_move() {
            return Some(if self.position.in_check() {
                Outcome::Checkmate {
                    winner: self.position.side_to_move().opposite(),
                }
            } else {
                Outcome::Stalemate
            });
        }
        if self.position.insufficient_material() {
            return Some(Outcome::Draw(DrawReason::InsufficientMaterial));
        }
        if self.repetitions() >= 5 {
            return Some(Outcome::Draw(DrawReason::FivefoldRepetition));
        }
        if self.position.halfmove_clock() >= 150 {
            return Some(Outcome::Draw(DrawReason::SeventyFiveMoveRule));
        }
        None
    }

    /// A draw the player to move may claim right now, if any.
    pub fn claimable_draw(&self) -> Option<ClaimableDraw> {
        if self.outcome().is_some() {
            return None;
        }
        if self.repetitions() >= 3 {
            Some(ClaimableDraw::ThreefoldRepetition)
        } else if self.position.halfmove_clock() >= 100 {
            Some(ClaimableDraw::FiftyMoveRule)
        } else {
            None
        }
    }

    /// The result when `flagged` runs out of time: a loss, unless the opponent has no way to checkmate.
    pub fn flag_result(&self, flagged: Color) -> FlagResult {
        if self.position.cannot_checkmate(flagged.opposite()) {
            FlagResult::Draw
        } else {
            FlagResult::Loses(flagged)
        }
    }

    pub fn apply(&mut self, m: Move) -> Result<(), MoveError> {
        if self.outcome().is_some() {
            return Err(MoveError::GameOver);
        }
        if !self.position.is_legal(m) {
            return Err(MoveError::Illegal);
        }
        self.position = self.position.make_move(m);
        self.moves.push(m);
        self.history.push(repetition_key(&self.position));
        Ok(())
    }

    /// Plays a move written in UCI notation and returns it.
    pub fn apply_uci(&mut self, text: &str) -> Result<Move, MoveError> {
        let m = Move::from_uci(text).ok_or(MoveError::InvalidNotation)?;
        self.apply(m)?;
        Ok(m)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn play(game: &mut Game, moves: &str) {
        for m in moves.split_whitespace() {
            game.apply_uci(m).unwrap_or_else(|e| panic!("{m}: {e}"));
        }
    }

    fn game(fen: &str) -> Game {
        Game::from_position(Position::from_fen(fen).unwrap())
    }

    #[test]
    fn fools_mate_is_checkmate_and_nothing_more_can_be_played() {
        let mut g = Game::new();
        play(&mut g, "f2f3 e7e5 g2g4");
        assert_eq!(g.outcome(), None);
        g.apply_uci("d8h4").unwrap();
        assert_eq!(
            g.outcome(),
            Some(Outcome::Checkmate {
                winner: Color::Black
            })
        );
        assert_eq!(g.apply_uci("a2a3"), Err(MoveError::GameOver));
        assert!(g.legal_moves().is_empty());
    }

    #[test]
    fn scholars_mate() {
        let mut g = Game::new();
        play(&mut g, "e2e4 e7e5 d1h5 b8c6 f1c4 g8f6 h5f7");
        assert_eq!(
            g.outcome(),
            Some(Outcome::Checkmate {
                winner: Color::White
            })
        );
    }

    #[test]
    fn stalemate_is_a_draw_not_a_win() {
        let g = game("7k/5Q2/6K1/8/8/8/8/8 b - - 0 1");
        assert!(!g.position().in_check());
        assert_eq!(g.outcome(), Some(Outcome::Stalemate));
    }

    #[test]
    fn insufficient_material() {
        // d1 and c8 are light squares; a1 and f8 are dark ones. Bishops on one colour of square can never mate.
        for (fen, no_mate_possible) in [
            ("4k3/8/8/8/8/8/8/4K3 w - - 0 1", true),    // bare kings
            ("4k3/8/8/8/8/8/8/3NK3 w - - 0 1", true),   // king and knight against king
            ("4k3/8/8/8/8/8/8/3BK3 w - - 0 1", true),   // king and bishop against king
            ("2b1k3/8/8/8/8/8/8/3BK3 w - - 0 1", true), // bishops, both on light squares
            ("4kb2/8/8/8/8/8/8/3BK3 w - - 0 1", false), // bishops on opposite colours: mate is possible
            ("4k3/8/8/8/8/8/8/2NNK3 w - - 0 1", false), // two knights can mate with the opponent's help
            ("4k3/8/8/8/8/8/8/2NBK3 w - - 0 1", false),
            ("4k3/8/8/8/8/8/P7/4K3 w - - 0 1", false), // any pawn can promote
            ("4k3/8/8/8/8/8/8/R3K3 w - - 0 1", false),
            ("4k3/8/8/8/8/8/8/3QK3 w - - 0 1", false),
        ] {
            assert_eq!(
                game(fen).position().insufficient_material(),
                no_mate_possible,
                "{fen}"
            );
        }
        assert_eq!(
            game("4k3/8/8/8/8/8/8/4K3 w - - 0 1").outcome(),
            Some(Outcome::Draw(DrawReason::InsufficientMaterial))
        );
    }

    #[test]
    fn repeating_a_position_can_be_claimed_at_three_and_is_automatic_at_five() {
        let mut g = Game::new();
        for cycle in 1..=4 {
            play(&mut g, "g1f3 g8f6 f3g1 f6g8");
            let seen = g.repetitions();
            assert_eq!(
                seen,
                cycle + 1,
                "the start position has now occurred {seen} times"
            );
            match cycle {
                1 => assert_eq!((g.outcome(), g.claimable_draw()), (None, None)),
                2 => assert_eq!(
                    (g.outcome(), g.claimable_draw()),
                    (None, Some(ClaimableDraw::ThreefoldRepetition))
                ),
                3 => assert_eq!(
                    g.outcome(),
                    None,
                    "a fourth occurrence is still only claimable"
                ),
                _ => assert_eq!(
                    g.outcome(),
                    Some(Outcome::Draw(DrawReason::FivefoldRepetition))
                ),
            }
        }
        assert_eq!(g.apply_uci("g1f3"), Err(MoveError::GameOver));
    }

    #[test]
    fn castling_rights_and_en_passant_make_positions_different_for_repetition() {
        // Same pieces, but White can castle in one and not in the other.
        let with =
            repetition_key(&Position::from_fen("r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1").unwrap());
        let without =
            repetition_key(&Position::from_fen("r3k2r/8/8/8/8/8/8/R3K2R w kq - 0 1").unwrap());
        assert_ne!(with, without);
        // An en passant square that nothing can capture on does not make a position different...
        let no_capture =
            repetition_key(&Position::from_fen("4k3/8/8/8/4P3/8/8/4K3 b - e3 0 1").unwrap());
        let plain = repetition_key(&Position::from_fen("4k3/8/8/8/4P3/8/8/4K3 b - - 0 1").unwrap());
        assert_eq!(no_capture, plain);
        // ...but one that a pawn really can capture on does.
        let capturable =
            repetition_key(&Position::from_fen("4k3/8/8/8/3pP3/8/8/4K3 b - e3 0 1").unwrap());
        let not_capturable =
            repetition_key(&Position::from_fen("4k3/8/8/8/3pP3/8/8/4K3 b - - 0 1").unwrap());
        assert_ne!(capturable, not_capturable);
    }

    #[test]
    fn the_fifty_and_seventy_five_move_rules() {
        let mut g = game("4k3/8/8/8/8/8/8/R3K3 w - - 98 100");
        play(&mut g, "a1a2");
        assert_eq!(g.claimable_draw(), None, "99 half-moves is not enough");
        play(&mut g, "e8d8");
        assert_eq!(
            (g.outcome(), g.claimable_draw()),
            (None, Some(ClaimableDraw::FiftyMoveRule)),
            "100 half-moves can be claimed"
        );

        let mut g = game("4k3/8/8/8/8/8/8/R3K3 w - - 149 100");
        assert_eq!(g.outcome(), None);
        play(&mut g, "a1a2");
        assert_eq!(
            g.outcome(),
            Some(Outcome::Draw(DrawReason::SeventyFiveMoveRule))
        );

        // A capture or a pawn move starts the count again.
        let mut g = game("4k3/8/8/8/8/8/P7/4K3 w - - 120 100");
        play(&mut g, "a2a3");
        assert_eq!(g.position().halfmove_clock(), 0);
    }

    #[test]
    fn checkmate_on_the_move_that_would_also_make_it_a_seventy_five_move_draw_is_still_checkmate() {
        let mut g = game("7k/8/6K1/8/8/8/8/R7 w - - 149 100");
        play(&mut g, "a1a8");
        assert_eq!(g.position().halfmove_clock(), 150);
        assert_eq!(
            g.outcome(),
            Some(Outcome::Checkmate {
                winner: Color::White
            })
        );
    }

    #[test]
    fn bad_input_is_refused_with_the_right_reason() {
        let mut g = Game::new();
        for bad in ["", "e2", "e2e4e5", "hello", "e2e9", "E2E4"] {
            assert_eq!(g.apply_uci(bad), Err(MoveError::InvalidNotation), "{bad:?}");
        }
        for illegal in ["e2e5", "e7e5", "a1a3", "e1g1", "d1d4", "b1b3"] {
            assert_eq!(g.apply_uci(illegal), Err(MoveError::Illegal), "{illegal}");
        }
        assert!(g.moves().is_empty(), "a refused move changes nothing");
        assert_eq!(g.position().to_fen(), Position::starting().to_fen());
    }

    #[test]
    fn promotion_needs_a_piece_and_offers_all_four() {
        let mut g = game("k7/4P3/8/8/8/8/8/4K3 w - - 0 1");
        assert_eq!(
            g.apply_uci("e7e8"),
            Err(MoveError::Illegal),
            "a pawn reaching the last rank must say what it becomes"
        );
        assert_eq!(g.apply_uci("e7e8k"), Err(MoveError::InvalidNotation));
        let promotions: Vec<String> = g
            .legal_moves()
            .iter()
            .filter(|m| m.to_uci().starts_with("e7e8"))
            .map(|m| m.to_uci())
            .collect();
        assert_eq!(promotions.len(), 4);
        g.apply_uci("e7e8n").unwrap();
        assert_eq!(g.position().to_fen(), "k3N3/8/8/8/8/8/8/4K3 b - - 0 1");
    }

    #[test]
    fn castling_en_passant_and_pins_are_enforced() {
        // Castling is allowed, then forbidden through an attacked square, out of check, and after the king has moved.
        let mut g = game("r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1");
        assert!(
            g.position().is_legal(Move::from_uci("e1g1").unwrap())
                && g.position().is_legal(Move::from_uci("e1c1").unwrap())
        );
        let through_check = game("r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1");
        assert!(through_check
            .position()
            .is_legal(Move::from_uci("e1g1").unwrap()));
        let attacked_f1 = game("4kr2/8/8/8/8/8/8/R3K2R w KQ - 0 1");
        assert!(
            !attacked_f1
                .position()
                .is_legal(Move::from_uci("e1g1").unwrap()),
            "cannot castle through an attacked square"
        );
        let in_check = game("4r1k1/8/8/8/8/8/8/R3K2R w KQ - 0 1");
        assert!(
            in_check.position().in_check()
                && !in_check
                    .position()
                    .is_legal(Move::from_uci("e1g1").unwrap()),
            "cannot castle out of check"
        );
        play(&mut g, "e1e2 e8e7 e2e1");
        assert!(
            !g.position().is_legal(Move::from_uci("e1g1").unwrap()),
            "the king has moved"
        );
        // Castling moves the rook too.
        let mut c = game("r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1");
        play(&mut c, "e1c1");
        assert_eq!(c.position().to_fen(), "r3k2r/8/8/8/8/8/8/2KR3R b kq - 1 1");

        // En passant is only available on the very next move, and removes the right pawn.
        let mut e = Game::new();
        play(&mut e, "e2e4 a7a6 e4e5 d7d5");
        assert!(e.position().is_legal(Move::from_uci("e5d6").unwrap()));
        play(&mut e, "e5d6");
        assert!(
            e.position()
                .piece_at(super::super::types::Square::parse("d5").unwrap())
                .is_none(),
            "the captured pawn is gone"
        );
        let mut late = Game::new();
        play(&mut late, "e2e4 a7a6 e4e5 d7d5 h2h3 h7h6");
        assert_eq!(
            late.apply_uci("e5d6"),
            Err(MoveError::Illegal),
            "the chance has passed"
        );

        // A pinned piece cannot move off the line to its king.
        let pinned_rook = game("4r1k1/8/8/8/8/8/4R3/4K3 w - - 0 1");
        assert!(
            pinned_rook
                .legal_moves()
                .iter()
                .all(|m| m.from.to_string() != "e2" || m.to.file() == 4),
            "a pinned rook may only slide along the pin"
        );
    }

    #[test]
    fn en_passant_that_would_expose_the_king_is_refused() {
        // Both pawns stand between the white king and the black rook; capturing would open the rank.
        let g = game("8/8/8/KPp4r/8/8/8/7k w - c6 0 1");
        assert!(!g.position().is_legal(Move::from_uci("b5c6").unwrap()));
        assert!(g.position().is_legal(Move::from_uci("b5b6").unwrap()));
    }
}
