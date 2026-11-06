//! A Horizon server as a [`Chain`].

use std::time::Duration;

use eschess_domain::money::{Amount, AssetId};
use serde_json::Value;

use crate::{AccountState, Chain, ChainError, IncomingPayment, Memo, Page, Record};

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
}
