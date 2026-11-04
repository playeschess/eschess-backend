//! The rule for a flag falling: the player whose time ran out loses, unless the opponent could never checkmate.
//! `Position::cannot_checkmate` is compared with python-chess on 600 sparse positions (see the generator script).

use eschess_domain::chess::{Color, FlagResult, Game, Position};

#[test]
fn agrees_with_python_chess_on_whether_each_side_can_ever_checkmate() {
    let data = include_str!("data/cannot_checkmate.txt");
    let (mut lines, mut white_stuck, mut black_stuck, mut both_can) = (0, 0, 0, 0);
    for line in data.lines() {
        let f: Vec<&str> = line.split(';').collect();
        assert_eq!(f.len(), 3, "bad line: {line}");
        let position = Position::from_fen(f[0]).unwrap_or_else(|e| panic!("{}: {e}", f[0]));
        let (white, black) = (f[1] == "1", f[2] == "1");
        assert_eq!(
            position.cannot_checkmate(Color::White),
            white,
            "White, position {}",
            f[0]
        );
        assert_eq!(
            position.cannot_checkmate(Color::Black),
            black,
            "Black, position {}",
            f[0]
        );
        lines += 1;
        white_stuck += white as u32;
        black_stuck += black as u32;
        both_can += (!white && !black) as u32;
    }
    // The sample must contain all the interesting kinds of position.
    assert!(
        lines >= 600 && white_stuck >= 100 && black_stuck >= 100 && both_can >= 100,
        "{lines} {white_stuck} {black_stuck} {both_can}"
    );
}

fn flag(fen: &str, flagged: Color) -> FlagResult {
    Game::from_position(Position::from_fen(fen).unwrap()).flag_result(flagged)
}

#[test]
fn running_out_of_time_loses_unless_the_opponent_could_never_mate() {
    // Black's time runs out. White has a rook and can mate: Black loses.
    assert_eq!(
        flag("4k3/8/8/8/8/8/8/R3K3 b - - 0 1", Color::Black),
        FlagResult::Loses(Color::Black)
    );
    // White has only a king and a knight: White can never mate, so it is a draw, not a win on time.
    assert_eq!(
        flag("4k3/8/8/8/8/8/8/3NK3 b - - 0 1", Color::Black),
        FlagResult::Draw
    );
    // A bare king cannot mate either.
    assert_eq!(
        flag("4k3/8/8/8/8/8/8/4K3 b - - 0 1", Color::Black),
        FlagResult::Draw
    );
    // A pawn can become a queen.
    assert_eq!(
        flag("4k3/8/8/8/8/8/P7/4K3 b - - 0 1", Color::Black),
        FlagResult::Loses(Color::Black)
    );
    // A knight can mate if the opponent still has a pawn (a helper): "king and knight against king and pawn".
    assert_eq!(
        flag("4k3/p7/8/8/8/8/8/3NK3 b - - 0 1", Color::Black),
        FlagResult::Loses(Color::Black)
    );
    // Two knights are enough in principle (with the opponent's help).
    assert_eq!(
        flag("4k3/8/8/8/8/8/8/2NNK3 b - - 0 1", Color::Black),
        FlagResult::Loses(Color::Black)
    );
    // The same for White's flag falling.
    assert_eq!(
        flag("3nk3/8/8/8/8/8/8/4K3 w - - 0 1", Color::White),
        FlagResult::Draw
    );
    assert_eq!(
        flag("4k3/8/8/8/8/8/8/R3K3 w - - 0 1", Color::White),
        FlagResult::Draw,
        "White's time ran out and Black has only a king"
    );
    assert_eq!(
        flag("r3k3/8/8/8/8/8/8/4K3 w - - 0 1", Color::White),
        FlagResult::Loses(Color::White)
    );
    // Bishops: same colour cannot mate, opposite colours can (with help).
    assert_eq!(
        flag("2b1k3/8/8/8/8/8/8/3BK3 b - - 0 1", Color::Black),
        FlagResult::Draw
    );
    assert_eq!(
        flag("4kb2/8/8/8/8/8/8/3BK3 b - - 0 1", Color::Black),
        FlagResult::Loses(Color::Black)
    );
}
