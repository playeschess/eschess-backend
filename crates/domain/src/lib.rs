//! The rules of eSchess, as pure functions.
//!
//! This crate has no database, no network, no reading of the system clock and no randomness: anything
//! involving time or chance is passed in by the caller. That is what lets the rules be tested in
//! milliseconds with no infrastructure, and what makes the move loop easy to reason about.
//!
//! Modules arrive one at a time: `money`, `chess` and `clock` so far; rating to come.

pub mod chess;
pub mod clock;
pub mod money;
