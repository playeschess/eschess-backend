//! Reading a move written in standard algebraic notation (SAN), as sites like Lichess export them: `e4`, `Nbd7`, `exd5`,
//! `O-O`, `e8=Q+`. The move is found among the legal moves of the position, so a SAN that is not exactly one legal move
//! here (wrong piece, ambiguous, impossible) is refused rather than guessed.

use super::{Move, PieceKind, Position, Square};

impl Position {
    /// The one legal move this SAN means in this position, or `None`. Check marks (`+`, `#`) and annotations (`!`, `?`)
    /// are ignored.
    pub fn move_from_san(&self, san: &str) -> Option<Move> {
        let s = san.trim().trim_end_matches(['+', '#', '!', '?']);
        if !s.is_ascii() || s.len() < 2 {
            return None;
        }
        let legal = self.legal_moves();
        let kind_at = |m: &Move| self.piece_at(m.from).map(|p| p.kind);

        let castle = match s {
            "O-O" | "0-0" => Some(2i8),
            "O-O-O" | "0-0-0" => Some(-2i8),
            _ => None,
        };
        if let Some(step) = castle {
            return only(legal.into_iter().filter(|m| {
                kind_at(m) == Some(PieceKind::King)
                    && m.to.file() as i8 - m.from.file() as i8 == step
                    && m.to.rank() == m.from.rank()
            }));
        }

        let mut rest = s;
        let kind = match rest.chars().next()? {
            'K' => PieceKind::King,
            'Q' => PieceKind::Queen,
            'R' => PieceKind::Rook,
            'B' => PieceKind::Bishop,
            'N' => PieceKind::Knight,
            _ => PieceKind::Pawn,
        };
        if kind != PieceKind::Pawn {
            rest = &rest[1..];
        }
        // A promotion is `=Q`, or just `Q` after the square in looser writers.
        let mut promotion = None;
        if let Some(i) = rest.rfind('=') {
            promotion = Some(promotion_piece(&rest[i + 1..])?);
            rest = &rest[..i];
        } else if kind == PieceKind::Pawn && rest.len() > 2 {
            if let Some(p) = promotion_piece(&rest[rest.len() - 1..]) {
                promotion = Some(p);
                rest = &rest[..rest.len() - 1];
            }
        }
        if rest.len() < 2 {
            return None;
        }
        let (head, dest) = rest.split_at(rest.len() - 2);
        let to = Square::parse(dest)?;
        let marked_capture = head.ends_with('x');
        let head = head.strip_suffix('x').unwrap_or(head);
        let (mut from_file, mut from_rank) = (None, None);
        for c in head.chars() {
            match c {
                'a'..='h' if from_file.is_none() => from_file = Some(c as u8 - b'a'),
                '1'..='8' if from_rank.is_none() => from_rank = Some(c as u8 - b'1'),
                _ => return None,
            }
        }
        only(legal.into_iter().filter(|m| {
            m.to == to
                && kind_at(m) == Some(kind)
                && m.promotion == promotion
                && from_file.is_none_or(|f| m.from.file() == f)
                && from_rank.is_none_or(|r| m.from.rank() == r)
                // `x` is written exactly when the move captures; a SAN that says otherwise is not this move.
                && (self.piece_at(m.to).is_some() || self.is_en_passant(*m)) == marked_capture
        }))
    }
}

fn promotion_piece(text: &str) -> Option<PieceKind> {
    match text {
        "Q" => Some(PieceKind::Queen),
        "R" => Some(PieceKind::Rook),
        "B" => Some(PieceKind::Bishop),
        "N" => Some(PieceKind::Knight),
        _ => None,
    }
}

/// The only item, or `None` if there are none or several.
fn only(mut moves: impl Iterator<Item = Move>) -> Option<Move> {
    let first = moves.next()?;
    moves.next().is_none().then_some(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn play(position: &Position, san: &str) -> (Move, Position) {
        let m = position
            .move_from_san(san)
            .unwrap_or_else(|| panic!("{san} is not one legal move"));
        (m, position.make_move(m))
    }

    #[test]
    fn the_opera_game_reads_move_by_move_and_ends_in_mate() {
        // Morphy v. the Duke and the Count, 1858: captures, a disambiguation (Nbd7), long castling, check and mate.
        let game = "e4 e5 Nf3 d6 d4 Bg4 dxe5 Bxf3 Qxf3 dxe5 Bc4 Nf6 Qb3 Qe7 Nc3 c6 Bg5 b5 Nxb5 cxb5 Bxb5+ Nbd7 \
                    O-O-O Rd8 Rxd7 Rxd7 Rd1 Qe6 Bxd7+ Nxd7 Qb8+ Nxb8 Rd8#";
        let mut p = Position::starting();
        for san in game.split_whitespace() {
            p = play(&p, san).1;
        }
        assert!(p.in_check() && !p.has_legal_move(), "ends in checkmate");
    }

    #[test]
    fn castling_en_passant_and_promotion() {
        let p = Position::from_fen("r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1").unwrap();
        assert_eq!(play(&p, "O-O").0.to_uci(), "e1g1");
        assert_eq!(play(&p, "O-O-O").0.to_uci(), "e1c1");

        let p = Position::from_fen("4k3/8/8/3pP3/8/8/8/4K3 w - d6 0 1").unwrap();
        assert_eq!(play(&p, "exd6").0.to_uci(), "e5d6");

        let p = Position::from_fen("7k/P7/8/8/8/8/8/K7 w - - 0 1").unwrap();
        assert_eq!(play(&p, "a8=Q+").0.to_uci(), "a7a8q");
        assert_eq!(play(&p, "a8=N").0.to_uci(), "a7a8n");
        assert_eq!(
            play(&p, "a8Q").0.to_uci(),
            "a7a8q",
            "looser writers drop the ="
        );
    }

    #[test]
    fn what_is_not_exactly_one_legal_move_is_refused() {
        let start = Position::starting();
        for bad in [
            "e5", "Nf6", "Ke2", "O-O", "Nd2", "xe4", "", "e", "Zf3", "e4e5", "Nbd7x",
        ] {
            assert!(start.move_from_san(bad).is_none(), "{bad:?}");
        }
        // Two knights can reach c3: without the file it is ambiguous, with it it is not.
        let p = Position::from_fen("4k3/8/8/8/8/8/8/1N1NK3 w - - 0 1").unwrap();
        assert!(p.move_from_san("Nc3").is_none(), "both knights reach c3");
        assert_eq!(p.move_from_san("Nbc3").unwrap().to_uci(), "b1c3");
        assert_eq!(p.move_from_san("Ndc3").unwrap().to_uci(), "d1c3");
        // A capture mark on a quiet move, or a missing one on a capture, is not that move.
        assert!(start.move_from_san("exe4").is_none() && start.move_from_san("Nxf3").is_none());
    }
}
