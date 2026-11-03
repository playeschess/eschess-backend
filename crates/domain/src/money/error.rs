use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoneyError {
    /// An amount cannot be below zero.
    Negative,
    /// The result does not fit in the amount type.
    Overflow,
    /// Subtracting more than is there.
    Underflow,
    /// Two amounts of different assets were combined or compared.
    AssetMismatch,
    /// The asset code, issuer or `CODE:ISSUER` text is not valid.
    InvalidAsset(&'static str),
    /// Decimal text could not be read as an exact amount.
    InvalidDecimal(&'static str),
}

impl fmt::Display for MoneyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MoneyError::Negative => write!(f, "an amount cannot be negative"),
            MoneyError::Overflow => write!(f, "the amount is too large"),
            MoneyError::Underflow => write!(f, "cannot subtract more than the amount held"),
            MoneyError::AssetMismatch => write!(f, "the amounts are in different assets"),
            MoneyError::InvalidAsset(why) => write!(f, "invalid asset: {why}"),
            MoneyError::InvalidDecimal(why) => write!(f, "invalid amount: {why}"),
        }
    }
}

impl std::error::Error for MoneyError {}
