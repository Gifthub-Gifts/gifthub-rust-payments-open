//! Redis I/O: jobs in, nothing out (results are POSTed to Django — see `callback.rs`).
//!
//! Mirrors the `gifthub-rust-ai` / `vpn-watchdog` / `telegram-happ-rust` pool idiom — build one
//! cloneable connection handle at boot and pass `&mut` clones into tasks. Commands are issued via
//! explicit `redis::cmd(...)` so the code is robust across redis-rs version churn in the typed-command
//! API.

use redis::aio::ConnectionManager;

/// Build the shared, auto-reconnecting connection manager once at startup.
pub async fn init_redis(url: &str) -> redis::RedisResult<ConnectionManager> {
    let client = redis::Client::open(url)?;
    ConnectionManager::new(client).await
}

/// Block up to `timeout_secs` for the next job on `key`. Returns the raw JSON payload, or `None` if
/// the blocking pop timed out (so the caller can re-check shutdown).
///
/// Uses `BLPOP` (pop the head) while Django enqueues with `RPUSH` (push the tail) — opposite ends,
/// so the queue is FIFO (oldest job first).
pub async fn pop_job(
    conn: &mut ConnectionManager,
    key: &str,
    timeout_secs: u64,
) -> redis::RedisResult<Option<String>> {
    let res: Option<(String, String)> = redis::cmd("BLPOP")
        .arg(key)
        .arg(timeout_secs)
        .query_async(conn)
        .await?;
    Ok(res.map(|(_key, value)| value))
}
