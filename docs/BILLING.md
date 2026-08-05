# Tacitus Billing (Tacitus Sync Pro)

The Stripe sidecar that sells hosted-relay quota. No user accounts anywhere:
Pro is per-vault and the vault_id is the customer key — the same 32-hex id
that is all the relay ever learns, so the privacy story survives billing.

```
browser ── GET /billing/checkout?vault_id&plan ──> tacitus-billing ── 303 ──> Stripe Checkout
Stripe  ── POST /billing/webhook (signed) ──────> tacitus-billing ── writes ──> entitlements.json
relay   ── hot-reloads entitlements.json on its own (mon-m1) — the two services share only that file
```

## Configuration

| Env var | Default | Notes |
|---|---|---|
| `TACITUS_BILLING_BIND` | `127.0.0.1:8092` | |
| `TACITUS_BILLING_ENTITLEMENTS` | `./relay-data/entitlements.json` | must be the file the relay reads |
| `TACITUS_BILLING_PRO_QUOTA` | `1073741824` (1 GiB) | invalid value → warn + default |
| `TACITUS_BILLING_SUCCESS_URL` | `https://tacitus.md` | post-payment redirect |
| `TACITUS_BILLING_CANCEL_URL` | `https://tacitus.md` | |
| `TACITUS_BILLING_STRIPE_URL` | `https://api.stripe.com` | tests point it at a fake |
| `STRIPE_SECRET_KEY` | — | **required; boot fails without it** |
| `STRIPE_WEBHOOK_SECRET` | — | required |
| `STRIPE_PRICE_MONTHLY` | — | required (`price_…`, €5/mo) |
| `STRIPE_PRICE_YEARLY` | — | required (`price_…`, €48/yr) |

The four `STRIPE_*` vars fail fast — one panic listing every missing name. A
half-configured billing daemon would silently drop paid upgrades; a
crash-loop is visible.

## entitlements.json — ownership rules

- **Billing is the writer; the relay only reads.** Writes are atomic
  (tmp + fsync + rename in the same directory), so the relay never sees a
  torn file — at worst one read is a generation stale, which its next read
  corrects.
- `version` is pinned to `1`. If the file is corrupt or has a foreign
  version, billing **refuses to rewrite it** (webhooks 500, Stripe retries,
  the relay keeps serving its last good set) until a human fixes it.
- Hand-edits are allowed and preserved: unknown fields on the file and on
  any entry survive billing's rewrites, and entries without Stripe
  bookkeeping (comps) are never touched. Example comp:
  `"vaults":{"<vault_id>":{"quota_bytes":536870912}}`.
- `billing_removed` (top level) holds tombstones: after a subscription is
  deleted, a stale out-of-order `active` webhook cannot resurrect the
  entitlement. The relay ignores the field.
- Delivery order is guarded by Stripe's `event.created` per vault
  (`last_event_ts`); webhook replays are byte-idempotent.

## Deploy (same box as the relay)

```bash
docker build -t tacitus-billing -f crates/tacitus-billing/Dockerfile .
docker run -d --name tacitus-billing \
  -p 127.0.0.1:8092:8092 \
  -v tacitus-relay-data:/data \
  -e STRIPE_SECRET_KEY=sk_… -e STRIPE_WEBHOOK_SECRET=whsec_… \
  -e STRIPE_PRICE_MONTHLY=price_… -e STRIPE_PRICE_YEARLY=price_… \
  tacitus-billing
```

- `/data` is the **same volume** the relay mounts. The relay container runs
  as user `relay`, billing as `billing` — run both with the same `--user`
  uid, or make the data dir group-writable, so billing can create and
  rename in it.
- nginx: the `location /billing/` block in `deploy/sync.tacitus.md.conf`
  proxies to 8092 (certbot/TLS unchanged). Public webhook URL:
  `https://sync.tacitus.md/billing/webhook`.
- If the server clock drifts more than 5 minutes, webhook signatures are
  rejected with a warn log showing the delta — check NTP, not the secret.

## Stripe dashboard checklist (entity: Docs Printing SRL, CIF RO22749955)

1. Create the Stripe account as **Docs Printing SRL** (office@docsprinting.ro);
   complete the business profile + bank account; statement descriptor
   `TACITUS SYNC`.
2. Do everything below in **Test mode** first; repeat in Live mode after
   activation.
3. Product Catalog → product **"Tacitus Sync Pro"** with two recurring EUR
   prices: **€5.00/month** and **€48.00/year**. Decide tax handling with the
   accountant (Stripe Tax with tax-inclusive prices is the low-friction
   option for RO VAT).
4. Copy the two price ids → `STRIPE_PRICE_MONTHLY`, `STRIPE_PRICE_YEARLY`.
5. Developers → API keys → secret key → `STRIPE_SECRET_KEY`.
6. Developers → Webhooks → Add endpoint
   `https://sync.tacitus.md/billing/webhook` with EXACTLY these events:
   `customer.subscription.created`, `customer.subscription.updated`,
   `customer.subscription.deleted`. Signing secret → `STRIPE_WEBHOOK_SECRET`.
7. Settings → Checkout: enable customer receipt emails.
8. Smoke test (Test mode): open
   `https://sync.tacitus.md/billing/checkout?vault_id=<32hex>&plan=monthly`,
   pay with card `4242 4242 4242 4242`, verify the vault appears in
   `entitlements.json` with the Pro quota; cancel in the dashboard and
   verify it disappears. Locally:
   `stripe listen --forward-to 127.0.0.1:8092/billing/webhook`.

## Out of scope (follow-ups)

Stripe Customer Portal endpoint (self-serve cancellation), per-user vault
counts, dunning/refund emails, the site's pricing section (launch
milestone), the Teams tier.
