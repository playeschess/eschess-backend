//! Our rules against an independent implementation (python-chess), on thousands of positions from random games.
//!
//! `tests/data/random_games.txt` was made by `tests/data/generate_random_games.py`. For every position in it we check
//! that we reach exactly the same position (compared by a short fingerprint of its FEN), find exactly the same set of
//! legal moves, and agree on whether the game is over and how. Any difference, anywhere, fails the test and prints
//! our position.

use eschess_domain::chess::{DrawReason, Game, Outcome, Position};

/// FNV-1a, shown as the first 12 hex digits: the same fingerprint the generator script writes.
fn fingerprint(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        h = (h ^ byte as u64).wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")[..12].to_string()
}

fn status(game: &Game) -> &'static str {
    match game.outcome() {
        None => "-",
        Some(Outcome::Checkmate { .. }) => "mate",
        Some(Outcome::Stalemate) => "stalemate",
        Some(Outcome::Draw(DrawReason::InsufficientMaterial)) => "insufficient",
        Some(Outcome::Draw(DrawReason::FivefoldRepetition)) => "fivefold",
        Some(Outcome::Draw(DrawReason::SeventyFiveMoveRule)) => "75",
    }
}

struct Ply {
    position: String,
    count: usize,
    moves: String,
    played: String,
    status: String,
}

#[test]
fn agrees_with_python_chess_on_every_recorded_position() {
    let data = include_str!("data/random_games.txt");
    let (mut games, mut plies, mut promotions, mut castles, mut en_passants) = (0, 0, 0, 0, 0);
    let mut current: Option<(String, Vec<Ply>)> = None;
    let mut finished: Vec<(String, Vec<Ply>)> = Vec::new();

    for line in data.lines() {
        if let Some(start) = line.strip_prefix("GAME ") {
            current = Some((start.to_string(), Vec::new()));
        } else if let Some(rest) = line.strip_prefix("PLY ") {
            let f: Vec<&str> = rest.split(' ').collect();
            assert_eq!(f.len(), 5, "bad line: {line}");
            current.as_mut().expect("PLY outside a game").1.push(Ply {
                position: f[0].into(),
                count: f[1].parse().unwrap(),
                moves: f[2].into(),
                played: f[3].into(),
                status: f[4].into(),
            });
        } else if line == "END" {
            finished.push(current.take().expect("END outside a game"));
        }
    }

    for (start, recorded) in finished {
        games += 1;
        let mut game = Game::from_position(
            Position::from_fen(&start).unwrap_or_else(|e| panic!("start {start}: {e}")),
        );
        for (i, ply) in recorded.iter().enumerate() {
            plies += 1;
            let ours_fen = game.position().to_fen();
            let context = format!("game from {start}, ply {i}, our position {ours_fen}");
            assert_eq!(
                fingerprint(&ours_fen),
                ply.position,
                "we reached a different position: {context}"
            );

            let mut ours: Vec<String> = game
                .position()
                .legal_moves()
                .iter()
                .map(|m| m.to_uci())
                .collect();
            ours.sort();
            assert_eq!(
                ours.len(),
                ply.count,
                "different number of legal moves: {context}\nours: {}",
                ours.join(" ")
            );
            assert_eq!(
                fingerprint(&ours.join(" ")),
                ply.moves,
                "different set of legal moves: {context}\nours: {}",
                ours.join(" ")
            );
            assert_eq!(
                status(&game),
                ply.status,
                "different game status: {context}"
            );

            if ply.played != "-" {
                let before = game.position().clone();
                let m = game
                    .apply_uci(&ply.played)
                    .unwrap_or_else(|e| panic!("could not play {}: {e}: {context}", ply.played));
                promotions += m.promotion.is_some() as u32;
                castles += (before
                    .piece_at(m.from)
                    .is_some_and(|p| p.kind == eschess_domain::chess::PieceKind::King)
                    && (m.from.file() as i8 - m.to.file() as i8).abs() == 2)
                    as u32;
                en_passants += before.is_en_passant(m) as u32;
            }
        }
    }

    // The data must actually contain the interesting cases, or agreeing on it would prove little.
    assert!(
        games >= 60 && plies >= 4000,
        "{games} games, {plies} positions"
    );
    assert!(
        promotions >= 40 && castles >= 40 && en_passants >= 10,
        "promotions {promotions}, castles {castles}, en passants {en_passants}"
    );
    println!("agreed on {plies} positions in {games} games ({promotions} promotions, {castles} castles, {en_passants} en passants)");
}
