//! Stellar addresses, as pure functions (SEP-23 "Strkeys").
//!
//! A plain account is `G...` (a version byte, the 32-byte public key and a CRC16 checksum, in base32). A *muxed* account
//! is `M...`: the same key plus an 8-byte id, so one Stellar account can have millions of distinct deposit addresses.
//! eSchess gives each player a muxed address over the platform's single hot wallet, so an incoming payment says which
//! player it is for without a memo (a memo-less transfer from an exchange cannot be mis-credited).
//!
//! [`Address`] accepts only `G` and `M` addresses: other kinds (`P` signed payloads, ...) are refused rather than
//! half-understood. A smart contract (`C...`) is a separate type, [`ContractId`], used only for the escrow.

use std::fmt;

const VERSION_ACCOUNT: u8 = 6 << 3; // 'G'
const VERSION_MUXED: u8 = 12 << 3; // 'M'
const VERSION_SEED: u8 = 18 << 3; // 'S'
const VERSION_CONTRACT: u8 = 2 << 3; // 'C'

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddressError {
    /// Not a well-formed strkey: wrong length, characters outside the base32 alphabet, or padding.
    Malformed,
    /// The checksum does not match: a typo.
    BadChecksum,
    /// A valid strkey of a kind eSchess does not use (contract, signed payload, ...).
    UnsupportedKind,
}

impl fmt::Display for AddressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AddressError::Malformed => write!(f, "not a valid Stellar address"),
            AddressError::BadChecksum => write!(
                f,
                "the Stellar address has a typo (its checksum does not match)"
            ),
            AddressError::UnsupportedKind => {
                write!(f, "only G... and M... Stellar addresses are supported")
            }
        }
    }
}

impl std::error::Error for AddressError {}

/// A plain Stellar account: a 32-byte ed25519 public key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AccountKey(pub [u8; 32]);

/// A muxed account: an account plus a 64-bit id (the whole range, including ids above the signed maximum).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MuxedAccount {
    pub key: AccountKey,
    pub id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Address {
    Account(AccountKey),
    Muxed(MuxedAccount),
}

/// A smart contract's address (`C...`): the 32-byte contract id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContractId(pub [u8; 32]);

impl ContractId {
    /// Reads a `C...` address, verifying the checksum and that it is written canonically.
    pub fn parse(s: &str) -> Result<ContractId, AddressError> {
        let bytes = base32_decode(s).ok_or(AddressError::Malformed)?;
        if bytes.len() != 35 {
            return Err(AddressError::Malformed);
        }
        let (body, check) = bytes.split_at(33);
        if crc16_xmodem(body) != u16::from_le_bytes([check[0], check[1]]) {
            return Err(AddressError::BadChecksum);
        }
        if encode(body[0], &body[1..]) != s {
            return Err(AddressError::Malformed);
        }
        if body[0] != VERSION_CONTRACT {
            return Err(AddressError::UnsupportedKind);
        }
        Ok(ContractId(body[1..33].try_into().unwrap()))
    }

    /// The `C...` form.
    pub fn to_c(&self) -> String {
        encode(VERSION_CONTRACT, &self.0)
    }
}

impl AccountKey {
    /// Reads a `G...` address.
    pub fn parse(s: &str) -> Result<AccountKey, AddressError> {
        match Address::parse(s)? {
            Address::Account(k) => Ok(k),
            Address::Muxed(_) => Err(AddressError::UnsupportedKind),
        }
    }

    /// The `G...` form.
    pub fn to_g(&self) -> String {
        encode(VERSION_ACCOUNT, &self.0)
    }

    /// The `M...` deposit address for `id` over this account.
    pub fn muxed(&self, id: u64) -> MuxedAccount {
        MuxedAccount { key: *self, id }
    }
}

impl MuxedAccount {
    pub fn parse(s: &str) -> Result<MuxedAccount, AddressError> {
        match Address::parse(s)? {
            Address::Muxed(m) => Ok(m),
            Address::Account(_) => Err(AddressError::UnsupportedKind),
        }
    }

    /// The `M...` form.
    pub fn to_m(&self) -> String {
        let mut payload = [0u8; 40];
        payload[..32].copy_from_slice(&self.key.0);
        payload[32..].copy_from_slice(&self.id.to_be_bytes()); // network byte order
        encode(VERSION_MUXED, &payload)
    }
}

impl Address {
    /// Reads a `G...` or `M...` address, verifying the checksum.
    pub fn parse(s: &str) -> Result<Address, AddressError> {
        let bytes = base32_decode(s).ok_or(AddressError::Malformed)?;
        if bytes.len() < 3 {
            return Err(AddressError::Malformed);
        }
        let (body, check) = bytes.split_at(bytes.len() - 2);
        // The checksum is stored little-endian.
        if crc16_xmodem(body) != u16::from_le_bytes([check[0], check[1]]) {
            // Telling "wrong length" from "typo" matters for the message, so check shape after the checksum only when
            // the length is one we know.
            return Err(if is_known_length(s.len()) {
                AddressError::BadChecksum
            } else {
                AddressError::Malformed
            });
        }
        // Re-encoding must give back exactly the text we were given: this rejects non-canonical base32 (stray bits
        // in the last character) that a lenient decoder would accept.
        if encode(body[0], &body[1..]) != s {
            return Err(AddressError::Malformed);
        }
        match (body[0], body.len()) {
            (VERSION_ACCOUNT, 33) => Ok(Address::Account(AccountKey(
                body[1..33].try_into().unwrap(),
            ))),
            (VERSION_MUXED, 41) => Ok(Address::Muxed(MuxedAccount {
                key: AccountKey(body[1..33].try_into().unwrap()),
                id: u64::from_be_bytes(body[33..41].try_into().unwrap()),
            })),
            // A valid strkey of another kind, or of an unexpected length.
            (v, _)
                if [3 << 3, 15 << 3, 11 << 3, 1 << 3, 2 << 3, 13 << 3, 14 << 3].contains(&v)
                    || v == 2 << 3 =>
            {
                Err(AddressError::UnsupportedKind)
            }
            _ => Err(AddressError::Malformed),
        }
    }

    /// The account this address pays into (the `G` account, for a muxed address too).
    pub fn account(&self) -> AccountKey {
        match self {
            Address::Account(k) => *k,
            Address::Muxed(m) => m.key,
        }
    }

    /// The id of a muxed address; `None` for a plain account.
    pub fn mux_id(&self) -> Option<u64> {
        match self {
            Address::Account(_) => None,
            Address::Muxed(m) => Some(m.id),
        }
    }
}

/// Reads an `S...` secret seed (the 32 bytes an ed25519 signing key is made from), verifying the checksum.
///
/// The caller should turn the bytes into a signing key at once and not keep them. Errors never contain any of the input.
pub fn decode_secret_seed(s: &str) -> Result<[u8; 32], AddressError> {
    let mut bytes = base32_decode(s).ok_or(AddressError::Malformed)?;
    let result = (|| {
        if bytes.len() != 35 {
            return Err(AddressError::Malformed);
        }
        let (body, check) = bytes.split_at(33);
        if crc16_xmodem(body) != u16::from_le_bytes([check[0], check[1]]) {
            return Err(AddressError::BadChecksum);
        }
        if body[0] != VERSION_SEED {
            return Err(AddressError::UnsupportedKind);
        }
        if encode(body[0], &body[1..]) != s {
            return Err(AddressError::Malformed);
        }
        Ok(body[1..33].try_into().unwrap())
    })();
    bytes.fill(0);
    result
}

fn is_known_length(len: usize) -> bool {
    len == 56 || len == 69
}

fn encode(version: u8, payload: &[u8]) -> String {
    let mut bytes = Vec::with_capacity(payload.len() + 3);
    bytes.push(version);
    bytes.extend_from_slice(payload);
    let crc = crc16_xmodem(&bytes);
    bytes.extend_from_slice(&crc.to_le_bytes());
    base32_encode(&bytes)
}

const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// RFC 4648 base32 without padding.
pub(crate) fn base32_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity((data.len() * 8).div_ceil(5));
    let (mut buffer, mut bits) = (0u32, 0u32);
    for &byte in data {
        buffer = (buffer << 8) | byte as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buffer >> bits) & 31) as usize] as char);
        }
        buffer &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// RFC 4648 base32 without padding: upper-case letters and 2-7 only.
pub(crate) fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 5 / 8);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        } as u32;
        buffer = (buffer << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// CRC16-XModem, the checksum Stellar uses.
pub(crate) fn crc16_xmodem(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test vectors from SEP-23.
    const G: &str = "GA7QYNF7SOWQ3GLR2BGMZEHXAVIRZA4KVWLTJJFC7MGXUA74P7UJVSGZ";
    const M0: &str = "MA7QYNF7SOWQ3GLR2BGMZEHXAVIRZA4KVWLTJJFC7MGXUA74P7UJUAAAAAAAAAAAACJUQ";
    const M_BIG: &str = "MA7QYNF7SOWQ3GLR2BGMZEHXAVIRZA4KVWLTJJFC7MGXUA74P7UJVAAAAAAAAAAAAAJLK";

    #[test]
    fn the_spec_examples_round_trip() {
        let key = AccountKey::parse(G).unwrap();
        assert_eq!(key.to_g(), G);
        assert_eq!(key.muxed(0).to_m(), M0);
        assert_eq!(
            key.muxed(9_223_372_036_854_775_808).to_m(),
            M_BIG,
            "ids above the signed maximum work"
        );
        assert_eq!(MuxedAccount::parse(M0).unwrap(), key.muxed(0));
        assert_eq!(MuxedAccount::parse(M_BIG).unwrap().id, 1 << 63);
        let addr = Address::parse(M_BIG).unwrap();
        assert_eq!((addr.account(), addr.mux_id()), (key, Some(1 << 63)));
        assert_eq!(Address::parse(G).unwrap().mux_id(), None);
    }

    #[test]
    fn a_contract_address_round_trips_and_is_not_an_account() {
        // The escrow deployed on the testnet on 2026-11-09.
        let c = "CAJLKHWOKJPUBLRG6GNZZQXYWSCFZE3VLMAVUOEDI3MC7GV4GVVUN6BT";
        let id = ContractId::parse(c).unwrap();
        assert_eq!(id.to_c(), c);
        assert_eq!(
            ContractId::parse(G),
            Err(AddressError::UnsupportedKind),
            "a G address is not a contract"
        );
        assert_eq!(Address::parse(c), Err(AddressError::UnsupportedKind));
        let typo = format!("{}X", &c[..c.len() - 1]);
        assert!(ContractId::parse(&typo).is_err());
        assert!(ContractId::parse("C").is_err());
    }

    #[test]
    fn every_invalid_example_in_the_spec_is_refused() {
        for bad in [
            "GAAAAAAAACGC6",
            "MA7QYNF7SOWQ3GLR2BGMZEHXAVIRZA4KVWLTJJFC7MGXUA74P7UJUAAAAAAAAAAAACJUR",
            "GA7QYNF7SOWQ3GLR2BGMZEHXAVIRZA4KVWLTJJFC7MGXUA74P7UJVSGZA",
            "GA7QYNF7SOWQ3GLR2BGMZEHXAVIRZA4KVWLTJJFC7MGXUA74P7UJUACUSI",
            "G47QYNF7SOWQ3GLR2BGMZEHXAVIRZA4KVWLTJJFC7MGXUA74P7UJVP2I",
            "MA7QYNF7SOWQ3GLR2BGMZEHXAVIRZA4KVWLTJJFC7MGXUA74P7UJVAAAAAAAAAAAAAJLKA",
            "MA7QYNF7SOWQ3GLR2BGMZEHXAVIRZA4KVWLTJJFC7MGXUA74P7UJVAAAAAAAAAAAAAAV75I",
            "M47QYNF7SOWQ3GLR2BGMZEHXAVIRZA4KVWLTJJFC7MGXUA74P7UJUAAAAAAAAAAAACJUQ",
            "MA7QYNF7SOWQ3GLR2BGMZEHXAVIRZA4KVWLTJJFC7MGXUA74P7UJUAAAAAAAAAAAACJUK===",
            "MA7QYNF7SOWQ3GLR2BGMZEHXAVIRZA4KVWLTJJFC7MGXUA74P7UJUAAAAAAAAAAAACJUO",
        ] {
            assert!(Address::parse(bad).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn other_kinds_of_strkey_are_refused_as_unsupported_not_misread() {
        for other in [
            "PA7QYNF7SOWQ3GLR2BGMZEHXAVIRZA4KVWLTJJFC7MGXUA74P7UJUAAAAAQACAQDAQCQMBYIBEFAWDANBYHRAEISCMKBKFQXDAMRUGY4DUPB6IBZGM",
            "CA7QYNF7SOWQ3GLR2BGMZEHXAVIRZA4KVWLTJJFC7MGXUA74P7UJUWDA",
            "LA7QYNF7SOWQ3GLR2BGMZEHXAVIRZA4KVWLTJJFC7MGXUA74P7UJUPJN",
            "BAAD6DBUX6J22DMZOHIEZTEQ64CVCHEDRKWZONFEUL5Q26QD7R76RGR4TU",
            "WA7QYNF7SOWQ3GLR2BGMZEHXAVIRZA4KVWLTJJFC7MGXUA74P7UJUAAAAAAAAAAAAAWWC",
        ] {
            assert!(Address::parse(other).is_err(), "{other}");
        }
        // And a G address is not an M address, nor the reverse.
        assert_eq!(MuxedAccount::parse(G), Err(AddressError::UnsupportedKind));
        assert_eq!(AccountKey::parse(M0), Err(AddressError::UnsupportedKind));
    }

    #[test]
    fn a_typo_is_called_a_typo() {
        // One letter changed in a valid G address.
        let typo = G.replacen('F', "E", 1);
        assert_eq!(Address::parse(&typo), Err(AddressError::BadChecksum));
        assert_eq!(Address::parse("hello"), Err(AddressError::Malformed));
        assert_eq!(Address::parse(""), Err(AddressError::Malformed));
        assert_eq!(
            Address::parse(&G.to_lowercase()),
            Err(AddressError::Malformed)
        );
    }

    #[test]
    fn many_ids_round_trip() {
        let key = AccountKey::parse(G).unwrap();
        for id in [
            0,
            1,
            255,
            256,
            1234,
            u32::MAX as u64,
            u64::MAX - 1,
            u64::MAX,
        ] {
            let m = key.muxed(id).to_m();
            assert_eq!(m.len(), 69);
            assert_eq!(MuxedAccount::parse(&m).unwrap(), key.muxed(id), "{id}");
        }
    }

    // A throwaway test seed (see crates/stellar/tests/data/generate_payment_vectors.py); it holds nothing anywhere.
    const SEED: &str = "SA3PXIPSNIG2RVLSIREL6KV6PQJTFI7PA4ZBYVP772LORMETDV5FQTNT";

    #[test]
    fn a_secret_seed_is_read_and_its_kind_and_checksum_are_checked() {
        let bytes = decode_secret_seed(SEED).unwrap();
        assert_eq!(encode(VERSION_SEED, &bytes), SEED);
        let mut typo = SEED.to_string();
        typo.replace_range(10..11, if &SEED[10..11] == "A" { "B" } else { "A" });
        assert_eq!(decode_secret_seed(&typo), Err(AddressError::BadChecksum));
        assert_eq!(decode_secret_seed(G), Err(AddressError::UnsupportedKind));
        assert_eq!(
            decode_secret_seed("not a seed"),
            Err(AddressError::Malformed)
        );
        assert_eq!(
            decode_secret_seed(&SEED[..55]),
            Err(AddressError::Malformed)
        );
    }

    #[test]
    fn a_secret_seed_is_never_accepted_as_an_address() {
        assert!(Address::parse(SEED).is_err());
    }
}
