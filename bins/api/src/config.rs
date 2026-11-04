use eschess_domain::money::AssetId;
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
}

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

fn var<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let d = Config::default();
        let wallet_assets = match std::env::var("WALLET_ASSETS") {
            Ok(list) if !list.trim().is_empty() => list
                .split(',')
                .map(|a| AssetId::parse(a.trim()).map_err(|e| format!("WALLET_ASSETS: {a:?}: {e}")))
                .collect::<Result<Vec<_>, _>>()?,
            _ => d.wallet_assets.clone(),
        };
        Ok(Config {
            access_secs: var("ACCESS_TOKEN_SECONDS", d.access_secs),
            refresh_secs: var("REFRESH_TOKEN_SECONDS", d.refresh_secs),
            session_max_secs: var("SESSION_MAX_SECONDS", d.session_max_secs),
            max_failed_logins: var("MAX_FAILED_LOGINS", d.max_failed_logins),
            lockout_minutes: var("LOCKOUT_MINUTES", d.lockout_minutes),
            verify_ttl_secs: var("VERIFY_LINK_SECONDS", d.verify_ttl_secs),
            reset_ttl_secs: var("RESET_LINK_SECONDS", d.reset_ttl_secs),
            email_cooldown_secs: var("EMAIL_COOLDOWN_SECONDS", d.email_cooldown_secs),
            wallet_assets,
            // Realtime settings keep their defaults (tests tune them directly).
            ..d
        })
    }

    pub fn lifetimes(&self) -> Lifetimes {
        Lifetimes {
            access_secs: self.access_secs,
            refresh_secs: self.refresh_secs,
            session_max_secs: self.session_max_secs,
        }
    }
}
