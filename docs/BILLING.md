# Tacitus Billing (Tacitus Sync Pro)

The Stripe sidecar that sells hosted-relay quota. No user accounts anywhere:
Pro is per-vault and the vault_id is the customer key — the same 32-hex id
that is all the relay ever learns, so the privacy story survives billing.

```
browser ── GET /billing/checkout?vault_id&plan ──> tacitus-billing ── 303 ──> Stripe Checkout
Stripe  ── POST /billing/webhook (signed) ──────> tacitus-billing ── writes ──> entitlements.json
relay   ── hot-reloads entitlements.json on its own (mon-m1) — the two services share only that file
operator ─ tacitus-billing --reconcile ────────> reads Stripe's live list ── repairs ──> entitlements.json
```

The client half of the funnel lives in `tacitus-mcp sync`: `status` prints
`storage: <used> of <quota>` and, past 80%, the checkout link for that
vault; `upgrade [--plan] [--open]` prints or opens it on demand. Both stay
silent on self-hosted relays (see docs/SYNC.md).

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

## Reconcile — when webhooks and reality drift apart

Stripe is the record of who paid; `entitlements.json` is only our cache of
it. `--reconcile` rebuilds the cache from the record:

```bash
docker compose exec billing tacitus-billing --reconcile          # dry run
docker compose exec billing tacitus-billing --reconcile --apply  # write it
```

It lists every subscription Stripe still tracks, keeps the ones whose
status entitles (the same `active | trialing | past_due` allowlist the
webhooks use) and whose metadata carries a valid `vault_id`, then prints a
diff: `+` grant a vault Pro that the file missed, `~` correct a stale entry
(old quota, resubscribed under a new id), `-` revoke Pro the file grants
and Stripe no longer backs. Nothing is written without `--apply`.

Two guarantees make it safe to run at any time, including while webhooks
are landing:

- **Comps are invisible to it.** An entry without `stripe_subscription` is
  operator-granted and never revoked or rewritten.
- **A live webhook always wins.** The run stamps everything it touches with
  a timestamp taken *before* the Stripe read, and skips (`=`) any entry
  whose `last_event_ts` is newer than that. A webhook that arrives mid-run
  is fresher truth than our list, and it stays.

Reach for it after: restoring the data volume from a backup, a webhook
endpoint outage (Stripe gives up retrying after ~3 days), or any suspicion
that a paying customer is not getting their quota. The dry run costs
nothing — a monthly one is a cheap audit.

## Self-serve cancellation (Stripe Customer Portal)

Enable the **hosted** portal — Dashboard → Settings → Billing → Customer
portal: turn on cancellation, plan switching, payment-method updates and
invoice history, then copy the **login link**
(`https://billing.stripe.com/p/login/…`) into `site/refunds.html`.

Deliberately NOT a `/billing/portal?vault_id=` endpoint of our own: the
vault id is an identifier, not a credential. It travels in checkout URLs
and support emails, and anyone who learned one could cancel that vault's
subscription. Stripe's hosted flow authenticates by emailing the customer a
sign-in link, which is the only identity in this system that means
anything — we hold none.

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

## Backups and liveness

- `deploy/backup.sh` tars `/var/lib/tacitus-relay` (the vault logs **and**
  `entitlements.json`) into `/var/backups/tacitus`, keeps 14, and writes
  through a `.part` file so an interrupted run can't leave a truncated
  archive that looks valid. Daily from cron is enough — the data is tiny.
  `--reconcile` rebuilds paid entitlements from Stripe, so the archive is
  what protects the two things Stripe has never heard of: comp entries and
  the encrypted logs themselves.
- Liveness worth alerting on, in the order it hurts: the relay
  (`/healthz`), billing on its loopback port (a crash-loop from a missing
  secret looks exactly like this), and billing **through nginx** — that
  last path is the one Stripe's webhooks travel, and a `404`/`502` there
  loses paid upgrades silently while everything else looks healthy. A
  request with a deliberately invalid `vault_id` is the cheapest probe of
  it: `400` is the healthy answer, and it never reaches Stripe.

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
8. Settings → Billing → Customer portal: enable it, allow cancellation and
   plan switching, set the links to `https://tacitus.md/terms.html` and
   `https://tacitus.md/privacy.html`, then paste the login link into
   `site/refunds.html`.
9. Settings → Business → public details: point "Terms of service" at
   `https://tacitus.md/terms.html`, "Privacy policy" at
   `/privacy.html`, "Refund policy" at `/refunds.html`. Stripe shows these
   at checkout and asks for them at activation.
10. Smoke test (Test mode): open
   `https://sync.tacitus.md/billing/checkout?vault_id=<32hex>&plan=monthly`,
   pay with card `4242 4242 4242 4242`, verify the vault appears in
   `entitlements.json` with the Pro quota; cancel in the dashboard and
   verify it disappears. Locally:
   `stripe listen --forward-to 127.0.0.1:8092/billing/webhook`.

## The legal pages the payment flow depends on

`site/terms.html`, `site/privacy.html` and `site/refunds.html` are part of
the product, not decoration: Stripe asks for them at account activation and
shows them at checkout, and EU distance-selling law requires the trader
identification, the 14-day withdrawal notice and the ANPC/SOL routes they
carry. They ship with `[COMPLETEAZĂ: …]` markers for the company details
that only the operator has (trade register number, registered office,
hosting provider, the portal login link) — grep for that string before
deploying the site.

## Out of scope (follow-ups)

Per-user vault counts, dunning/refund emails, a status page, the Teams
tier.
