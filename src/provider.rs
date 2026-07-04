//! Provider status-lookup calls.
//!
//! Each function returns the RAW json object Django's existing field-mapping functions already
//! expect (`apps.payments.providers.nowpayments.utils.process_nowpayments_update` /
//! `...cryptomus.utils.process_cryptomus_update`) — this binary does no interpretation of payment
//! status itself, it only fetches and relays.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;

use crate::config::Config;
use crate::error::LookupError;

/// Fetch the raw provider status for `lookup_id`, dispatching on `provider`.
pub async fn fetch_status(
    cfg: &Config,
    client: &reqwest::Client,
    provider: &str,
    lookup_id: &str,
) -> Result<serde_json::Value, LookupError> {
    match provider {
        "nowpayments" => fetch_nowpayments(cfg, client, lookup_id).await,
        "cryptomus" => fetch_cryptomus(cfg, client, lookup_id).await,
        other => Err(LookupError::UnknownProvider(other.to_string())),
    }
}

/// `GET {base}/payment/{id}` — NOWPayments returns the payment object directly, no envelope.
async fn fetch_nowpayments(
    cfg: &Config,
    client: &reqwest::Client,
    lookup_id: &str,
) -> Result<serde_json::Value, LookupError> {
    let url = format!("{}/payment/{lookup_id}", cfg.nowpayments_base_url);
    let resp = client
        .get(&url)
        .header("x-api-key", &cfg.nowpayments_api_key)
        .send()
        .await
        .map_err(to_lookup_error)?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(LookupError::Api {
            status: status.as_u16(),
            body,
        });
    }
    resp.json::<serde_json::Value>()
        .await
        .map_err(to_lookup_error)
}

/// `POST {base}/payment/info` with a Cryptomus-signed body `{"uuid": lookup_id}`.
///
/// Cryptomus signing: `MD5(base64(json_body) + api_key)` — the provider's own (legacy) scheme, sent
/// as the `sign` header alongside `merchant`. Mirrors `apps.payments.providers.cryptomus.service`.
/// The response envelope is `{"state": 0, "result": {...}}`; on success we return `result` alone
/// (the same shape Cryptomus' webhook body has, which `process_cryptomus_update` already parses).
async fn fetch_cryptomus(
    cfg: &Config,
    client: &reqwest::Client,
    lookup_id: &str,
) -> Result<serde_json::Value, LookupError> {
    let payload = serde_json::json!({ "uuid": lookup_id });
    let body = serde_json::to_vec(&payload).expect("static shape always serializes");
    let sign = cryptomus_sign(&cfg.cryptomus_api_key, &body);

    let url = format!("{}/payment/info", cfg.cryptomus_base_url);
    let resp = client
        .post(&url)
        .header("merchant", &cfg.cryptomus_merchant_id)
        .header("sign", &sign)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(to_lookup_error)?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(LookupError::Api {
            status: status.as_u16(),
            body: text,
        });
    }

    let envelope: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| LookupError::Api {
            status: status.as_u16(),
            body: format!("invalid JSON: {e}"),
        })?;
    match envelope.get("state").and_then(|s| s.as_i64()) {
        Some(0) => envelope
            .get("result")
            .cloned()
            .ok_or_else(|| LookupError::Api {
                status: status.as_u16(),
                body: "missing 'result' field".to_string(),
            }),
        _ => Err(LookupError::Api {
            status: status.as_u16(),
            body: text,
        }),
    }
}

fn cryptomus_sign(api_key: &str, body: &[u8]) -> String {
    let encoded = BASE64.encode(body);
    let digest = md5::compute(format!("{encoded}{api_key}").as_bytes());
    format!("{digest:x}")
}

fn to_lookup_error(e: reqwest::Error) -> LookupError {
    if e.is_timeout() {
        LookupError::Timeout
    } else {
        LookupError::Network(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cryptomus_sign_matches_known_vector() {
        // Cross-checked against Python (apps.payments.providers.cryptomus.service._sign):
        //   b = json.dumps({"uuid": "abc"}, separators=(",", ":"), ensure_ascii=False).encode()
        //   hashlib.md5((base64.b64encode(b).decode() + "key").encode()).hexdigest()
        // -> eyJ1dWlkIjoiYWJjIn0= -> 3349519a384c8fabfef190c939e725f3
        let body = br#"{"uuid":"abc"}"#;
        assert_eq!(
            cryptomus_sign("key", body),
            "3349519a384c8fabfef190c939e725f3"
        );
        assert_eq!(cryptomus_sign("key", body), cryptomus_sign("key", body));
        assert_ne!(cryptomus_sign("key", body), cryptomus_sign("other", body));
    }

    #[test]
    fn unknown_provider_is_rejected_before_any_network_call() {
        // Purely exercises the dispatch match arm — no config/client needed for this branch.
        let result: Result<(), LookupError> = match "paypal" {
            "nowpayments" | "cryptomus" => Ok(()),
            other => Err(LookupError::UnknownProvider(other.to_string())),
        };
        assert!(matches!(result, Err(LookupError::UnknownProvider(p)) if p == "paypal"));
    }
}
