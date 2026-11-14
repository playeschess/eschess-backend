//! Fiat in and out through a Stellar anchor (SEP-24): follows each open session at the anchor, and makes a cash-out's USDC
//! payment. See `bins/api/src/routes/fiat.rs` for how a session starts.
//!
//! A **deposit** needs nothing from us once the player is in the anchor's screens: the anchor sends USDC to the player's own
//! address and the deposit watcher credits it. We only follow the anchor's status, to show it.
//!
//! A **cash-out** is the anchor telling us (status `pending_user_transfer_start`) how much to send, where and with which memo.
//! We send it as an ordinary withdrawal from the player's own account, so every safeguard applies (limits, approval of large
//! amounts, the fair-play hold, the books check). Before that we check what the anchor said, because it moves money:
//!   * the account is one the anchor's own `stellar.toml` lists, and is neither our wallet nor the USDC issuer;
//!   * the memo is one we can make, and the amount is a positive USDC amount.
//!
//! Anything else fails the session with the reason, and nothing is sent. The withdrawal is keyed by the session, so a retry
//! can never make a second payment.

use std::collections::HashMap;
use std::sync::Arc;

use eschess_domain::money::{Amount, AssetId};
use eschess_domain::stellar::AccountKey;
use eschess_stellar::custody::KeyVault;
use eschess_stellar::sep::{Anchor, AnchorStatus, AnchorToken, AnchorTx, SepError};
use eschess_stellar::tx::{PaymentMemo, MAX_TEXT_MEMO_BYTES};
use eschess_storage::fiat::{self, Kind, Session, Status};
use eschess_storage::wallets;
use eschess_storage::withdrawals::{self, NewWithdrawal, Policy, WithdrawalError};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct Config {
    pub usdc: AssetId,
    /// Our own wallet, which a cash-out may never be sent to.
    pub hot_account: AccountKey,
    pub policy: Policy,
    /// The smallest withdrawal the platform sends.
    pub min_minor: i64,
    /// Sessions the player never finished are given up after this many hours.
    pub stale_hours: i32,
    /// Sessions looked at per round.
    pub batch: i64,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub skipped: bool,
    pub checked: usize,
    pub withdrawals_made: usize,
    pub completed: usize,
    pub failed: usize,
    pub expired: u64,
}

/// What a cash-out's request from the anchor means for us, or why we will not do it.
fn cash_out_request(
    tx: &AnchorTx,
    anchor_accounts: &[AccountKey],
    cfg: &Config,
) -> Result<(AccountKey, Amount, PaymentMemo), String> {
    let account = tx
        .withdraw_anchor_account
        .as_deref()
        .ok_or("the partner gave no account to send to")?;
    let account =
        AccountKey::parse(account).map_err(|_| "the partner's account is not a Stellar account")?;
    if anchor_accounts.is_empty() {
        return Err(
            "the partner lists no accounts of its own, so we cannot check where to send".into(),
        );
    }
    if !anchor_accounts.contains(&account) {
        return Err("the account is not one the partner lists as its own".into());
    }
    if account == cfg.hot_account {
        return Err("the account is our own wallet".into());
    }
    if cfg.usdc.issuer().and_then(|i| AccountKey::parse(i).ok()) == Some(account) {
        return Err("the account is the USDC issuer".into());
    }
    let amount = tx
        .amount_in
        .as_deref()
        .ok_or("the partner did not say how much to send")?;
    let amount = Amount::from_decimal_str(amount.trim(), cfg.usdc.clone())
        .map_err(|_| "the amount is not a USDC amount")?;
    if amount.minor() <= 0 {
        return Err("the amount is not more than zero".into());
    }
    let memo_value = tx
        .withdraw_memo
        .as_deref()
        .ok_or("the partner gave no memo")?;
    let memo = match tx.withdraw_memo_type.as_deref().unwrap_or("text") {
        "id" => PaymentMemo::Id(
            memo_value
                .parse()
                .map_err(|_| "the id memo is not a number")?,
        ),
        "text" if memo_value.len() <= MAX_TEXT_MEMO_BYTES => {
            PaymentMemo::Text(memo_value.to_string())
        }
        "text" => return Err("the text memo is too long".into()),
        _ => return Err("the memo type is not one we send".into()),
    };
    Ok((account, amount, memo))
}

pub async fn tick(
    pool: &PgPool,
    anchor: &dyn Anchor,
    vault: Arc<dyn KeyVault>,
    cfg: &Config,
) -> Result<Report, String> {
    let mut lock = pool.begin().await.map_err(|e| e.to_string())?;
    let got: bool = sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock(hashtext(current_database() || ':' || $1))",
    )
    .bind("fiat-sessions")
    .fetch_one(&mut *lock)
    .await
    .map_err(|e| e.to_string())?;
    if !got {
        return Ok(Report {
            skipped: true,
            ..Report::default()
        });
    }
    let mut report = Report::default();
    let mut tokens: HashMap<Uuid, Option<AnchorToken>> = HashMap::new();
    let anchor_accounts = anchor.accounts().await.unwrap_or_default();

    for s in fiat::open(pool, cfg.batch)
        .await
        .map_err(|e| e.to_string())?
    {
        report.checked += 1;
        // One login per player per round.
        if let std::collections::hash_map::Entry::Vacant(slot) = tokens.entry(s.user_id) {
            let signer = match wallets::signer_for(pool, vault.clone(), s.user_id).await {
                Ok(Some(signer)) => signer,
                _ => {
                    finish(
                        pool,
                        &s,
                        Status::Failed,
                        "the player has no wallet key",
                        &mut report,
                    )
                    .await;
                    continue;
                }
            };
            let token = match anchor.authenticate(&signer).await {
                Ok(t) => Some(t),
                Err(e) => {
                    tracing::warn!(session = %s.id, "logging in to the anchor failed (will retry): {e}");
                    None
                }
            };
            slot.insert(token);
        }
        let Some(Some(token)) = tokens.get(&s.user_id) else {
            let _ = fiat::touch(pool, s.id).await;
            continue;
        };
        let tx = match anchor.transaction(token, &s.anchor_tx_id).await {
            Ok(tx) => tx,
            Err(SepError::Unavailable(m)) => {
                tracing::warn!(session = %s.id, "reading the anchor's transaction failed (will retry): {m}");
                let _ = fiat::touch(pool, s.id).await;
                continue;
            }
            Err(e) => {
                tracing::warn!(session = %s.id, "the anchor would not tell us about the transaction (will retry): {e}");
                let _ = fiat::touch(pool, s.id).await;
                continue;
            }
        };
        follow(pool, &s, &tx, &anchor_accounts, cfg, &mut report).await;
    }
    report.expired = fiat::expire_stale(pool, cfg.stale_hours)
        .await
        .map_err(|e| e.to_string())?;
    lock.commit().await.map_err(|e| e.to_string())?;
    Ok(report)
}

async fn finish(pool: &PgPool, s: &Session, status: Status, message: &str, report: &mut Report) {
    let anchor_status = s.anchor_status.clone().unwrap_or_default();
    if let Ok(true) = fiat::record(
        pool,
        s.id,
        status,
        &anchor_status,
        None,
        Some(message),
        None,
    )
    .await
    {
        match status {
            Status::Completed => report.completed += 1,
            Status::Failed => report.failed += 1,
            _ => {}
        }
    }
}

async fn follow(
    pool: &PgPool,
    s: &Session,
    tx: &AnchorTx,
    anchor_accounts: &[AccountKey],
    cfg: &Config,
    report: &mut Report,
) {
    let anchor_status = format!("{:?}", tx.status).to_lowercase();
    let message = tx.message.as_deref();
    let more = tx.more_info_url.as_deref();

    match &tx.status {
        AnchorStatus::Completed => {
            if rec(
                pool,
                s,
                Status::Completed,
                &anchor_status,
                more,
                None,
                message,
            )
            .await
            {
                report.completed += 1;
            }
        }
        AnchorStatus::Refunded
        | AnchorStatus::Expired
        | AnchorStatus::NoMarket
        | AnchorStatus::TooSmall
        | AnchorStatus::TooLarge
        | AnchorStatus::Error => {
            // Nothing more will come of it. A cash-out whose payment is still waiting is given back to the player.
            if let Some(w) = s.withdrawal_id {
                let _ = withdrawals::cancel(pool, w, s.user_id).await;
            }
            let why = message.unwrap_or(match tx.status {
                AnchorStatus::Refunded => "the partner refunded it",
                AnchorStatus::Expired => "the partner let it expire",
                AnchorStatus::TooSmall => "the amount is too small",
                AnchorStatus::TooLarge => "the amount is too large",
                AnchorStatus::NoMarket => "the partner has no market for that",
                _ => "the partner reported an error",
            });
            if rec(
                pool,
                s,
                Status::Failed,
                &anchor_status,
                more,
                None,
                Some(why),
            )
            .await
            {
                report.failed += 1;
            }
        }
        AnchorStatus::PendingUserTransferStart
            if s.kind == Kind::Withdraw && s.withdrawal_id.is_none() =>
        {
            cash_out(pool, s, tx, anchor_accounts, cfg, report, &anchor_status).await;
        }
        _ => {
            let status = if s.withdrawal_id.is_some() {
                Status::Sending
            } else {
                Status::InProgress
            };
            // A cash-out's withdrawal that failed or was rejected will never reach the anchor: say so.
            if let Some(w) = s.withdrawal_id {
                if let Ok(wd) = withdrawals::get(pool, w).await {
                    if matches!(
                        wd.status,
                        withdrawals::Status::Failed
                            | withdrawals::Status::Rejected
                            | withdrawals::Status::Cancelled
                    ) {
                        if rec(pool, s, Status::Failed, &anchor_status, more, None, Some("the payment to our partner did not go through; your money is back in your wallet")).await {
                            report.failed += 1;
                        }
                        return;
                    }
                }
            }
            let _ = rec(pool, s, status, &anchor_status, more, None, message).await;
        }
    }
}

/// Records what the anchor said; `true` if this call changed the session.
async fn rec(
    pool: &PgPool,
    s: &Session,
    status: Status,
    anchor_status: &str,
    more: Option<&str>,
    amount: Option<i64>,
    message: Option<&str>,
) -> bool {
    matches!(
        fiat::record(pool, s.id, status, anchor_status, amount, message, more).await,
        Ok(true)
    )
}

async fn cash_out(
    pool: &PgPool,
    s: &Session,
    tx: &AnchorTx,
    anchor_accounts: &[AccountKey],
    cfg: &Config,
    report: &mut Report,
    anchor_status: &str,
) {
    let (account, amount, memo) = match cash_out_request(tx, anchor_accounts, cfg) {
        Ok(v) => v,
        Err(why) => {
            tracing::error!(session = %s.id, "refusing the partner's cash-out request: {why}");
            let _ = fiat::record(
                pool,
                s.id,
                Status::Failed,
                anchor_status,
                None,
                Some(&format!("could not be sent: {why}")),
                None,
            )
            .await;
            report.failed += 1;
            return;
        }
    };
    if amount.minor() < cfg.min_minor {
        let _ = fiat::record(
            pool,
            s.id,
            Status::Failed,
            anchor_status,
            None,
            Some("below the smallest cash-out we send"),
            None,
        )
        .await;
        report.failed += 1;
        return;
    }
    let new = NewWithdrawal {
        user_id: s.user_id,
        // The session is the request: a retry finds the first withdrawal and never makes a second.
        client_request_id: s.id,
        amount: amount.clone(),
        destination: account.to_g(),
        memo,
    };
    match withdrawals::request(pool, &new, cfg.policy).await {
        Ok((w, _)) => {
            if let Ok(true) = fiat::attach_withdrawal(pool, s.id, w.id).await {
                report.withdrawals_made += 1;
                tracing::info!(session = %s.id, withdrawal = %w.id, "cash-out sent to the anchor's withdrawal path");
            }
            let _ = fiat::record(
                pool,
                s.id,
                Status::Sending,
                anchor_status,
                Some(amount.minor()),
                None,
                None,
            )
            .await;
        }
        // Not enough in the wallet right now, or the day's limit is reached: the anchor keeps waiting, so we keep trying.
        Err(e @ (WithdrawalError::InsufficientFunds | WithdrawalError::DailyLimit { .. })) => {
            let why = match e {
                WithdrawalError::InsufficientFunds => {
                    "waiting for enough money in your wallet to cover the cash-out"
                }
                _ => "waiting: you have reached today's withdrawal limit",
            };
            let _ = fiat::record(
                pool,
                s.id,
                Status::InProgress,
                anchor_status,
                Some(amount.minor()),
                Some(why),
                None,
            )
            .await;
        }
        Err(e) => {
            tracing::error!(session = %s.id, "making the cash-out withdrawal failed (will retry): {e}")
        }
    }
}
