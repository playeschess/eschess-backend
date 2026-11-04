//! FEN: the standard one-line text form of a position, e.g.
//! `rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1`.
//!
//! Reading is strict. A FEN describing an impossible position (no king, a pawn on the back rank, castling rights
//! with no rook, the side not to move already in check ...) is refused, because the rules below would otherwise
//! be asked about positions that can never arise in a game.

use std::fmt;

use super::position::{Position, BLACK_KINGSIDE, BLACK_QUEENSIDE, WHITE_KINGSIDE, WHITE_QUEENSIDE};
use super::types::{Color, Piece, PieceKind, Square};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FenError(pub &'static str);

impl fmt::Display for FenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid FEN: {}", self.0)
    }
}

impl std::error::Error for FenError {}

fn bad<T>(why: &'static str) -> Result<T, FenError> {
    Err(FenError(why))
}

impl Position {
    pub fn from_fen(fen: &str) -> Result<Position, FenError> {
        let fields: Vec<&str> = fen.split(' ').collect();
        if fields.len() != 4 && fields.len() != 6 {
            return bad("expected 4 or 6 space-separated fields");
        }
        if fields.iter().any(|f| f.is_empty()) {
            return bad("fields must be separated by single spaces");
        }
        let mut board = [None; 64];

        let ranks: Vec<&str> = fields[0].split('/').collect();
        if ranks.len() != 8 {
            return bad("the board needs 8 ranks separated by '/'");
        }
        for (i, rank_text) in ranks.iter().enumerate() {
            let rank = 7 - i as u8; // FEN lists the 8th rank first
            let mut file = 0u8;
            let mut last_was_digit = false;
            for c in rank_text.chars() {
                if let Some(d) = c.to_digit(10) {
                    if last_was_digit || !(1..=8).contains(&d) {
                        return bad("a rank has an invalid run of empty squares");
                    }
                    file += d as u8;
                    last_was_digit = true;
                } else {
                    let piece = Piece::from_fen_char(c).ok_or(FenError("unknown piece letter"))?;
                    if file >= 8 {
                        return bad("a rank has more than 8 squares");
                    }
                    board[Square::new(file, rank).expect("checked above").index()] = Some(piece);
                    file += 1;
                    last_was_digit = false;
                }
                if file > 8 {
                    return bad("a rank has more than 8 squares");
                }
            }
            if file != 8 {
                return bad("a rank has fewer than 8 squares");
            }
        }

        let side = match fields[1] {
            "w" => Color::White,
            "b" => Color::Black,
            _ => return bad("the side to move must be w or b"),
        };

        let mut castling = 0u8;
        if fields[2] != "-" {
            for c in fields[2].chars() {
                let flag = match c {
                    'K' => WHITE_KINGSIDE,
                    'Q' => WHITE_QUEENSIDE,
                    'k' => BLACK_KINGSIDE,
                    'q' => BLACK_QUEENSIDE,
                    _ => return bad("castling rights must be - or letters from KQkq"),
                };
                if castling & flag != 0 {
                    return bad("a castling right is listed twice");
                }
                castling |= flag;
            }
            if fields[2].is_empty() {
                return bad("castling rights must be - or letters from KQkq");
            }
        }

        let en_passant = if fields[3] == "-" {
            None
        } else {
            Some(
                Square::parse(fields[3])
                    .ok_or(FenError("the en passant square is not a square"))?,
            )
        };

        let (halfmove_clock, fullmove_number) = if fields.len() == 6 {
            let half: u32 = fields[4]
                .parse()
                .map_err(|_| FenError("the halfmove clock must be a number"))?;
            let full: u32 = fields[5]
                .parse()
                .map_err(|_| FenError("the move number must be a number"))?;
            if half > 9999 || !(1..=9999).contains(&full) {
                return bad("the halfmove clock or move number is out of range");
            }
            (half, full)
        } else {
            (0, 1)
        };

        let position = Position {
            board,
            side,
            castling,
            en_passant,
            halfmove_clock,
            fullmove_number,
        };
        position.validate()?;
        Ok(position)
    }

    fn validate(&self) -> Result<(), FenError> {
        for color in [Color::White, Color::Black] {
            let (mut kings, mut pawns, mut total) = (0, 0, 0);
            for s in Square::all() {
                if let Some(p) = self.piece_at(s).filter(|p| p.color == color) {
                    total += 1;
                    match p.kind {
                        PieceKind::King => kings += 1,
                        PieceKind::Pawn => {
                            pawns += 1;
                            if s.rank() == 0 || s.rank() == 7 {
                                return bad("a pawn cannot stand on the first or last rank");
                            }
                        }
                        _ => {}
                    }
                }
            }
            if kings != 1 {
                return bad("each side needs exactly one king");
            }
            if pawns > 8 || total > 16 {
                return bad("a side has too many pieces");
            }
        }

        let sq = |f, r| Square::new(f, r).expect("constant");
        for (flag, king, rook, color) in [
            (WHITE_KINGSIDE, sq(4, 0), sq(7, 0), Color::White),
            (WHITE_QUEENSIDE, sq(4, 0), sq(0, 0), Color::White),
            (BLACK_KINGSIDE, sq(4, 7), sq(7, 7), Color::Black),
            (BLACK_QUEENSIDE, sq(4, 7), sq(0, 7), Color::Black),
        ] {
            if self.castling & flag != 0
                && (self.piece_at(king) != Some(Piece::new(color, PieceKind::King))
                    || self.piece_at(rook) != Some(Piece::new(color, PieceKind::Rook)))
            {
                return bad("a castling right is listed but the king or rook is not at home");
            }
        }

        if let Some(ep) = self.en_passant {
            // After a double push the skipped square is on the 6th rank (White to move) or 3rd rank (Black to move),
            // the pawn that moved stands in front of it, and the square it came from is empty.
            let (ep_rank, pawn_rank, from_rank, pusher) = match self.side {
                Color::White => (5, 4, 6, Color::Black),
                Color::Black => (2, 3, 1, Color::White),
            };
            let pawn_square = Square::new(ep.file(), pawn_rank).expect("on board");
            let from_square = Square::new(ep.file(), from_rank).expect("on board");
            if ep.rank() != ep_rank
                || self.piece_at(ep).is_some()
                || self.piece_at(from_square).is_some()
                || self.piece_at(pawn_square) != Some(Piece::new(pusher, PieceKind::Pawn))
            {
                return bad("the en passant square is not consistent with the board");
            }
        }

        // The side that just moved cannot have left its own king attacked.
        let waiting = self.side.opposite();
        if self
            .king_square(waiting)
            .is_some_and(|k| self.is_attacked(k, self.side))
        {
            return bad("the side not to move is in check");
        }
        Ok(())
    }

    pub fn to_fen(&self) -> String {
        let mut out = String::new();
        for rank in (0..8).rev() {
            let mut empty = 0;
            for file in 0..8 {
                match self.piece_at(Square::new(file, rank).expect("on board")) {
                    None => empty += 1,
                    Some(p) => {
                        if empty > 0 {
                            out.push_str(&empty.to_string());
                            empty = 0;
                        }
                        out.push(p.fen_char());
                    }
                }
            }
            if empty > 0 {
                out.push_str(&empty.to_string());
            }
            if rank > 0 {
                out.push('/');
            }
        }
        let castling: String = [
            (WHITE_KINGSIDE, 'K'),
            (WHITE_QUEENSIDE, 'Q'),
            (BLACK_KINGSIDE, 'k'),
            (BLACK_QUEENSIDE, 'q'),
        ]
        .iter()
        .filter(|(f, _)| self.castling & f != 0)
        .map(|(_, c)| *c)
        .collect();
        format!(
            "{out} {} {} {} {} {}",
            if self.side == Color::White { 'w' } else { 'b' },
            if castling.is_empty() {
                "-".to_string()
            } else {
                castling
            },
            self.en_passant.map_or("-".to_string(), |s| s.to_string()),
            self.halfmove_clock,
            self.fullmove_number
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: &str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";

    #[test]
    fn well_formed_positions_round_trip() {
        for fen in [
            START,
            "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
            "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1",
            "rnbqkbnr/pppp1ppp/8/4p3/4P3/8/PPPP1PPP/RNBQKBNR w KQkq e6 0 2",
            "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq e3 0 1",
            "4k3/8/8/8/8/8/8/4K3 w - - 12 40",
        ] {
            assert_eq!(Position::from_fen(fen).unwrap().to_fen(), fen);
        }
        assert_eq!(Position::starting().to_fen(), START);
    }

    #[test]
    fn four_field_fens_get_default_counters_and_castling_letters_may_come_in_any_order() {
        let p = Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w qkQK -").unwrap();
        assert_eq!((p.halfmove_clock(), p.fullmove_number()), (0, 1));
        assert_eq!(p.to_fen(), START, "written back in the standard order");
    }

    #[test]
    fn broken_text_is_refused() {
        let cases = [
            ("", "empty"),
            (
                "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq -  0 1",
                "double space",
            ),
            ("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP w KQkq - 0 1", "7 ranks"),
            (
                "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR/8 w KQkq - 0 1",
                "9 ranks",
            ),
            (
                "rnbqkbnr/ppppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
                "9 squares in a rank",
            ),
            (
                "rnbqkbn/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
                "7 squares in a rank",
            ),
            (
                "rnbqkbnr/pppppppp/44/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
                "adjacent digits",
            ),
            (
                "rnbqkbnr/pppppppp/9/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
                "digit 9",
            ),
            (
                "rnbqkbnr/pppppppp/0/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
                "digit 0",
            ),
            (
                "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNX w KQkq - 0 1",
                "unknown piece",
            ),
            (
                "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR x KQkq - 0 1",
                "bad side",
            ),
            (
                "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkqK - 0 1",
                "duplicate castling",
            ),
            (
                "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQxq - 0 1",
                "bad castling letter",
            ),
            (
                "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq e9 0 1",
                "bad ep square",
            ),
            (
                "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - x 1",
                "halfmove not a number",
            ),
            (
                "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 0",
                "move number 0",
            ),
            (
                "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0",
                "5 fields",
            ),
        ];
        for (fen, why) in cases {
            assert!(
                Position::from_fen(fen).is_err(),
                "should refuse ({why}): {fen}"
            );
        }
    }

    #[test]
    fn impossible_positions_are_refused() {
        let cases = [
            ("8/8/8/8/8/8/8/4K3 w - - 0 1", "no black king"),
            ("4k3/8/8/8/8/8/8/8 w - - 0 1", "no white king"),
            ("4k3/8/8/8/8/8/8/3KK3 w - - 0 1", "two white kings"),
            ("p3k3/8/8/8/8/8/8/4K3 w - - 0 1", "pawn on the 8th rank"),
            ("4k3/8/8/8/8/8/8/P3K3 w - - 0 1", "pawn on the 1st rank"),
            ("4k3/pppppppp/p7/8/8/8/8/4K3 w - - 0 1", "nine black pawns"),
            (
                "4k3/8/8/8/8/8/8/R3K3 w Qq - 0 1",
                "black castling right without rooks",
            ),
            (
                "r3k3/8/8/8/8/8/8/4K3 w K - 0 1",
                "white kingside right without a rook",
            ),
            ("4k3/8/8/8/8/8/8/4K3 w - e3 0 1", "en passant with no pawns"),
            (
                "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR w KQkq e3 0 1",
                "ep square on the wrong rank for the side to move",
            ),
            (
                "4k3/8/8/8/8/8/8/4K2r w - - 0 1",
                "white to move but black is... (valid, white is in check)",
            ),
            (
                "4k3/8/8/8/8/8/8/4K2r b - - 0 1",
                "black to move while white's king is attacked",
            ),
            ("4k3/8/8/8/8/8/8/3kK3 w - - 0 1", "adjacent kings"),
        ];
        for (fen, why) in cases {
            let valid = why.contains("valid");
            assert_eq!(Position::from_fen(fen).is_ok(), valid, "{why}: {fen}");
        }
    }

    #[test]
    fn a_side_in_check_may_be_to_move_but_not_the_side_that_just_moved() {
        assert!(Position::from_fen("4k3/8/8/8/8/8/8/4K2r w - - 0 1")
            .unwrap()
            .in_check());
        assert!(Position::from_fen("4k3/8/8/8/8/8/8/4K2r b - - 0 1").is_err());
    }
}
