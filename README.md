# gifthub-rust-payments

A lean **payment-reconciliation gateway** for GiftHub — one Rust service covering both crypto
payment providers (NOWPayments and Cryptomus), sibling to `gifthub-rust-ai`.

**Django stays the system-of-record.** This service never touches the database, and it never
interprets a payment status — it reads jobs from Redis, fetches the provider's raw status, and
**POSTs the raw response back to Django over HTTP**. All field-mapping and business logic (crediting
points, dispatching fulfillment, releasing promos, …) stays in the same Python functions Django's
webhooks already use (`process_nowpayments_update` / `process_cryptomus_update`) — nothing is
duplicated across languages.

## Why this exists

NOWPayments was reconciled by an in-process Celery poller every 30s. Cryptomus was **webhook-only,
with no fallback** — if a webhook was ever lost or never sent, the payment (and its order) stayed
non-terminal forever, permanently blocking the order's retry-payment endpoint. This gateway replaces
the NOWPayments poller and adds the missing Cryptomus safety net, both through one consistent seam.

## How it fits

```
Django (Celery beat)                          gifthub-rust-payments (this service)
  decide what needs checking:                    BLPOP payment_jobs  ─────────────────┐
   - NOWPayments: still-pending set, every 30s                                        │
   - Cryptomus: non-terminal + invoice window                                         │
     passed with no webhook, every 10 min          fetch raw status from the named    │
  RPUSH payment_jobs {job_id,provider,              provider (GET / signed POST)      │
              lookup_id,callback}  ────────────▶                                      │
  (returns immediately — fire-and-forget)                                             │
  POST /internal/payments/callback/  ◀──────────  POST result {job_id,callback,       │
  (HMAC-verified) → process_*_update()              provider,ok,raw|error}  ──────────┘
                                                   (X-Payment-Signature: sha256=<HMAC>)
```

### Job contract

Producer (Django) → `payment_jobs` (Redis list), JSON:

```json
{ "job_id": "uuid", "provider": "nowpayments", "lookup_id": "<provider's own payment id>", "callback": { "payment_id": 7 } }
```

- `provider` is `"nowpayments"` or `"cryptomus"` — selects which API to call.
- `lookup_id` is whatever the provider's status API needs: NOWPayments' numeric payment id, or
  Cryptomus' uuid.
- `callback` is an **opaque** blob the gateway echoes back verbatim — it never inspects or mutates it.

Result → **HTTP POST** to `DJANGO_CALLBACK_URL`, signed `X-Payment-Signature: sha256=<hex
HMAC-SHA256 of the body, keyed by the shared secret>`, JSON:

```json
{ "job_id": "uuid", "callback": { "payment_id": 7 }, "provider": "nowpayments", "ok": true, "raw": { "...provider's raw response..." }, "error": null }
```

- `ok` discriminates success (`raw` set, untouched) from failure (`error` set).
- Any 2xx is "delivered"; on non-2xx / transport error the gateway retries with backoff a bounded
  number of times, then drops — the next beat tick enqueues a fresh job for the same payment, so a
  lost callback costs one cycle, never a stuck payment.

## Design

- **No failover chain, no rate-limit budget to learn.** Unlike `gifthub-rust-ai`, there's exactly one
  provider per job (selected by `provider`, not raced against alternatives) and no shared quota to
  pace — much smaller surface than the AI gateway.
- **Cryptomus signing** (`MD5(base64(json_body) + api_key)`) mirrors
  `apps.payments.providers.cryptomus.service.CryptomusService._sign` byte-for-byte — cross-checked
  against the Python implementation in `provider.rs`'s tests.
- **Graceful shutdown**: SIGTERM/SIGINT flips a `tokio::sync::watch` bus; in-flight jobs (bounded by
  `MAX_CONCURRENCY`) drain before exit.

See `DEPLOY.md` for systemd + CI deploy setup, and `.env.example` for the full config reference.
