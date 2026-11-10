//! Sets users' own Stellar accounts up on the network (ADR 0006): create, fund, and add the USDC trustline.
//!
//! The chain is asked every round what state each account is in, so the job can be stopped and started, run on several
//! workers, or repeated without harm:
//!   - the account already holds USDC: it is ready;
//!   - it does not exist: create it from the funding account and add the trustline, in one transaction;
//!   - it exists without the trustline: add the trustline, paid for by the account's own XLM.
//!
//! A transaction is recorded before it is sent. While the last one could still land, the job waits for it instead of
//! building another (the rule from ADR 0001).

use std::sync::Arc;

use eschess_domain::money::AssetId;
use eschess_stellar::account_setup::{self, STROOPS_PER_XLM};
use eschess_stellar::custody::KeyVault;
use eschess_stellar::signer::Signer;
use eschess_stellar::tx::Network;
use eschess_stellar::{Chain, Submitted};
use eschess_storage::wallets::{self, PendingSetup};
use sqlx::PgPool;

#[derive(Debug, Clone)]
pub struct SetupConfig {
    pub network: Network,
    pub usdc: AssetId,
    /// XLM to give a new account, in stroops: its reserves and fees.
    pub starting_balance_stroops: i64,
    pub batch: i64,
}

/// `WALLET_FUNDING_XLM`: how much XLM each new account starts with. Default 2; at least 1.5.
pub fn funding_stroops(get: &dyn Fn(&str) -> Option<String>) -> Result<i64, String> {
    let Some(text) = get("WALLET_FUNDING_XLM")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    else {
        return Ok(2 * STROOPS_PER_XLM);
    };
    let xlm: f64 = text
        .parse()
        .map_err(|_| format!("WALLET_FUNDING_XLM: {text:?} is not a number"))?;
    let stroops = (xlm * STROOPS_PER_XLM as f64).round() as i64;
    if !(account_setup::MIN_STARTING_BALANCE_STROOPS..=100 * STROOPS_PER_XLM).contains(&stroops) {
        return Err(format!("WALLET_FUNDING_XLM must be 1.5 to 100, not {text}"));
    }
    Ok(stroops)
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SetupReport {
    pub skipped: bool,
    pub ready: usize,
    pub sent: usize,
    /// Waiting for a transaction already sent.
    pub waiting: usize,
    pub failed: usize,
}

pub async fn tick(
    pool: &PgPool,
    chain: &dyn Chain,
    vault: Arc<dyn KeyVault>,
    funder: &dyn Signer,
    config: &SetupConfig,
) -> Result<SetupReport, String> {
    let mut lock = pool.begin().await.map_err(|e| e.to_string())?;
    let got: bool = sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock(hashtext(current_database() || ':' || $1))",
    )
    .bind("wallet-setup")
    .fetch_one(&mut *lock)
    .await
    .map_err(|e| e.to_string())?;
    if !got {
        return Ok(SetupReport {
            skipped: true,
            ..SetupReport::default()
        });
    }
    let mut report = SetupReport::default();
    for p in wallets::pending_setups(pool, config.batch)
        .await
        .map_err(|e| e.to_string())?
    {
        match one(pool, chain, &vault, funder, config, &p).await {
            Ok(Step::Ready) => report.ready += 1,
            Ok(Step::Sent) => report.sent += 1,
            Ok(Step::Waiting) => report.waiting += 1,
            Err(e) => {
                tracing::warn!(user = %p.user_id, "wallet setup failed (will retry): {e}");
                let _ = wallets::record_setup_error(pool, p.user_id, &e).await;
                report.failed += 1;
            }
        }
    }
    lock.commit().await.map_err(|e| e.to_string())?;
    Ok(report)
}

enum Step {
    Ready,
    Sent,
    Waiting,
}

async fn one(
    pool: &PgPool,
    chain: &dyn Chain,
    vault: &Arc<dyn KeyVault>,
    funder: &dyn Signer,
    config: &SetupConfig,
    p: &PendingSetup,
) -> Result<Step, String> {
    let address = p.address.to_g();
    let state = chain.account(&address).await.map_err(|e| e.to_string())?;
    if state.as_ref().is_some_and(|a| a.can_receive(&config.usdc)) {
        wallets::mark_setup_ready(pool, p.user_id)
            .await
            .map_err(|e| e.to_string())?;
        return Ok(Step::Ready);
    }
    let now = chain
        .latest_ledger_close_time()
        .await
        .map_err(|e| e.to_string())?;
    // A transaction sent earlier may still land: wait for it. Once the network is past its expiry it never can.
    if let Some((hash, max_time)) = &p.last_tx {
        if now <= *max_time {
            return Ok(Step::Waiting);
        }
        // Past its expiry: if it landed, the account state above would show it (and a later round will); if not, it is
        // dead and a new one may be built. One last look by hash covers a landing between the two reads.
        if chain
            .transaction(hash)
            .await
            .map_err(|e| e.to_string())?
            .is_some()
        {
            return Ok(Step::Waiting);
        }
    }
    let user_signer = wallets::signer_for(pool, vault.clone(), p.user_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("the user has no wallet key")?;
    let max_time = (now + 120) as u64;
    let signed = match state {
        None => {
            let funder_key = funder.public_key();
            let funder_account = chain
                .account(&funder_key.to_g())
                .await
                .map_err(|e| e.to_string())?
                .ok_or("the funding account does not exist on the network")?;
            let tx = account_setup::create_and_trust(
                &funder_key,
                funder_account.sequence + 1,
                &p.address,
                config.starting_balance_stroops,
                &config.usdc,
                max_time,
            )
            .map_err(|e| e.to_string())?;
            account_setup::sign(tx, config.network, &[funder, &user_signer]).await
        }
        Some(account) => {
            let tx =
                account_setup::trust_only(&p.address, account.sequence + 1, &config.usdc, max_time)
                    .map_err(|e| e.to_string())?;
            account_setup::sign(tx, config.network, &[&user_signer]).await
        }
    }
    .map_err(|e| e.to_string())?;
    // Recorded before it is sent: if we die now, the next round looks for this hash instead of building another.
    wallets::record_setup_tx(pool, p.user_id, &signed.hash_hex(), max_time as i64)
        .await
        .map_err(|e| e.to_string())?;
    match chain
        .submit(&signed.envelope_xdr)
        .await
        .map_err(|e| e.to_string())?
    {
        Submitted::Accepted => {}
        Submitted::Refused(why) => return Err(format!("the network refused it: {why}")),
    }
    tracing::info!(user = %p.user_id, tx = %signed.hash_hex(), "set up a player's wallet account");
    Ok(Step::Sent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_funding_amount_defaults_to_two_xlm_and_refuses_too_little_or_too_much() {
        let get = |v: Option<&'static str>| move |_: &str| v.map(String::from);
        assert_eq!(funding_stroops(&get(None)), Ok(20_000_000));
        assert_eq!(funding_stroops(&get(Some("3.5"))), Ok(35_000_000));
        for bad in ["1.4", "101", "lots", "-2"] {
            assert!(funding_stroops(&get(Some(bad))).is_err(), "{bad}");
        }
    }
}
