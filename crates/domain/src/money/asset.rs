use std::fmt;

use super::MoneyError;
use crate::stellar::{base32_decode, crc16_xmodem};

/// A Stellar asset: the native lumen, or a credit asset identified by its code *and* issuer account.
///
/// Two assets with the same code but different issuers are different assets and never compare equal.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AssetId {
    code: String,
    issuer: Option<String>,
}

impl AssetId {
    /// The native asset (XLM).
    pub fn native() -> Self {
        AssetId {
            code: "XLM".into(),
            issuer: None,
        }
    }

    /// A credit asset. The code is 1 to 12 letters or digits; the issuer is a Stellar account id
    /// (`G...`, 56 characters) whose checksum is verified, so a mistyped issuer is rejected.
    pub fn credit(code: &str, issuer: &str) -> Result<Self, MoneyError> {
        if code.is_empty() || code.len() > 12 || !code.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(MoneyError::InvalidAsset(
                "code must be 1 to 12 letters or digits",
            ));
        }
        if code == "XLM" {
            return Err(MoneyError::InvalidAsset(
                "XLM is the native asset and has no issuer",
            ));
        }
        if !valid_account_id(issuer) {
            return Err(MoneyError::InvalidAsset(
                "issuer is not a valid Stellar account id",
            ));
        }
        Ok(AssetId {
            code: code.into(),
            issuer: Some(issuer.into()),
        })
    }

    /// Reads `XLM` or `CODE:ISSUER`. A bare code such as `USDC` is refused: it does not say who issued it.
    pub fn parse(text: &str) -> Result<Self, MoneyError> {
        match text.split_once(':') {
            None if text == "XLM" => Ok(AssetId::native()),
            None => Err(MoneyError::InvalidAsset(
                "a credit asset needs an issuer, written CODE:ISSUER",
            )),
            Some((code, issuer)) => AssetId::credit(code, issuer),
        }
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    /// `None` for the native asset.
    pub fn issuer(&self) -> Option<&str> {
        self.issuer.as_deref()
    }

    pub fn is_native(&self) -> bool {
        self.issuer.is_none()
    }
}

/// Prints `XLM` or `CODE:ISSUER`, the form [`AssetId::parse`] reads back.
impl fmt::Display for AssetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.issuer {
            None => write!(f, "{}", self.code),
            Some(issuer) => write!(f, "{}:{}", self.code, issuer),
        }
    }
}

/// A Stellar account id is a version byte, a 32-byte public key and a CRC16 checksum, base32 encoded.
fn valid_account_id(s: &str) -> bool {
    if s.len() != 56 || !s.starts_with('G') {
        return false;
    }
    let Some(bytes) = base32_decode(s) else {
        return false;
    };
    // 1 version byte + 32 key bytes + 2 checksum bytes; version 6 << 3 marks a public key.
    if bytes.len() != 35 || bytes[0] != 6 << 3 {
        return false;
    }
    let checksum = crc16_xmodem(&bytes[..33]);
    bytes[33] == (checksum & 0xff) as u8 && bytes[34] == (checksum >> 8) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real public account ids, used here only as well-formed examples.
    const ISSUER_A: &str = "GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZVN";
    const ISSUER_B: &str = "GAAZI4TCR3TY5OJHCTJC2A4QSY6CJWJH5IAJTGKIN2ER7LBNVKOCCWN7";

    #[test]
    fn native_and_credit_assets() {
        assert!(AssetId::native().is_native());
        let usdc = AssetId::credit("USDC", ISSUER_A).unwrap();
        assert_eq!((usdc.code(), usdc.issuer()), ("USDC", Some(ISSUER_A)));
        assert!(!usdc.is_native());
    }

    #[test]
    fn same_code_from_a_different_issuer_is_a_different_asset() {
        let a = AssetId::credit("USDC", ISSUER_A).unwrap();
        let b = AssetId::credit("USDC", ISSUER_B).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn a_bare_code_is_ambiguous_and_refused() {
        assert!(matches!(
            AssetId::parse("USDC"),
            Err(MoneyError::InvalidAsset(_))
        ));
        assert_eq!(AssetId::parse("XLM").unwrap(), AssetId::native());
    }

    #[test]
    fn codes_are_checked() {
        for bad in ["", "TOOLONGCODE123", "US DC", "US-DC", "ÜSDC", "XLM"] {
            assert!(
                AssetId::credit(bad, ISSUER_A).is_err(),
                "{bad:?} should be refused"
            );
        }
        assert!(AssetId::credit("A", ISSUER_A).is_ok());
        assert!(AssetId::credit("ABCDEFGHIJKL", ISSUER_A).is_ok());
    }

    #[test]
    fn issuers_are_checked_including_the_checksum() {
        assert!(AssetId::credit("USDC", "").is_err());
        assert!(AssetId::credit("USDC", "not-an-account").is_err());
        assert!(
            AssetId::credit("USDC", &ISSUER_A[..55]).is_err(),
            "too short"
        );
        assert!(
            AssetId::credit("USDC", &ISSUER_A.replacen('G', "S", 1)).is_err(),
            "wrong prefix"
        );
        // Change one character in the middle: the right length and alphabet, but the checksum no longer matches.
        let mut typo = ISSUER_A.to_string();
        typo.replace_range(20..21, if &ISSUER_A[20..21] == "A" { "B" } else { "A" });
        assert!(
            AssetId::credit("USDC", &typo).is_err(),
            "a single mistyped character is caught"
        );
        assert!(AssetId::credit("USDC", &ISSUER_A.to_lowercase()).is_err());
    }

    #[test]
    fn text_form_round_trips() {
        for text in [
            "XLM".to_string(),
            format!("USDC:{ISSUER_A}"),
            format!("EURC:{ISSUER_B}"),
        ] {
            let asset = AssetId::parse(&text).unwrap();
            assert_eq!(asset.to_string(), text);
        }
        assert!(AssetId::parse("USDC:").is_err());
        assert!(AssetId::parse(&format!("USDC:{ISSUER_A}:extra")).is_err());
    }
}
