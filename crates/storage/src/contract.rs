//! A record of every transaction sent to the network for a staked game on the contract path (ADR 0007), written before it
//! is sent. It is what lets a worker that died mid-step, or a second worker, find the transaction by its hash and wait for
//! it instead of building another one.

use sqlx::{PgPool, Row};
use uuid::Uuid;

/// What a recorded transaction was for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Create,
    /// Money moved from the platform wallet to a player's account so they can cover their stake.
    TopUp,
    Deposit,
    Cancel,
    Settle,
}

impl Step {
    pub fn as_str(self) -> &'static str {
        match self {
            Step::Create => "create",
            Step::TopUp => "top_up",
            Step::Deposit => "deposit",
            Step::Cancel => "cancel",
            Step::Settle => "settle",
        }
    }

    fn parse(s: &str) -> Option<Step> {
        Some(match s {
            "create" => Step::Create,
            "top_up" => Step::TopUp,
            "deposit" => Step::Deposit,
            "cancel" => Step::Cancel,
            "settle" => Step::Settle,
            _ => return None,
        })
    }
}

/// A transaction sent (or about to be) for a game.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentTx {
    pub hash_hex: String,
    pub step: Step,
    pub player: Option<Uuid>,
    /// Unix seconds after which the network can never apply it.
    pub max_time: i64,
}

/// Records a transaction. Call it before sending, never after.
pub async fn record_tx(
    pool: &PgPool,
    game: Uuid,
    step: Step,
    player: Option<Uuid>,
    hash_hex: &str,
    max_time: i64,
) -> Result<(), sqlx::Error> {
    record_tx_with(pool, game, step, player, hash_hex, max_time, None).await
}

/// [`record_tx`] with a note about what it does. A settle records the outcome it tells the contract.
pub async fn record_tx_with(
    pool: &PgPool,
    game: Uuid,
    step: Step,
    player: Option<Uuid>,
    hash_hex: &str,
    max_time: i64,
    detail: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO contract_txs (tx_hash, game_id, step, player_id, max_time, detail) VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (tx_hash) DO NOTHING",
    )
    .bind(hash_hex)
    .bind(game)
    .bind(step.as_str())
    .bind(player)
    .bind(max_time)
    .bind(detail)
    .execute(pool)
    .await?;
    Ok(())
}

/// The outcome the game's settle transaction told the contract, if one was recorded. There is at most one outcome that can
/// have landed: the contract refuses a second settle, and nothing is recorded for a call the network would refuse.
pub async fn settle_outcome(
    pool: &PgPool,
    game: Uuid,
) -> Result<Option<crate::stakes::ChainOutcome>, sqlx::Error> {
    let detail: Option<String> = sqlx::query_scalar(
        "SELECT detail FROM contract_txs WHERE game_id = $1 AND step = 'settle' AND detail IS NOT NULL \
         ORDER BY created_at, tx_hash LIMIT 1",
    )
    .bind(game)
    .fetch_optional(pool)
    .await?
    .flatten();
    Ok(detail.and_then(|d| crate::stakes::ChainOutcome::parse(&d)))
}

/// The game's most recently recorded transaction, if any.
pub async fn latest_tx(pool: &PgPool, game: Uuid) -> Result<Option<SentTx>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT tx_hash, step, player_id, max_time FROM contract_txs WHERE game_id = $1 \
         ORDER BY created_at DESC, tx_hash LIMIT 1",
    )
    .bind(game)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|r| {
        Some(SentTx {
            hash_hex: r.get("tx_hash"),
            step: Step::parse(&r.get::<String, _>("step"))?,
            player: r.get("player_id"),
            max_time: r.get("max_time"),
        })
    }))
}

/// How many transactions of this step (for this player, if given) were recorded for the game.
pub async fn count_txs(
    pool: &PgPool,
    game: Uuid,
    step: Step,
    player: Option<Uuid>,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(*) FROM contract_txs WHERE game_id = $1 AND step = $2 AND player_id IS NOT DISTINCT FROM $3",
    )
    .bind(game)
    .bind(step.as_str())
    .bind(player)
    .fetch_one(pool)
    .await
}
