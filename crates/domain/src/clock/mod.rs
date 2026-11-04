//! The game clock, as pure arithmetic on millisecond timestamps.
//!
//! Nothing here reads the time: the caller says what time it is. That makes the clock testable to the millisecond,
//! and lets the server rebuild it after a restart from stored values instead of holding a live countdown in memory
//! (losing a server then costs at most the part of one turn that had elapsed).

mod control;
mod game_clock;

pub use control::{Category, TimeControl, MAX_BASE_MS, MAX_INCREMENT_MS, MIN_BASE_MS};
pub use game_clock::{Clock, ClockError, ClockState, LagPolicy, MoveTiming, Timed};
