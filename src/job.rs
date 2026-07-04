//! The Django <-> Rust job contract.
//!
//! Django (the producer) enqueues a [`Job`] as JSON onto the `payment_jobs` list; the gateway pops
//! it, fetches the raw provider status (GET for NOWPayments, signed POST for Cryptomus), and
//! **POSTs** a [`JobResult`] back to Django's callback URL (HMAC-signed — see `callback.rs`). Django
//! owns every database write AND all field-mapping — this gateway only fetches and relays the
//! provider's raw response, it never interprets payment status itself.
//!
//! `callback` is an **opaque** routing blob (`{"payment_id": 7}`): the gateway never inspects or
//! mutates it, it just echoes it back verbatim so Django can route the result to the right row.

use serde::{Deserialize, Serialize};

/// A unit of work enqueued by Django.
#[derive(Debug, Deserialize)]
pub struct Job {
    pub job_id: String,
    /// `"nowpayments"` or `"cryptomus"` — selects which provider API to call.
    pub provider: String,
    /// The provider's own payment identifier: NOWPayments' numeric payment id, or Cryptomus' uuid.
    pub lookup_id: String,
    /// Opaque blob Django attaches for its own routing; echoed back verbatim in the callback.
    #[serde(default)]
    pub callback: serde_json::Value,
}

/// The gateway's reply, POSTed to Django. `ok` discriminates success from failure; on success `raw`
/// holds the provider's raw response object (untouched — Django's existing field-mapping functions
/// parse it), on failure `error` is set. `callback` is echoed from the [`Job`].
#[derive(Debug, Serialize)]
pub struct JobResult {
    pub job_id: String,
    pub callback: serde_json::Value,
    pub provider: String,
    pub ok: bool,
    pub raw: Option<serde_json::Value>,
    pub error: Option<String>,
}

impl JobResult {
    pub(crate) fn ok(
        job_id: &str,
        callback: serde_json::Value,
        provider: &str,
        raw: serde_json::Value,
    ) -> Self {
        Self {
            job_id: job_id.to_string(),
            callback,
            provider: provider.to_string(),
            ok: true,
            raw: Some(raw),
            error: None,
        }
    }

    pub(crate) fn err(
        job_id: &str,
        callback: serde_json::Value,
        provider: &str,
        error: String,
    ) -> Self {
        Self {
            job_id: job_id.to_string(),
            callback,
            provider: provider.to_string(),
            ok: false,
            raw: None,
            error: Some(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_deserializes_with_optional_callback() {
        let raw = r#"{"job_id":"abc","provider":"nowpayments","lookup_id":"123"}"#;
        let job: Job = serde_json::from_str(raw).unwrap();
        assert_eq!(job.job_id, "abc");
        assert_eq!(job.provider, "nowpayments");
        assert_eq!(job.lookup_id, "123");
        assert!(job.callback.is_null());
    }

    #[test]
    fn job_preserves_opaque_callback() {
        let raw = r#"{"job_id":"abc","provider":"cryptomus","lookup_id":"u-1","callback":{"payment_id":7}}"#;
        let job: Job = serde_json::from_str(raw).unwrap();
        assert_eq!(job.callback["payment_id"], 7);
    }

    #[test]
    fn result_serializes_success_and_error_shapes() {
        let cb = serde_json::json!({"payment_id": 7});
        let ok = JobResult::ok(
            "abc",
            cb.clone(),
            "nowpayments",
            serde_json::json!({"payment_status": "finished"}),
        );
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&ok).unwrap()).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["raw"]["payment_status"], "finished");
        assert_eq!(v["callback"]["payment_id"], 7); // echoed verbatim
        assert!(v["error"].is_null());

        let err = JobResult::err("abc", cb, "cryptomus", "network error".into());
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&err).unwrap()).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"], "network error");
        assert!(v["raw"].is_null());
    }
}
