//! The worker: background jobs, run apart from the API (architecture §5.3) so that a settlement retry storm cannot slow
//! the move loop. It credits Stellar deposits (`eschess_api::stellar_watcher`), sends withdrawals ([`sender`]), and
//! scores finished games for fair play ([`fairplay`]).
//!
//! Every job is safe to run on several workers at once: each round takes a database lock, and every step is idempotent.

pub mod contract_settle;
pub mod expire;
pub mod fairplay;
pub mod fiat;
pub mod funding;
pub mod lichess_games;
pub mod reconcile;
pub mod sender;
pub mod settle;
pub mod wallet_setup;

pub use sender::{SenderConfig, TickReport};
