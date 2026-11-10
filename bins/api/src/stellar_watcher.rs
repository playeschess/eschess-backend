//! Watches the platform wallet on Stellar and credits players for what arrives.
//!
//! One round ([`poll_once`]) reads every record after the saved cursor, handles each, and only then moves the cursor
//! forward. If anything fails part-way the cursor stays put and the next round repeats the work, which is safe because
//! handling a payment is idempotent. Several servers may run this at once; a database lock lets only one poll at a time.

use std::sync::Arc;
use std::time::Duration;

use eschess_domain::money::AssetId;
use eschess_stellar::{Chain, Record};
use eschess_storage::stellar as store;
use eschess_storage::wallets::{self, WatchTiming};
use sqlx::PgPool;

use crate::config::StellarConfig;

const PAGE: u32 = 100;
/// A round reads at most this many pages, so a long backlog is worked through over several rounds, not one huge one.
const MAX_PAGES_PER_ROUND: usize = 50;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub credited: usize,
    pub needs_review: usize,
    pub already_handled: usize,
    pub ignored: usize,
    /// Another server holds the polling lock, so nothing was done.
    pub skipped: bool,
}

pub fn cursor_name(hot_account: &str) -> String {
    format!("deposits:{hot_account}")
}

/// One round of reading and crediting.
pub async fn poll_once(
    pool: &PgPool,
    chain: &dyn Chain,
    hot_account: &str,
    accepted: &[AssetId],
) -> Result<Stats, String> {
    // Only one server polls at a time. The lock lives as long as this transaction.
    let mut lock = pool.begin().await.map_err(|e| e.to_string())?;
    // Advisory locks are shared by every database on the server, so the key names this database: rounds for
    // different databases (parallel test databases, say) must not make each other skip.
    let got: bool = sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock(hashtext(current_database() || ':' || $1))",
    )
    .bind(format!("stellar-watch:{hot_account}"))
    .fetch_one(&mut *lock)
    .await
    .map_err(|e| e.to_string())?;
    if !got {
        return Ok(Stats {
            skipped: true,
            ..Stats::default()
        });
    }

    let name = cursor_name(hot_account);
    let mut cursor = store::cursor(pool, &name)
        .await
        .map_err(|e| e.to_string())?;
    let mut stats = Stats::default();
    for _ in 0..MAX_PAGES_PER_ROUND {
        let page = chain
            .payments(hot_account, cursor.as_deref(), PAGE)
            .await
            .map_err(|e| e.to_string())?;
        let count = page.records.len();
        for record in &page.records {
            match record {
                Record::Ignored => stats.ignored += 1,
                Record::Malformed { op_id, reason } => {
                    store::record_unreadable(pool, op_id, reason)
                        .await
                        .map_err(|e| e.to_string())?;
                    stats.needs_review += 1;
                }
                Record::Incoming(payment) => {
                    match store::handle_payment(pool, payment, accepted)
                        .await
                        .map_err(|e| e.to_string())?
                    {
                        store::Outcome::Credited(_) => stats.credited += 1,
                        store::Outcome::NeedsReview(_) => stats.needs_review += 1,
                        store::Outcome::AlreadyHandled => stats.already_handled += 1,
                    }
                }
            }
        }
        // Everything on the page is dealt with: only now is it safe to remember the position.
        if let Some(next) = &page.next_cursor {
            store::set_cursor(pool, &name, next)
                .await
                .map_err(|e| e.to_string())?;
            cursor = Some(next.clone());
        }
        if count < PAGE as usize {
            break;
        }
    }
    lock.commit().await.map_err(|e| e.to_string())?;
    Ok(stats)
}

/// Most addresses looked at in one round, and most pages read for one address.
const WALLETS_PER_ROUND: i64 = 25;
const PAGES_PER_WALLET: usize = 5;

pub fn wallet_cursor_name(address: &str) -> String {
    format!("deposits:{address}")
}

/// One round over the players' own addresses (ADR 0006): look at the ones that are due, credit what arrived, and decide
/// when to look again. Each address keeps its own position, so a late look misses nothing.
pub async fn poll_wallets_once(
    pool: &PgPool,
    chain: &dyn Chain,
    accepted: &[AssetId],
    timing: WatchTiming,
) -> Result<Stats, String> {
    let mut lock = pool.begin().await.map_err(|e| e.to_string())?;
    let got: bool = sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock(hashtext(current_database() || ':' || $1))",
    )
    .bind("stellar-watch-wallets")
    .fetch_one(&mut *lock)
    .await
    .map_err(|e| e.to_string())?;
    if !got {
        return Ok(Stats {
            skipped: true,
            ..Stats::default()
        });
    }
    let mut stats = Stats::default();
    for (user, address) in wallets::due_for_watch(pool, WALLETS_PER_ROUND)
        .await
        .map_err(|e| e.to_string())?
    {
        let account = address.to_g();
        let name = wallet_cursor_name(&account);
        let mut cursor = store::cursor(pool, &name)
            .await
            .map_err(|e| e.to_string())?;
        let before = stats.credited + stats.needs_review;
        for _ in 0..PAGES_PER_WALLET {
            let page = chain
                .payments(&account, cursor.as_deref(), PAGE)
                .await
                .map_err(|e| e.to_string())?;
            let count = page.records.len();
            for record in &page.records {
                match record {
                    Record::Ignored => stats.ignored += 1,
                    Record::Malformed { op_id, reason } => {
                        store::record_unreadable(pool, op_id, reason)
                            .await
                            .map_err(|e| e.to_string())?;
                        stats.needs_review += 1;
                    }
                    Record::Incoming(payment) => {
                        match store::handle_payment(pool, payment, accepted)
                            .await
                            .map_err(|e| e.to_string())?
                        {
                            store::Outcome::Credited(_) => stats.credited += 1,
                            store::Outcome::NeedsReview(_) => stats.needs_review += 1,
                            store::Outcome::AlreadyHandled => stats.already_handled += 1,
                        }
                    }
                }
            }
            if let Some(next) = &page.next_cursor {
                store::set_cursor(pool, &name, next)
                    .await
                    .map_err(|e| e.to_string())?;
                cursor = Some(next.clone());
            }
            if count < PAGE as usize {
                break;
            }
        }
        let found = stats.credited + stats.needs_review > before;
        wallets::record_watched(pool, user, found, timing)
            .await
            .map_err(|e| e.to_string())?;
    }
    lock.commit().await.map_err(|e| e.to_string())?;
    Ok(stats)
}

/// Starts the background watcher.
pub fn spawn_watcher(
    pool: PgPool,
    chain: Arc<dyn Chain>,
    config: StellarConfig,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let hot = config.hot_account.to_g();
        let accepted = vec![config.usdc.clone()];
        tracing::info!(
            "watching {hot} on the Stellar {} for USDC deposits",
            config.network
        );
        loop {
            let timing = WatchTiming {
                hot_secs: (config.poll_ms as f64 / 1000.0).max(1.0),
                ..WatchTiming::default()
            };
            match poll_wallets_once(&pool, chain.as_ref(), &accepted, timing).await {
                Ok(s) if s.credited + s.needs_review > 0 => tracing::info!(
                    "players' own addresses: {} credited, {} need review",
                    s.credited,
                    s.needs_review
                ),
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!("check of players' own addresses failed (will retry): {e}")
                }
            }
            match poll_once(&pool, chain.as_ref(), &hot, &accepted).await {
                Ok(s) if s.credited + s.needs_review > 0 => tracing::info!(
                    "stellar deposits: {} credited, {} need review",
                    s.credited,
                    s.needs_review
                ),
                Ok(_) => {}
                Err(e) => tracing::warn!("stellar deposit check failed (will retry): {e}"),
            }
            tokio::time::sleep(Duration::from_millis(config.poll_ms)).await;
        }
    })
}
