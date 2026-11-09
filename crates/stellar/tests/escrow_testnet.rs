//! The escrow contract on the real Stellar testnet, driven by our own code instead of the command-line tool.
//!
//! Needs the network and testnet keys, so it is ignored by default. Run it with:
//!   ESCROW_CONTRACT=C... ESCROW_TOKEN=C... ADMIN_SECRET_FILE=... PLAYER_A_SECRET_FILE=... PLAYER_B_SECRET_FILE=... \
//!   cargo test -p eschess-stellar --test escrow_testnet -- --ignored --nocapture
//! The keys must be testnet keys (the signer refuses anything else), the admin must be the contract's current admin, and
//! both players need a USDC trustline and a little USDC (the stake is 0.5).

use std::path::Path;
use std::time::Duration;

use eschess_domain::stellar::ContractId;
use eschess_stellar::escrow::{
    contract_error, parse_match, Escrow, EscrowError, MatchStatus, Outcome,
};
use eschess_stellar::horizon::HorizonChain;
use eschess_stellar::signer::{LocalSigner, Signer};
use eschess_stellar::soroban::{
    prepare, send_and_wait, simulate, ContractCall, Resolved, SorobanError, SorobanHttp,
};
use eschess_stellar::tx::Network;
use eschess_stellar::Chain;

const STAKE: i128 = 5_000_000; // 0.5 USDC (seven decimals)

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("set {name}"))
}

fn signer(name: &str) -> LocalSigner {
    LocalSigner::from_file(Path::new(&env(name)), Network::Testnet).expect("a testnet key file")
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

struct Net {
    rpc: SorobanHttp,
    horizon: HorizonChain,
}

impl Net {
    /// Prepares, sends and waits for one call signed by `who`.
    async fn run(&self, who: &LocalSigner, call: &ContractCall) -> Result<Resolved, SorobanError> {
        let account = self
            .horizon
            .account(&who.public_key().to_g())
            .await
            .unwrap()
            .expect("the account exists");
        let prepared = prepare(
            &self.rpc,
            Network::Testnet,
            who,
            call,
            account.sequence + 1,
            now() + 120,
        )
        .await?;
        println!("  {} -> {}", call.function, prepared.signed.hash_hex());
        send_and_wait(
            &self.rpc,
            &prepared,
            Duration::from_secs(60),
            Duration::from_secs(2),
        )
        .await
    }

    async fn read(
        &self,
        who: &LocalSigner,
        call: &ContractCall,
    ) -> eschess_stellar::soroban::Simulation {
        let account = self
            .horizon
            .account(&who.public_key().to_g())
            .await
            .unwrap()
            .unwrap();
        simulate(&self.rpc, &who.public_key(), account.sequence + 1, call)
            .await
            .unwrap()
    }
}

#[tokio::test]
#[ignore = "needs the Stellar testnet and testnet keys; see the file header"]
async fn a_match_is_created_funded_and_settled_through_our_own_code() {
    let net = Net {
        rpc: SorobanHttp::new("https://soroban-testnet.stellar.org").unwrap(),
        horizon: HorizonChain::new("https://horizon-testnet.stellar.org").unwrap(),
    };
    let escrow = Escrow::new(ContractId::parse(&env("ESCROW_CONTRACT")).unwrap());
    let token = ContractId::parse(&env("ESCROW_TOKEN")).unwrap();
    let (admin, a, b) = (
        signer("ADMIN_SECRET_FILE"),
        signer("PLAYER_A_SECRET_FILE"),
        signer("PLAYER_B_SECRET_FILE"),
    );

    // Reading needs no signature: the contract's admin is who we think it is.
    let sim = net.read(&admin, &escrow.admin()).await;
    println!("admin on chain: {:?}", sim.return_value);

    let id: [u8; 16] = *uuid_like();
    let call = escrow.create(
        id,
        &a.public_key(),
        &b.public_key(),
        &token,
        STAKE,
        now() + 3600,
    );
    assert!(matches!(
        net.run(&admin, &call).await.unwrap(),
        Resolved::Success { .. }
    ));

    let state = parse_match(
        &net.read(&admin, &escrow.get_match(id))
            .await
            .return_value
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        (state.status, state.stake, state.a_funded, state.b_funded),
        (MatchStatus::Open, STAKE, false, false)
    );

    // Settling too early is caught by the simulation, and nothing is signed or sent.
    let early = net
        .run(&admin, &escrow.settle(id, Outcome::WinA))
        .await
        .unwrap_err();
    let SorobanError::Simulation(msg) = early else {
        panic!("expected the simulation to refuse, got {early:?}")
    };
    assert_eq!(contract_error(&msg), Some(EscrowError::NotFunded));

    assert!(matches!(
        net.run(&a, &escrow.deposit(id, &a.public_key()))
            .await
            .unwrap(),
        Resolved::Success { .. }
    ));
    assert!(matches!(
        net.run(&b, &escrow.deposit(id, &b.public_key()))
            .await
            .unwrap(),
        Resolved::Success { .. }
    ));
    let state = parse_match(
        &net.read(&admin, &escrow.get_match(id))
            .await
            .return_value
            .unwrap(),
    )
    .unwrap();
    assert!(state.a_funded && state.b_funded);

    assert!(matches!(
        net.run(&admin, &escrow.settle(id, Outcome::WinA))
            .await
            .unwrap(),
        Resolved::Success { .. }
    ));
    let state = parse_match(
        &net.read(&admin, &escrow.get_match(id))
            .await
            .return_value
            .unwrap(),
    )
    .unwrap();
    assert_eq!(state.status, MatchStatus::Settled);

    // A settled match cannot be settled again.
    let again = net
        .run(&admin, &escrow.settle(id, Outcome::WinB))
        .await
        .unwrap_err();
    let SorobanError::Simulation(msg) = again else {
        panic!("expected a refusal")
    };
    assert_eq!(contract_error(&msg), Some(EscrowError::NotOpen));
}

/// 16 bytes that are unique to this run (a v7-style time prefix and some randomness from the clock).
fn uuid_like() -> Box<[u8; 16]> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let mut b = [0u8; 16];
    b.copy_from_slice(&nanos.to_be_bytes());
    Box::new(b)
}
