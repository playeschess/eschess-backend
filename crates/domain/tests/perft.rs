//! Move generation against the published "perft" counts: the number of positions reachable in exactly N moves.
//!
//! These positions (from the Chess Programming Wiki) are famous for catching bugs in castling, en passant,
//! pins, promotion and discovered checks. Every number below was also reproduced for the shallower depths by an
//! independent chess library (python-chess); matching them all is strong evidence the move generator is right.

use eschess_domain::chess::{perft, Position};

fn check(name: &str, fen: &str, expected: &[u64]) {
    let position = Position::from_fen(fen).unwrap_or_else(|e| panic!("{name}: {e}"));
    for (i, want) in expected.iter().enumerate() {
        let depth = i as u32 + 1;
        assert_eq!(
            perft(&position, depth),
            *want,
            "{name}: wrong count at depth {depth}"
        );
    }
}

#[test]
fn the_starting_position() {
    check(
        "start",
        "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
        &[20, 400, 8_902, 197_281],
    );
}

/// "Kiwipete": castling both ways, pins, en passant and promotions all at once.
#[test]
fn kiwipete() {
    check(
        "kiwipete",
        "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
        &[48, 2_039, 97_862, 4_085_603],
    );
}

/// Rook, pawns and kings: en passant that would expose the king along a rank, and checks by discovery.
#[test]
fn position_3() {
    check(
        "pos3",
        "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1",
        &[14, 191, 2_812, 43_238, 674_624],
    );
}

/// Promotions, including underpromotion, capturing promotions and castling out of check.
#[test]
fn position_4_and_its_mirror() {
    check(
        "pos4",
        "r3k2r/Pppp1ppp/1b3nbN/nP6/BBP1P3/q4N2/Pp1P2PP/R2Q1RK1 w kq - 0 1",
        &[6, 264, 9_467, 422_333],
    );
    check(
        "pos4 mirrored",
        "r2q1rk1/pP1p2pp/Q4n2/bbp1p3/Np6/1B3NBn/pPPP1PPP/R3K2R b KQ - 0 1",
        &[6, 264, 9_467, 422_333],
    );
}

#[test]
fn position_5() {
    check(
        "pos5",
        "rnbq1k1r/pp1Pbppp/2p5/8/2B5/8/PPP1NnPP/RNBQK2R w KQ - 1 8",
        &[44, 1_486, 62_379, 2_103_487],
    );
}

#[test]
fn position_6() {
    check(
        "pos6",
        "r4rk1/1pp1qppp/p1np1n2/2b1p1B1/2B1P1b1/P1NP1N2/1PP1QPPP/R4RK1 w - - 0 10",
        &[46, 2_079, 89_890, 3_894_594],
    );
}
