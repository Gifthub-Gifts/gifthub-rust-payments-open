//! Process configuration, loaded once from the environment.
//!
//! House idiom (mirrors `gifthub-rust-ai`): a flat `Config` struct, a `from_env()` constructor, and
//! a `OnceLock` accessor returning `&'static Config`. Required vars `.expect("X must be set")` so a
//! misconfigured service fails loudly at boot; optional vars parse-or-default.
//!
//! Simpler than the AI gateway on purpose: there is no provider failover chain. No Redis either —
//! this gateway is pull-based (it polls Django for what to check) rather than queue-based.

use std::env;
use std::sync::OnceLock;

use dotenv::dotenv;

static CONFIG: OnceLock<Config> = OnceLock::new();

pub fn get_config() -> &'static Config {
    CONFIG.get_or_init(Config::from_env)
}

pub struct Config {
    /// Django endpoint we GET the due-for-reconciliation list from. Required.
    pub due_url: String,
    /// Django endpoint we POST each finished [`crate::job::JobResult`] to (HMAC-signed). Required.
    pub callback_url: String,
    /// Shared secret: `Authorization: Bearer <secret>` on the due-list GET, and the
    /// `X-Payment-Signature` HMAC key on the callback POST. Must match Django's
    /// `PAYMENT_GATEWAY_WEBHOOK_SECRET`.
    pub webhook_secret: String,
    /// How often to poll the due-list. Ticks overlap (a new fetch starts on schedule regardless of
    /// whether the previous tick's items finished processing) — items are independent and idempotent.
    pub poll_interval_secs: u64,
    pub callback_max_attempts: u32,
    pub callback_timeout_secs: u64,
    pub max_concurrency: usize,
    pub connect_timeout_secs: u64,
    pub request_timeout_secs: u64,

    pub nowpayments_base_url: String,
    pub nowpayments_api_key: String,

    pub cryptomus_base_url: String,
    pub cryptomus_merchant_id: String,
    pub cryptomus_api_key: String,
}

impl Config {
    pub fn from_env() -> Self {
        dotenv().ok();

        Config {
            due_url: env::var("DJANGO_DUE_URL")
                .expect("DJANGO_DUE_URL must be set (e.g. http://127.0.0.1:8000/internal/payments/due/)"),
            callback_url: env::var("DJANGO_CALLBACK_URL").expect(
                "DJANGO_CALLBACK_URL must be set (e.g. http://127.0.0.1:8000/internal/payments/callback/)",
            ),
            webhook_secret: env::var("PAYMENT_GATEWAY_WEBHOOK_SECRET")
                .expect("PAYMENT_GATEWAY_WEBHOOK_SECRET must be set (must match Django)"),
            poll_interval_secs: parse_or("POLL_INTERVAL_SECS", 15u64),
            callback_max_attempts: parse_or("CALLBACK_MAX_ATTEMPTS", 5u32),
            callback_timeout_secs: parse_or("CALLBACK_TIMEOUT_SECS", 15u64),
            max_concurrency: parse_or("MAX_CONCURRENCY", 8usize),
            connect_timeout_secs: parse_or("CONNECT_TIMEOUT_SECS", 10u64),
            request_timeout_secs: parse_or("REQUEST_TIMEOUT_SECS", 30u64),

            nowpayments_base_url: env::var("NOWPAYMENTS_BASE_URL")
                .unwrap_or_else(|_| "https://api.nowpayments.io/v1".to_string()),
            nowpayments_api_key: env::var("NOWPAYMENTS_API_KEY").expect("NOWPAYMENTS_API_KEY must be set"),

            cryptomus_base_url: env::var("CRYPTOMUS_BASE_URL")
                .unwrap_or_else(|_| "https://api.cryptomus.com/v1".to_string()),
            cryptomus_merchant_id: env::var("CRYPTOMUS_MERCHANT_ID").expect("CRYPTOMUS_MERCHANT_ID must be set"),
            cryptomus_api_key: env::var("CRYPTOMUS_API_KEY").expect("CRYPTOMUS_API_KEY must be set"),
        }
    }
}

/// Parse an env var into `T`, falling back to `default` when unset or unparseable.
fn parse_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}
