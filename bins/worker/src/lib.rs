//! The worker: money jobs, run apart from the API (architecture §5.3) so that a settlement retry storm cannot slow the
//! move loop. It credits Stellar deposits (`eschess_api::stellar_watcher`) and sends withdrawals ([`sender`]).
//!
//! Every job is safe to run on several workers at once: each round takes a database lock, and every step is idempotent.

pub mod sender;

pub use sender::{SenderConfig, TickReport};
