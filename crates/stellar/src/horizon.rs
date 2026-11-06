//! A Horizon server as a [`Chain`].

use std::time::Duration;

use eschess_domain::money::{Amount, AssetId};
use serde_json::Value;

use crate::{
    AccountState, Chain, ChainError, IncomingPayment, Memo, Page, Record, Submitted, TxOutcome,
};

pub struct HorizonChain {
    client: reqwest::Client,
    base_url: String,
}

impl HorizonChain {
    pub fn new(base_url: &str) -> Result<HorizonChain, ChainError> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent("eschess-backend")
            .build()
            .map_err(|e| ChainError::Unavailable(e.to_string()))?;
        Ok(HorizonChain {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
        })
    }
}

#[async_trait::async_trait]
impl Chain for HorizonChain {
    async fn payments(
        &self,
        account: &str,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Page, ChainError> {
        let mut request = self
            .client
            .get(format!("{}/accounts/{account}/payments", self.base_url))
            .query(&[
                ("order", "asc"),
                ("limit", &limit.min(200).to_string()),
                ("join", "transactions"),
                ("include_failed", "false"),
            ]);
        if let Some(c) = cursor {
            request = request.query(&[("cursor", c)]);
        }
        let response = request
            .send()
            .await
            .map_err(|e| ChainError::Unavailable(e.to_string()))?;
        let status = response.status();
        // An account that has never been funded does not exist yet, so it has no payments.
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(Page {
                records: vec![],
                next_cursor: None,
            });
        }
        if !status.is_success() {
            return Err(ChainError::Unavailable(format!(
                "Horizon answered {status}"
            )));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|e| ChainError::Unexpected(e.to_string()))?;
        parse_page(&body, account)
    }

    async fn account(&self, account: &str) -> Result<Option<AccountState>, ChainError> {
        let response = self
            .client
            .get(format!("{}/accounts/{account}", self.base_url))
            .send()
            .await
            .map_err(|e| ChainError::Unavailable(e.to_string()))?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(ChainError::Unavailable(format!(
                "Horizon answered {status}"
            )));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|e| ChainError::Unexpected(e.to_string()))?;
        parse_account(&body).map(Some)
    }

    async fn submit(&self, envelope_xdr: &str) -> Result<Submitted, ChainError> {
        let response = self
            .client
            .post(format!("{}/transactions", self.base_url))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(format!("tx={}", form_encode(envelope_xdr)))
            .send()
            .await
            .map_err(|e| ChainError::Unavailable(e.to_string()))?;
        let status = response.status();
        if status.is_success() {
            return Ok(Submitted::Accepted);
        }
        if status == reqwest::StatusCode::BAD_REQUEST {
            let body: Value = response
                .json()
                .await
                .map_err(|e| ChainError::Unexpected(e.to_string()))?;
            return Ok(Submitted::Refused(refusal_codes(&body)));
        }
        // 504 (timed out waiting for a ledger) and the rest: unknown.
        Err(ChainError::Unavailable(format!(
            "Horizon answered {status}"
        )))
    }

    async fn transaction(&self, hash: &str) -> Result<Option<TxOutcome>, ChainError> {
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ChainError::Unexpected(format!(
                "{hash:?} is not a transaction hash"
            )));
        }
        let response = self
            .client
            .get(format!("{}/transactions/{hash}", self.base_url))
            .send()
            .await
            .map_err(|e| ChainError::Unavailable(e.to_string()))?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(ChainError::Unavailable(format!(
                "Horizon answered {status}"
            )));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|e| ChainError::Unexpected(e.to_string()))?;
        parse_transaction(&body).map(Some)
    }

    async fn latest_ledger_close_time(&self) -> Result<i64, ChainError> {
        let response = self
            .client
            .get(format!("{}/ledgers", self.base_url))
            .query(&[("order", "desc"), ("limit", "1")])
            .send()
            .await
            .map_err(|e| ChainError::Unavailable(e.to_string()))?;
        if !response.status().is_success() {
            return Err(ChainError::Unavailable(format!(
                "Horizon answered {}",
                response.status()
            )));
        }
        let body: Value = response
            .json()
            .await
            .map_err(|e| ChainError::Unexpected(e.to_string()))?;
        parse_latest_ledger(&body)
    }
}

/// Percent-encodes base64 text for a form body (`+`, `/` and `=` are not safe there).
fn form_encode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '+' => "%2B".to_string(),
            '/' => "%2F".to_string(),
            '=' => "%3D".to_string(),
            c => c.to_string(),
        })
        .collect()
}

/// The result codes from a refused submission, e.g. `tx_failed: op_no_trust`.
fn refusal_codes(body: &Value) -> String {
    let codes = body.pointer("/extras/result_codes");
    let tx = codes
        .and_then(|c| text(c, "transaction"))
        .unwrap_or("unknown");
    let ops: Vec<&str> = codes
        .and_then(|c| c.get("operations"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if ops.is_empty() {
        tx.to_string()
    } else {
        format!("{tx}: {}", ops.join(", "))
    }
}

/// Reads Horizon's answer for one transaction. Pure, so it is tested on recorded JSON.
pub fn parse_transaction(body: &Value) -> Result<TxOutcome, ChainError> {
    let successful = body
        .get("successful")
        .and_then(Value::as_bool)
        .ok_or_else(|| ChainError::Unexpected("a transaction without `successful`".into()))?;
    let fee_charged = match body.get("fee_charged") {
        Some(Value::String(s)) => s.parse().ok(),
        Some(Value::Number(n)) => n.as_i64(),
        _ => None,
    }
    .ok_or_else(|| ChainError::Unexpected("a transaction without a readable fee_charged".into()))?;
    let result_code = text(body, "result_xdr")
        .map(result_code)
        .unwrap_or_else(|| "unknown".into());
    Ok(TxOutcome {
        successful,
        fee_charged,
        result_code,
    })
}

/// Turns a base64 `TransactionResult` into a readable code: `tx_success`, or `tx_failed: payment_no_trust`.
pub fn result_code(result_xdr: &str) -> String {
    use stellar_xdr::{
        Limits, OperationResult, OperationResultTr, ReadXdr, TransactionResult,
        TransactionResultResult,
    };
    let Ok(r) = TransactionResult::from_xdr_base64(result_xdr, Limits::none()) else {
        return "unreadable".into();
    };
    let snake = |name: &str| {
        let mut out = String::new();
        for (i, c) in name.chars().enumerate() {
            if c.is_ascii_uppercase() && i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        }
        out
    };
    let tx = snake(r.result.name());
    match &r.result {
        TransactionResultResult::TxFailed(ops) => {
            let first_failure = ops.iter().find_map(|op| match op {
                OperationResult::OpInner(OperationResultTr::Payment(p)) => {
                    (p.name() != "Success").then(|| format!("payment_{}", snake(p.name())))
                }
                OperationResult::OpInner(_) => None,
                other => Some(snake(other.name())),
            });
            match first_failure {
                Some(op) => format!("{tx}: {op}"),
                None => tx,
            }
        }
        _ => tx,
    }
}

/// The close time of the newest ledger in a `/ledgers?order=desc&limit=1` answer.
pub fn parse_latest_ledger(body: &Value) -> Result<i64, ChainError> {
    let closed_at = body
        .pointer("/_embedded/records/0/closed_at")
        .and_then(Value::as_str)
        .ok_or_else(|| ChainError::Unexpected("no latest ledger".into()))?;
    unix_seconds(closed_at)
        .ok_or_else(|| ChainError::Unexpected(format!("unreadable closed_at {closed_at:?}")))
}

/// Reads `YYYY-MM-DDTHH:MM:SSZ` (UTC, as Horizon writes it) as unix seconds.
pub fn unix_seconds(t: &str) -> Option<i64> {
    let b = t.as_bytes();
    if b.len() != 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'Z'
    {
        return None;
    }
    let num = |r: std::ops::Range<usize>| t.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hh, mm, ss) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    // Days from 1970-01-01 to the civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3_600 + mm * 60 + ss)
}

/// Turns Horizon's answer for one account into an [`AccountState`]. Pure, so it is tested on recorded JSON.
///
/// A trustline counts only if the issuer has authorized it (`is_authorized`): a payment to an unauthorized trustline
/// fails on chain. A trustline we cannot read is left out rather than guessed, so the account looks unable to receive it.
pub fn parse_account(body: &Value) -> Result<AccountState, ChainError> {
    let sequence = text(body, "sequence")
        .and_then(|s| s.parse::<i64>().ok())
        .ok_or_else(|| ChainError::Unexpected("an account without a readable sequence".into()))?;
    let balances = body
        .get("balances")
        .and_then(Value::as_array)
        .ok_or_else(|| ChainError::Unexpected("an account without balances".into()))?;
    let can_receive = balances
        .iter()
        .filter(|b| {
            matches!(
                text(b, "asset_type"),
                Some("credit_alphanum4" | "credit_alphanum12")
            )
        })
        .filter(|b| b.get("is_authorized").and_then(Value::as_bool) == Some(true))
        .filter_map(|b| AssetId::credit(text(b, "asset_code")?, text(b, "asset_issuer")?).ok())
        .collect();
    Ok(AccountState {
        sequence,
        can_receive,
    })
}

/// Turns one page of Horizon's `payments` answer into records. Pure, so it is tested on recorded JSON.
pub fn parse_page(body: &Value, account: &str) -> Result<Page, ChainError> {
    let items = body
        .pointer("/_embedded/records")
        .and_then(Value::as_array)
        .ok_or_else(|| ChainError::Unexpected("no _embedded.records in the answer".into()))?;
    let mut records = Vec::with_capacity(items.len());
    let mut next_cursor = None;
    for item in items {
        if let Some(token) = item.get("paging_token").and_then(Value::as_str) {
            next_cursor = Some(token.to_string());
        }
        records.push(parse_record(item, account));
    }
    Ok(Page {
        records,
        next_cursor,
    })
}

fn text<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn parse_record(r: &Value, account: &str) -> Record {
    // Only payments of money into our account matter. `path_payment_*` deliver the destination asset in `amount`.
    match text(r, "type") {
        Some("payment" | "path_payment_strict_receive" | "path_payment_strict_send") => {}
        _ => return Record::Ignored,
    }
    if text(r, "to") != Some(account) || text(r, "from") == Some(account) {
        return Record::Ignored;
    }
    if r.get("transaction_successful").and_then(Value::as_bool) != Some(true) {
        return Record::Ignored;
    }
    // From here on it is money for us, so anything we cannot understand is reported, not skipped.
    let op_id = text(r, "id").unwrap_or("unknown").to_string();
    let bad = |reason: &str| Record::Malformed {
        op_id: op_id.clone(),
        reason: reason.to_string(),
    };

    let Some(tx_hash) = text(r, "transaction_hash") else {
        return bad("no transaction_hash");
    };
    let asset = match text(r, "asset_type") {
        Some("native") => AssetId::native(),
        Some("credit_alphanum4" | "credit_alphanum12") => {
            match (text(r, "asset_code"), text(r, "asset_issuer")) {
                (Some(code), Some(issuer)) => match AssetId::credit(code, issuer) {
                    Ok(a) => a,
                    Err(e) => return bad(&format!("invalid asset: {e}")),
                },
                _ => return bad("credit asset without a code and issuer"),
            }
        }
        _ => return bad("unknown asset_type"),
    };
    let Some(amount_text) = text(r, "amount") else {
        return bad("no amount");
    };
    let amount = match Amount::from_decimal_str(amount_text, asset.clone()) {
        Ok(a) if !a.is_zero() => a,
        Ok(_) => return Record::Ignored, // a zero payment moves nothing
        Err(e) => return bad(&format!("invalid amount {amount_text:?}: {e}")),
    };
    // The network reports the id of an `M...` address as a string (it can exceed what JSON numbers hold exactly).
    let mux_id = match r.get("to_muxed_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => match s.parse::<u64>() {
            Ok(id) => Some(id),
            Err(_) => return bad("to_muxed_id is not a number"),
        },
        Some(_) => return bad("to_muxed_id is not a string"),
    };
    let memo = match parse_memo(r.get("transaction")) {
        Ok(m) => m,
        Err(reason) => return bad(&reason),
    };
    Record::Incoming(Box::new(IncomingPayment {
        op_id: op_id.clone(),
        tx_hash: tx_hash.to_string(),
        asset,
        amount,
        from: text(r, "from").unwrap_or_default().to_string(),
        to: account.to_string(),
        mux_id,
        memo,
        created_at: text(r, "created_at").unwrap_or_default().to_string(),
    }))
}

fn parse_memo(tx: Option<&Value>) -> Result<Memo, String> {
    let Some(tx) = tx else {
        return Err("the transaction was not included, so its memo is unknown".into());
    };
    let memo = text(tx, "memo");
    match (text(tx, "memo_type"), memo) {
        (None | Some("none"), _) => Ok(Memo::None),
        (Some("id"), Some(m)) => m
            .parse::<u64>()
            .map(Memo::Id)
            .map_err(|_| "memo_type id with a non-numeric memo".to_string()),
        (Some("text"), Some(m)) => Ok(Memo::Text(m.to_string())),
        (Some("hash"), Some(m)) => Ok(Memo::Hash(m.to_string())),
        (Some("return"), Some(m)) => Ok(Memo::Return(m.to_string())),
        // An empty text memo may omit the field.
        (Some("text"), None) => Ok(Memo::Text(String::new())),
        (Some(other), _) => Err(format!("unreadable memo of type {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const HOT: &str = "GBL4BGHUU6TAX7WXPSXAES7XBKEPJWLOO3HBIOYHNJH4KMLQZIQVZIF5";
    const SENDER: &str = "GDK4GYHCFWWDDFISK266LRQCRKNIOG2Y4PJLVR6FOLCBM4ECA2LXGGHQ";

    fn payment(id: &str, extra: Value) -> Value {
        let mut base = json!({
            "id": id, "paging_token": id, "transaction_successful": true,
            "source_account": SENDER, "type": "payment", "type_i": 1,
            "created_at": "2026-11-05T00:59:30Z", "transaction_hash": "cf1fc56048df79127e2aca6b0158e76d67e8a53df44ec9b85534bbdea3543029",
            "transaction": { "successful": true, "memo_type": "none" },
            "asset_type": "credit_alphanum4", "asset_code": "USDC", "asset_issuer": SENDER,
            "from": SENDER, "to": HOT, "amount": "49.2960000",
        });
        for (k, v) in extra.as_object().unwrap() {
            if v.is_null() {
                base.as_object_mut().unwrap().remove(k);
            } else {
                base[k] = v.clone();
            }
        }
        base
    }

    fn page_of(records: Vec<Value>) -> Value {
        json!({ "_embedded": { "records": records } })
    }

    fn incoming(r: &Record) -> &IncomingPayment {
        match r {
            Record::Incoming(p) => p,
            other => panic!("expected an incoming payment, got {other:?}"),
        }
    }

    #[test]
    fn a_usdc_payment_with_a_memo_is_read_exactly() {
        let p = payment(
            "100",
            json!({ "transaction": { "successful": true, "memo_type": "id", "memo": "1042" } }),
        );
        let page = parse_page(&page_of(vec![p]), HOT).unwrap();
        let got = incoming(&page.records[0]);
        assert_eq!(got.op_id, "100");
        assert_eq!(got.memo, Memo::Id(1042));
        assert_eq!(got.mux_id, None);
        assert_eq!(
            (got.amount.minor(), got.asset.code()),
            (492_960_000, "USDC")
        );
        assert_eq!(got.to, HOT);
        assert_eq!(page.next_cursor.as_deref(), Some("100"));
    }

    #[test]
    fn a_native_payment_and_a_muxed_destination() {
        let native = payment(
            "1",
            json!({ "asset_type": "native", "asset_code": null, "asset_issuer": null, "amount": "0.0000001" }),
        );
        let muxed = payment(
            "2",
            json!({ "to_muxed": "M...", "to_muxed_id": "9223372036854775808" }),
        );
        let page = parse_page(&page_of(vec![native, muxed]), HOT).unwrap();
        let (a, b) = (incoming(&page.records[0]), incoming(&page.records[1]));
        assert!(a.asset.is_native() && a.amount.minor() == 1);
        assert_eq!(
            b.mux_id,
            Some(1 << 63),
            "ids above the signed maximum survive"
        );
    }

    #[test]
    fn path_payments_deliver_the_destination_asset() {
        let p = payment(
            "3",
            json!({ "type": "path_payment_strict_receive", "type_i": 2, "source_amount": "1", "source_asset_type": "native" }),
        );
        let page = parse_page(&page_of(vec![p]), HOT).unwrap();
        assert_eq!(incoming(&page.records[0]).amount.minor(), 492_960_000);
    }

    #[test]
    fn things_that_are_not_money_for_us_are_ignored_but_the_cursor_still_moves_past_them() {
        let outgoing = payment("10", json!({ "from": HOT, "to": SENDER }));
        let elsewhere = payment("11", json!({ "to": SENDER }));
        let failed = payment("12", json!({ "transaction_successful": false }));
        let create = json!({ "id": "13", "paging_token": "13", "type": "create_account", "account": HOT, "funder": SENDER, "starting_balance": "10000.0000000", "transaction_successful": true });
        let to_self = payment("14", json!({ "from": HOT, "to": HOT }));
        let zero = payment("15", json!({ "amount": "0.0000000" }));
        let other_type =
            json!({ "id": "16", "paging_token": "16", "type": "invoke_host_function" });
        let page = parse_page(
            &page_of(vec![
                outgoing, elsewhere, failed, create, to_self, zero, other_type,
            ]),
            HOT,
        )
        .unwrap();
        assert!(
            page.records.iter().all(|r| *r == Record::Ignored),
            "{:?}",
            page.records
        );
        assert_eq!(page.next_cursor.as_deref(), Some("16"));
    }

    #[test]
    fn money_that_cannot_be_understood_is_reported_never_dropped() {
        let cases = [
            ("amount", json!({ "amount": "ten" })),
            ("amount", json!({ "amount": "1.12345678" })),
            ("asset", json!({ "asset_issuer": "not-an-issuer" })),
            ("asset", json!({ "asset_type": "mystery" })),
            ("asset", json!({ "asset_code": null })),
            ("mux", json!({ "to_muxed_id": "abc" })),
            ("mux", json!({ "to_muxed_id": 5 })),
            (
                "memo",
                json!({ "transaction": { "memo_type": "id", "memo": "x" } }),
            ),
            ("memo", json!({ "transaction": null })),
            ("hash", json!({ "transaction_hash": null })),
        ];
        for (what, extra) in cases {
            let page = parse_page(&page_of(vec![payment("77", extra)]), HOT).unwrap();
            match &page.records[0] {
                Record::Malformed { op_id, .. } => assert_eq!(op_id, "77", "{what}"),
                other => panic!("{what}: expected Malformed, got {other:?}"),
            }
        }
    }

    #[test]
    fn an_answer_of_the_wrong_shape_is_an_error() {
        assert!(parse_page(&json!({ "status": 400 }), HOT).is_err());
        assert!(parse_page(&json!({ "_embedded": { "records": "nope" } }), HOT).is_err());
        let empty = parse_page(&page_of(vec![]), HOT).unwrap();
        assert!(empty.records.is_empty() && empty.next_cursor.is_none());
    }

    #[test]
    fn memos_of_every_type_are_understood() {
        let t = |memo_type: &str, memo: Option<&str>| {
            let mut tx = json!({ "memo_type": memo_type });
            if let Some(m) = memo {
                tx["memo"] = json!(m);
            }
            parse_memo(Some(&tx))
        };
        assert_eq!(t("none", None), Ok(Memo::None));
        assert_eq!(t("text", Some("hi")), Ok(Memo::Text("hi".into())));
        assert_eq!(t("text", None), Ok(Memo::Text(String::new())));
        assert_eq!(
            t("id", Some("18446744073709551615")),
            Ok(Memo::Id(u64::MAX))
        );
        assert!(matches!(t("hash", Some("abc")), Ok(Memo::Hash(_))));
        assert!(matches!(t("return", Some("abc")), Ok(Memo::Return(_))));
        assert!(t("weird", Some("x")).is_err());
    }

    /// Reads real data from the Stellar testnet: `cargo test -p eschess-stellar -- --ignored`.
    #[tokio::test]
    #[ignore = "needs internet access to horizon-testnet.stellar.org"]
    async fn a_live_testnet_page_is_read_without_surprises() {
        let chain = HorizonChain::new("https://horizon-testnet.stellar.org").unwrap();
        // An account that has received credit-asset payments on the testnet.
        let page = chain
            .payments(HOT, None, 50)
            .await
            .expect("Horizon answers");
        eprintln!(
            "{} records, next cursor {:?}",
            page.records.len(),
            page.next_cursor
        );
        assert!(!page.records.is_empty());
        assert!(
            page.records
                .iter()
                .all(|r| !matches!(r, Record::Malformed { .. })),
            "{:?}",
            page.records
        );
        // Continuing from the cursor reads further on (or finds nothing more), never the same records again.
        let more = chain
            .payments(HOT, page.next_cursor.as_deref(), 50)
            .await
            .unwrap();
        assert!(more.next_cursor != page.next_cursor || more.records.is_empty());
        // An account that does not exist has no payments.
        let unfunded = eschess_domain::stellar::AccountKey([7u8; 32]).to_g();
        let nobody = chain
            .payments(&unfunded, None, 5)
            .await
            .expect("an unfunded account is not an error");
        assert!(nobody.records.is_empty());
    }

    const USDC_ISSUER: &str = "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5";

    #[test]
    fn an_account_is_read_with_only_the_trustlines_that_can_receive() {
        let body = json!({
            "id": "GBUUQEXJEVAN2FLTOEBQ2OUJREAQOAEYYNW5R2X7O7RE4LZ2DNMBUEAV",
            "sequence": "4294967296",
            "balances": [
                {"balance": "25.0000000", "asset_type": "credit_alphanum4", "asset_code": "USDC",
                 "asset_issuer": USDC_ISSUER, "is_authorized": true},
                {"balance": "0.0000000", "asset_type": "credit_alphanum12", "asset_code": "FROZENTOKEN",
                 "asset_issuer": USDC_ISSUER, "is_authorized": false},
                {"balance": "0.0000000", "asset_type": "liquidity_pool_shares", "liquidity_pool_id": "abc"},
                {"balance": "9999.9999900", "asset_type": "native"}
            ]
        });
        let a = parse_account(&body).unwrap();
        assert_eq!(a.sequence, 4_294_967_296);
        let usdc = AssetId::credit("USDC", USDC_ISSUER).unwrap();
        assert_eq!(a.can_receive, vec![usdc.clone()]);
        assert!(a.can_receive(&usdc));
        assert!(a.can_receive(&AssetId::native()));
        assert!(!a.can_receive(&AssetId::credit("FROZENTOKEN", USDC_ISSUER).unwrap()));
    }

    #[test]
    fn an_account_answer_of_the_wrong_shape_is_an_error() {
        assert!(parse_account(&json!({"balances": []})).is_err());
        assert!(parse_account(&json!({"sequence": "x", "balances": []})).is_err());
        assert!(parse_account(&json!({"sequence": "1"})).is_err());
    }

    /// Reads real accounts from the Stellar testnet: `cargo test -p eschess-stellar -- --ignored`.
    #[tokio::test]
    #[ignore = "needs internet access to horizon-testnet.stellar.org"]
    async fn live_testnet_accounts_are_read_and_a_missing_one_is_none() {
        let chain = HorizonChain::new("https://horizon-testnet.stellar.org").unwrap();
        let issuer = chain
            .account(USDC_ISSUER)
            .await
            .unwrap()
            .expect("Circle's issuer exists");
        assert!(issuer.sequence > 0);
        // A valid address nobody has funded (a throwaway test key).
        let unfunded = "GCLSLSBBGOP2GTENWWKA2DXRTNWMRBE6KGVV4UNZK7LMIGMCOQ6EJT2J";
        if let Some(a) = chain.account(unfunded).await.unwrap() {
            // Someone may have funded the public test key; then it must still read cleanly.
            assert!(a.sequence > 0);
        }
    }

    fn encoded_result(result: stellar_xdr::TransactionResultResult) -> String {
        use stellar_xdr::{Limits, TransactionResult, TransactionResultExt, WriteXdr};
        TransactionResult {
            fee_charged: 100,
            result,
            ext: TransactionResultExt::V0,
        }
        .to_xdr_base64(Limits::none())
        .unwrap()
    }

    #[test]
    fn result_codes_are_readable() {
        use stellar_xdr::{
            OperationResult, OperationResultTr, PaymentResult, TransactionResultResult as R,
        };
        let pay = |p| OperationResult::OpInner(OperationResultTr::Payment(p));
        let ok = encoded_result(R::TxSuccess(
            vec![pay(PaymentResult::Success)].try_into().unwrap(),
        ));
        assert_eq!(result_code(&ok), "tx_success");
        let no_trust = encoded_result(R::TxFailed(
            vec![pay(PaymentResult::NoTrust)].try_into().unwrap(),
        ));
        assert_eq!(result_code(&no_trust), "tx_failed: payment_no_trust");
        let no_account = encoded_result(R::TxFailed(
            vec![OperationResult::OpNoAccount].try_into().unwrap(),
        ));
        assert_eq!(result_code(&no_account), "tx_failed: op_no_account");
        assert_eq!(result_code(&encoded_result(R::TxTooLate)), "tx_too_late");
        assert_eq!(result_code("not xdr"), "unreadable");
    }

    #[test]
    fn a_transaction_is_read_whether_it_paid_or_not() {
        use stellar_xdr::{
            OperationResult, OperationResultTr, PaymentResult, TransactionResultResult as R,
        };
        let failed = encoded_result(R::TxFailed(
            vec![OperationResult::OpInner(OperationResultTr::Payment(
                PaymentResult::Underfunded,
            ))]
            .try_into()
            .unwrap(),
        ));
        let t = parse_transaction(&json!({
            "hash": "ab".repeat(32), "successful": false, "fee_charged": "100", "result_xdr": failed
        }))
        .unwrap();
        assert_eq!(
            t,
            TxOutcome {
                successful: false,
                fee_charged: 100,
                result_code: "tx_failed: payment_underfunded".into()
            }
        );
        let t = parse_transaction(&json!({ "successful": true, "fee_charged": 250 })).unwrap();
        assert!(t.successful && t.fee_charged == 250);
        assert!(parse_transaction(&json!({ "fee_charged": "100" })).is_err());
        assert!(parse_transaction(&json!({ "successful": true })).is_err());
    }

    #[test]
    fn refusals_name_the_transaction_and_operation_codes() {
        let body = json!({ "extras": { "result_codes": { "transaction": "tx_failed", "operations": ["op_no_trust"] } } });
        assert_eq!(refusal_codes(&body), "tx_failed: op_no_trust");
        assert_eq!(
            refusal_codes(
                &json!({ "extras": { "result_codes": { "transaction": "tx_bad_seq" } } })
            ),
            "tx_bad_seq"
        );
        assert_eq!(refusal_codes(&json!({})), "unknown");
        assert_eq!(form_encode("a+b/c=="), "a%2Bb%2Fc%3D%3D");
    }

    #[test]
    fn ledger_close_times_are_read_as_unix_seconds() {
        assert_eq!(unix_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(unix_seconds("2023-11-14T22:13:20Z"), Some(1_700_000_000));
        assert_eq!(unix_seconds("2024-02-29T12:00:00Z"), Some(1_709_208_000));
        assert_eq!(unix_seconds("2026-11-06T15:28:20Z"), Some(1_793_978_900));
        for bad in [
            "2026-11-06 15:28:20Z",
            "2026-13-01T00:00:00Z",
            "2026-11-06T15:28:20+01:00",
            "",
        ] {
            assert_eq!(unix_seconds(bad), None, "{bad}");
        }
        let body = json!({ "_embedded": { "records": [ { "sequence": 5, "closed_at": "2023-11-14T22:13:20Z" } ] } });
        assert_eq!(parse_latest_ledger(&body).unwrap(), 1_700_000_000);
        assert!(parse_latest_ledger(&json!({ "_embedded": { "records": [] } })).is_err());
    }

    /// `cargo test -p eschess-stellar -- --ignored`
    #[tokio::test]
    #[ignore = "needs internet access to horizon-testnet.stellar.org"]
    async fn live_testnet_ledger_time_is_recent_and_an_unknown_hash_is_none() {
        let chain = HorizonChain::new("https://horizon-testnet.stellar.org").unwrap();
        let closed = chain.latest_ledger_close_time().await.unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(
            (now - closed).abs() < 300,
            "latest ledger closed at {closed}, now is {now}"
        );
        assert_eq!(chain.transaction(&"00".repeat(32)).await.unwrap(), None);
    }
}
