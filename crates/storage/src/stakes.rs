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
use sqlx::{PgConnection, PgPool, Row};
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

pub(crate) async fn account(
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

/// The ledger account for the platform's fee on staked games (revenue; credit-normal).
pub const FEE_KIND: &str = "stake_fees";

/// Most the platform may take from a pot, in basis points (20%). A configuration typo cannot take more than this.
pub const MAX_FEE_BPS: u32 = 2_000;

/// What settling a game did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settled {
    /// The winner received the pot (less the fee, if any).
    Paid { winner: Uuid, fee_minor: i64 },
    /// Each player got their own stake back: a draw, a game that was never really played, or both players confirmed.
    Returned,
    /// Pot confiscated from a confirmed cheater and paid to the opponent in full (ADR 0004 §5, OD-14).
    Confiscated { to: Uuid },
}

/// Why a game was not settled this time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Waiting {
    /// The fair-play scorer has not finished with the game.
    NotScored,
    /// A player has an open fair-play case.
    OpenCase,
    /// Not a finished, staked, unsettled game (already settled by someone else, say).
    NothingToDo,
}

/// Finished staked games that have not been settled, oldest first.
pub async fn unsettled(pool: &PgPool, limit: i64) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT id FROM games WHERE stake_minor IS NOT NULL AND settled_at IS NULL AND status = 'finished' \
         ORDER BY ended_at, id LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Settles one finished staked game exactly once (ADR 0004 §5): one ledger posting and `settled_at`, in one
/// transaction, with the game locked so two workers cannot both do it (and the posting's key refuses a repeat anyway).
///
/// Waits ([`Waiting`]) until `scorer` has finished with the game and neither player has an open case. Then:
/// a case confirmed against exactly one player pays the other the whole pot; both confirmed returns both stakes; else
/// a draw, or a game that ended before each side had made two moves, returns the stakes; else the winner takes the pot
/// less `fee_bps` of it.
pub async fn settle(
    pool: &PgPool,
    game: Uuid,
    scorer: &str,
    fee_bps: u32,
) -> Result<Result<Settled, Waiting>, GameError> {
    let fee_bps = fee_bps.min(MAX_FEE_BPS) as i64;
    let mut tx = pool.begin().await?;
    let Some(g) = sqlx::query(
        "SELECT white_id, black_id, result, ply, stake_minor, stake_asset FROM games \
         WHERE id = $1 AND status = 'finished' AND stake_minor IS NOT NULL AND settled_at IS NULL FOR UPDATE",
    )
    .bind(game)
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(Err(Waiting::NothingToDo));
    };
    let (white, black): (Uuid, Uuid) = (g.get("white_id"), g.get("black_id"));
    let result: String = g.get("result");
    let ply: i32 = g.get("ply");
    let Some(stake) = stake_of(
        g.get("stake_minor"),
        g.get::<Option<String>, _>("stake_asset").as_deref(),
    )?
    else {
        return Ok(Err(Waiting::NothingToDo));
    };

    let scored: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM fairplay_runs WHERE game_id = $1 AND scorer = $2)",
    )
    .bind(game)
    .bind(scorer)
    .fetch_one(&mut *tx)
    .await?;
    if !scored {
        return Ok(Err(Waiting::NotScored));
    }
    let open_case: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM fairplay_cases WHERE player_id IN ($1, $2) AND status = 'open')",
    )
    .bind(white)
    .bind(black)
    .fetch_one(&mut *tx)
    .await?;
    if open_case {
        return Ok(Err(Waiting::OpenCase));
    }
    let confirmed = |player: Uuid| {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM fairplay_case_games cg JOIN fairplay_cases c ON c.id = cg.case_id \
             WHERE cg.game_id = $1 AND c.player_id = $2 AND c.status = 'confirmed')",
        )
        .bind(game)
        .bind(player)
    };
    let white_cheated = confirmed(white).fetch_one(&mut *tx).await?;
    let black_cheated = confirmed(black).fetch_one(&mut *tx).await?;

    let pot = stake
        .minor
        .checked_mul(2)
        .ok_or_else(|| GameError::Ledger("pot overflow".into()))?;
    let escrow = account(&mut tx, ESCROW_KIND, &stake.asset, None, Side::Credit).await?;
    let amount = |minor: i64| {
        Amount::new(minor, stake.asset.clone()).map_err(|e| GameError::Ledger(e.to_string()))
    };
    let mut entries = vec![Entry::debit(escrow, amount(pot)?)];
    let settled;
    let credit = |entries: &mut Vec<Entry>, acct: AccountId, minor: i64| -> Result<(), GameError> {
        if minor > 0 {
            entries.push(Entry::credit(acct, amount(minor)?));
        }
        Ok(())
    };
    let wallet_w = account(
        &mut tx,
        "user_wallet",
        &stake.asset,
        Some(white),
        Side::Credit,
    )
    .await?;
    let wallet_b = account(
        &mut tx,
        "user_wallet",
        &stake.asset,
        Some(black),
        Side::Credit,
    )
    .await?;
    match (white_cheated, black_cheated) {
        (true, true) => {
            credit(&mut entries, wallet_w, stake.minor)?;
            credit(&mut entries, wallet_b, stake.minor)?;
            settled = Settled::Returned;
        }
        (true, false) => {
            credit(&mut entries, wallet_b, pot)?;
            settled = Settled::Confiscated { to: black };
        }
        (false, true) => {
            credit(&mut entries, wallet_w, pot)?;
            settled = Settled::Confiscated { to: white };
        }
        (false, false) => {
            let winner = match result.as_str() {
                "white" => Some((white, wallet_w)),
                "black" => Some((black, wallet_b)),
                _ => None,
            };
            match winner {
                Some((who, wallet)) if ply >= crate::ratings::MIN_PLIES_FOR_RATING => {
                    let fee = pot * fee_bps / 10_000;
                    credit(&mut entries, wallet, pot - fee)?;
                    if fee > 0 {
                        let fees =
                            account(&mut tx, FEE_KIND, &stake.asset, None, Side::Credit).await?;
                        credit(&mut entries, fees, fee)?;
                    }
                    settled = Settled::Paid {
                        winner: who,
                        fee_minor: fee,
                    };
                }
                _ => {
                    credit(&mut entries, wallet_w, stake.minor)?;
                    credit(&mut entries, wallet_b, stake.minor)?;
                    settled = Settled::Returned;
                }
            }
        }
    }
    let key = IdempotencyKey::new(&format!("stake:{game}:settle"))
        .map_err(|e| GameError::Ledger(e.to_string()))?;
    let game_ref = game.to_string();
    let posting = Posting::new(key, "stake_settle", Some(("game", &game_ref)), entries)
        .map_err(|e| GameError::Ledger(e.to_string()))?;
    ledger::post_in(&mut tx, &posting)
        .await
        .map_err(ledger_err)?;
    sqlx::query("UPDATE games SET settled_at = now() WHERE id = $1")
        .bind(game)
        .execute(&mut *tx)
        .await?;
    tx.commit()
        .await
        .map_err(|e| ledger_err(LedgerStoreError::from(e)))?;
    Ok(Ok(settled))
}
