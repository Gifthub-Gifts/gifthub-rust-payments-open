//! Per-provider 429-learning rate limiter.
//!
//! Unlike `gifthub-rust-ai`'s limiter, this doesn't assume the provider returns structured quota
//! info in the 429 body (Google's Gemini API does; NOWPayments/Cryptomus don't document one) — it
//! honors the standard `Retry-After` header when the provider sends it, and falls back to
//! exponential backoff when it doesn't. Learned backoff is kept in-memory only (not persisted across
//! restarts) — this gateway deliberately has no Redis dependency, and a restart re-learning within a
//! few requests is a cheap enough cost.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const MAX_BACKOFF_SECS: u64 = 300;

struct ProviderState {
    blocked_until: Option<Instant>,
    consecutive_429s: u32,
}

pub struct RateLimiter {
    state: Mutex<HashMap<String, ProviderState>>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(HashMap::new()),
        }
    }

    /// True if `provider` is currently clear to call.
    pub fn is_ready(&self, provider: &str) -> bool {
        let state = self.state.lock().expect("ratelimit mutex poisoned");
        match state.get(provider).and_then(|s| s.blocked_until) {
            Some(deadline) => Instant::now() >= deadline,
            None => true,
        }
    }

    /// A request to `provider` succeeded — clear the consecutive-429 streak. Doesn't lift an
    /// still-in-the-future `blocked_until` early; success just means we were allowed through.
    pub fn record_success(&self, provider: &str) {
        let mut state = self.state.lock().expect("ratelimit mutex poisoned");
        if let Some(s) = state.get_mut(provider) {
            s.consecutive_429s = 0;
        }
    }

    /// A 429 came back. Honors the provider's own `retry_after` if given; otherwise backs off
    /// exponentially (2s, 4s, 8s, ... capped at 5 minutes) based on the consecutive-429 streak, so a
    /// persistent rate-limit storm doesn't thrash — a fresh 429 right after backing off pushes the
    /// deadline out further rather than resetting to the base delay.
    pub fn record_429(&self, provider: &str, retry_after: Option<Duration>) {
        let mut state = self.state.lock().expect("ratelimit mutex poisoned");
        let s = state.entry(provider.to_string()).or_insert(ProviderState {
            blocked_until: None,
            consecutive_429s: 0,
        });
        s.consecutive_429s = s.consecutive_429s.saturating_add(1);
        let backoff = retry_after.unwrap_or_else(|| {
            let secs = 2u64
                .saturating_pow(s.consecutive_429s.min(8))
                .min(MAX_BACKOFF_SECS);
            Duration::from_secs(secs)
        });
        log::warn!(
            "[{provider}] rate limited — backing off {backoff:?} (consecutive 429s: {})",
            s.consecutive_429s
        );
        s.blocked_until = Some(Instant::now() + backoff);
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_by_default_and_blocked_after_429() {
        let rl = RateLimiter::new();
        assert!(rl.is_ready("nowpayments"));
        rl.record_429("nowpayments", Some(Duration::from_secs(60)));
        assert!(!rl.is_ready("nowpayments"));
        // A different provider is unaffected.
        assert!(rl.is_ready("cryptomus"));
    }

    #[test]
    fn honors_explicit_retry_after_exactly() {
        let rl = RateLimiter::new();
        rl.record_429("cryptomus", Some(Duration::from_millis(10)));
        std::thread::sleep(Duration::from_millis(20));
        assert!(rl.is_ready("cryptomus"));
    }

    #[test]
    fn exponential_backoff_grows_without_explicit_retry_after() {
        let rl = RateLimiter::new();
        rl.record_429("nowpayments", None); // 1st: 2s
        let first_state = {
            let state = rl.state.lock().unwrap();
            state.get("nowpayments").unwrap().blocked_until.unwrap()
        };
        rl.record_429("nowpayments", None); // 2nd: 4s — strictly later than the first deadline
        let second_state = {
            let state = rl.state.lock().unwrap();
            state.get("nowpayments").unwrap().blocked_until.unwrap()
        };
        assert!(second_state > first_state);
    }

    #[test]
    fn success_resets_consecutive_counter_but_not_active_block() {
        let rl = RateLimiter::new();
        rl.record_429("nowpayments", Some(Duration::from_secs(60)));
        rl.record_success("nowpayments");
        // Still blocked — success doesn't lift an active, still-future deadline.
        assert!(!rl.is_ready("nowpayments"));
        let consecutive = {
            let state = rl.state.lock().unwrap();
            state.get("nowpayments").unwrap().consecutive_429s
        };
        assert_eq!(consecutive, 0);
    }
}
