use std::cmp::Ordering;
use std::fmt;

use super::{AssetId, MoneyError};

/// Stellar assets have 7 decimal places; the smallest unit (a "stroop") is 0.0000001.
pub const DECIMALS: u32 = 7;
/// How many minor units make one whole unit.
pub const MINOR_PER_UNIT: i64 = 10_000_000;

/// A non-negative amount of one asset, in whole minor units.
///
/// There is deliberately no `Ord`, and no arithmetic operators: comparing or combining amounts of
/// different assets is a mistake, so those operations return a `Result`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Amount {
    minor: i64,
    asset: AssetId,
}

impl Amount {
    pub fn new(minor: i64, asset: AssetId) -> Result<Self, MoneyError> {
        if minor < 0 {
            return Err(MoneyError::Negative);
        }
        Ok(Amount { minor, asset })
    }

    pub fn zero(asset: AssetId) -> Self {
        Amount { minor: 0, asset }
    }

    pub fn minor(&self) -> i64 {
        self.minor
    }

    pub fn asset(&self) -> &AssetId {
        &self.asset
    }

    pub fn is_zero(&self) -> bool {
        self.minor == 0
    }

    fn same_asset(&self, other: &Amount) -> Result<(), MoneyError> {
        if self.asset == other.asset {
            Ok(())
        } else {
            Err(MoneyError::AssetMismatch)
        }
    }

    pub fn checked_add(&self, other: &Amount) -> Result<Amount, MoneyError> {
        self.same_asset(other)?;
        let minor = self
            .minor
            .checked_add(other.minor)
            .ok_or(MoneyError::Overflow)?;
        Ok(Amount {
            minor,
            asset: self.asset.clone(),
        })
    }

    pub fn checked_sub(&self, other: &Amount) -> Result<Amount, MoneyError> {
        self.same_asset(other)?;
        if other.minor > self.minor {
            return Err(MoneyError::Underflow);
        }
        Ok(Amount {
            minor: self.minor - other.minor,
            asset: self.asset.clone(),
        })
    }

    /// Orders two amounts of the same asset.
    pub fn compare(&self, other: &Amount) -> Result<Ordering, MoneyError> {
        self.same_asset(other)?;
        Ok(self.minor.cmp(&other.minor))
    }

    /// Reads exact decimal text such as `12`, `12.5` or `0.0000001` (at most 7 decimal places). Signs,
    /// spaces, thousands separators, exponents and a bare `.` are refused, and nothing is ever rounded.
    pub fn from_decimal_str(text: &str, asset: AssetId) -> Result<Self, MoneyError> {
        let (whole, frac) = match text.split_once('.') {
            Some((w, f)) => (w, f),
            None => (text, ""),
        };
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        if !digits(whole) {
            return Err(MoneyError::InvalidDecimal(
                "expected digits before the decimal point",
            ));
        }
        if text.contains('.') && !digits(frac) {
            return Err(MoneyError::InvalidDecimal(
                "expected digits after the decimal point",
            ));
        }
        if frac.len() > DECIMALS as usize {
            return Err(MoneyError::InvalidDecimal("at most 7 decimal places"));
        }
        let whole: i64 = whole.parse().map_err(|_| MoneyError::Overflow)?;
        let frac_minor: i64 = if frac.is_empty() {
            0
        } else {
            format!("{frac:0<7}").parse().expect("at most 7 digits")
        };
        let minor = whole
            .checked_mul(MINOR_PER_UNIT)
            .and_then(|w| w.checked_add(frac_minor))
            .ok_or(MoneyError::Overflow)?;
        Amount::new(minor, asset)
    }

    /// The decimal text for this amount, with no trailing zeros: `12.5`, `3`, `0.0000001`.
    /// For display; the wire and the database carry `minor` and the asset.
    pub fn to_decimal_string(&self) -> String {
        let (whole, frac) = (self.minor / MINOR_PER_UNIT, self.minor % MINOR_PER_UNIT);
        if frac == 0 {
            return whole.to_string();
        }
        format!("{whole}.{}", format!("{frac:07}").trim_end_matches('0'))
    }
}

impl fmt::Display for Amount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.to_decimal_string(), self.asset.code())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const ISSUER: &str = "GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZVN";

    fn xlm(minor: i64) -> Amount {
        Amount::new(minor, AssetId::native()).unwrap()
    }
    fn usdc(minor: i64) -> Amount {
        Amount::new(minor, AssetId::credit("USDC", ISSUER).unwrap()).unwrap()
    }

    #[test]
    fn amounts_cannot_be_negative() {
        assert_eq!(
            Amount::new(-1, AssetId::native()),
            Err(MoneyError::Negative)
        );
        assert!(Amount::new(0, AssetId::native()).unwrap().is_zero());
        assert_eq!(
            Amount::new(i64::MAX, AssetId::native()).unwrap().minor(),
            i64::MAX
        );
    }

    #[test]
    fn adding_and_subtracting() {
        assert_eq!(xlm(5).checked_add(&xlm(7)).unwrap().minor(), 12);
        assert_eq!(xlm(7).checked_sub(&xlm(7)).unwrap().minor(), 0);
        assert_eq!(xlm(7).checked_sub(&xlm(5)).unwrap().minor(), 2);
    }

    #[test]
    fn overflow_and_underflow_are_errors_not_wraparound() {
        assert_eq!(
            xlm(i64::MAX).checked_add(&xlm(1)),
            Err(MoneyError::Overflow)
        );
        assert_eq!(xlm(5).checked_sub(&xlm(6)), Err(MoneyError::Underflow));
        assert_eq!(xlm(0).checked_sub(&xlm(1)), Err(MoneyError::Underflow));
    }

    #[test]
    fn different_assets_never_mix() {
        assert_eq!(xlm(1).checked_add(&usdc(1)), Err(MoneyError::AssetMismatch));
        assert_eq!(xlm(1).checked_sub(&usdc(1)), Err(MoneyError::AssetMismatch));
        assert_eq!(xlm(1).compare(&usdc(1)), Err(MoneyError::AssetMismatch));
        assert_ne!(
            xlm(1),
            usdc(1),
            "equal numbers of different assets are not equal"
        );
    }

    #[test]
    fn comparison_within_an_asset() {
        assert_eq!(xlm(1).compare(&xlm(2)), Ok(Ordering::Less));
        assert_eq!(xlm(2).compare(&xlm(2)), Ok(Ordering::Equal));
        assert_eq!(xlm(3).compare(&xlm(2)), Ok(Ordering::Greater));
    }

    #[test]
    fn decimal_text_is_read_exactly() {
        let read = |s: &str| Amount::from_decimal_str(s, AssetId::native()).map(|a| a.minor());
        assert_eq!(read("0"), Ok(0));
        assert_eq!(read("1"), Ok(10_000_000));
        assert_eq!(read("12.5"), Ok(125_000_000));
        assert_eq!(read("0.0000001"), Ok(1));
        assert_eq!(
            read("0.1"),
            Ok(1_000_000),
            "0.1 is exact here, unlike a float"
        );
        assert_eq!(read("007"), Ok(70_000_000));
        assert_eq!(read("922337203685.4775807"), Ok(i64::MAX));
    }

    #[test]
    fn bad_decimal_text_is_refused() {
        for bad in [
            "",
            ".",
            ".5",
            "5.",
            "-1",
            "+1",
            "1e3",
            "1,000",
            " 1",
            "1 ",
            "1.2.3",
            "1.00000001",
            "NaN",
            "٣",
        ] {
            assert!(
                matches!(
                    Amount::from_decimal_str(bad, AssetId::native()),
                    Err(MoneyError::InvalidDecimal(_))
                ),
                "{bad:?} should be refused"
            );
        }
        assert_eq!(
            Amount::from_decimal_str("922337203685.4775808", AssetId::native()),
            Err(MoneyError::Overflow)
        );
        assert_eq!(
            Amount::from_decimal_str("99999999999999999999", AssetId::native()),
            Err(MoneyError::Overflow)
        );
    }

    #[test]
    fn decimal_text_is_printed_without_noise() {
        assert_eq!(xlm(0).to_decimal_string(), "0");
        assert_eq!(xlm(10_000_000).to_decimal_string(), "1");
        assert_eq!(xlm(125_000_000).to_decimal_string(), "12.5");
        assert_eq!(xlm(1).to_decimal_string(), "0.0000001");
        assert_eq!(xlm(10_000_001).to_decimal_string(), "1.0000001");
        assert_eq!(xlm(i64::MAX).to_decimal_string(), "922337203685.4775807");
        assert_eq!(usdc(125_000_000).to_string(), "12.5 USDC");
    }

    proptest! {
        #[test]
        fn adding_then_subtracting_gets_back_where_you_started(a in 0..=i64::MAX / 2, b in 0..=i64::MAX / 2) {
            let sum = xlm(a).checked_add(&xlm(b)).unwrap();
            prop_assert_eq!(sum.checked_sub(&xlm(b)).unwrap(), xlm(a));
        }

        #[test]
        fn addition_never_silently_wraps(a in 0..=i64::MAX, b in 0..=i64::MAX) {
            match xlm(a).checked_add(&xlm(b)) {
                Ok(sum) => prop_assert_eq!(sum.minor() as i128, a as i128 + b as i128),
                Err(e) => { prop_assert_eq!(e, MoneyError::Overflow); prop_assert!(a as i128 + b as i128 > i64::MAX as i128); }
            }
        }

        #[test]
        fn decimal_text_round_trips(minor in 0..=i64::MAX) {
            let text = xlm(minor).to_decimal_string();
            prop_assert_eq!(Amount::from_decimal_str(&text, AssetId::native()).unwrap().minor(), minor);
        }

        #[test]
        fn text_that_is_not_an_exact_amount_never_parses_to_a_surprise(s in "\\PC{0,12}") {
            // Whatever arrives, the result is either an error or an amount that prints back to the same text.
            if let Ok(a) = Amount::from_decimal_str(&s, AssetId::native()) {
                let reparsed = Amount::from_decimal_str(&a.to_decimal_string(), AssetId::native()).unwrap();
                prop_assert_eq!(reparsed, a);
            }
        }
    }
}
