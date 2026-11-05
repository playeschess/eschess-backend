//! Which player is a payment for? The pure rule.
//!
//! Each player has a *deposit id*. They are told to pay either to the `M...` address that embeds it, or to the plain
//! wallet address with that number as the memo. Either way the payment names the player; nothing else is trusted
//! (not the sender's address, not a text memo).

use crate::{IncomingPayment, Memo};

/// The deposit id a payment carries, or why it carries none that can be trusted.
pub fn deposit_id_of(payment: &IncomingPayment) -> Result<i64, String> {
    let from_memo = match &payment.memo {
        Memo::Id(id) => Some(*id),
        _ => None,
    };
    let id = match (payment.mux_id, from_memo) {
        (Some(a), Some(b)) if a != b => {
            return Err(format!(
                "the M-address id ({a}) and the memo id ({b}) name different players"
            ));
        }
        (Some(id), _) | (None, Some(id)) => id,
        (None, None) => {
            return Err(match &payment.memo {
                Memo::None => "no deposit id: no M-address id and no memo".to_string(),
                _ => "no deposit id: the memo is not a numeric id".to_string(),
            });
        }
    };
    // Deposit ids are positive and fit a signed 64-bit column.
    i64::try_from(id)
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| format!("deposit id {id} is out of range"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use eschess_domain::money::{Amount, AssetId};

    fn payment(mux_id: Option<u64>, memo: Memo) -> IncomingPayment {
        IncomingPayment {
            op_id: "1".into(),
            tx_hash: "ab".into(),
            asset: AssetId::native(),
            amount: Amount::new(1, AssetId::native()).unwrap(),
            from: "G".into(),
            to: "G".into(),
            mux_id,
            memo,
            created_at: String::new(),
        }
    }

    #[test]
    fn the_id_comes_from_the_muxed_address_or_the_memo() {
        assert_eq!(
            deposit_id_of(&payment(Some(100_001), Memo::None)),
            Ok(100_001)
        );
        assert_eq!(
            deposit_id_of(&payment(None, Memo::Id(100_002))),
            Ok(100_002)
        );
        assert_eq!(
            deposit_id_of(&payment(Some(7), Memo::Id(7))),
            Ok(7),
            "both agreeing is fine"
        );
    }

    #[test]
    fn nothing_is_guessed() {
        assert!(deposit_id_of(&payment(None, Memo::None)).is_err());
        assert!(
            deposit_id_of(&payment(None, Memo::Text("100001".into()))).is_err(),
            "a text memo is not trusted"
        );
        assert!(deposit_id_of(&payment(None, Memo::Hash("ab".into()))).is_err());
        assert!(deposit_id_of(&payment(Some(5), Memo::Id(6)))
            .unwrap_err()
            .contains("different players"));
        assert!(deposit_id_of(&payment(Some(0), Memo::None)).is_err());
        assert!(
            deposit_id_of(&payment(Some(1 << 63), Memo::None)).is_err(),
            "too big for the database"
        );
        assert!(deposit_id_of(&payment(None, Memo::Id(u64::MAX))).is_err());
    }
}
