//! Settles finished staked games through the escrow contract (ADR 0007 §3).
//!
//! The rules for who gets the pot are the ones in ADR 0004 §5, unchanged and shared with the ledger path: wait until the
//! fair-play scorer has looked at the game and no case is open, a confirmed cheat pays the opponent, a draw or a very short
//! game returns the stakes. What differs is the money. For each finished game on the contract path, each round:
//!   1. reads the match from the network and does only the next missing thing;
//!   2. if it is still open and the rules allow, tells the contract the outcome (the contract pays); the outcome is recorded
//!      with the transaction, before it is sent;
//!   3. **only once the network shows the match closed**, posts the same pot in the ledger and marks the game settled. The
//!      ledger therefore never says "paid" before the network has paid, and it follows what the network was told, not a fresh
//!      decision.
//!
//! A match the network shows refunded (expired, or cancelled) returns both stakes in the ledger. The contract has no fee,
//! so a pot is always whole.

use eschess_stellar::escrow::{MatchStatus, Outcome};
use eschess_stellar::soroban::Resolved;
use eschess_storage::contract::{self, Step};
use eschess_storage::stakes::{self, ChainOutcome, Waiting};
use uuid::Uuid;

use crate::funding::{landed, read_match, run_call, FundingConfig, Note, Parts};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ContractSettleReport {
    pub skipped: bool,
    /// Games whose ledger move was posted this round (the network had paid).
    pub settled: usize,
    /// Settle transactions sent and landed this round.
    pub sent: usize,
    /// Left for later: fair play has not finished, a case is open, a transaction has not landed, or payments are halted.
    pub waiting: usize,
    pub failed: usize,
}

pub async fn tick(
    parts: &Parts<'_>,
    config: &FundingConfig,
    scorer: &str,
    batch: i64,
) -> Result<ContractSettleReport, String> {
    let mut lock = parts.pool.begin().await.map_err(|e| e.to_string())?;
    let got: bool = sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock(hashtext(current_database() || ':' || $1))",
    )
    .bind("contract-settle")
    .fetch_one(&mut *lock)
    .await
    .map_err(|e| e.to_string())?;
    if !got {
        return Ok(ContractSettleReport {
            skipped: true,
            ..ContractSettleReport::default()
        });
    }
    let mut report = ContractSettleReport::default();
    for game in stakes::contract_unsettled(parts.pool, batch)
        .await
        .map_err(|e| e.to_string())?
    {
        // Two steps at most per game per round: tell the contract, then post the ledger move.
        for _ in 0..2 {
            match settle_one(parts, config, scorer, game).await {
                Ok(Next::Sent) => report.sent += 1,
                Ok(Next::Settled) => {
                    report.settled += 1;
                    break;
                }
                Ok(Next::Waiting) => {
                    report.waiting += 1;
                    break;
                }
                Err(e) => {
                    tracing::error!(%game, "settling a staked game through the contract failed (will retry): {e}");
                    report.failed += 1;
                    break;
                }
            }
        }
    }
    lock.commit().await.map_err(|e| e.to_string())?;
    Ok(report)
}

enum Next {
    Sent,
    Settled,
    Waiting,
}

fn escrow_outcome(o: ChainOutcome) -> Outcome {
    match o {
        ChainOutcome::WinWhite => Outcome::WinA,
        ChainOutcome::WinBlack => Outcome::WinB,
        ChainOutcome::Draw => Outcome::Draw,
    }
}

async fn settle_one(
    parts: &Parts<'_>,
    config: &FundingConfig,
    scorer: &str,
    game: Uuid,
) -> Result<Next, String> {
    let now = parts
        .chain
        .latest_ledger_close_time()
        .await
        .map_err(|e| e.to_string())?;
    // A transaction sent earlier may still land: wait for it, never build another beside it.
    if let Some(last) = contract::latest_tx(parts.pool, game)
        .await
        .map_err(|e| e.to_string())?
    {
        if now <= last.max_time && !landed(parts, &last.hash_hex, last.step).await? {
            return Ok(Next::Waiting);
        }
    }
    let state = read_match(parts, config, game)
        .await?
        .ok_or("a finished staked game has no match on the network")?;
    match state.status {
        // The network has paid. The ledger follows what the network was told.
        MatchStatus::Settled => {
            let outcome = contract::settle_outcome(parts.pool, game)
                .await
                .map_err(|e| e.to_string())?
                .ok_or("the match is settled on the network but no outcome was recorded for it: a person must look")?;
            post(parts, game, outcome).await
        }
        // Refunded (it expired, or was cancelled): each player has their own stake back.
        MatchStatus::Cancelled => post(parts, game, ChainOutcome::Draw).await,
        MatchStatus::Open => {
            let verdict = match stakes::contract_verdict(parts.pool, game, scorer)
                .await
                .map_err(|e| e.to_string())?
            {
                Ok(v) => v,
                Err(Waiting::NothingToDo) => return Ok(Next::Waiting),
                Err(_) => return Ok(Next::Waiting),
            };
            // The decision above was made just now; the call below is signed from it and nothing else.
            let call = config
                .escrow
                .settle(*game.as_bytes(), escrow_outcome(verdict.outcome));
            let r = run_call(
                parts,
                config,
                game,
                Note {
                    step: Step::Settle,
                    player: None,
                    detail: Some(verdict.outcome.as_str()),
                },
                parts.admin,
                &call,
            )
            .await?;
            match r {
                Resolved::Success { .. } => {
                    tracing::info!(%game, outcome = verdict.outcome.as_str(), "the contract was told the outcome");
                    Ok(Next::Sent)
                }
                Resolved::Unresolved => Ok(Next::Waiting),
                Resolved::Failed(why) => {
                    Err(format!("settling the match failed on the network: {why}"))
                }
            }
        }
    }
}

async fn post(parts: &Parts<'_>, game: Uuid, outcome: ChainOutcome) -> Result<Next, String> {
    match stakes::record_chain_settlement(parts.pool, game, outcome)
        .await
        .map_err(|e| e.to_string())?
    {
        Some(settled) => {
            tracing::info!(%game, ?settled, "staked game settled: the network has paid and the ledger agrees");
            Ok(Next::Settled)
        }
        // Someone else posted it a moment ago.
        None => Ok(Next::Waiting),
    }
}
