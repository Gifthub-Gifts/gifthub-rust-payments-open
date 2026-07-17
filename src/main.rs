//! gifthub-rust-payments — one gateway reconciling BOTH payment providers (NOWPayments + Cryptomus).
//!
//! Pull-based, no Redis, no DB access of its own: on its own internal timer, this service GETs
//! `/internal/payments/due/` (every non-terminal payment, both providers — no expiration filtering,
//! since the point is to actively re-verify everything rather than trust either provider's webhook
//! to be the only channel), fetches each one's raw status from the named provider, and **POSTs**
//! `{job_id, callback, provider, ok, raw|error}` back to Django's callback URL (HMAC-signed). Django
//! owns every database write AND all field-mapping — this binary only fetches and relays.
//!
//! Self-throttles per provider via a 429-learning rate limiter (`ratelimit.rs`): honors the
//! provider's own `Retry-After` header when given, exponential backoff otherwise. Learned state is
//! in-memory only — no persistence, by design (see `ratelimit.rs`).

mod callback;
mod config;
mod due;
mod error;
mod http;
mod job;
mod provider;
mod ratelimit;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::sync::Semaphore;

use config::Config;
use ratelimit::RateLimiter;

#[tokio::main]
async fn main() {
    env_logger::init();
    let cfg = config::get_config();
    log::info!("gifthub-rust-payments starting");

    let client = http::build_http_client(cfg.connect_timeout_secs, cfg.request_timeout_secs);
    let sem = Arc::new(Semaphore::new(cfg.max_concurrency));
    let limiter = Arc::new(RateLimiter::new());

    // Unified shutdown bus: one watch sender flipped by SIGINT or SIGTERM (systemctl stop / deploy).
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        let mut sigterm =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    log::error!("failed to install SIGTERM handler: {e}");
                    return;
                }
            };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => log::info!("SIGINT received, shutting down…"),
            _ = sigterm.recv() => log::info!("SIGTERM received, shutting down…"),
        }
        shutdown_tx.send(true).ok();
    });

    // Greppable ready marker (the deploy health check looks for this line in journald).
    log::info!(
        "🚀 gifthub-rust-payments started: polling {} every {}s",
        cfg.due_url,
        cfg.poll_interval_secs
    );

    loop {
        match due::fetch_due(&client, cfg).await {
            Ok(items) => {
                if !items.is_empty() {
                    log::info!("{} payment(s) due for reconciliation", items.len());
                }
                for item in items {
                    if !limiter.is_ready(&item.provider) {
                        continue; // still backing off from a recent 429 — retry next tick
                    }

                    // Backpressure: take a permit BEFORE spawning, so ticks can't pile up unbounded
                    // in-flight work if the callback POSTs are slow.
                    let permit = tokio::select! {
                        _ = shutdown_rx.changed() => break,
                        p = Arc::clone(&sem).acquire_owned() => p.expect("semaphore should not be closed"),
                    };

                    let client = client.clone();
                    let limiter = Arc::clone(&limiter);
                    let shutdown = shutdown_rx.clone();
                    tokio::spawn(async move {
                        let _permit = permit;
                        handle_item(cfg, client, limiter, shutdown, item).await;
                    });
                }
            }
            Err(e) => log::error!("failed to fetch due list: {e}"),
        }

        tokio::select! {
            _ = shutdown_rx.changed() => break,
            _ = tokio::time::sleep(Duration::from_secs(cfg.poll_interval_secs)) => {}
        }
    }

    log::info!("shutdown signal received; draining in-flight items…");
    let drain = Arc::clone(&sem).acquire_many_owned(cfg.max_concurrency as u32);
    match tokio::time::timeout(Duration::from_secs(cfg.request_timeout_secs + 5), drain).await {
        Ok(_) => log::info!("all in-flight items finished"),
        Err(_) => log::warn!("shutdown grace expired with items still in flight"),
    }

    log::info!("gifthub-rust-payments stopped");
}

/// Process one due item: fetch the provider's raw status, then POST the result to Django.
async fn handle_item(
    cfg: &'static Config,
    client: reqwest::Client,
    limiter: Arc<RateLimiter>,
    shutdown: watch::Receiver<bool>,
    item: due::DueItem,
) {
    let job_id = format!("{}-{}", item.provider, item.payment_id);
    let callback = serde_json::json!({ "payment_id": item.payment_id });
    let mut shutdown_cb = shutdown;

    match provider::fetch_status(cfg, &client, &item.provider, &item.lookup_id).await {
        Ok(raw) => {
            limiter.record_success(&item.provider);
            log::info!("[{}] payment {} ok", item.provider, item.payment_id);
            let result = job::JobResult::ok(&job_id, callback, &item.provider, raw);
            callback::deliver(&client, cfg, &mut shutdown_cb, &result).await;
        }
        Err(error::LookupError::RateLimited { retry_after }) => {
            // Don't report this to Django at all — it isn't a lookup failure, just "try again
            // later." Reporting it would risk tripping the overdue-force-expire fallback for no
            // reason; the next tick (once the backoff clears) retries cleanly.
            limiter.record_429(&item.provider, retry_after);
        }
        Err(e) => {
            log::warn!(
                "[{}] payment {} failed: {e}",
                item.provider,
                item.payment_id
            );
            let result = job::JobResult::err(&job_id, callback, &item.provider, e.to_string());
            callback::deliver(&client, cfg, &mut shutdown_cb, &result).await;
        }
    }
}
