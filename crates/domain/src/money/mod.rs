//! Money: an [`Amount`] of a specific [`AssetId`], held as a whole number of minor units.
//!
//! Three rules shape everything here:
//! * No floating point, anywhere. Amounts are integers, and decimal text is parsed and printed exactly.
//! * An amount always knows its asset, so adding USDC to XLM is an error, not a silent mistake.
//! * An asset is a code *and* an issuer. `"USDC"` alone is ambiguous because anyone can issue a token with
//!   that code, so a credit asset without an issuer cannot be built.

mod amount;
mod asset;
mod error;

pub use amount::{Amount, DECIMALS, MINOR_PER_UNIT};
pub use asset::AssetId;
pub use error::MoneyError;
