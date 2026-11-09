//! Stakes (ADR 0004): who may stake, and holding and returning a stake in the ledger.
//!
//! A stake is held by one posting, `user_wallet -> stake_escrow`, keyed by the game and the player, so a retry cannot
//! hold it twice. It is returned by the reverse posting, keyed the same way with `release`. Both run inside the
//! caller's database transaction, so the hold commits together with the game it pays for, or neither does. The ledger
//! refuses an overdraft, so a player cannot stake money they do not have.
//!
//! Paying a pot out is not here: that is the settle job (ADR 0004 build step 2).

use eschess_domain::money::{Amount, AssetId};
use eschess_ledger::{AccountId, Entry, IdempotencyKey, Posting, Side};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::games::GameError;
use crate::ledger::{self, LedgerStoreError};

/// The ledger account holding stakes of games that are not settled yet. A platform liability (credit-normal): its
/// balance is the total of stakes held for games in progress. It is per asset; a game's share is found by the
/// reference on its postings.
pub const ESCROW_KIND: &str = "stake_escrow";

/// What each player puts in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stake {
    pub asset: AssetId,
    pub minor: i64,
}

impl Stake {
    fn amount(&self) -> Result<Amount, GameError> {
        Amount::new(self.minor, self.asset.clone()).map_err(|e| GameError::Ledger(e.to_string()))
    }
}

/// The stakes a deployment offers: one asset and a fixed list of amounts, so nobody types a stake freely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StakePolicy {
    pub asset: AssetId,
    pub allowed_minor: Vec<i64>,
}

impl StakePolicy {
    /// The stake for a requested amount, or `None` if that amount is not one of the allowed ones.
    pub fn stake(&self, minor: i64) -> Option<Stake> {
        self.allowed_minor.contains(&minor).then(|| Stake {
            asset: self.asset.clone(),
            minor,
        })
    }
}

fn ledger_err(e: LedgerStoreError) -> GameError {
    match e {
        LedgerStoreError::InsufficientFunds => GameError::InsufficientFunds,
        other => GameError::Ledger(other.to_string()),
    }
}

/// Refuses a player who may not stake (ADR 0004 §2): an unconfirmed email, an open fair-play case, a suspension or a
/// closure. Checked inside the transaction that takes the stake. Their balance is checked by the hold itself.
pub async fn require_can_stake(tx: &mut PgConnection, player: Uuid) -> Result<(), GameError> {
    let confirmed: Option<bool> =
        sqlx::query_scalar("SELECT email_verified_at IS NOT NULL FROM users WHERE id = $1")
            .bind(player)
            .fetch_optional(&mut *tx)
            .await?;
    if confirmed != Some(true) {
        return Err(GameError::EmailNotConfirmed);
    }
    let restricted: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM fairplay_cases WHERE player_id = $1 AND status = 'open')
             OR EXISTS (SELECT 1 FROM fairplay_restrictions WHERE player_id = $1 AND kind IN ('suspension', 'closure'))",
    )
    .bind(player)
    .fetch_one(&mut *tx)
    .await?;
    if restricted {
        return Err(GameError::StakingRestricted);
    }
    Ok(())
}

async fn account(
    tx: &mut PgConnection,
    kind: &str,
    asset: &AssetId,
    owner: Option<Uuid>,
    normal_side: Side,
) -> Result<AccountId, GameError> {
    sqlx::query("INSERT INTO ledger_accounts (id, kind, asset, normal_side, allow_negative, owner_id) VALUES ($1,$2,$3,$4,FALSE,$5) ON CONFLICT DO NOTHING")
        .bind(Uuid::now_v7())
        .bind(kind)
        .bind(asset.to_string())
        .bind(normal_side.as_str())
        .bind(owner)
        .execute(&mut *tx)
        .await?;
    let id: Uuid = sqlx::query_scalar(
        "SELECT id FROM ledger_accounts WHERE kind = $1 AND asset = $2 AND owner_id IS NOT DISTINCT FROM $3",
    )
    .bind(kind)
    .bind(asset.to_string())
    .bind(owner)
    .fetch_one(&mut *tx)
    .await?;
    Ok(AccountId(id))
}

#[derive(Clone, Copy)]
enum Step {
    Hold,
    Release,
}

async fn move_stake(
    tx: &mut PgConnection,
    game: Uuid,
    player: Uuid,
    stake: &Stake,
    step: Step,
) -> Result<(), GameError> {
    let wallet = account(tx, "user_wallet", &stake.asset, Some(player), Side::Credit).await?;
    let escrow = account(tx, ESCROW_KIND, &stake.asset, None, Side::Credit).await?;
    let (name, from, to) = match step {
        Step::Hold => ("hold", wallet, escrow),
        Step::Release => ("release", escrow, wallet),
    };
    let key = IdempotencyKey::new(&format!("stake:{game}:{player}:{name}"))
        .map_err(|e| GameError::Ledger(e.to_string()))?;
    let game_ref = game.to_string();
    let amount = stake.amount()?;
    let posting = Posting::new(
        key,
        &format!("stake_{name}"),
        Some(("game", &game_ref)),
        vec![
            Entry::debit(from, amount.clone()),
            Entry::credit(to, amount),
        ],
    )
    .map_err(|e| GameError::Ledger(e.to_string()))?;
    ledger::post_in(tx, &posting).await.map_err(ledger_err)?;
    Ok(())
}

/// Takes a player's stake for a game, from their wallet into escrow. Refuses a player who may not stake and one who
/// cannot cover it. Holding twice for the same game and player does nothing the second time.
pub async fn hold(
    tx: &mut PgConnection,
    game: Uuid,
    player: Uuid,
    stake: &Stake,
) -> Result<(), GameError> {
    require_can_stake(tx, player).await?;
    move_stake(tx, game, player, stake, Step::Hold).await
}

/// Returns a player's stake from escrow to their wallet: the game was cancelled or never started. Not for a game that
/// was played; that is settled by the settle job.
pub async fn release(
    tx: &mut PgConnection,
    game: Uuid,
    player: Uuid,
    stake: &Stake,
) -> Result<(), GameError> {
    move_stake(tx, game, player, stake, Step::Release).await
}

/// The stake of a game, from its row's columns.
pub fn stake_of(minor: Option<i64>, asset: Option<&str>) -> Result<Option<Stake>, GameError> {
    match (minor, asset) {
        (None, None) => Ok(None),
        (Some(minor), Some(asset)) => Ok(Some(Stake {
            asset: AssetId::parse(asset).map_err(|e| GameError::Ledger(e.to_string()))?,
            minor,
        })),
        _ => Err(GameError::Ledger("a game has half a stake".into())),
    }
}
