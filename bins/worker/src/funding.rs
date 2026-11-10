//! Locks the stakes of staked games in the escrow contract (ADR 0007): the job behind the `funding` state.
//!
//! A game made on the contract path waits in `funding` with its ledger holds in place. Each round this job looks at what
//! the network says about the game's match and does the next missing thing, so it can be stopped, restarted, or run on
//! several workers without repeating a step the network already shows done:
//!   1. the match does not exist: the admin key creates it (players `A` = White and `B` = Black, the token, the stake);
//!   2. a player's account holds less than the stake: the platform wallet pays the shortfall into it;
//!   3. a player has not deposited: their own key signs the deposit;
//!   4. both have deposited: the game becomes `active` and White's clock starts.
//!
//! A game that cannot be funded within the time limit is cancelled: the match is cancelled on the network (which returns
//! any deposit) and both ledger holds are released. Every transaction is recorded before it is sent, and while one could
//! still land the job waits for it instead of building another (ADR 0001's rule).

use std::sync::Arc;
use std::time::Duration;

use eschess_domain::money::{Amount, AssetId};
use eschess_domain::stellar::{AccountKey, Address, ContractId};
use eschess_stellar::custody::KeyVault;
use eschess_stellar::escrow::{
    contract_error, parse_match, Escrow, EscrowError, MatchState, MatchStatus,
};
use eschess_stellar::signer::Signer;
use eschess_stellar::soroban::{
    prepare, send_and_wait, simulate, ContractCall, Resolved, RpcSend, SorobanError,
};
use eschess_stellar::tx::{self, Network, Payment, PaymentMemo};
use eschess_stellar::Chain;
use eschess_storage::contract::{self, Step};
use eschess_storage::games::{self, FundingGame};
use eschess_storage::wallets;
use sqlx::PgPool;
use uuid::Uuid;

/// The most times one step (for one player) is sent for one game.
const MAX_TRIES_PER_STEP: i64 = 4;

/// A player's account needs at least this much XLM (0.5) to pay the fee of its own deposit.
const MIN_XLM_FOR_FEES: i64 = 5_000_000;

#[derive(Debug, Clone)]
pub struct FundingConfig {
    pub network: Network,
    pub escrow: Escrow,
    /// The token the stakes are in (the USDC asset contract).
    pub token: ContractId,
    pub usdc: AssetId,
    /// A game still funding after this long is cancelled.
    pub timeout_secs: i64,
    /// How far ahead a match expires. The contract allows at most 30 days; we take nearly all of it, because the pot may
    /// have to wait for a fair-play decision (ADR 0007 §5).
    pub match_expiry_secs: u64,
    /// How long one transaction is waited for before the round moves on.
    pub tx_wait: Duration,
    pub batch: i64,
}

pub const DEFAULT_TIMEOUT_SECS: i64 = 120;
pub const DEFAULT_MATCH_EXPIRY_SECS: u64 = 29 * 24 * 3600;

/// Everything a round needs to reach the network and the keys.
pub struct Parts<'a> {
    pub pool: &'a PgPool,
    pub rpc: &'a dyn RpcSend,
    pub chain: &'a dyn Chain,
    pub vault: Arc<dyn KeyVault>,
    /// The contract's admin key: creates and cancels matches.
    pub admin: &'a dyn Signer,
    /// The platform wallet, which tops players' accounts up. Without it a player who is short is waited on, then cancelled.
    pub hot: Option<&'a dyn Signer>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FundingReport {
    pub skipped: bool,
    pub activated: usize,
    pub cancelled: usize,
    /// Transactions sent this round.
    pub sent: usize,
    /// Games waiting for a transaction already sent.
    pub waiting: usize,
    pub failed: usize,
}

pub async fn tick(parts: &Parts<'_>, config: &FundingConfig) -> Result<FundingReport, String> {
    let mut lock = parts.pool.begin().await.map_err(|e| e.to_string())?;
    let got: bool = sqlx::query_scalar(
        "SELECT pg_try_advisory_xact_lock(hashtext(current_database() || ':' || $1))",
    )
    .bind("contract-funding")
    .fetch_one(&mut *lock)
    .await
    .map_err(|e| e.to_string())?;
    if !got {
        return Ok(FundingReport {
            skipped: true,
            ..FundingReport::default()
        });
    }
    let mut report = FundingReport::default();
    for game in games::funding_games(parts.pool, config.batch)
        .await
        .map_err(|e| e.to_string())?
    {
        // A few steps per game per round, so a healthy network funds a game in one round.
        for _ in 0..6 {
            match fund_one(parts, config, &game).await {
                Ok(Next::Sent) => report.sent += 1,
                Ok(Next::Waiting) => {
                    report.waiting += 1;
                    break;
                }
                Ok(Next::Activated) => {
                    report.activated += 1;
                    break;
                }
                Ok(Next::Cancelled) => {
                    report.cancelled += 1;
                    break;
                }
                Err(e) => {
                    tracing::warn!(game = %game.id, "funding a staked game failed (will retry): {e}");
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
    /// A transaction was sent and landed; look again.
    Sent,
    Waiting,
    Activated,
    Cancelled,
}

fn match_id(game: Uuid) -> [u8; 16] {
    *game.as_bytes()
}

/// The game's match as the contract has it, or `None` if it was never created.
async fn read_match(
    parts: &Parts<'_>,
    config: &FundingConfig,
    game: Uuid,
) -> Result<Option<MatchState>, String> {
    let admin = parts.admin.public_key();
    let account = parts
        .chain
        .account(&admin.to_g())
        .await
        .map_err(|e| e.to_string())?
        .ok_or("the contract's admin account does not exist on the network")?;
    match simulate(
        parts.rpc,
        &admin,
        account.sequence + 1,
        &config.escrow.get_match(match_id(game)),
    )
    .await
    {
        Ok(sim) => {
            let v = sim.return_value.ok_or("get_match returned nothing")?;
            Ok(Some(parse_match(&v)?))
        }
        Err(SorobanError::Simulation(msg))
            if contract_error(&msg) == Some(EscrowError::MatchNotFound) =>
        {
            Ok(None)
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Prepares, records, sends and waits for one contract call signed by `signer`.
async fn run_call(
    parts: &Parts<'_>,
    config: &FundingConfig,
    game: Uuid,
    step: Step,
    player: Option<Uuid>,
    signer: &dyn Signer,
    call: &ContractCall,
) -> Result<Resolved, String> {
    // A step that keeps failing is not retried forever: each try costs a fee. The game's time limit then cancels it.
    if contract::count_txs(parts.pool, game, step, player)
        .await
        .map_err(|e| e.to_string())?
        >= MAX_TRIES_PER_STEP
    {
        return Err(format!("the {} step has been tried {MAX_TRIES_PER_STEP} times; giving up until the game times out", step.as_str()));
    }
    let key = signer.public_key();
    let account = parts
        .chain
        .account(&key.to_g())
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("account {} does not exist on the network", key.to_g()))?;
    let now = parts
        .chain
        .latest_ledger_close_time()
        .await
        .map_err(|e| e.to_string())?;
    let max_time = (now + 120) as u64;
    let prepared = prepare(
        parts.rpc,
        config.network,
        signer,
        call,
        account.sequence + 1,
        max_time,
    )
    .await
    .map_err(|e| e.to_string())?;
    // Recorded before it is sent: if we die now, the next round finds this hash instead of building another.
    contract::record_tx(
        parts.pool,
        game,
        step,
        player,
        &prepared.signed.hash_hex(),
        max_time as i64,
    )
    .await
    .map_err(|e| e.to_string())?;
    send_and_wait(
        parts.rpc,
        &prepared,
        config.tx_wait,
        Duration::from_millis(1_500),
    )
    .await
    .map_err(|e| e.to_string())
}

fn expect_success(what: &str, r: Resolved) -> Result<Next, String> {
    match r {
        Resolved::Success { .. } => Ok(Next::Sent),
        Resolved::Unresolved => Ok(Next::Waiting),
        Resolved::Failed(why) => Err(format!("{what} failed on the network: {why}")),
    }
}

async fn fund_one(
    parts: &Parts<'_>,
    config: &FundingConfig,
    game: &FundingGame,
) -> Result<Next, String> {
    let now = parts
        .chain
        .latest_ledger_close_time()
        .await
        .map_err(|e| e.to_string())?;

    // A transaction sent earlier may still land: wait for it, never build another beside it.
    if let Some(last) = contract::latest_tx(parts.pool, game.id)
        .await
        .map_err(|e| e.to_string())?
    {
        if now <= last.max_time && !landed(parts, &last.hash_hex, last.step).await? {
            return Ok(Next::Waiting);
        }
    }

    let state = read_match(parts, config, game.id).await?;
    let started_secs = game.started_at_ms / 1000;
    if now - started_secs > config.timeout_secs {
        return cancel(parts, config, game, state).await;
    }
    let stake = game.stake.minor;
    let Some(state) = state else {
        let (a, b) = (
            addr(parts, game.white).await?,
            addr(parts, game.black).await?,
        );
        let call = config.escrow.create(
            match_id(game.id),
            &a,
            &b,
            &config.token,
            stake as i128,
            (now as u64) + config.match_expiry_secs,
        );
        let r = run_call(
            parts,
            config,
            game.id,
            Step::Create,
            None,
            parts.admin,
            &call,
        )
        .await?;
        return expect_success("creating the match", r);
    };
    if state.status != MatchStatus::Open {
        return Err(format!("the match is {:?}, not open", state.status));
    }
    // Whoever has not deposited yet, in order: White, then Black.
    for (player, funded) in [(game.white, state.a_funded), (game.black, state.b_funded)] {
        if funded {
            continue;
        }
        let address = addr(parts, player).await?;
        let account = parts
            .chain
            .account(&address.to_g())
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("player {player}'s account does not exist on the network"))?;
        if account.balance_of(&AssetId::native()) < MIN_XLM_FOR_FEES {
            return Err(format!(
                "player {player}'s account has too little XLM to pay the fee"
            ));
        }
        let have = account.balance_of(&config.usdc);
        if have < stake {
            return top_up(parts, config, game.id, player, &address, stake - have).await;
        }
        let signer = wallets::signer_for(parts.pool, parts.vault.clone(), player)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("player {player} has no wallet key"))?;
        let call = config.escrow.deposit(match_id(game.id), &address);
        let r = run_call(
            parts,
            config,
            game.id,
            Step::Deposit,
            Some(player),
            &signer,
            &call,
        )
        .await?;
        return expect_success("the deposit", r);
    }
    // Both stakes are locked on the network.
    if games::activate_funded(parts.pool, game.id)
        .await
        .map_err(|e| e.to_string())?
    {
        tracing::info!(game = %game.id, "stakes locked in the contract; the game is on");
    }
    Ok(Next::Activated)
}

async fn addr(parts: &Parts<'_>, user: Uuid) -> Result<AccountKey, String> {
    wallets::address_of(parts.pool, user)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("player {user} has no wallet"))
}

/// Did a recorded transaction land (or fail in a ledger)? Contract calls are looked up on the RPC server, payments on
/// Horizon.
async fn landed(parts: &Parts<'_>, hash: &str, step: Step) -> Result<bool, String> {
    match step {
        Step::TopUp => Ok(parts
            .chain
            .transaction(hash)
            .await
            .map_err(|e| e.to_string())?
            .is_some()),
        _ => Ok(parts
            .rpc
            .transaction(hash)
            .await
            .map_err(|e| e.to_string())?
            .is_some()),
    }
}

/// The platform wallet pays a player the part of their stake their own account lacks.
async fn top_up(
    parts: &Parts<'_>,
    config: &FundingConfig,
    game: Uuid,
    player: Uuid,
    to: &AccountKey,
    shortfall: i64,
) -> Result<Next, String> {
    let hot = parts
        .hot
        .ok_or("the player's account is short and there is no platform wallet to top it up")?;
    let hot_key = hot.public_key();
    let account = parts
        .chain
        .account(&hot_key.to_g())
        .await
        .map_err(|e| e.to_string())?
        .ok_or("the platform wallet does not exist on the network")?;
    if account.balance_of(&config.usdc) < shortfall {
        return Err("the platform wallet cannot cover the shortfall".into());
    }
    let now = parts
        .chain
        .latest_ledger_close_time()
        .await
        .map_err(|e| e.to_string())?;
    let max_time = (now + 120) as u64;
    let payment = Payment {
        source: hot_key,
        sequence: account.sequence + 1,
        fee_stroops: 1_000,
        min_time: 0,
        max_time,
        destination: Address::Account(*to),
        amount: Amount::new(shortfall, config.usdc.clone()).map_err(|e| e.to_string())?,
        memo: PaymentMemo::None,
    };
    let signed = tx::sign(&payment, config.network, hot)
        .await
        .map_err(|e| e.to_string())?;
    contract::record_tx(
        parts.pool,
        game,
        Step::TopUp,
        Some(player),
        &signed.hash_hex(),
        max_time as i64,
    )
    .await
    .map_err(|e| e.to_string())?;
    match parts
        .chain
        .submit(&signed.envelope_xdr)
        .await
        .map_err(|e| e.to_string())?
    {
        eschess_stellar::Submitted::Accepted => {}
        eschess_stellar::Submitted::Refused(why) => {
            return Err(format!("the top-up was refused: {why}"))
        }
    }
    tracing::info!(%game, %player, shortfall, "topped a player's account up from the platform wallet");
    Ok(Next::Sent)
}

/// The game ran out of time: cancel the match on the network (returning any deposit), then release the ledger holds.
async fn cancel(
    parts: &Parts<'_>,
    config: &FundingConfig,
    game: &FundingGame,
    state: Option<MatchState>,
) -> Result<Next, String> {
    match state {
        // Nothing was ever created on the network, or it is already closed there: only the ledger remains.
        None => {}
        Some(s) if s.status == MatchStatus::Cancelled => {}
        Some(s) if s.status == MatchStatus::Open => {
            let call = config.escrow.cancel(match_id(game.id));
            match run_call(
                parts,
                config,
                game.id,
                Step::Cancel,
                None,
                parts.admin,
                &call,
            )
            .await?
            {
                Resolved::Success { .. } => {}
                Resolved::Unresolved => return Ok(Next::Waiting),
                Resolved::Failed(why) => return Err(format!("cancelling the match failed: {why}")),
            }
        }
        Some(_) => return Err("the match was settled while the game was still funding".into()),
    }
    if games::cancel_funding(parts.pool, game.id)
        .await
        .map_err(|e| e.to_string())?
    {
        tracing::warn!(game = %game.id, "a staked game could not be funded in time; cancelled and the stakes returned");
    }
    Ok(Next::Cancelled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_match_id_is_the_games_id() {
        let g = Uuid::now_v7();
        assert_eq!(match_id(g), *g.as_bytes());
        assert_eq!(match_id(g).len(), 16);
    }
}
