//! gifthub-rust-payments — one gateway reconciling BOTH payment providers (NOWPayments + Cryptomus).
//!
//! Django enqueues `{job_id, provider, lookup_id, callback}` onto the `payment_jobs` Redis list (a
//! Celery beat task decides what needs checking — NOWPayments' still-pending set every 30s, and
//! Cryptomus payments whose invoice window passed with no terminal webhook, every 10 minutes). This
//! service pops each job, fetches the raw status from the named provider, and **POSTs**
//! `{job_id, callback, provider, ok, raw|error}` back to Django's callback URL (HMAC-signed). Django
//! owns every database write AND all field-mapping — this binary only fetches and relays.
//!
//! Much simpler than `gifthub-rust-ai`: no failover chain, no rpm/rpd budget to learn — reconciling a
//! payment either succeeds or it doesn't, and the next beat tick tries again either way.

mod callback;
mod config;
mod error;
mod http;
mod job;
mod provider;
mod redis_io;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::sync::Semaphore;

use config::Config;

#[tokio::main]
async fn main() {
    env_logger::init();
    let cfg = config::get_config();
    log::info!("gifthub-rust-payments starting");

    let mut consumer = match redis_io::init_redis(&cfg.redis_url).await {
        Ok(c) => c,
        Err(e) => {
            log::error!("❌ Redis connection failed: {e}");
            return;
        }
    };
    log::info!("✅ connected to Redis");

    let client = http::build_http_client(cfg.connect_timeout_secs, cfg.request_timeout_secs);
    let sem = Arc::new(Semaphore::new(cfg.max_concurrency));

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
        "🚀 gifthub-rust-payments started: consuming Redis list '{}'",
        cfg.jobs_key
    );

    loop {
        // Backpressure: take a permit BEFORE popping, so we never pop more jobs than we can run.
        let permit = tokio::select! {
            _ = shutdown_rx.changed() => break,
            p = Arc::clone(&sem).acquire_owned() => p.expect("semaphore should not be closed"),
        };

        let raw = tokio::select! {
            _ = shutdown_rx.changed() => break, // permit dropped here, returns to the pool
            res = redis_io::pop_job(&mut consumer, &cfg.jobs_key, cfg.pop_timeout_secs) => match res {
                Ok(Some(raw)) => raw,
                Ok(None) => continue, // BLPOP timed out — drop permit, re-check shutdown
                Err(e) => {
                    log::error!("BLPOP error: {e} (backing off 1s)");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            },
        };

        let client = client.clone();
        let shutdown = shutdown_rx.clone();
        tokio::spawn(async move {
            let _permit = permit;
            handle_job(cfg, client, shutdown, raw).await;
        });
    }

    log::info!("shutdown signal received; draining in-flight jobs…");
    let drain = Arc::clone(&sem).acquire_many_owned(cfg.max_concurrency as u32);
    match tokio::time::timeout(Duration::from_secs(cfg.request_timeout_secs + 5), drain).await {
        Ok(_) => log::info!("all in-flight jobs finished"),
        Err(_) => log::warn!("shutdown grace expired with jobs still in flight"),
    }

    log::info!("gifthub-rust-payments stopped");
}

/// Process one job: fetch the provider's raw status, then POST the result to Django.
async fn handle_job(
    cfg: &'static Config,
    client: reqwest::Client,
    shutdown: watch::Receiver<bool>,
    raw: String,
) {
    let job: job::Job = match serde_json::from_str(&raw) {
        Ok(j) => j,
        Err(e) => {
            // No job_id/callback to answer on — drop and move on.
            log::error!("dropping malformed job ({e}); raw {} bytes", raw.len());
            return;
        }
    };

    let mut shutdown_cb = shutdown;
    let result = match provider::fetch_status(cfg, &client, &job.provider, &job.lookup_id).await {
        Ok(raw) => {
            log::info!(
                "[{}] job {} ok (lookup {})",
                job.provider,
                job.job_id,
                job.lookup_id
            );
            job::JobResult::ok(&job.job_id, job.callback.clone(), &job.provider, raw)
        }
        Err(e) => {
            log::warn!("[{}] job {} failed: {e}", job.provider, job.job_id);
            job::JobResult::err(
                &job.job_id,
                job.callback.clone(),
                &job.provider,
                e.to_string(),
            )
        }
    };
    callback::deliver(&client, cfg, &mut shutdown_cb, &result).await;
}
