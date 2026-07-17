//! Fetch the reconciliation worklist from Django's `GET /internal/payments/due/`.
//!
//! This gateway has no DB access — Django decides *what* needs checking (every non-terminal
//! payment, both providers, no expiration filtering: the gateway actively re-verifies everything on
//! its own timer rather than trusting a webhook to be the only channel). This module just fetches
//! that list; `main.rs` decides *when* (its own poll loop) and dispatches each item to `provider.rs`.

use serde::Deserialize;

use crate::config::Config;

#[derive(Debug, Clone, Deserialize)]
pub struct DueItem {
    pub payment_id: i64,
    pub provider: String,
    pub lookup_id: String,
}

#[derive(Debug, Deserialize)]
struct DueResponse {
    payments: Vec<DueItem>,
}

pub async fn fetch_due(client: &reqwest::Client, cfg: &Config) -> Result<Vec<DueItem>, String> {
    let resp = client
        .get(&cfg.due_url)
        .header("Authorization", format!("Bearer {}", cfg.webhook_secret))
        .send()
        .await
        .map_err(|e| format!("transport error: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    resp.json::<DueResponse>()
        .await
        .map(|r| r.payments)
        .map_err(|e| format!("invalid response body: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn due_response_deserializes() {
        let raw = r#"{"payments":[{"payment_id":7,"provider":"nowpayments","lookup_id":"123"}]}"#;
        let parsed: DueResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.payments.len(), 1);
        assert_eq!(parsed.payments[0].payment_id, 7);
        assert_eq!(parsed.payments[0].provider, "nowpayments");
        assert_eq!(parsed.payments[0].lookup_id, "123");
    }

    #[test]
    fn empty_list_deserializes() {
        let parsed: DueResponse = serde_json::from_str(r#"{"payments":[]}"#).unwrap();
        assert!(parsed.payments.is_empty());
    }
}
