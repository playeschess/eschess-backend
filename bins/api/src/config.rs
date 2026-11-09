use eschess_domain::money::{Amount, AssetId};
use eschess_domain::stellar::AccountKey;
use eschess_storage::identity::Lifetimes;

#[derive(Clone, Debug)]
pub struct Config {
    /// Access tokens are short-lived; a refresh issues the next one.
    pub access_secs: i64,
    pub refresh_secs: i64,
    /// A session ends this long after sign-in however often it is refreshed.
    pub session_max_secs: i64,
    pub max_failed_logins: i32,
    pub lockout_minutes: i32,
    /// How long an email-confirmation link stays valid, and (much shorter) a password-reset link.
    pub verify_ttl_secs: i64,
    pub reset_ttl_secs: i64,
    /// At most one email of each kind per address in this many seconds.
    pub email_cooldown_secs: i64,
    /// Realtime (WebSocket) behaviour. Times are in milliseconds.
    pub ws_hello_timeout_ms: u64,
    pub ws_ping_interval_ms: u64,
    /// A connection that has not answered a ping for this long is closed.
    pub ws_pong_timeout_ms: u64,
    /// How often the server looks for fallen flags and re-checks that the player's session is still valid.
    pub ws_tick_ms: u64,
    pub ws_clock_sync_ms: u64,
    pub ws_reauth_ms: u64,
    pub ws_max_message_bytes: usize,
    pub ws_max_subscriptions: usize,
    pub ws_max_messages_per_second: u32,
    /// A custodial wallet account is created for each of these when a player signs up.
    pub wallet_assets: Vec<AssetId>,
    /// Stellar deposits. `None` (the default) means the platform has no wallet on the network and deposits are off.
    pub stellar: Option<StellarConfig>,
    /// The stakes offered for staked games, in minor units of USDC (ADR 0004 §1). Empty (the default) means staked games
    /// are off. Needs Stellar, since the stake is USDC.
    pub stakes_minor: Vec<i64>,
}

impl Config {
    /// What staked games offer, or `None` while they are off.
    pub fn stake_policy(&self) -> Option<eschess_storage::stakes::StakePolicy> {
        let st = self.stellar.as_ref()?;
        (!self.stakes_minor.is_empty()).then(|| eschess_storage::stakes::StakePolicy {
            asset: st.usdc.clone(),
            allowed_minor: self.stakes_minor.clone(),
        })
    }
}

fn stakes_from(
    get: &dyn Fn(&str) -> Option<String>,
    stellar: &Option<StellarConfig>,
) -> Result<Vec<i64>, String> {
    let Some(list) = get("STAKES_USDC").filter(|l| !l.trim().is_empty()) else {
        return Ok(Vec::new());
    };
    let Some(st) = stellar else {
        return Err("STAKES_USDC needs Stellar to be configured (the stake is USDC)".into());
    };
    let mut out = Vec::new();
    for part in list.split(',') {
        let minor = Amount::from_decimal_str(part.trim(), st.usdc.clone())
            .map_err(|e| format!("STAKES_USDC: {part:?} is not an amount ({e})"))?
            .minor();
        if minor <= 0 {
            return Err("STAKES_USDC: every stake must be more than zero".into());
        }
        if !out.contains(&minor) {
            out.push(minor);
        }
    }
    Ok(out)
}

/// The platform's connection to the Stellar network.
#[derive(Clone, Debug)]
pub struct StellarConfig {
    /// `testnet` is the only network supported for now; real money on the public network is a deliberate later step.
    pub network: String,
    pub horizon_url: String,
    /// The platform wallet that receives every deposit.
    pub hot_account: AccountKey,
    /// The asset players deposit (USDC, from one specific issuer).
    pub usdc: AssetId,
    /// How often the wallet is checked for new payments.
    pub poll_ms: u64,
    /// Limits on withdrawals of `usdc`.
    pub withdraw: WithdrawLimits,
}

/// Limits on withdrawals, in minor units of the withdrawn asset (seven decimals: 1 USDC is 10,000,000).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WithdrawLimits {
    /// The smallest withdrawal accepted. Below it the network fee and the work are not worth it.
    pub min_minor: i64,
    /// At or below this a withdrawal is sent without a person approving it.
    pub auto_approve_minor: i64,
    /// The most one player may withdraw in any 24 hours.
    pub daily_limit_minor: i64,
}

impl Default for WithdrawLimits {
    /// 1 USDC minimum, up to 100 USDC sent without approval, 1,000 USDC a day. Starting values for the testnet; the real
    /// figures are a product decision.
    fn default() -> Self {
        WithdrawLimits {
            min_minor: 10_000_000,
            auto_approve_minor: 1_000_000_000,
            daily_limit_minor: 10_000_000_000,
        }
    }
}

/// Reads a USDC amount such as `100` or `0.5`; absent or empty gives the default. Anything else stops the server starting.
fn usdc_amount(
    get: &dyn Fn(&str) -> Option<String>,
    name: &str,
    usdc: &AssetId,
    default: i64,
) -> Result<i64, String> {
    match get(name) {
        None => Ok(default),
        Some(v) if v.trim().is_empty() => Ok(default),
        Some(v) => Amount::from_decimal_str(v.trim(), usdc.clone())
            .map(|a| a.minor())
            .map_err(|e| format!("{name}: {v:?} is not an amount ({e})")),
    }
}

fn withdraw_limits_from(
    get: &dyn Fn(&str) -> Option<String>,
    usdc: &AssetId,
) -> Result<WithdrawLimits, String> {
    let d = WithdrawLimits::default();
    let limits = WithdrawLimits {
        min_minor: usdc_amount(get, "WITHDRAW_MIN_USDC", usdc, d.min_minor)?,
        auto_approve_minor: usdc_amount(
            get,
            "WITHDRAW_AUTO_APPROVE_USDC",
            usdc,
            d.auto_approve_minor,
        )?,
        daily_limit_minor: usdc_amount(
            get,
            "WITHDRAW_DAILY_LIMIT_USDC",
            usdc,
            d.daily_limit_minor,
        )?,
    };
    if limits.min_minor == 0 {
        return Err("WITHDRAW_MIN_USDC must be more than zero".into());
    }
    if limits.daily_limit_minor < limits.min_minor {
        return Err(
            "WITHDRAW_DAILY_LIMIT_USDC is below WITHDRAW_MIN_USDC: nobody could ever withdraw"
                .into(),
        );
    }
    Ok(limits)
}

/// Circle's USDC issuer on the Stellar testnet (https://developers.circle.com/stablecoins/usdc-on-test-networks).
pub const TESTNET_USDC_ISSUER: &str = "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5";
pub const TESTNET_HORIZON_URL: &str = "https://horizon-testnet.stellar.org";

impl Default for Config {
    fn default() -> Self {
        Config {
            access_secs: 15 * 60,
            refresh_secs: 30 * 24 * 3600,
            session_max_secs: 90 * 24 * 3600,
            max_failed_logins: 5,
            lockout_minutes: 15,
            verify_ttl_secs: 24 * 3600,
            reset_ttl_secs: 30 * 60,
            email_cooldown_secs: 60,
            wallet_assets: vec![AssetId::native()],
            stellar: None,
            stakes_minor: Vec::new(),
            ws_hello_timeout_ms: 5_000,
            ws_ping_interval_ms: 10_000,
            ws_pong_timeout_ms: 30_000,
            ws_tick_ms: 1_000,
            ws_clock_sync_ms: 5_000,
            ws_reauth_ms: 5_000,
            ws_max_message_bytes: 16 * 1024,
            ws_max_subscriptions: 20,
            ws_max_messages_per_second: 30,
        }
    }
}

fn var_from<T: std::str::FromStr>(
    get: &dyn Fn(&str) -> Option<String>,
    name: &str,
    default: T,
) -> T {
    get(name).and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Like `var_from`, but a value that is present and not a valid number is an error, so a typo in a safety limit stops the
/// server from starting instead of silently running with the default.
fn strict<T: std::str::FromStr>(
    get: &dyn Fn(&str) -> Option<String>,
    name: &str,
    default: T,
) -> Result<T, String> {
    match get(name) {
        None => Ok(default),
        Some(v) if v.trim().is_empty() => Ok(default),
        Some(v) => v
            .trim()
            .parse()
            .map_err(|_| format!("{name}: {v:?} is not a valid number")),
    }
}

/// Reads the Stellar settings. Deposits are on only when `STELLAR_HOT_ACCOUNT` is set.
fn stellar_from(get: &dyn Fn(&str) -> Option<String>) -> Result<Option<StellarConfig>, String> {
    let Some(hot) = get("STELLAR_HOT_ACCOUNT").filter(|v| !v.trim().is_empty()) else {
        return Ok(None);
    };
    let network = get("STELLAR_NETWORK")
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "testnet".into());
    if network != "testnet" {
        return Err(format!(
            "STELLAR_NETWORK: {network:?} is not supported yet; only \"testnet\" (the public network is a deliberate later step)"
        ));
    }
    let hot_account =
        AccountKey::parse(hot.trim()).map_err(|e| format!("STELLAR_HOT_ACCOUNT: {e}"))?;
    let issuer = get("STELLAR_USDC_ISSUER")
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| TESTNET_USDC_ISSUER.into());
    let usdc =
        AssetId::credit("USDC", issuer.trim()).map_err(|e| format!("STELLAR_USDC_ISSUER: {e}"))?;
    let horizon_url = get("STELLAR_HORIZON_URL")
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| TESTNET_HORIZON_URL.into());
    if !horizon_url.starts_with("https://")
        && !horizon_url.starts_with("http://127.0.0.1")
        && !horizon_url.starts_with("http://localhost")
    {
        return Err("STELLAR_HORIZON_URL must be an https:// address".into());
    }
    let poll_ms = strict(get, "STELLAR_POLL_MS", 5_000u64)?;
    if poll_ms < 500 {
        return Err("STELLAR_POLL_MS must be at least 500".into());
    }
    let withdraw = withdraw_limits_from(get, &usdc)?;
    Ok(Some(StellarConfig {
        network,
        horizon_url,
        hot_account,
        usdc,
        poll_ms,
        withdraw,
    }))
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        Config::from_lookup(&|name| std::env::var(name).ok())
    }

    /// Builds the settings from any source of named values (the environment, or a map in a test).
    pub fn from_lookup(get: &dyn Fn(&str) -> Option<String>) -> Result<Self, String> {
        let d = Config::default();
        let wallet_assets = match get("WALLET_ASSETS") {
            Some(list) if !list.trim().is_empty() => list
                .split(',')
                .map(|a| AssetId::parse(a.trim()).map_err(|e| format!("WALLET_ASSETS: {a:?}: {e}")))
                .collect::<Result<Vec<_>, _>>()?,
            _ => d.wallet_assets.clone(),
        };
        let stellar = stellar_from(get)?;
        let stakes_minor = stakes_from(get, &stellar)?;
        let mut wallet_assets = wallet_assets;
        // With Stellar on, new players get a USDC wallet too (unless the list was set explicitly).
        if let Some(st) = &stellar {
            if get("WALLET_ASSETS").is_none_or(|l| l.trim().is_empty())
                && !wallet_assets.contains(&st.usdc)
            {
                wallet_assets.push(st.usdc.clone());
            }
        }
        let config = Config {
            access_secs: var_from(get, "ACCESS_TOKEN_SECONDS", d.access_secs),
            refresh_secs: var_from(get, "REFRESH_TOKEN_SECONDS", d.refresh_secs),
            session_max_secs: var_from(get, "SESSION_MAX_SECONDS", d.session_max_secs),
            max_failed_logins: var_from(get, "MAX_FAILED_LOGINS", d.max_failed_logins),
            lockout_minutes: var_from(get, "LOCKOUT_MINUTES", d.lockout_minutes),
            verify_ttl_secs: var_from(get, "VERIFY_LINK_SECONDS", d.verify_ttl_secs),
            reset_ttl_secs: var_from(get, "RESET_LINK_SECONDS", d.reset_ttl_secs),
            email_cooldown_secs: var_from(get, "EMAIL_COOLDOWN_SECONDS", d.email_cooldown_secs),
            wallet_assets,
            stellar,
            stakes_minor,
            ws_hello_timeout_ms: strict(get, "WS_HELLO_TIMEOUT_MS", d.ws_hello_timeout_ms)?,
            ws_ping_interval_ms: strict(get, "WS_PING_INTERVAL_MS", d.ws_ping_interval_ms)?,
            ws_pong_timeout_ms: strict(get, "WS_PONG_TIMEOUT_MS", d.ws_pong_timeout_ms)?,
            ws_tick_ms: strict(get, "WS_TICK_MS", d.ws_tick_ms)?,
            ws_clock_sync_ms: strict(get, "WS_CLOCK_SYNC_MS", d.ws_clock_sync_ms)?,
            ws_reauth_ms: strict(get, "WS_REAUTH_MS", d.ws_reauth_ms)?,
            ws_max_message_bytes: strict(get, "WS_MAX_MESSAGE_BYTES", d.ws_max_message_bytes)?,
            ws_max_subscriptions: strict(get, "WS_MAX_SUBSCRIPTIONS", d.ws_max_subscriptions)?,
            ws_max_messages_per_second: strict(
                get,
                "WS_MAX_MESSAGES_PER_SECOND",
                d.ws_max_messages_per_second,
            )?,
        };
        config.check_realtime()?;
        Ok(config)
    }

    /// Refuses realtime settings that would make the server misbehave (a zero timer spins; a pong timeout shorter than the
    /// ping interval would close every healthy connection).
    pub fn check_realtime(&self) -> Result<(), String> {
        let positive = [
            ("WS_HELLO_TIMEOUT_MS", self.ws_hello_timeout_ms),
            ("WS_PING_INTERVAL_MS", self.ws_ping_interval_ms),
            ("WS_PONG_TIMEOUT_MS", self.ws_pong_timeout_ms),
            ("WS_TICK_MS", self.ws_tick_ms),
            ("WS_CLOCK_SYNC_MS", self.ws_clock_sync_ms),
            ("WS_REAUTH_MS", self.ws_reauth_ms),
            ("WS_MAX_MESSAGE_BYTES", self.ws_max_message_bytes as u64),
            ("WS_MAX_SUBSCRIPTIONS", self.ws_max_subscriptions as u64),
            (
                "WS_MAX_MESSAGES_PER_SECOND",
                self.ws_max_messages_per_second as u64,
            ),
        ];
        for (name, value) in positive {
            if value == 0 {
                return Err(format!("{name} must be greater than zero"));
            }
        }
        if self.ws_pong_timeout_ms <= self.ws_ping_interval_ms {
            return Err(format!(
                "WS_PONG_TIMEOUT_MS ({}) must be longer than WS_PING_INTERVAL_MS ({}), or every healthy connection would be closed",
                self.ws_pong_timeout_ms, self.ws_ping_interval_ms
            ));
        }
        Ok(())
    }

    pub fn lifetimes(&self) -> Lifetimes {
        Lifetimes {
            access_secs: self.access_secs,
            refresh_secs: self.refresh_secs,
            session_max_secs: self.session_max_secs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn load(pairs: &[(&str, &str)]) -> Result<Config, String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(&|name| map.get(name).cloned())
    }

    #[test]
    fn nothing_set_gives_the_defaults() {
        let c = load(&[]).unwrap();
        let d = Config::default();
        assert_eq!(
            (
                c.ws_ping_interval_ms,
                c.ws_max_subscriptions,
                c.ws_max_message_bytes
            ),
            (
                d.ws_ping_interval_ms,
                d.ws_max_subscriptions,
                d.ws_max_message_bytes
            )
        );
    }

    #[test]
    fn websocket_limits_can_be_set_from_the_environment() {
        let c = load(&[
            ("WS_PING_INTERVAL_MS", "2000"),
            ("WS_PONG_TIMEOUT_MS", "9000"),
            ("WS_MAX_SUBSCRIPTIONS", "5"),
            ("WS_MAX_MESSAGES_PER_SECOND", "10"),
            ("WS_MAX_MESSAGE_BYTES", " 4096 "),
        ])
        .unwrap();
        assert_eq!(
            (c.ws_ping_interval_ms, c.ws_pong_timeout_ms),
            (2_000, 9_000)
        );
        assert_eq!(
            (
                c.ws_max_subscriptions,
                c.ws_max_messages_per_second,
                c.ws_max_message_bytes
            ),
            (5, 10, 4_096)
        );
        // An empty value means "use the default", like an unset one.
        assert_eq!(
            load(&[("WS_TICK_MS", "")]).unwrap().ws_tick_ms,
            Config::default().ws_tick_ms
        );
    }

    #[test]
    fn a_typo_in_a_limit_stops_the_server_starting() {
        let e = load(&[("WS_MAX_SUBSCRIPTIONS", "twenty")]).unwrap_err();
        assert!(
            e.contains("WS_MAX_SUBSCRIPTIONS") && e.contains("twenty"),
            "{e}"
        );
        assert!(load(&[("WS_TICK_MS", "-5")]).is_err());
        assert!(load(&[("WS_PING_INTERVAL_MS", "1.5")]).is_err());
    }

    #[test]
    fn settings_that_would_break_the_server_are_refused() {
        let zero = load(&[("WS_TICK_MS", "0")]).unwrap_err();
        assert!(zero.contains("WS_TICK_MS"), "{zero}");
        assert!(load(&[("WS_MAX_MESSAGES_PER_SECOND", "0")]).is_err());
        // Pings sent less often than the connection is allowed to stay silent would close everyone.
        let e = load(&[
            ("WS_PING_INTERVAL_MS", "30000"),
            ("WS_PONG_TIMEOUT_MS", "30000"),
        ])
        .unwrap_err();
        assert!(e.contains("WS_PONG_TIMEOUT_MS"), "{e}");
        assert!(Config::default().check_realtime().is_ok());
    }

    const HOT: &str = "GBL4BGHUU6TAX7WXPSXAES7XBKEPJWLOO3HBIOYHNJH4KMLQZIQVZIF5";

    #[test]
    fn deposits_are_off_unless_a_wallet_is_configured() {
        let c = load(&[]).unwrap();
        assert!(c.stellar.is_none());
        assert_eq!(c.wallet_assets, vec![AssetId::native()]);
    }

    #[test]
    fn a_configured_wallet_turns_deposits_on_with_testnet_usdc_by_default() {
        let c = load(&[("STELLAR_HOT_ACCOUNT", HOT)]).unwrap();
        let st = c.stellar.unwrap();
        assert_eq!(
            (st.network.as_str(), st.horizon_url.as_str(), st.poll_ms),
            ("testnet", TESTNET_HORIZON_URL, 5_000)
        );
        assert_eq!(st.usdc.to_string(), format!("USDC:{TESTNET_USDC_ISSUER}"));
        assert_eq!(st.hot_account.to_g(), HOT);
        // New players get a USDC wallet as well as the XLM one.
        assert!(c.wallet_assets.contains(&st.usdc) && c.wallet_assets.len() == 2);
        // An explicit list is respected as it is.
        let explicit = load(&[("STELLAR_HOT_ACCOUNT", HOT), ("WALLET_ASSETS", "XLM")]).unwrap();
        assert_eq!(explicit.wallet_assets, vec![AssetId::native()]);
    }

    #[test]
    fn withdrawal_limits_are_read_as_usdc_amounts() {
        let st = load(&[("STELLAR_HOT_ACCOUNT", HOT)])
            .unwrap()
            .stellar
            .unwrap();
        assert_eq!(st.withdraw, WithdrawLimits::default());
        let st = load(&[
            ("STELLAR_HOT_ACCOUNT", HOT),
            ("WITHDRAW_MIN_USDC", "0.5"),
            ("WITHDRAW_AUTO_APPROVE_USDC", "20"),
            ("WITHDRAW_DAILY_LIMIT_USDC", "250.25"),
        ])
        .unwrap()
        .stellar
        .unwrap();
        assert_eq!(
            st.withdraw,
            WithdrawLimits {
                min_minor: 5_000_000,
                auto_approve_minor: 200_000_000,
                daily_limit_minor: 2_502_500_000,
            }
        );
    }

    #[test]
    fn withdrawal_limits_that_make_no_sense_stop_the_server() {
        let with = |k: &str, v: &str| load(&[("STELLAR_HOT_ACCOUNT", HOT), (k, v)]);
        assert!(with("WITHDRAW_MIN_USDC", "1,000")
            .unwrap_err()
            .contains("WITHDRAW_MIN_USDC"));
        assert!(with("WITHDRAW_MIN_USDC", "-1").is_err());
        assert!(with("WITHDRAW_MIN_USDC", "0").is_err());
        assert!(
            with("WITHDRAW_AUTO_APPROVE_USDC", "0.00000001").is_err(),
            "more than 7 decimals"
        );
        assert!(
            with("WITHDRAW_DAILY_LIMIT_USDC", "0.5").is_err(),
            "below the minimum"
        );
        assert!(
            with("WITHDRAW_AUTO_APPROVE_USDC", "0").is_ok(),
            "0: a person approves every withdrawal"
        );
    }

    #[test]
    fn stellar_settings_that_could_lose_money_are_refused() {
        assert!(load(&[("STELLAR_HOT_ACCOUNT", "GNOTANACCOUNT")])
            .unwrap_err()
            .contains("STELLAR_HOT_ACCOUNT"));
        assert!(
            load(&[(
                "STELLAR_HOT_ACCOUNT",
                "MA7QYNF7SOWQ3GLR2BGMZEHXAVIRZA4KVWLTJJFC7MGXUA74P7UJUAAAAAAAAAAAACJUQ"
            )])
            .is_err(),
            "the wallet is a plain account"
        );
        assert!(
            load(&[("STELLAR_HOT_ACCOUNT", HOT), ("STELLAR_NETWORK", "public")])
                .unwrap_err()
                .contains("not supported yet")
        );
        assert!(load(&[
            ("STELLAR_HOT_ACCOUNT", HOT),
            ("STELLAR_USDC_ISSUER", "GABC")
        ])
        .unwrap_err()
        .contains("STELLAR_USDC_ISSUER"));
        assert!(
            load(&[
                ("STELLAR_HOT_ACCOUNT", HOT),
                ("STELLAR_HORIZON_URL", "http://example.com")
            ])
            .is_err(),
            "no plain http to a remote server"
        );
        assert!(load(&[("STELLAR_HOT_ACCOUNT", HOT), ("STELLAR_POLL_MS", "10")]).is_err());
    }
}
