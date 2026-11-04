use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Color {
    White,
    Black,
}

impl Color {
    pub fn opposite(self) -> Color {
        match self {
            Color::White => Color::Black,
            Color::Black => Color::White,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PieceKind {
    Pawn,
    Knight,
    Bishop,
    Rook,
    Queen,
    King,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Piece {
    pub color: Color,
    pub kind: PieceKind,
}

impl Piece {
    pub fn new(color: Color, kind: PieceKind) -> Self {
        Piece { color, kind }
    }

    /// The letter FEN uses: uppercase for White, lowercase for Black.
    pub fn fen_char(self) -> char {
        let c = match self.kind {
            PieceKind::Pawn => 'p',
            PieceKind::Knight => 'n',
            PieceKind::Bishop => 'b',
            PieceKind::Rook => 'r',
            PieceKind::Queen => 'q',
            PieceKind::King => 'k',
        };
        if self.color == Color::White {
            c.to_ascii_uppercase()
        } else {
            c
        }
    }

    pub fn from_fen_char(c: char) -> Option<Piece> {
        let kind = match c.to_ascii_lowercase() {
            'p' => PieceKind::Pawn,
            'n' => PieceKind::Knight,
            'b' => PieceKind::Bishop,
            'r' => PieceKind::Rook,
            'q' => PieceKind::Queen,
            'k' => PieceKind::King,
            _ => return None,
        };
        Some(Piece {
            color: if c.is_ascii_uppercase() {
                Color::White
            } else {
                Color::Black
            },
            kind,
        })
    }
}

/// A square on the board. Index 0 is a1, 1 is b1, ... 7 is h1, 8 is a2, ... 63 is h8.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Square(u8);

impl Square {
    pub fn from_index(index: u8) -> Option<Square> {
        (index < 64).then_some(Square(index))
    }

    /// `file` and `rank` count from 0: file 0 is the a-file, rank 0 is the first rank.
    pub fn new(file: u8, rank: u8) -> Option<Square> {
        (file < 8 && rank < 8).then_some(Square(rank * 8 + file))
    }

    pub fn index(self) -> usize {
        self.0 as usize
    }

    pub fn file(self) -> u8 {
        self.0 % 8
    }

    pub fn rank(self) -> u8 {
        self.0 / 8
    }

    /// The square `df` files and `dr` ranks away, if it is on the board.
    pub fn offset(self, df: i8, dr: i8) -> Option<Square> {
        let (f, r) = (self.file() as i8 + df, self.rank() as i8 + dr);
        if (0..8).contains(&f) && (0..8).contains(&r) {
            Square::new(f as u8, r as u8)
        } else {
            None
        }
    }

    /// Reads `e4`. Only lowercase letters a to h and digits 1 to 8 are accepted.
    pub fn parse(text: &str) -> Option<Square> {
        let b = text.as_bytes();
        if b.len() != 2 || !(b'a'..=b'h').contains(&b[0]) || !(b'1'..=b'8').contains(&b[1]) {
            return None;
        }
        Square::new(b[0] - b'a', b[1] - b'1')
    }

    /// Whether this is a light square (a1 is dark).
    pub fn is_light(self) -> bool {
        (self.file() + self.rank()) % 2 == 1
    }

    pub fn all() -> impl Iterator<Item = Square> {
        (0..64).map(Square)
    }
}

impl fmt::Display for Square {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}{}",
            (b'a' + self.file()) as char,
            (b'1' + self.rank()) as char
        )
    }
}

/// A move: where from, where to, and what a pawn promotes to. Castling is the king's two-square move
/// (`e1g1`), and en passant is the pawn's diagonal move to the empty square, as in the UCI notation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Move {
    pub from: Square,
    pub to: Square,
    pub promotion: Option<PieceKind>,
}

impl Move {
    /// Reads UCI notation: `e2e4`, or `e7e8q` for a promotion (`q`, `r`, `b` or `n`).
    pub fn from_uci(text: &str) -> Option<Move> {
        if !text.is_ascii() || !(4..=5).contains(&text.len()) {
            return None;
        }
        let (from, to) = (Square::parse(&text[0..2])?, Square::parse(&text[2..4])?);
        let promotion = match text.get(4..5) {
            None => None,
            Some("q") => Some(PieceKind::Queen),
            Some("r") => Some(PieceKind::Rook),
            Some("b") => Some(PieceKind::Bishop),
            Some("n") => Some(PieceKind::Knight),
            Some(_) => return None,
        };
        Some(Move {
            from,
            to,
            promotion,
        })
    }

    pub fn to_uci(self) -> String {
        let promo = match self.promotion {
            Some(PieceKind::Queen) => "q",
            Some(PieceKind::Rook) => "r",
            Some(PieceKind::Bishop) => "b",
            Some(PieceKind::Knight) => "n",
            _ => "",
        };
        format!("{}{}{}", self.from, self.to, promo)
    }
}

impl fmt::Display for Move {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_uci())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn squares_parse_print_and_walk() {
        assert_eq!(Square::parse("a1").unwrap().index(), 0);
        assert_eq!(Square::parse("h8").unwrap().index(), 63);
        assert_eq!(Square::parse("e4").unwrap().to_string(), "e4");
        for bad in ["", "e", "e9", "i1", "E4", "e44", "4e", "é4"] {
            assert!(Square::parse(bad).is_none(), "{bad:?}");
        }
        let e4 = Square::parse("e4").unwrap();
        assert_eq!(e4.offset(1, 2).unwrap().to_string(), "f6");
        assert!(Square::parse("a1").unwrap().offset(-1, 0).is_none());
        assert!(Square::parse("h8").unwrap().offset(0, 1).is_none());
        assert_eq!(Square::all().count(), 64);
        assert!(
            !Square::parse("a1").unwrap().is_light() && Square::parse("h1").unwrap().is_light()
        );
    }

    #[test]
    fn uci_moves_round_trip_and_bad_ones_are_refused() {
        for text in ["e2e4", "e1g1", "e7e8q", "a7a8n", "h2h1r", "b2b1b"] {
            assert_eq!(Move::from_uci(text).unwrap().to_uci(), text);
        }
        for bad in [
            "", "e2", "e2e", "e2e4x", "e2e4qq", "e7e8k", "e7e8p", "E2E4", "e2-e4", "e2e9", "e2e4Q",
        ] {
            assert!(Move::from_uci(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn pieces_map_to_and_from_fen_letters() {
        for c in "pnbrqkPNBRQK".chars() {
            assert_eq!(Piece::from_fen_char(c).unwrap().fen_char(), c);
        }
        assert!(Piece::from_fen_char('x').is_none() && Piece::from_fen_char('1').is_none());
        assert_eq!(
            Piece::from_fen_char('K').unwrap(),
            Piece::new(Color::White, PieceKind::King)
        );
    }
}
