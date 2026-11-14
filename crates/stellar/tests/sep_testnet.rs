#![cfg(feature = "live-testnet")]
//! The real Stellar test anchor (testanchor.stellar.org, run by the Stellar Development Foundation): its `stellar.toml`, the
//! SEP-10 login with a throwaway key, and the start of a SEP-24 deposit and withdrawal. Needs the network, nothing else.
//!
//!   cargo test -p eschess-stellar --features live-testnet --test sep_testnet -- --ignored --nocapture

use ed25519_dalek::{Signer as _, SigningKey};
use eschess_domain::stellar::AccountKey;
use eschess_stellar::sep::{Anchor, HttpAnchor, Kind};
use eschess_stellar::signer::{SignError, Signer};
use eschess_stellar::tx::Network;

const ISSUER: &str = "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5";

struct Throwaway(SigningKey);

#[async_trait::async_trait]
impl Signer for Throwaway {
    fn public_key(&self) -> AccountKey {
        AccountKey(self.0.verifying_key().to_bytes())
    }
    async fn sign(&self, hash: &[u8; 32]) -> Result<[u8; 64], SignError> {
        Ok(self.0.sign(hash).to_bytes())
    }
}

#[tokio::test]
#[ignore = "needs the network"]
async fn the_real_test_anchor_logs_us_in_and_opens_its_screens() {
    let anchor = HttpAnchor::connect(
        "testanchor.stellar.org",
        "https://testanchor.stellar.org",
        Network::Testnet,
        ("USDC", ISSUER),
    )
    .await
    .expect("its stellar.toml lists our USDC");
    let mut seed = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut seed);
    let user = Throwaway(SigningKey::from_bytes(&seed));
    let token = anchor.authenticate(&user).await.expect("the SEP-10 login");
    for kind in [Kind::Deposit, Kind::Withdraw] {
        let i = anchor
            .start(&token, kind, "USDC", &user.public_key())
            .await
            .unwrap_or_else(|e| panic!("starting a {}: {e}", kind.as_str()));
        // The address carries a login token for the screen: print where it points, not the token.
        println!(
            "{} screen on {}  (id {})",
            kind.as_str(),
            i.url.split('?').next().unwrap_or(""),
            i.id
        );
        let t = anchor
            .transaction(&token, &i.id)
            .await
            .expect("reading it back");
        println!("  status {:?}, kind {}", t.status, t.kind);
        assert_eq!(t.id, i.id);
    }
}
