//! Sends approved withdrawals (ADR 0001, "Sending, and resolving `UNKNOWN`").
//!
//! One round ([`tick`]) does two things, in this order:
//!
//! 1. **Resolve** every unresolved attempt, by hash, never by guessing:
//!    - on chain and paid: the withdrawal is confirmed and settled;
//!    - on chain and failed (it paid nothing and was charged its fee): the withdrawal fails and the money is refunded;
//!    - not on chain, and the network has closed a ledger past the attempt's `max_time` (plus a margin for a lagging
//!      server): proved never to land, so it is marked expired, which is the only thing that allows a new attempt;
//!    - not on chain yet: the **same** envelope is submitted again (same hash, same sequence number: the network applies
//!      it at most once). A new transaction is never built while an old one could still land.
//! 2. **Send** the next withdrawal, only if nothing is in flight: the hot wallet has one sequence number, so payments go
//!    one at a time. The transaction is built, signed, and **recorded before it is submitted**, so a crash right after
//!    submitting leaves a hash to look up rather than a payment nobody knows about.
//!
//! A round holds a database lock, so several workers can run without two of them sending at once.
//!
//! **Where the money comes from (ADR 0006).** A player whose own account holds enough of the asset, and enough XLM to pay
//! the fee and keep its reserve, is paid **from their own account**, signed with their key through the vault. Otherwise
//! (money that arrived before they had an account of their own, or a player without one) it is paid from the platform's
//! hot wallet, as before. The ledger treats all custody as one pool, so either way it settles the same; the books-against-
//! chain check compares the total. Resolving an attempt works from its hash and does not care which account it came from.

use eschess_domain::stellar::{AccountKey, Address};
use eschess_stellar::signer::Signer;
use eschess_stellar::tx::{self, Network, Payment};
use eschess_stellar::{Chain, Submitted};
use eschess_storage::withdrawals::{self, AttemptStatus, NewAttempt, Status, Withdrawal};
use sqlx::PgPool;
use uuid::Uuid;

/// How the sender behaves. Times are in seconds.
#[derive(Debug, Clone)]
pub struct SenderConfig {
    pub network: Network,
    /// The account money is sent from. The signer must hold its key.
    pub hot_account: AccountKey,
    /// The fee offered per transaction, in stroops of XLM.
    pub fee_stroops: u32,
    /// How long a transaction may take to land. Short, so an unknown outcome is settled quickly.
    pub tx_timeout_secs: i64,
    /// How far past `max_time` the network's latest ledger must be before "not found" is believed. Covers a server
    /// behind a load balancer that has not caught up with the one that answered the ledger question.
    pub expiry_margin_secs: i64,
}

impl SenderConfig {
    /// Reads `WITHDRAW_FEE_STROOPS` (1000), `WITHDRAW_TX_TIMEOUT_SECS` (120) and `WITHDRAW_EXPIRY_MARGIN_SECS` (60). A
    /// value that is not a number, or is out of range, is an error so the worker does not start with a broken setting.
    pub fn from_lookup(
        get: &dyn Fn(&str) -> Option<String>,
        network: Network,
        hot_account: AccountKey,
    ) -> Result<SenderConfig, String> {
        fn num<T: std::str::FromStr>(
            get: &dyn Fn(&str) -> Option<String>,
            name: &str,
            default: T,
        ) -> Result<T, String> {
            match get(name).filter(|v| !v.trim().is_empty()) {
                None => Ok(default),
                Some(v) => v
                    .trim()
                    .parse()
                    .map_err(|_| format!("{name}: {v:?} is not a valid number")),
            }
        }
        let config = SenderConfig {
            network,
            hot_account,
            fee_stroops: num(get, "WITHDRAW_FEE_STROOPS", 1_000u32)?,
            tx_timeout_secs: num(get, "WITHDRAW_TX_TIMEOUT_SECS", 120i64)?,
            expiry_margin_secs: num(get, "WITHDRAW_EXPIRY_MARGIN_SECS", 60i64)?,
        };
        if config.fee_stroops < tx::MIN_FEE_STROOPS {
            return Err(format!(
                "WITHDRAW_FEE_STROOPS must be at least {}",
                tx::MIN_FEE_STROOPS
            ));
        }
        if !(10..=3_600).contains(&config.tx_timeout_secs) {
            return Err("WITHDRAW_TX_TIMEOUT_SECS must be from 10 to 3600".into());
        }
        if !(0..=3_600).contains(&config.expiry_margin_secs) {
            return Err("WITHDRAW_EXPIRY_MARGIN_SECS must be from 0 to 3600".into());
        }
        Ok(config)
    }
}

/// What one round did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TickReport {
    /// Another worker holds the lock; nothing was done.
    pub skipped: bool,
    pub confirmed: Vec<Uuid>,
    pub failed: Vec<Uuid>,
    /// Attempts proved never to land.
    pub expired: usize,
    /// Attempts that may still land, whose envelope was submitted again.
    pub still_waiting: usize,
    /// The withdrawal a new transaction was sent for.
    pub sent: Option<Uuid>,
}

/// Lets the sender pay from players' own accounts (ADR 0006): the vault that opens their keys.
#[derive(Clone)]
pub struct OwnAccounts {
    pub vault: std::sync::Arc<dyn eschess_stellar::custody::KeyVault>,
}

/// The least XLM a player's own account must hold to be paid from: its reserve (two base reserves and a trustline), and
/// room for the fee. Below this the platform's wallet pays instead, so the account is never driven under its reserve.
pub const OWN_ACCOUNT_MIN_XLM_STROOPS: i64 = 15_000_000;

/// One round: resolve what is in flight, then send the next withdrawal if nothing is. `now` is unix seconds. Pays only
/// from the hot wallet; see [`tick_with`] to pay from players' own accounts too.
pub async fn tick(
    pool: &PgPool,
    chain: &dyn Chain,
    signer: &dyn Signer,
    config: &SenderConfig,
    now: i64,
) -> Result<TickReport, String> {
    tick_with(pool, chain, signer, config, None, now).await
}

/// [`tick`], paying from a player's own account when it holds the money and `own` is given.
pub async fn tick_with(
    pool: &PgPool,
    chain: &dyn Chain,
    signer: &dyn Signer,
    config: &SenderConfig,
    own: Option<&OwnAccounts>,
    now: i64,
) -> Result<TickReport, String> {
    let mut lock = pool.begin().await.map_err(|e| e.to_string())?;
    // Advisory locks are shared by every database on the server, so the key names this database: rounds for
    // different databases (parallel test databases, say) must not make each other skip.
    let got: bool = sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock(hashtext(current_database() || ':' || $1))",
    )
    .bind(format!("withdrawal-sender:{}", config.hot_account.to_g()))
    .fetch_one(&mut *lock)
    .await
    .map_err(|e| e.to_string())?;
    if !got {
        return Ok(TickReport {
            skipped: true,
            ..TickReport::default()
        });
    }
    let mut report = TickReport::default();
    resolve(pool, chain, config, &mut report).await?;
    repair_failed(pool).await?;
    if report.still_waiting == 0 {
        if let Some(w) = next_to_send(pool).await? {
            send(pool, chain, signer, config, own, &w, now, &mut report).await?;
        }
    }
    lock.commit().await.map_err(|e| e.to_string())?;
    Ok(report)
}

async fn resolve(
    pool: &PgPool,
    chain: &dyn Chain,
    config: &SenderConfig,
    report: &mut TickReport,
) -> Result<(), String> {
    let pending = withdrawals::pending_attempts(pool, 100)
        .await
        .map_err(|e| e.to_string())?;
    if pending.is_empty() {
        return Ok(());
    }
    // Read the ledger time *before* looking the hashes up: a transaction in a ledger closed before this time is already
    // visible to the lookups that follow.
    let closed = chain
        .latest_ledger_close_time()
        .await
        .map_err(|e| e.to_string())?;
    for a in pending {
        match chain
            .transaction(&a.tx_hash)
            .await
            .map_err(|e| e.to_string())?
        {
            Some(outcome) if outcome.successful => {
                withdrawals::attempt_landed(pool, a.id, outcome.fee_charged)
                    .await
                    .map_err(|e| e.to_string())?;
                tracing::info!(withdrawal = %a.withdrawal_id, tx = %a.tx_hash, "withdrawal paid");
                report.confirmed.push(a.withdrawal_id);
            }
            Some(outcome) => {
                withdrawals::attempt_failed(pool, a.id, &outcome.result_code, outcome.fee_charged)
                    .await
                    .map_err(|e| e.to_string())?;
                fail(pool, a.withdrawal_id, &outcome.result_code, report).await?;
            }
            None if closed > a.max_time + config.expiry_margin_secs => {
                withdrawals::attempt_expired(pool, a.id)
                    .await
                    .map_err(|e| e.to_string())?;
                tracing::warn!(withdrawal = %a.withdrawal_id, tx = %a.tx_hash, "withdrawal attempt expired unsent; a new one will be built");
                report.expired += 1;
            }
            None => {
                match chain.submit(&a.envelope_xdr).await {
                    Ok(Submitted::Accepted) => {}
                    Ok(Submitted::Refused(code)) => {
                        tracing::info!(tx = %a.tx_hash, "re-submission refused ({code}); waiting for it to land or expire")
                    }
                    Err(e) => tracing::info!(tx = %a.tx_hash, "re-submission: {e}"),
                }
                report.still_waiting += 1;
            }
        }
    }
    Ok(())
}

/// A payment that landed and failed paid nothing: refund the player, and say why.
async fn fail(pool: &PgPool, id: Uuid, code: &str, report: &mut TickReport) -> Result<(), String> {
    let reason = match code {
        c if c.ends_with("payment_no_trust") => {
            "the destination account cannot receive USDC (it has no trustline)".to_string()
        }
        c if c.ends_with("payment_no_destination") || c.ends_with("op_no_account") => {
            "the destination account does not exist".to_string()
        }
        c if c.ends_with("payment_not_authorized") => {
            "the destination is not authorized to hold USDC".to_string()
        }
        c if c.ends_with("payment_line_full") => {
            "the destination cannot hold that much USDC".to_string()
        }
        c if c.ends_with("payment_underfunded") => {
            tracing::error!(withdrawal = %id, "the hot wallet does not hold enough to pay a withdrawal");
            "the payment could not be made from the eSchess wallet; please try again later"
                .to_string()
        }
        c => format!("the payment failed on the network ({c})"),
    };
    withdrawals::give_up(pool, id, &reason)
        .await
        .map_err(|e| e.to_string())?;
    tracing::warn!(withdrawal = %id, "withdrawal failed and was refunded: {code}");
    report.failed.push(id);
    Ok(())
}

/// The next withdrawal that needs a transaction: one already being sent whose attempts have all expired (or one whose
/// failure was recorded but not yet refunded, after a crash), else the oldest approved one whose owner has no open fair-play case.
async fn next_to_send(pool: &PgPool) -> Result<Option<Withdrawal>, String> {
    for w in withdrawals::with_status(pool, Status::Submitting, 20)
        .await
        .map_err(|e| e.to_string())?
    {
        let attempts = withdrawals::attempts(pool, w.id)
            .await
            .map_err(|e| e.to_string())?;
        if attempts.iter().any(|a| {
            matches!(
                a.status,
                AttemptStatus::Pending | AttemptStatus::Success | AttemptStatus::Failed
            )
        }) {
            // In flight, done, or failed: a failure is finished by `repair_failed`, not retried.
            continue;
        }
        return Ok(Some(w));
    }
    // A player with an open fair-play case is skipped, not rejected (ADR 0003 §2).
    withdrawals::next_approved_not_held(pool)
        .await
        .map_err(|e| e.to_string())
}

/// Finishes a withdrawal whose attempt failed on chain but which a crash left unrefunded.
pub async fn repair_failed(pool: &PgPool) -> Result<usize, String> {
    let mut repaired = 0;
    for w in withdrawals::with_status(pool, Status::Submitting, 100)
        .await
        .map_err(|e| e.to_string())?
    {
        let attempts = withdrawals::attempts(pool, w.id)
            .await
            .map_err(|e| e.to_string())?;
        let open = attempts
            .iter()
            .any(|a| matches!(a.status, AttemptStatus::Pending | AttemptStatus::Success));
        if let (false, Some(failed)) = (
            open,
            attempts.iter().find(|a| a.status == AttemptStatus::Failed),
        ) {
            let code = failed.result_code.clone().unwrap_or_default();
            fail(pool, w.id, &code, &mut TickReport::default()).await?;
            repaired += 1;
        }
    }
    Ok(repaired)
}

/// Where a withdrawal is paid from.
enum Source {
    Hot,
    /// The player's own account, with the signer that opens their key.
    Own(eschess_stellar::custody::VaultSigner),
}

/// Prefers the player's own account when it is set up, holds at least the amount, and has the XLM to pay the fee and keep
/// its reserve; otherwise the hot wallet.
async fn choose_source(
    pool: &PgPool,
    chain: &dyn Chain,
    config: &SenderConfig,
    own: Option<&OwnAccounts>,
    w: &Withdrawal,
) -> Result<Source, String> {
    let Some(own) = own else {
        return Ok(Source::Hot);
    };
    let ready = eschess_storage::wallets::setup_state(pool, w.user_id)
        .await
        .map_err(|e| e.to_string())?
        == eschess_storage::wallets::SetupState::Ready;
    if !ready {
        return Ok(Source::Hot);
    }
    let Some(signer) = eschess_storage::wallets::signer_for(pool, own.vault.clone(), w.user_id)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(Source::Hot);
    };
    let Some(state) = chain
        .account(&signer.public_key().to_g())
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(Source::Hot);
    };
    let enough_money = state.balance_of(w.amount.asset()) >= w.amount.minor();
    let enough_xlm = state.balance_of(&eschess_domain::money::AssetId::native())
        >= OWN_ACCOUNT_MIN_XLM_STROOPS + config.fee_stroops as i64;
    Ok(if enough_money && enough_xlm {
        Source::Own(signer)
    } else {
        Source::Hot
    })
}

#[allow(clippy::too_many_arguments)]
async fn send(
    pool: &PgPool,
    chain: &dyn Chain,
    signer: &dyn Signer,
    config: &SenderConfig,
    own: Option<&OwnAccounts>,
    w: &Withdrawal,
    now: i64,
    report: &mut TickReport,
) -> Result<(), String> {
    let destination = Address::parse(&w.destination)
        .map_err(|e| format!("withdrawal {}: stored destination unreadable: {e}", w.id))?;
    let source = choose_source(pool, chain, config, own, w).await?;
    let (signer, source_key): (&dyn Signer, AccountKey) = match &source {
        Source::Hot => (signer, config.hot_account),
        Source::Own(s) => (s, s.public_key()),
    };
    let hot = source_key.to_g();
    let account = chain
        .account(&hot)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("the account {hot} we pay from does not exist on the network"))?;
    let payment = Payment {
        source: source_key,
        sequence: account.sequence + 1,
        fee_stroops: config.fee_stroops,
        min_time: 0,
        max_time: (now + config.tx_timeout_secs) as u64,
        destination,
        amount: w.amount.clone(),
        memo: w.memo.clone(),
    };
    let signed = tx::sign(&payment, config.network, signer)
        .await
        .map_err(|e| format!("withdrawal {}: {e}", w.id))?;
    let attempt = withdrawals::record_attempt(
        pool,
        w.id,
        &NewAttempt {
            tx_hash: signed.hash_hex(),
            source_account: hot,
            sequence: payment.sequence,
            fee_stroops: payment.fee_stroops as i64,
            max_time: payment.max_time as i64,
            envelope_xdr: signed.envelope_xdr.clone(),
        },
    )
    .await
    .map_err(|e| e.to_string())?;
    report.sent = Some(w.id);
    tracing::info!(withdrawal = %w.id, tx = %attempt.tx_hash, attempt = attempt.attempt_no, "sending withdrawal");

    // Recorded: now it may be submitted. Whatever the answer, the outcome is decided by looking the hash up.
    match chain.submit(&signed.envelope_xdr).await {
        Ok(Submitted::Accepted) => {
            if let Ok(Some(outcome)) = chain.transaction(&attempt.tx_hash).await {
                if outcome.successful {
                    withdrawals::attempt_landed(pool, attempt.id, outcome.fee_charged)
                        .await
                        .map_err(|e| e.to_string())?;
                    report.confirmed.push(w.id);
                    tracing::info!(withdrawal = %w.id, tx = %attempt.tx_hash, "withdrawal paid");
                }
            }
        }
        Ok(Submitted::Refused(code)) => {
            tracing::info!(withdrawal = %w.id, "submission refused ({code}); it will be looked up by hash")
        }
        Err(e) => {
            tracing::info!(withdrawal = %w.id, "submission outcome unknown ({e}); it will be looked up by hash")
        }
    }
    Ok(())
}
