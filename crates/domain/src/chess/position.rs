use super::types::{Color, Move, Piece, PieceKind, Square};

/// Castling rights as bit flags.
pub const WHITE_KINGSIDE: u8 = 1;
pub const WHITE_QUEENSIDE: u8 = 2;
pub const BLACK_KINGSIDE: u8 = 4;
pub const BLACK_QUEENSIDE: u8 = 8;

const KNIGHT_STEPS: [(i8, i8); 8] = [
    (1, 2),
    (2, 1),
    (2, -1),
    (1, -2),
    (-1, -2),
    (-2, -1),
    (-2, 1),
    (-1, 2),
];
const KING_STEPS: [(i8, i8); 8] = [
    (1, 0),
    (1, 1),
    (0, 1),
    (-1, 1),
    (-1, 0),
    (-1, -1),
    (0, -1),
    (1, -1),
];
const DIAGONALS: [(i8, i8); 4] = [(1, 1), (-1, 1), (-1, -1), (1, -1)];
const STRAIGHTS: [(i8, i8); 4] = [(1, 0), (0, 1), (-1, 0), (0, -1)];
const PROMOTIONS: [PieceKind; 4] = [
    PieceKind::Queen,
    PieceKind::Rook,
    PieceKind::Bishop,
    PieceKind::Knight,
];

fn sq(file: u8, rank: u8) -> Square {
    Square::new(file, rank).expect("constant squares are on the board")
}

/// A chess position: the pieces, whose turn it is, and the few facts that are not visible on the board
/// (castling rights, the en passant square, and the move counters). A position never reads the clock or
/// anything outside itself, so the same position always has the same moves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Position {
    pub(crate) board: [Option<Piece>; 64],
    pub(crate) side: Color,
    pub(crate) castling: u8,
    /// The square a pawn just skipped over with a two-square move, as FEN records it (even if nothing can
    /// actually capture there).
    pub(crate) en_passant: Option<Square>,
    /// Half-moves since the last capture or pawn move, for the fifty-move rule.
    pub(crate) halfmove_clock: u32,
    pub(crate) fullmove_number: u32,
}

impl Position {
    /// The standard starting position.
    pub fn starting() -> Position {
        Position::from_fen("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1")
            .expect("the starting position is valid")
    }

    pub fn side_to_move(&self) -> Color {
        self.side
    }

    pub fn piece_at(&self, square: Square) -> Option<Piece> {
        self.board[square.index()]
    }

    pub fn castling_rights(&self) -> u8 {
        self.castling
    }

    pub fn en_passant_square(&self) -> Option<Square> {
        self.en_passant
    }

    pub fn halfmove_clock(&self) -> u32 {
        self.halfmove_clock
    }

    pub fn fullmove_number(&self) -> u32 {
        self.fullmove_number
    }

    pub fn king_square(&self, color: Color) -> Option<Square> {
        Square::all().find(|s| self.piece_at(*s) == Some(Piece::new(color, PieceKind::King)))
    }

    /// Is `target` attacked by any piece of colour `by`?
    pub fn is_attacked(&self, target: Square, by: Color) -> bool {
        // A pawn attacks diagonally forward, so look one rank back from the target.
        let behind = if by == Color::White { -1 } else { 1 };
        for df in [-1, 1] {
            if target.offset(df, behind).and_then(|s| self.piece_at(s))
                == Some(Piece::new(by, PieceKind::Pawn))
            {
                return true;
            }
        }
        for (df, dr) in KNIGHT_STEPS {
            if target.offset(df, dr).and_then(|s| self.piece_at(s))
                == Some(Piece::new(by, PieceKind::Knight))
            {
                return true;
            }
        }
        for (df, dr) in KING_STEPS {
            if target.offset(df, dr).and_then(|s| self.piece_at(s))
                == Some(Piece::new(by, PieceKind::King))
            {
                return true;
            }
        }
        let slides = |dirs: &[(i8, i8); 4], kind: PieceKind| {
            dirs.iter().any(|&(df, dr)| {
                let mut cur = target;
                while let Some(next) = cur.offset(df, dr) {
                    match self.piece_at(next) {
                        None => cur = next,
                        Some(p) => {
                            return p.color == by && (p.kind == kind || p.kind == PieceKind::Queen)
                        }
                    }
                }
                false
            })
        };
        slides(&DIAGONALS, PieceKind::Bishop) || slides(&STRAIGHTS, PieceKind::Rook)
    }

    /// Is the side to move in check?
    pub fn in_check(&self) -> bool {
        self.king_square(self.side)
            .is_some_and(|k| self.is_attacked(k, self.side.opposite()))
    }

    /// Every legal move for the side to move, in a fixed order.
    pub fn legal_moves(&self) -> Vec<Move> {
        let mut pseudo = Vec::with_capacity(48);
        self.pseudo_legal_moves(&mut pseudo);
        pseudo.retain(|m| self.leaves_king_safe(*m));
        pseudo
    }

    pub fn is_legal(&self, m: Move) -> bool {
        self.legal_moves().contains(&m)
    }

    pub fn has_legal_move(&self) -> bool {
        let mut pseudo = Vec::with_capacity(48);
        self.pseudo_legal_moves(&mut pseudo);
        pseudo.into_iter().any(|m| self.leaves_king_safe(m))
    }

    fn leaves_king_safe(&self, m: Move) -> bool {
        let next = self.make_move(m);
        next.king_square(self.side)
            .is_none_or(|k| !next.is_attacked(k, self.side.opposite()))
    }

    /// Does this move capture a pawn en passant?
    pub fn is_en_passant(&self, m: Move) -> bool {
        self.piece_at(m.from)
            .is_some_and(|p| p.kind == PieceKind::Pawn)
            && Some(m.to) == self.en_passant
            && m.from.file() != m.to.file()
            && self.piece_at(m.to).is_none()
    }

    /// Plays a move and returns the new position. The move must be legal: this does not check.
    pub fn make_move(&self, m: Move) -> Position {
        let mut p = self.clone();
        let moving = self
            .piece_at(m.from)
            .expect("a legal move starts on a piece");
        let captured = self.piece_at(m.to);
        let en_passant = self.is_en_passant(m);

        if en_passant {
            // The captured pawn is beside the mover's start square, not on the destination.
            p.board[sq(m.to.file(), m.from.rank()).index()] = None;
        }
        p.board[m.to.index()] = Some(match m.promotion {
            Some(kind) => Piece::new(moving.color, kind),
            None => moving,
        });
        p.board[m.from.index()] = None;

        if moving.kind == PieceKind::King && (m.to.file() as i8 - m.from.file() as i8).abs() == 2 {
            let rank = m.from.rank();
            let (rook_from, rook_to) = if m.to.file() == 6 {
                (sq(7, rank), sq(5, rank))
            } else {
                (sq(0, rank), sq(3, rank))
            };
            p.board[rook_to.index()] = p.board[rook_from.index()].take();
        }

        // Castling rights are lost when the king or a rook moves, or when a rook is captured at home.
        if moving.kind == PieceKind::King {
            p.castling &= if moving.color == Color::White {
                !(WHITE_KINGSIDE | WHITE_QUEENSIDE)
            } else {
                !(BLACK_KINGSIDE | BLACK_QUEENSIDE)
            };
        }
        for touched in [m.from, m.to] {
            p.castling &= match (touched.file(), touched.rank()) {
                (0, 0) => !WHITE_QUEENSIDE,
                (7, 0) => !WHITE_KINGSIDE,
                (0, 7) => !BLACK_QUEENSIDE,
                (7, 7) => !BLACK_KINGSIDE,
                _ => 0xff,
            };
        }

        let double_push =
            moving.kind == PieceKind::Pawn && (m.to.rank() as i8 - m.from.rank() as i8).abs() == 2;
        p.en_passant = double_push.then(|| sq(m.from.file(), (m.from.rank() + m.to.rank()) / 2));
        p.halfmove_clock = if moving.kind == PieceKind::Pawn || captured.is_some() || en_passant {
            0
        } else {
            self.halfmove_clock + 1
        };
        if self.side == Color::Black {
            p.fullmove_number += 1;
        }
        p.side = self.side.opposite();
        p
    }

    fn pseudo_legal_moves(&self, out: &mut Vec<Move>) {
        for from in Square::all() {
            let Some(piece) = self.piece_at(from) else {
                continue;
            };
            if piece.color != self.side {
                continue;
            }
            match piece.kind {
                PieceKind::Pawn => self.pawn_moves(from, out),
                PieceKind::Knight => self.step_moves(from, &KNIGHT_STEPS, out),
                PieceKind::King => {
                    self.step_moves(from, &KING_STEPS, out);
                    self.castling_moves(from, out);
                }
                PieceKind::Bishop => self.slide_moves(from, &DIAGONALS, out),
                PieceKind::Rook => self.slide_moves(from, &STRAIGHTS, out),
                PieceKind::Queen => {
                    self.slide_moves(from, &DIAGONALS, out);
                    self.slide_moves(from, &STRAIGHTS, out);
                }
            }
        }
    }

    fn step_moves(&self, from: Square, steps: &[(i8, i8); 8], out: &mut Vec<Move>) {
        for &(df, dr) in steps {
            if let Some(to) = from.offset(df, dr) {
                if self.piece_at(to).is_none_or(|p| p.color != self.side) {
                    out.push(Move {
                        from,
                        to,
                        promotion: None,
                    });
                }
            }
        }
    }

    fn slide_moves(&self, from: Square, dirs: &[(i8, i8); 4], out: &mut Vec<Move>) {
        for &(df, dr) in dirs {
            let mut cur = from;
            while let Some(to) = cur.offset(df, dr) {
                match self.piece_at(to) {
                    None => out.push(Move {
                        from,
                        to,
                        promotion: None,
                    }),
                    Some(p) => {
                        if p.color != self.side {
                            out.push(Move {
                                from,
                                to,
                                promotion: None,
                            });
                        }
                        break;
                    }
                }
                cur = to;
            }
        }
    }

    fn pawn_moves(&self, from: Square, out: &mut Vec<Move>) {
        let (dir, start_rank, last_rank) = if self.side == Color::White {
            (1, 1, 7)
        } else {
            (-1, 6, 0)
        };
        let mut push = |to: Square| {
            if to.rank() == last_rank {
                out.extend(PROMOTIONS.iter().map(|&kind| Move {
                    from,
                    to,
                    promotion: Some(kind),
                }));
            } else {
                out.push(Move {
                    from,
                    to,
                    promotion: None,
                });
            }
        };
        if let Some(one) = from.offset(0, dir) {
            if self.piece_at(one).is_none() {
                push(one);
                if from.rank() == start_rank {
                    if let Some(two) = one.offset(0, dir) {
                        if self.piece_at(two).is_none() {
                            push(two);
                        }
                    }
                }
            }
        }
        for df in [-1, 1] {
            if let Some(to) = from.offset(df, dir) {
                let enemy_there = self.piece_at(to).is_some_and(|p| p.color != self.side);
                if enemy_there || Some(to) == self.en_passant {
                    push(to);
                }
            }
        }
    }

    fn castling_moves(&self, from: Square, out: &mut Vec<Move>) {
        let (rank, king_side, queen_side) = match self.side {
            Color::White => (0, WHITE_KINGSIDE, WHITE_QUEENSIDE),
            Color::Black => (7, BLACK_KINGSIDE, BLACK_QUEENSIDE),
        };
        if from != sq(4, rank) || self.castling & (king_side | queen_side) == 0 || self.in_check() {
            return;
        }
        let enemy = self.side.opposite();
        let empty = |files: &[u8]| files.iter().all(|&f| self.piece_at(sq(f, rank)).is_none());
        let safe = |files: &[u8]| files.iter().all(|&f| !self.is_attacked(sq(f, rank), enemy));
        let own_rook = |file: u8| {
            self.piece_at(sq(file, rank)) == Some(Piece::new(self.side, PieceKind::Rook))
        };
        // The king may not castle out of, through, or into check; the rook may pass over an attacked square.
        if self.castling & king_side != 0 && own_rook(7) && empty(&[5, 6]) && safe(&[5, 6]) {
            out.push(Move {
                from,
                to: sq(6, rank),
                promotion: None,
            });
        }
        if self.castling & queen_side != 0 && own_rook(0) && empty(&[1, 2, 3]) && safe(&[2, 3]) {
            out.push(Move {
                from,
                to: sq(2, rank),
                promotion: None,
            });
        }
    }

    /// No sequence of legal moves can checkmate: bare kings, a king and one minor piece against a king, or only
    /// bishops that all stand on the same colour of square.
    pub fn insufficient_material(&self) -> bool {
        let mut bishops_light = 0;
        let mut bishops_dark = 0;
        let mut knights = 0;
        for s in Square::all() {
            let Some(p) = self.piece_at(s) else { continue };
            match p.kind {
                PieceKind::King => {}
                PieceKind::Knight => knights += 1,
                PieceKind::Bishop if s.is_light() => bishops_light += 1,
                PieceKind::Bishop => bishops_dark += 1,
                PieceKind::Pawn | PieceKind::Rook | PieceKind::Queen => return false,
            }
        }
        let minors = knights + bishops_light + bishops_dark;
        minors <= 1 || (knights == 0 && (bishops_light == 0 || bishops_dark == 0))
    }
}

impl Position {
    /// Can `color` never checkmate in this position, whatever the opponent does? (FIDE Article 6.9: when a player's
    /// time runs out but the opponent could not possibly checkmate, the game is a draw, not a win on time.)
    ///
    /// Stricter than [`insufficient_material`](Position::insufficient_material), which asks about both sides at once:
    /// here the opponent may still have pieces that could help *them* be mated.
    pub fn cannot_checkmate(&self, color: Color) -> bool {
        let (mut own_total, mut own_knights, mut own_bishops) = (0, 0, 0);
        let (mut light_bishops, mut dark_bishops) = (0, 0);
        let (mut any_pawn, mut any_knight) = (false, false);
        let mut opponent_has_helpers = false; // pawns, knights, bishops or rooks
        for s in Square::all() {
            let Some(p) = self.piece_at(s) else { continue };
            any_pawn |= p.kind == PieceKind::Pawn;
            any_knight |= p.kind == PieceKind::Knight;
            if p.kind == PieceKind::Bishop {
                if s.is_light() {
                    light_bishops += 1;
                } else {
                    dark_bishops += 1;
                }
            }
            if p.color == color {
                own_total += 1;
                match p.kind {
                    PieceKind::Pawn | PieceKind::Rook | PieceKind::Queen => return false,
                    PieceKind::Knight => own_knights += 1,
                    PieceKind::Bishop => own_bishops += 1,
                    PieceKind::King => {}
                }
            } else if !matches!(p.kind, PieceKind::King | PieceKind::Queen) {
                opponent_has_helpers = true;
            }
        }
        if own_knights > 0 {
            // A lone knight cannot mate, unless the opponent has pieces of their own that it could be mated against.
            return own_total <= 2 && !opponent_has_helpers;
        }
        if own_bishops > 0 {
            // Bishops cannot mate if they all share a colour (counting the opponent's bishops too) and nothing else
            // on the board can block or help.
            let same_colour = light_bishops == 0 || dark_bishops == 0;
            return same_colour && !any_pawn && !any_knight;
        }
        true
    }
}

/// Counts the leaf positions reachable in exactly `depth` moves. The numbers for well-known positions are
/// published, so matching them is strong evidence that move generation is right.
pub fn perft(position: &Position, depth: u32) -> u64 {
    if depth == 0 {
        return 1;
    }
    let moves = position.legal_moves();
    if depth == 1 {
        return moves.len() as u64;
    }
    moves
        .into_iter()
        .map(|m| perft(&position.make_move(m), depth - 1))
        .sum()
}
