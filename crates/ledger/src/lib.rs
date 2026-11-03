//! Double-entry bookkeeping, as plain data and rules.
//!
//! Money in eSchess only ever moves as a [`Posting`]: a set of entries that debit some accounts and credit
//! others by exactly the same total, for every asset. A posting that does not balance cannot be built, so
//! the mistake is caught before it reaches the database, which enforces the same rule again at commit.
//!
//! A posting carries an [`IdempotencyKey`]. Sending the same posting twice under one key records it once.

use std::collections::BTreeMap;
use std::fmt;

use eschess_domain::money::Amount;
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Most entries one posting may carry. Real postings have two to six; this is a guard, not a target.
pub const MAX_ENTRIES: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AccountId(pub Uuid);

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Side {
    Debit,
    Credit,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Debit => "debit",
            Side::Credit => "credit",
        }
    }

    pub fn parse(s: &str) -> Option<Side> {
        match s {
            "debit" => Some(Side::Debit),
            "credit" => Some(Side::Credit),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub account: AccountId,
    pub side: Side,
    pub amount: Amount,
}

impl Entry {
    pub fn debit(account: AccountId, amount: Amount) -> Self {
        Entry {
            account,
            side: Side::Debit,
            amount,
        }
    }

    pub fn credit(account: AccountId, amount: Amount) -> Self {
        Entry {
            account,
            side: Side::Credit,
            amount,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LedgerError {
    /// The key must be 8 to 128 letters, digits or `- _ . :`.
    InvalidKey,
    /// The kind must be 1 to 64 lowercase letters, digits or `_ . -`.
    InvalidKind,
    /// A reference type or id is empty or too long.
    InvalidReference,
    /// A posting needs at least two entries, and no more than [`MAX_ENTRIES`].
    EntryCount,
    /// An entry with a zero amount.
    ZeroAmount,
    /// Debits and credits differ for this asset.
    Unbalanced(String),
}

impl fmt::Display for LedgerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LedgerError::InvalidKey => write!(
                f,
                "the idempotency key must be 8 to 128 letters, digits or - _ . :"
            ),
            LedgerError::InvalidKind => write!(
                f,
                "the kind must be 1 to 64 lowercase letters, digits or _ . -"
            ),
            LedgerError::InvalidReference => {
                write!(f, "the reference type and id must be 1 to 128 characters")
            }
            LedgerError::EntryCount => {
                write!(f, "a posting needs between 2 and {MAX_ENTRIES} entries")
            }
            LedgerError::ZeroAmount => write!(f, "an entry cannot have a zero amount"),
            LedgerError::Unbalanced(asset) => {
                write!(f, "debits and credits are not equal in {asset}")
            }
        }
    }
}

impl std::error::Error for LedgerError {}

/// Names one intended posting. Retrying a request with the same key must not post twice.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    pub fn new(key: &str) -> Result<Self, LedgerError> {
        let ok = (8..=128).contains(&key.len())
            && key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b));
        if ok {
            Ok(IdempotencyKey(key.to_string()))
        } else {
            Err(LedgerError::InvalidKey)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A balanced set of entries, ready to be recorded exactly once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Posting {
    key: IdempotencyKey,
    kind: String,
    reference: Option<(String, String)>,
    entries: Vec<Entry>,
}

impl Posting {
    /// Builds a posting, or says why it cannot be one. `kind` is what happened (`deposit`, `stake_hold` ...);
    /// `reference` optionally points at the thing that caused it (a game, a settlement).
    pub fn new(
        key: IdempotencyKey,
        kind: &str,
        reference: Option<(&str, &str)>,
        entries: Vec<Entry>,
    ) -> Result<Self, LedgerError> {
        if kind.is_empty()
            || kind.len() > 64
            || !kind
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b))
        {
            return Err(LedgerError::InvalidKind);
        }
        let reference = match reference {
            None => None,
            Some((t, id)) if (1..=128).contains(&t.len()) && (1..=128).contains(&id.len()) => {
                Some((t.to_string(), id.to_string()))
            }
            Some(_) => return Err(LedgerError::InvalidReference),
        };
        if entries.len() < 2 || entries.len() > MAX_ENTRIES {
            return Err(LedgerError::EntryCount);
        }
        if entries.iter().any(|e| e.amount.is_zero()) {
            return Err(LedgerError::ZeroAmount);
        }
        // Per asset, debits minus credits must be exactly zero. i128 cannot overflow on at most 100 i64 amounts.
        let mut net: BTreeMap<String, i128> = BTreeMap::new();
        for e in &entries {
            let signed = e.amount.minor() as i128;
            *net.entry(e.amount.asset().to_string()).or_default() += if e.side == Side::Debit {
                signed
            } else {
                -signed
            };
        }
        if let Some((asset, _)) = net.into_iter().find(|(_, n)| *n != 0) {
            return Err(LedgerError::Unbalanced(asset));
        }
        Ok(Posting {
            key,
            kind: kind.to_string(),
            reference,
            entries,
        })
    }

    pub fn key(&self) -> &IdempotencyKey {
        &self.key
    }

    pub fn kind(&self) -> &str {
        &self.kind
    }

    pub fn reference(&self) -> Option<(&str, &str)> {
        self.reference
            .as_ref()
            .map(|(t, i)| (t.as_str(), i.as_str()))
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// A hash of what this posting says (kind, reference, entries), not of its key. Replaying a key with the
    /// same fingerprint is a harmless retry; the same key with a different fingerprint is a bug in the caller.
    /// The order the entries were listed in does not matter.
    pub fn fingerprint(&self) -> String {
        let mut lines: Vec<String> = self
            .entries
            .iter()
            .map(|e| {
                format!(
                    "{}|{}|{}|{}",
                    e.account,
                    e.side.as_str(),
                    e.amount.asset(),
                    e.amount.minor()
                )
            })
            .collect();
        lines.sort();
        let reference = self
            .reference()
            .map(|(t, i)| format!("{t}:{i}"))
            .unwrap_or_default();
        let text = format!("{}\n{}\n{}", self.kind, reference, lines.join("\n"));
        Sha256::digest(text.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eschess_domain::money::AssetId;
    use proptest::prelude::*;

    const ISSUER: &str = "GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZVN";

    fn acct(n: u128) -> AccountId {
        AccountId(Uuid::from_u128(n))
    }
    fn usdc(minor: i64) -> Amount {
        Amount::new(minor, AssetId::credit("USDC", ISSUER).unwrap()).unwrap()
    }
    fn xlm(minor: i64) -> Amount {
        Amount::new(minor, AssetId::native()).unwrap()
    }
    fn key(s: &str) -> IdempotencyKey {
        IdempotencyKey::new(s).unwrap()
    }
    fn transfer(from: u128, to: u128, amount: Amount) -> Vec<Entry> {
        vec![
            Entry::debit(acct(from), amount.clone()),
            Entry::credit(acct(to), amount),
        ]
    }

    #[test]
    fn a_balanced_posting_is_accepted() {
        let p = Posting::new(
            key("deposit-0001"),
            "deposit",
            Some(("deposit", "d-1")),
            transfer(1, 2, usdc(500)),
        )
        .unwrap();
        assert_eq!(p.entries().len(), 2);
        assert_eq!(p.kind(), "deposit");
        assert_eq!(p.reference(), Some(("deposit", "d-1")));
    }

    #[test]
    fn an_unbalanced_posting_cannot_be_built() {
        let entries = vec![
            Entry::debit(acct(1), usdc(500)),
            Entry::credit(acct(2), usdc(499)),
        ];
        assert!(matches!(
            Posting::new(key("unbalanced-1"), "x", None, entries),
            Err(LedgerError::Unbalanced(_))
        ));
    }

    #[test]
    fn each_asset_must_balance_on_its_own() {
        // 5 XLM out and 5 USDC in: the numbers match, the assets do not.
        let entries = vec![
            Entry::debit(acct(1), xlm(5)),
            Entry::credit(acct(2), usdc(5)),
        ];
        assert!(matches!(
            Posting::new(key("two-assets-1"), "swap", None, entries),
            Err(LedgerError::Unbalanced(_))
        ));
        // A genuine two-asset posting balances in both.
        let both = vec![
            Entry::debit(acct(1), xlm(5)),
            Entry::credit(acct(2), xlm(5)),
            Entry::debit(acct(3), usdc(7)),
            Entry::credit(acct(4), usdc(7)),
        ];
        assert!(Posting::new(key("two-assets-2"), "swap", None, both).is_ok());
    }

    #[test]
    fn shape_rules() {
        let k = || key("shape-rules-1");
        assert_eq!(
            Posting::new(k(), "x", None, vec![]),
            Err(LedgerError::EntryCount)
        );
        assert_eq!(
            Posting::new(k(), "x", None, vec![Entry::debit(acct(1), usdc(5))]),
            Err(LedgerError::EntryCount)
        );
        assert_eq!(
            Posting::new(k(), "x", None, transfer(1, 2, usdc(0))),
            Err(LedgerError::ZeroAmount)
        );
        let many: Vec<Entry> = (0..MAX_ENTRIES as u128 + 1)
            .flat_map(|i| transfer(i * 2, i * 2 + 1, usdc(1)))
            .collect();
        assert_eq!(
            Posting::new(k(), "x", None, many),
            Err(LedgerError::EntryCount)
        );
        for bad in ["", "Has Space", "UPPER", &"x".repeat(65)] {
            assert_eq!(
                Posting::new(k(), bad, None, transfer(1, 2, usdc(1))),
                Err(LedgerError::InvalidKind),
                "{bad:?}"
            );
        }
        assert_eq!(
            Posting::new(k(), "x", Some(("", "id")), transfer(1, 2, usdc(1))),
            Err(LedgerError::InvalidReference)
        );
    }

    #[test]
    fn idempotency_keys_are_checked() {
        for bad in [
            "",
            "short",
            "has space in it",
            "bad/char/here",
            &"k".repeat(129),
        ] {
            assert_eq!(
                IdempotencyKey::new(bad),
                Err(LedgerError::InvalidKey),
                "{bad:?}"
            );
        }
        for good in [
            "settlement:abc-123",
            "deposit_0001",
            "a.b-c:d_e1",
            &"k".repeat(128),
        ] {
            assert!(IdempotencyKey::new(good).is_ok(), "{good:?}");
        }
    }

    #[test]
    fn the_fingerprint_ignores_entry_order_and_the_key_but_not_the_content() {
        let a = Posting::new(
            key("fingerprint-a"),
            "stake_hold",
            Some(("game", "g1")),
            transfer(1, 2, usdc(500)),
        )
        .unwrap();
        let mut swapped = transfer(1, 2, usdc(500));
        swapped.reverse();
        let b = Posting::new(
            key("fingerprint-b"),
            "stake_hold",
            Some(("game", "g1")),
            swapped,
        )
        .unwrap();
        assert_eq!(a.fingerprint(), b.fingerprint());
        for different in [
            Posting::new(
                key("fingerprint-c"),
                "stake_hold",
                Some(("game", "g1")),
                transfer(1, 2, usdc(501)),
            )
            .unwrap(),
            Posting::new(
                key("fingerprint-c"),
                "stake_hold",
                Some(("game", "g1")),
                transfer(1, 3, usdc(500)),
            )
            .unwrap(),
            Posting::new(
                key("fingerprint-c"),
                "stake_hold",
                Some(("game", "g2")),
                transfer(1, 2, usdc(500)),
            )
            .unwrap(),
            Posting::new(
                key("fingerprint-c"),
                "payout",
                Some(("game", "g1")),
                transfer(1, 2, usdc(500)),
            )
            .unwrap(),
            Posting::new(
                key("fingerprint-c"),
                "stake_hold",
                Some(("game", "g1")),
                transfer(2, 1, usdc(500)),
            )
            .unwrap(),
        ] {
            assert_ne!(a.fingerprint(), different.fingerprint());
        }
    }

    /// Splits `total` into positive parts at the given cut points.
    fn split(total: i64, cuts: Vec<i64>) -> Vec<i64> {
        let mut points: Vec<i64> = cuts
            .into_iter()
            .map(|c| 1 + c.rem_euclid(total.max(2) - 1))
            .collect();
        points.sort_unstable();
        points.dedup();
        let mut parts = Vec::new();
        let mut last = 0;
        for p in points.into_iter().chain([total]) {
            if p > last {
                parts.push(p - last);
                last = p;
            }
        }
        parts
    }

    proptest! {
        #[test]
        fn any_split_of_one_total_across_debits_and_credits_balances(
            total in 2i64..1_000_000_000,
            debit_cuts in proptest::collection::vec(any::<i64>(), 0..5),
            credit_cuts in proptest::collection::vec(any::<i64>(), 0..5),
        ) {
            let debits = split(total, debit_cuts);
            let credits = split(total, credit_cuts);
            let mut entries: Vec<Entry> = debits.iter().enumerate().map(|(i, a)| Entry::debit(acct(i as u128), usdc(*a))).collect();
            entries.extend(credits.iter().enumerate().map(|(i, a)| Entry::credit(acct(100 + i as u128), usdc(*a))));
            prop_assert!(Posting::new(key("property-case-1"), "x", None, entries).is_ok());
        }

        #[test]
        fn changing_any_single_amount_by_one_unbalances_it(
            total in 2i64..1_000_000_000,
            debit_cuts in proptest::collection::vec(any::<i64>(), 0..5),
            credit_cuts in proptest::collection::vec(any::<i64>(), 0..5),
            which in any::<usize>(),
        ) {
            let debits = split(total, debit_cuts);
            let credits = split(total, credit_cuts);
            let mut entries: Vec<Entry> = debits.iter().enumerate().map(|(i, a)| Entry::debit(acct(i as u128), usdc(*a))).collect();
            entries.extend(credits.iter().enumerate().map(|(i, a)| Entry::credit(acct(100 + i as u128), usdc(*a))));
            let i = which % entries.len();
            entries[i].amount = usdc(entries[i].amount.minor() + 1);
            prop_assert!(matches!(Posting::new(key("property-case-2"), "x", None, entries), Err(LedgerError::Unbalanced(_))));
        }
    }
}
