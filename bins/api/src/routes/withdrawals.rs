//! Withdrawals: a player asks for USDC to be sent out of their wallet to a Stellar address (ADR 0001).
//!
//! A request is checked here, then recorded and the money held in one transaction (`eschess_storage::withdrawals`).
//! Nothing is sent from the API: a worker builds, signs and submits the payment. Small withdrawals are approved at once;
//! larger ones wait for a person with the `withdrawal_approver` role, who may not be the player.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use eschess_domain::money::{Amount, AssetId};
use eschess_domain::stellar::{AccountKey, Address};
use eschess_stellar::tx::{PaymentMemo, MAX_TEXT_MEMO_BYTES};
use eschess_storage::withdrawals::{self, Attempt, Policy, Status, Withdrawal, APPROVER_ROLE};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::config::StellarConfig;
use crate::error::ApiError;
use crate::extract::AuthUser;
use crate::AppState;

fn enabled(state: &AppState) -> Result<&StellarConfig, ApiError> {
    state.config.stellar.as_ref().ok_or_else(|| {
        ApiError::Coded(
            StatusCode::NOT_FOUND,
            "withdrawals_unavailable",
            "withdrawals are not available on this server".into(),
        )
    })
}

fn refused(code: &'static str, message: String) -> ApiError {
    ApiError::Coded(StatusCode::UNPROCESSABLE_ENTITY, code, message)
}

/// A whole number of minor units as decimal text. Every Stellar asset has seven decimals, so the asset does not matter.
fn decimal(minor: i64) -> String {
    Amount::new(minor.max(0), AssetId::native())
        .expect("not negative")
        .to_decimal_string()
}

fn hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let v: Vec<u8> = (0..64)
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect();
    v.try_into().ok()
}

#[derive(Deserialize)]
pub struct MemoBody {
    #[serde(rename = "type")]
    kind: String,
    value: String,
}

fn parse_memo(memo: Option<MemoBody>) -> Result<PaymentMemo, ApiError> {
    let Some(m) = memo else {
        return Ok(PaymentMemo::None);
    };
    let bad = |why: &str| ApiError::Validation(format!("memo: {why}"));
    Ok(match m.kind.as_str() {
        "id" => PaymentMemo::Id(
            m.value
                .parse()
                .map_err(|_| bad("an id memo is a whole number from 0 to 18446744073709551615"))?,
        ),
        "text" if m.value.len() <= MAX_TEXT_MEMO_BYTES => PaymentMemo::Text(m.value),
        "text" => {
            return Err(bad(&format!(
                "a text memo is at most {MAX_TEXT_MEMO_BYTES} bytes"
            )))
        }
        "hash" => {
            PaymentMemo::Hash(hex32(&m.value).ok_or_else(|| bad("a hash memo is 64 hex digits"))?)
        }
        "return" => PaymentMemo::Return(
            hex32(&m.value).ok_or_else(|| bad("a return memo is 64 hex digits"))?,
        ),
        _ => return Err(bad("the type must be id, text, hash or return")),
    })
}

fn memo_json(memo: &PaymentMemo) -> Value {
    let hex = |b: &[u8; 32]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    match memo {
        PaymentMemo::None => Value::Null,
        PaymentMemo::Id(id) => json!({ "type": "id", "value": id.to_string() }),
        PaymentMemo::Text(t) => json!({ "type": "text", "value": t }),
        PaymentMemo::Hash(h) => json!({ "type": "hash", "value": hex(h) }),
        PaymentMemo::Return(h) => json!({ "type": "return", "value": hex(h) }),
    }
}

fn withdrawal_json(w: &Withdrawal) -> Value {
    json!({
        "id": w.id,
        "client_request_id": w.client_request_id,
        "status": w.status.as_str(),
        "asset": w.amount.asset().to_string(),
        "amount": w.amount.to_decimal_string(),
        "amount_minor": w.amount.minor().to_string(),
        "destination": w.destination,
        "memo": memo_json(&w.memo),
        "failure_reason": w.failure_reason,
        "requested_at_ms": w.requested_at_ms,
        "finished_at_ms": w.finished_at_ms,
    })
}

fn attempt_json(a: &Attempt) -> Value {
    json!({
        "attempt": a.attempt_no,
        "tx_hash": a.tx_hash,
        "status": format!("{:?}", a.status).to_lowercase(),
        "result_code": a.result_code,
    })
}

/// A withdrawal with its attempts: the transaction hashes let anyone look the payment up on the network.
async fn detailed(state: &AppState, w: &Withdrawal) -> Result<Value, ApiError> {
    let attempts = withdrawals::attempts(&state.pool, w.id).await?;
    let mut v = withdrawal_json(w);
    v["attempts"] = attempts.iter().map(attempt_json).collect();
    Ok(v)
}

#[derive(Deserialize)]
pub struct RequestBody {
    client_request_id: Uuid,
    /// USDC as decimal text, e.g. "12.5".
    amount: String,
    /// A `G...` address, or an `M...` address (which needs no memo).
    destination: String,
    memo: Option<MemoBody>,
}

/// Asks for USDC to be sent out. 201 with the new withdrawal, or 200 with the earlier one when this `client_request_id`
/// was already used for the same request (a retry is safe).
pub async fn request(
    State(state): State<AppState>,
    user: AuthUser,
    Json(b): Json<RequestBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    user.require_verified()?;
    let st = enabled(&state)?;
    let amount = Amount::from_decimal_str(b.amount.trim(), st.usdc.clone())
        .map_err(|e| ApiError::Validation(format!("amount: {e}")))?;
    if amount.minor() < st.withdraw.min_minor {
        return Err(refused(
            "below_minimum",
            format!(
                "the smallest withdrawal is {} USDC",
                decimal(st.withdraw.min_minor)
            ),
        ));
    }
    let destination = b.destination.trim();
    let address = Address::parse(destination)
        .map_err(|e| ApiError::Validation(format!("destination: {e}")))?;
    if address.account() == st.hot_account {
        return Err(refused(
            "destination_is_platform",
            "that address is the eSchess wallet itself; use the address of the wallet you want the USDC in".into(),
        ));
    }
    let issuer = st.usdc.issuer().map(AccountKey::parse);
    if let Some(Ok(issuer)) = issuer {
        if address.account() == issuer {
            return Err(refused(
                "destination_is_issuer",
                "that address is USDC's issuer: USDC sent there is destroyed".into(),
            ));
        }
    }
    let memo = parse_memo(b.memo)?;

    // A retry of a request already made skips the network check (it passed the first time, and the network may be
    // unreachable now); storage answers it with the original.
    let earlier = withdrawals::of_request(&state.pool, user.user_id, b.client_request_id).await?;
    if earlier.is_none() {
        check_destination(&state, &address.account().to_g(), &st.usdc).await?;
    }

    let new = withdrawals::NewWithdrawal {
        user_id: user.user_id,
        client_request_id: b.client_request_id,
        amount,
        destination: destination.to_string(),
        memo,
    };
    let policy = Policy {
        auto_approve_up_to_minor: st.withdraw.auto_approve_minor,
        daily_limit_minor: st.withdraw.daily_limit_minor,
    };
    let (w, created) = withdrawals::request(&state.pool, &new, policy).await?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(withdrawal_json(&w))))
}

/// The destination must exist and be able to receive the asset; otherwise the payment would fail on chain and still
/// cost a fee. This can change after the check (a trustline removed), which is safe: a failed payment is refunded.
async fn check_destination(
    state: &AppState,
    account: &str,
    asset: &AssetId,
) -> Result<(), ApiError> {
    let chain = state
        .chain
        .as_ref()
        .ok_or_else(|| ApiError::internal("Stellar is configured but no network client is"))?;
    let unavailable = || {
        ApiError::Coded(
            StatusCode::SERVICE_UNAVAILABLE,
            "network_unavailable",
            "the Stellar network cannot be reached to check that address; try again in a moment"
                .into(),
        )
    };
    match chain.account(account).await {
        Err(e) => {
            tracing::warn!("withdrawal destination check: {e}");
            Err(unavailable())
        }
        Ok(None) => Err(refused(
            "destination_not_found",
            "that account does not exist on the Stellar network yet (it has never been funded)"
                .into(),
        )),
        Ok(Some(a)) if !a.can_receive(asset) => Err(refused(
            "destination_cannot_receive",
            format!(
                "that account cannot receive {} yet: it needs a trustline to issuer {}",
                asset.code(),
                asset.issuer().unwrap_or_default()
            ),
        )),
        Ok(Some(_)) => Ok(()),
    }
}

#[derive(Deserialize)]
pub struct ListQuery {
    limit: Option<i64>,
    status: Option<String>,
}

/// The player's own withdrawals, newest first.
pub async fn list(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    let rows = withdrawals::of_user(
        &state.pool,
        user.user_id,
        q.limit.unwrap_or(50).clamp(1, 200),
    )
    .await?;
    Ok(Json(
        json!({ "withdrawals": rows.iter().map(withdrawal_json).collect::<Vec<_>>() }),
    ))
}

/// One of the player's withdrawals, with its attempts. Someone else's is "not found".
pub async fn get(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let w = withdrawals::get(&state.pool, id).await?;
    if w.user_id != user.user_id {
        return Err(ApiError::NotFound);
    }
    Ok(Json(detailed(&state, &w).await?))
}

/// Withdraws the request while nothing has been signed; the money goes back to the wallet.
pub async fn cancel(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let w = withdrawals::cancel(&state.pool, id, user.user_id).await?;
    Ok(Json(withdrawal_json(&w)))
}

/// An approver, or "not found" for anyone else: the admin routes do not announce themselves.
pub(crate) async fn approver(state: &AppState, user: &AuthUser) -> Result<(), ApiError> {
    if !withdrawals::has_role(&state.pool, user.user_id, APPROVER_ROLE).await? {
        return Err(ApiError::NotFound);
    }
    user.require_verified()
}

/// The approval queue (or any other state, with `?status=`), oldest first, with whose they are.
pub async fn admin_list(
    State(state): State<AppState>,
    user: AuthUser,
    Query(q): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    approver(&state, &user).await?;
    let status = match q.status.as_deref() {
        None => Status::AwaitingApproval,
        Some(s) => {
            Status::parse(s).ok_or_else(|| ApiError::Validation(format!("unknown status {s:?}")))?
        }
    };
    let rows =
        withdrawals::with_status(&state.pool, status, q.limit.unwrap_or(50).clamp(1, 200)).await?;
    let mut out = Vec::with_capacity(rows.len());
    for w in &rows {
        let mut v = detailed(&state, w).await?;
        v["user_id"] = json!(w.user_id);
        out.push(v);
    }
    Ok(Json(json!({ "withdrawals": out })))
}

pub async fn admin_approve(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    approver(&state, &user).await?;
    let w = withdrawals::approve(&state.pool, id, user.user_id).await?;
    tracing::info!(withdrawal = %id, approver = %user.user_id, "withdrawal approved");
    Ok(Json(withdrawal_json(&w)))
}

#[derive(Deserialize)]
pub struct RejectBody {
    reason: String,
}

pub async fn admin_reject(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<Uuid>,
    Json(b): Json<RejectBody>,
) -> Result<Json<Value>, ApiError> {
    approver(&state, &user).await?;
    let reason = b.reason.trim();
    if reason.is_empty() || reason.len() > 500 {
        return Err(ApiError::Validation(
            "reason: 1 to 500 characters, shown to the player".into(),
        ));
    }
    let w = withdrawals::reject(&state.pool, id, user.user_id, reason).await?;
    tracing::info!(withdrawal = %id, approver = %user.user_id, "withdrawal rejected");
    Ok(Json(withdrawal_json(&w)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memo(kind: &str, value: &str) -> Result<PaymentMemo, ApiError> {
        parse_memo(Some(MemoBody {
            kind: kind.into(),
            value: value.into(),
        }))
    }

    #[test]
    fn memos_are_read_strictly_and_shown_the_same_way() {
        assert_eq!(parse_memo(None).unwrap(), PaymentMemo::None);
        let cases = [
            ("id", "18446744073709551615"),
            ("text", "exchange ref 1234"),
            ("hash", &"ab".repeat(32)),
            ("return", &"01".repeat(32)),
        ];
        for (kind, value) in cases {
            let m = memo(kind, value).unwrap();
            assert_eq!(memo_json(&m), json!({ "type": kind, "value": value }));
        }
        assert!(memo("id", "-1").is_err());
        assert!(memo("id", "18446744073709551616").is_err());
        assert!(memo("text", &"x".repeat(29)).is_err());
        assert!(memo("hash", "abc").is_err());
        assert!(memo("hash", &"zz".repeat(32)).is_err());
        assert!(memo("note", "hi").is_err());
    }

    #[test]
    fn amounts_are_shown_as_decimals() {
        assert_eq!(decimal(10_000_000), "1");
        assert_eq!(decimal(5), "0.0000005");
    }
}
