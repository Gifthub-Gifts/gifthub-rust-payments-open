//! Deliver a finished [`JobResult`] to Django over HTTP.
//!
//! POST `application/json` to `cfg.callback_url` with header
//! `X-Payment-Signature: sha256=<hex HMAC-SHA256 of the exact body bytes, keyed by the shared secret>`,
//! which Django verifies with the same secret. Delivery is best-effort with bounded backoff: any 2xx
//! is "delivered"; on non-2xx / transport error we retry a few times then drop and log — the next
//! beat tick re-enqueues a fresh reconciliation job for the same payment, so a lost callback just
//! costs one cycle, never a stuck payment.

use std::time::Duration;

use hmac::{Hmac, Mac};
use sha2::Sha256;
use tokio::sync::watch;

use crate::config::Config;
use crate::job::JobResult;

type HmacSha256 = Hmac<Sha256>;

/// `sha256=<hex>` HMAC-SHA256 of `body` keyed by `secret`. Matches Django's `verify_signature`,
/// which recomputes the same digest over the raw request body and compares in constant time.
pub fn sign(secret: &str, body: &[u8]) -> String {
    // `new_from_slice` only errors for key sizes HMAC can't take — HMAC accepts any length, so this
    // never fails in practice.
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// POST `result` to Django, signed, retrying a bounded number of times on failure. Never panics — a
/// failed delivery must not kill the worker task.
pub async fn deliver(
    client: &reqwest::Client,
    cfg: &Config,
    shutdown: &mut watch::Receiver<bool>,
    result: &JobResult,
) {
    let body = match serde_json::to_vec(result) {
        Ok(b) => b,
        Err(e) => {
            log::error!("failed to serialize result for job {}: {e}", result.job_id);
            return;
        }
    };
    let signature = sign(&cfg.webhook_secret, &body);

    let mut backoff = 2u64;
    for attempt in 1..=cfg.callback_max_attempts {
        match client
            .post(&cfg.callback_url)
            .timeout(Duration::from_secs(cfg.callback_timeout_secs))
            .header("Content-Type", "application/json")
            .header("X-Payment-Signature", &signature)
            .body(body.clone())
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => return,
            Ok(resp) => log::warn!(
                "callback for job {} -> HTTP {} (attempt {attempt}/{})",
                result.job_id,
                resp.status(),
                cfg.callback_max_attempts
            ),
            Err(e) => log::warn!(
                "callback for job {} transport error: {e} (attempt {attempt}/{})",
                result.job_id,
                cfg.callback_max_attempts
            ),
        }
        if attempt < cfg.callback_max_attempts {
            tokio::select! {
                _ = shutdown.changed() => {
                    log::warn!(
                        "callback for job {} abandoned on shutdown (next beat tick re-enqueues)",
                        result.job_id
                    );
                    return;
                }
                _ = tokio::time::sleep(Duration::from_secs(backoff)) => {}
            }
            backoff = backoff.saturating_mul(2).saturating_add(1);
        }
    }
    log::error!(
        "callback for job {} dropped after {} attempts (next beat tick re-enqueues)",
        result.job_id,
        cfg.callback_max_attempts
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_matches_known_hmac_vector() {
        // Cross-checked against Python: hmac.new(b"key", b"The quick brown fox jumps over the lazy
        // dog", hashlib.sha256).hexdigest() — the canonical HMAC-SHA256 test vector.
        let sig = sign("key", b"The quick brown fox jumps over the lazy dog");
        assert_eq!(
            sig,
            "sha256=f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
    }

    #[test]
    fn sign_is_deterministic_and_sensitive() {
        assert_eq!(sign("topsecret", b"abc"), sign("topsecret", b"abc"));
        assert_ne!(sign("topsecret", b"abc"), sign("topsecret", b"abd"));
        assert_ne!(sign("k1", b"abc"), sign("k2", b"abc"));
    }
}
