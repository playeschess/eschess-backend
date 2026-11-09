//! The escrow contract (`eschess-contracts`): which calls it has, in the shape [`crate::soroban`] sends them, and how to
//! read what it says back.
//!
//! The contract holds both players' stakes for a match and pays them out. The admin (our server's key) creates a match,
//! reports its outcome, or cancels it; each player authorizes their own deposit; anyone can refund an expired match. The
//! contract can only ever pay the two players recorded at `create`. A match is identified by 16 bytes: we use the
//! game's UUID.

use eschess_domain::stellar::{AccountKey, ContractId};
use stellar_xdr::{ScAddress, ScMap, ScVal};

use crate::soroban::{val, ContractCall};

/// How a match ended, as the contract takes it. `A` and `B` are the players in the order given to `create`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    WinA,
    WinB,
    Draw,
}

impl Outcome {
    fn name(self) -> &'static str {
        match self {
            Outcome::WinA => "WinA",
            Outcome::WinB => "WinB",
            Outcome::Draw => "Draw",
        }
    }
}

/// One deployed escrow contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Escrow {
    pub contract: ContractId,
}

impl Escrow {
    pub fn new(contract: ContractId) -> Escrow {
        Escrow { contract }
    }

    fn call(&self, function: &str, args: Vec<ScVal>) -> ContractCall {
        ContractCall {
            contract: self.contract,
            function: function.to_string(),
            args,
        }
    }

    /// Opens a match. `stake` is per player, in the token's smallest unit. `expires_at` is unix seconds, must be in the
    /// future when the transaction lands, and at most 30 days away. Signed by the admin.
    pub fn create(
        &self,
        match_id: [u8; 16],
        player_a: &AccountKey,
        player_b: &AccountKey,
        token: &ContractId,
        stake: i128,
        expires_at: u64,
    ) -> ContractCall {
        self.call(
            "create",
            vec![
                val::bytes(&match_id),
                val::account(player_a),
                val::account(player_b),
                val::contract(token),
                val::i128(stake),
                val::u64(expires_at),
            ],
        )
    }

    /// A player puts in their stake. Signed by that player.
    pub fn deposit(&self, match_id: [u8; 16], player: &AccountKey) -> ContractCall {
        self.call("deposit", vec![val::bytes(&match_id), val::account(player)])
    }

    /// Reports the outcome and pays out. Signed by the admin; needs both deposits.
    pub fn settle(&self, match_id: [u8; 16], outcome: Outcome) -> ContractCall {
        self.call(
            "settle",
            vec![val::bytes(&match_id), val::unit_variant(outcome.name())],
        )
    }

    /// Closes an open match and refunds whatever was deposited. Signed by the admin.
    pub fn cancel(&self, match_id: [u8; 16]) -> ContractCall {
        self.call("cancel", vec![val::bytes(&match_id)])
    }

    /// After the expiry plus the grace period, anyone can refund an unsettled match.
    pub fn refund_expired(&self, match_id: [u8; 16]) -> ContractCall {
        self.call("refund_expired", vec![val::bytes(&match_id)])
    }

    /// A player collects a payout that could not be delivered when the match closed. Signed by that player.
    pub fn claim(&self, match_id: [u8; 16], player: &AccountKey) -> ContractCall {
        self.call("claim", vec![val::bytes(&match_id), val::account(player)])
    }

    /// Read: what the match looks like now.
    pub fn get_match(&self, match_id: [u8; 16]) -> ContractCall {
        self.call("get_match", vec![val::bytes(&match_id)])
    }

    /// Read: a player's payout waiting to be claimed.
    pub fn unclaimed(&self, match_id: [u8; 16], player: &AccountKey) -> ContractCall {
        self.call(
            "unclaimed",
            vec![val::bytes(&match_id), val::account(player)],
        )
    }

    /// Read: the current admin.
    pub fn admin(&self) -> ContractCall {
        self.call("admin", vec![])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchStatus {
    Open,
    Settled,
    Cancelled,
}

/// A match as the contract stores it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchState {
    pub player_a: AccountKey,
    pub player_b: AccountKey,
    pub token: ContractId,
    pub stake: i128,
    pub expires_at: u64,
    pub a_funded: bool,
    pub b_funded: bool,
    pub status: MatchStatus,
}

fn field<'a>(map: &'a ScMap, name: &str) -> Result<&'a ScVal, String> {
    map.0
        .iter()
        .find(|e| matches!(&e.key, ScVal::Symbol(s) if s.0.as_slice() == name.as_bytes()))
        .map(|e| &e.val)
        .ok_or_else(|| format!("the match has no field {name}"))
}

fn account_of(v: &ScVal) -> Result<AccountKey, String> {
    match v {
        ScVal::Address(ScAddress::Account(stellar_xdr::AccountId(
            stellar_xdr::PublicKey::PublicKeyTypeEd25519(k),
        ))) => Ok(AccountKey(k.0)),
        other => Err(format!("expected an account address, got {other:?}")),
    }
}

/// Reads the `Match` the contract returns from `get_match`.
pub fn parse_match(v: &ScVal) -> Result<MatchState, String> {
    let ScVal::Map(Some(map)) = v else {
        return Err("a match is a map".into());
    };
    let boolean = |name: &str| match field(map, name)? {
        ScVal::Bool(b) => Ok(*b),
        other => Err(format!("{name}: expected a bool, got {other:?}")),
    };
    let token = match field(map, "token")? {
        ScVal::Address(ScAddress::Contract(stellar_xdr::ContractId(h))) => ContractId(h.0),
        other => return Err(format!("token: expected a contract address, got {other:?}")),
    };
    let stake = match field(map, "stake")? {
        ScVal::I128(p) => ((p.hi as i128) << 64) | p.lo as i128,
        other => return Err(format!("stake: expected an i128, got {other:?}")),
    };
    let expires_at = match field(map, "expires_at")? {
        ScVal::U64(n) => *n,
        other => return Err(format!("expires_at: expected a u64, got {other:?}")),
    };
    let status = match field(map, "status")? {
        ScVal::Vec(Some(items)) => match items.0.as_slice() {
            [ScVal::Symbol(s)] => match s.0.as_slice() {
                b"Open" => MatchStatus::Open,
                b"Settled" => MatchStatus::Settled,
                b"Cancelled" => MatchStatus::Cancelled,
                other => {
                    return Err(format!(
                        "unknown match status {}",
                        String::from_utf8_lossy(other)
                    ))
                }
            },
            other => return Err(format!("status: unexpected shape {other:?}")),
        },
        other => return Err(format!("status: expected a variant, got {other:?}")),
    };
    Ok(MatchState {
        player_a: account_of(field(map, "player_a")?)?,
        player_b: account_of(field(map, "player_b")?)?,
        token,
        stake,
        expires_at,
        a_funded: boolean("a_funded")?,
        b_funded: boolean("b_funded")?,
        status,
    })
}

/// The contract's own error codes (its README lists them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscrowError {
    NotInitialized,
    InvalidAmount,
    SamePlayer,
    InvalidExpiry,
    MatchExists,
    MatchNotFound,
    NotOpen,
    Expired,
    NotExpired,
    NotAPlayer,
    AlreadyDeposited,
    NotFunded,
    NothingToClaim,
    NoPendingAdmin,
}

/// Finds the contract error in a failure message such as `Error(Contract, #13)`. Careful with 11 and 14: the README notes
/// that a failure inside the token can show up under a contract's own numbers, so check the match before trusting them
/// from `deposit` and `claim`.
pub fn contract_error(message: &str) -> Option<EscrowError> {
    let at = message.find("Error(Contract, #")? + "Error(Contract, #".len();
    let digits: String = message[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    Some(match digits.parse::<u32>().ok()? {
        2 => EscrowError::NotInitialized,
        3 => EscrowError::InvalidAmount,
        4 => EscrowError::SamePlayer,
        5 => EscrowError::InvalidExpiry,
        6 => EscrowError::MatchExists,
        7 => EscrowError::MatchNotFound,
        8 => EscrowError::NotOpen,
        9 => EscrowError::Expired,
        10 => EscrowError::NotExpired,
        11 => EscrowError::NotAPlayer,
        12 => EscrowError::AlreadyDeposited,
        13 => EscrowError::NotFunded,
        14 => EscrowError::NothingToClaim,
        15 => EscrowError::NoPendingAdmin,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use stellar_xdr::{Int128Parts, ScMapEntry, ScSymbol, StringM};

    fn sym(s: &str) -> ScVal {
        ScVal::Symbol(ScSymbol(
            StringM::<32>::try_from(s.as_bytes().to_vec()).unwrap(),
        ))
    }

    fn entry(k: &str, v: ScVal) -> ScMapEntry {
        ScMapEntry {
            key: sym(k),
            val: v,
        }
    }

    fn sample(status: &str, a_funded: bool) -> ScVal {
        let entries = vec![
            entry("a_funded", ScVal::Bool(a_funded)),
            entry("b_funded", ScVal::Bool(false)),
            entry("expires_at", ScVal::U64(1_800_000_000)),
            entry("player_a", val::account(&AccountKey([1; 32]))),
            entry("player_b", val::account(&AccountKey([2; 32]))),
            entry(
                "stake",
                ScVal::I128(Int128Parts {
                    hi: 0,
                    lo: 10_000_000,
                }),
            ),
            entry("status", val::unit_variant(status)),
            entry("token", val::contract(&ContractId([3; 32]))),
        ];
        ScVal::Map(Some(ScMap(entries.try_into().unwrap())))
    }

    #[test]
    fn a_match_the_contract_returns_is_read_back() {
        let m = parse_match(&sample("Open", true)).unwrap();
        assert_eq!(
            m,
            MatchState {
                player_a: AccountKey([1; 32]),
                player_b: AccountKey([2; 32]),
                token: ContractId([3; 32]),
                stake: 10_000_000,
                expires_at: 1_800_000_000,
                a_funded: true,
                b_funded: false,
                status: MatchStatus::Open,
            }
        );
        assert_eq!(
            parse_match(&sample("Settled", false)).unwrap().status,
            MatchStatus::Settled
        );
        assert_eq!(
            parse_match(&sample("Cancelled", false)).unwrap().status,
            MatchStatus::Cancelled
        );
        assert!(parse_match(&sample("Exploded", false)).is_err());
        assert!(parse_match(&ScVal::Void).is_err());
    }

    #[test]
    fn the_calls_have_the_arguments_the_contract_expects() {
        let e = Escrow::new(ContractId([5; 32]));
        let id = [4u8; 16];
        let (a, b) = (AccountKey([1; 32]), AccountKey([2; 32]));
        let create = e.create(id, &a, &b, &ContractId([3; 32]), 10_000_000, 1_800_000_000);
        assert_eq!(create.function, "create");
        assert_eq!(
            create.args.len(),
            6,
            "match_id, player_a, player_b, token, stake, expires_at"
        );
        assert_eq!(e.deposit(id, &a).args.len(), 2);
        assert_eq!(
            e.settle(id, Outcome::Draw).args[1],
            val::unit_variant("Draw")
        );
        assert_eq!(e.cancel(id).function, "cancel");
        assert_eq!(e.refund_expired(id).args.len(), 1);
        assert_eq!(e.claim(id, &b).function, "claim");
        assert_eq!(e.admin().args.len(), 0);
    }

    #[test]
    fn the_contracts_error_codes_are_recognised_in_a_failure_message() {
        let msg = "HostError: Error(Contract, #13) event log ...";
        assert_eq!(contract_error(msg), Some(EscrowError::NotFunded));
        assert_eq!(
            contract_error("Error(Contract, #8)"),
            Some(EscrowError::NotOpen)
        );
        assert_eq!(contract_error("Error(Contract, #99)"), None);
        assert_eq!(contract_error("something else entirely"), None);
    }
}
