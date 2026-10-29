# Deploying gifthub-rust-payments

Runs on the **same host as the Django app** as a systemd unit, alongside the other
`/srv/gifthub*` services (including the sibling `gifthub-rust-ai`). CI
(`.github/workflows/deploy.yml`) builds the release binary, scp's it over SSH, restarts the unit,
health-checks journald for the ready marker, and rolls back to the previous binary on failure.

**The deploy ships only the binary.** Provider API keys and the shared webhook secret live in a
`.env` *on the host* (read by the systemd unit) — never in the repo or in CI. No Redis is involved
at all — this gateway is pull-based: it polls Django's `/internal/payments/due/` on its own timer
and POSTs results to `/internal/payments/callback/`.

---

## 1. The only CI secret: `SSH_PRIVATE_KEY`

Same convention as `gifthub-rust-ai`: host and user come from the repo variables `PROD_HOST` and
`PROD_USER`, and the host key is scanned live at deploy time. Reuse the same deploy key
as the other repos, or generate a dedicated one:

```bash
ssh-keygen -t ed25519 -C "gifthub-rust-payments-gha-deploy" -f ./gha_deploy_key -N ""
ssh "$PROD_USER@$PROD_HOST" "echo '$(cat gha_deploy_key.pub)' >> ~/.ssh/authorized_keys"
gh secret set SSH_PRIVATE_KEY --repo Gifthub-Gifts/gifthub-rust-payments < gha_deploy_key
rm gha_deploy_key gha_deploy_key.pub
```

---

## 2. One-time host setup (on the production host)

The deploy user needs passwordless sudo for `systemctl` and `journalctl`.

```bash
# 2.1  Deploy dir (owned by the deploy user so scp can write the binary; the unit runs it as root)
sudo mkdir -p /srv/gifthub-rust-payments
sudo chown "$PROD_USER:$PROD_USER" /srv/gifthub-rust-payments

# 2.2  Runtime .env — Django endpoints + the shared secret + provider keys. All REQUIRED: the
#      binary fails to boot without them (a deploy then health-checks red and rolls back). The
#      secret must be byte-identical to Django's PAYMENT_GATEWAY_WEBHOOK_SECRET (it's used both as
#      the due-list Bearer token and the callback HMAC key), and the provider keys must be the SAME
#      keys Django's NOWPAYMENTS_API_KEY / CRYPTOMUS_MERCHANT_ID / CRYPTOMUS_API_KEY use (this
#      gateway calls the exact same provider accounts). RUST_LOG=info is required to see ANY log
#      output — env_logger is silent without it. See .env.example for the full list.
sudo tee /srv/gifthub-rust-payments/.env >/dev/null <<'ENV'
DJANGO_DUE_URL=http://127.0.0.1:8000/internal/payments/due/
DJANGO_CALLBACK_URL=http://127.0.0.1:8000/internal/payments/callback/
PAYMENT_GATEWAY_WEBHOOK_SECRET=<same value as Django's PAYMENT_GATEWAY_WEBHOOK_SECRET>
NOWPAYMENTS_API_KEY=<same value as Django's NOWPAYMENTS_API_KEY>
CRYPTOMUS_MERCHANT_ID=<same value as Django's CRYPTOMUS_MERCHANT_ID>
CRYPTOMUS_API_KEY=<same value as Django's CRYPTOMUS_API_KEY>
RUST_LOG=info
ENV
sudo chmod 600 /srv/gifthub-rust-payments/.env

# 2.3  Install + enable the unit (don't start yet — no binary until the first deploy)
sudo cp deploy/gifthub-rust-payments.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable gifthub-rust-payments
```

---

## 3. Deploy

- **Automatic:** every push/merge to `main`.
- **Manual (recommended first):** Actions → *Deploy to production* → *Run workflow* → type `DEPLOY`.

The first deploy is safe before any binary exists (the backup step tolerates it). Verify:

```bash
sudo systemctl status gifthub-rust-payments
sudo journalctl -u gifthub-rust-payments -n 50 --no-pager   # look for: 🚀 gifthub-rust-payments started
```

---

## 4. Cutover order (coordinated with the Django `gifthub` deploy)

This gateway **replaces `gifthub-payments-worker` entirely** — Django no longer runs any Celery
task or beat schedule for payment reconciliation. Deploy in this order so nothing goes un-reconciled
in between:

1. **Set the shared secret + provider keys on both hosts.** Same `PAYMENT_GATEWAY_WEBHOOK_SECRET` in
   `/srv/gifthub-rust-payments/.env` (here) and `/srv/gifthub/.env` (Django).
2. **Deploy this gateway first** (or confirm it's already running with the new pull-based binary —
   it's fine for it to poll `/internal/payments/due/` before Django's matching deploy lands; that
   endpoint didn't exist in older Django versions, so it'll just log fetch errors harmlessly until
   step 3 lands).
3. **Deploy Django.** `/internal/payments/due/` and `/internal/payments/callback/` become live; the
   gateway's very next poll tick picks up every non-terminal payment.
4. **Decommission `gifthub-payments-worker`** (only once step 3 is confirmed healthy):
   ```bash
   sudo systemctl stop gifthub-payments-worker
   sudo systemctl disable gifthub-payments-worker
   sudo rm /etc/systemd/system/gifthub-payments-worker.service
   sudo systemctl daemon-reload
   ```
   Also remove it from `deploy/deploy.sh`'s `SERVICES` array in the `gifthub` repo, or the next
   Django deploy will just fail trying to restart a unit that no longer exists.

Verify after step 3:

```bash
sudo journalctl -u gifthub-rust-payments -n 50 --no-pager | grep -E "payment\(s\) due|\] payment .* ok|failed:"
```

> Confirm nginx denies external `/internal/` (both routes) — the gateway reaches Django on
> `127.0.0.1:8000` directly, so `/internal/` never needs to be internet-reachable:
> `location /internal/ { allow 127.0.0.1; deny all; }` (already required for `gifthub-rust-ai`'s
> `/internal/ai/callback/`, if that gateway is deployed — same nginx block covers all of them).
