# gifthub-rust-payments

A lean **payment-reconciliation gateway** for GiftHub — one Rust service covering both crypto
payment providers (NOWPayments and Cryptomus), sibling to `gifthub-rust-ai`.

**Django stays the system-of-record.** This service never touches the database, and it never
interprets a payment status — on its own internal timer, it asks Django what needs checking,
fetches the provider's raw status, and **POSTs the raw response back to Django over HTTP**. All
field-mapping and business logic (crediting points, dispatching fulfillment, releasing promos, …)
stays in the same Python functions Django's webhooks already use (`process_nowpayments_update` /
`process_cryptomus_update`) — nothing is duplicated across languages.

## Why this exists

NOWPayments was reconciled by an in-process Celery poller every 30s. Cryptomus was **webhook-only,
with no fallback** — if a webhook was ever lost or never sent, the payment (and its order) stayed
non-terminal forever, permanently blocking the order's retry-payment endpoint. This gateway replaces
that whole setup (Celery worker, beat schedule, Redis job queue — all gone) with one pull-based loop
that actively re-verifies **every** non-terminal payment for **both** providers, regardless of
whether a webhook ever showed up. Cryptomus's webhook still runs and updates state near-instantly
when it arrives — it's a fast path now, not a correctness dependency.

## How it fits

```
Django                                        gifthub-rust-payments (this service)
  GET /internal/payments/due/  ◀─────────────  on its own timer (POLL_INTERVAL_SECS, default 15s):
  (Bearer <shared secret>)                       GET the due-list
  → { payments: [{payment_id,provider,                                                │
                  lookup_id}, ...] }  ─────────▶  for each item (unless still          │
    every non-terminal payment,                   rate-limited from a recent 429):     │
    both providers, no expiration filter            fetch raw status (GET / signed POST)
                                                                                        │
  POST /internal/payments/callback/  ◀──────────  POST result {job_id,callback,        │
  (HMAC-verified) → process_*_update()              provider,ok,raw|error}  ───────────┘
                                                   (X-Payment-Signature: sha256=<HMAC>)
```

### Due-list contract

`GET DJANGO_DUE_URL`, header `Authorization: Bearer <shared secret>` → JSON:

```json
{ "payments": [ { "payment_id": 7, "provider": "nowpayments", "lookup_id": "<provider's own payment id>" } ] }
```

- `provider` is `"nowpayments"` or `"cryptomus"` — selects which API to call.
- `lookup_id` is whatever the provider's status API needs: NOWPayments' numeric payment id, or
  Cryptomus' uuid.
- No expiration/overdue filtering — every non-terminal payment for both providers comes back every
  tick. The gateway is what makes that cheap: polling everything every 15s is fine because there's
  no Celery task overhead per item, just an HTTP call.

Result → **HTTP POST** to `DJANGO_CALLBACK_URL`, signed `X-Payment-Signature: sha256=<hex
HMAC-SHA256 of the body, keyed by the shared secret>`, JSON:

```json
{ "job_id": "nowpayments-7", "callback": { "payment_id": 7 }, "provider": "nowpayments", "ok": true, "raw": { "...provider's raw response..." }, "error": null }
```

- `ok` discriminates success (`raw` set, untouched) from failure (`error` set).
- Any 2xx is "delivered"; on non-2xx / transport error the gateway retries with backoff a bounded
  number of times, then drops — the next poll tick picks the same payment back up (it's still
  non-terminal, so it's still in the due-list), so a lost callback costs one cycle, never a stuck
  payment.
- A 429 is **not** reported to Django at all — it isn't a lookup failure, just "try again once the
  rate limiter clears" (see below). Reporting it as a failure would risk tripping the
  overdue-force-expire fallback in Django's callback view for no reason.

## Design

- **No failover chain.** Unlike `gifthub-rust-ai`, there's exactly one provider per item (selected
  by `provider`, not raced against alternatives) — much smaller surface than the AI gateway.
- **429-learning rate limiter** (`ratelimit.rs`), one per provider: honors the provider's own
  `Retry-After` header when given, exponential backoff (2s, 4s, 8s, … capped at 5 min) otherwise.
  Unlike the AI gateway's version, this doesn't assume a structured quota body (Google's Gemini API
  has one; NOWPayments/Cryptomus don't document one) and doesn't persist learned state across
  restarts — this gateway has no Redis dependency at all, by design, and a restart re-learning
  within a few requests is a cheap enough cost.
- **Cryptomus signing** (`MD5(base64(json_body) + api_key)`) mirrors
  `apps.payments.providers.cryptomus.service.CryptomusService._sign` byte-for-byte — cross-checked
  against the Python implementation in `provider.rs`'s tests.
- **Graceful shutdown**: SIGTERM/SIGINT flips a `tokio::sync::watch` bus; in-flight items (bounded
  by `MAX_CONCURRENCY`) drain before exit.

See `.env.example` for the full config reference. The systemd and CI deploy setup is kept in the
private production repo.
