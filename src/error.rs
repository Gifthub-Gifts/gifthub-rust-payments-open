//! Provider status-lookup error type.
//!
//! House style is a hand-written enum with a manual `Display` (no `anyhow`/`thiserror`), mirroring
//! `gifthub-rust-ai`'s `ProviderError`. Much smaller than that one: there's no failover chain here,
//! so callers only need "did the lookup succeed" plus a human-readable reason to relay to Django.

use std::time::Duration;

#[derive(Debug)]
pub enum LookupError {
    /// Non-2xx response from the provider (other than 429, which gets its own variant).
    Api { status: u16, body: String },
    /// 429 Too Many Requests. `retry_after` is the provider's own `Retry-After` header value, if it
    /// sent one — the rate limiter honors it directly instead of guessing.
    RateLimited { retry_after: Option<Duration> },
    /// Transport-level failure (connection refused, DNS, body read, TLS).
    Network(String),
    /// Request exceeded the client timeout.
    Timeout,
    /// `job.provider` wasn't `"nowpayments"` or `"cryptomus"`.
    UnknownProvider(String),
}

impl std::fmt::Display for LookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LookupError::Api { status, body } => write!(f, "api error {status}: {body}"),
            LookupError::RateLimited { retry_after } => match retry_after {
                Some(d) => write!(f, "rate limited (retry after {d:?})"),
                None => write!(f, "rate limited"),
            },
            LookupError::Network(m) => write!(f, "network error: {m}"),
            LookupError::Timeout => write!(f, "request timed out"),
            LookupError::UnknownProvider(p) => write!(f, "unknown provider: {p}"),
        }
    }
}

impl std::error::Error for LookupError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_is_human_readable() {
        assert_eq!(LookupError::Timeout.to_string(), "request timed out");
        assert_eq!(
            LookupError::Api {
                status: 502,
                body: "bad gateway".into()
            }
            .to_string(),
            "api error 502: bad gateway"
        );
        assert_eq!(
            LookupError::UnknownProvider("paypal".into()).to_string(),
            "unknown provider: paypal"
        );
        assert_eq!(
            LookupError::RateLimited { retry_after: None }.to_string(),
            "rate limited"
        );
        assert_eq!(
            LookupError::RateLimited {
                retry_after: Some(Duration::from_secs(30))
            }
            .to_string(),
            "rate limited (retry after 30s)"
        );
    }
}
