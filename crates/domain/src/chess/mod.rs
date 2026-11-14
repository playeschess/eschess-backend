//! The rules of chess, as pure functions over plain data.
//!
//! Nothing here touches a clock, a database, the network or a random number generator, so a position and a
//! move always give the same answer, and the whole module can be tested in milliseconds.

mod fen;
mod game;
mod position;
mod san;
mod types;

pub use fen::FenError;
pub use game::{ClaimableDraw, DrawReason, FlagResult, Game, MoveError, Outcome};
pub use position::{
    perft, Position, BLACK_KINGSIDE, BLACK_QUEENSIDE, WHITE_KINGSIDE, WHITE_QUEENSIDE,
};
pub use types::{Color, Move, Piece, PieceKind, Square};
