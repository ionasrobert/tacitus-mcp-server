//! tacitus-billing — the Stripe sidecar for Tacitus Sync Pro.
//!
//! No user accounts: Pro is per-vault, the vault_id is the customer key.
//! `GET /billing/checkout?vault_id&plan` 303-redirects to Stripe Checkout
//! (the vault_id rides the subscription's metadata); Stripe's webhooks then
//! land on `POST /billing/webhook`, which rewrites entitlements.json
//! atomically — the relay hot-reloads it on its own (mon-m1), so the two
//! services share nothing but that file.
//!
//!   TACITUS_BILLING_BIND          (default 127.0.0.1:8092)
//!   TACITUS_BILLING_ENTITLEMENTS  (default ./relay-data/entitlements.json)
//!   TACITUS_BILLING_PRO_QUOTA     (bytes; default 1 GiB)
//!   TACITUS_BILLING_SUCCESS_URL   (default https://tacitus.md)
//!   TACITUS_BILLING_CANCEL_URL    (default https://tacitus.md)
//!   TACITUS_BILLING_STRIPE_URL    (default https://api.stripe.com)
//!   STRIPE_SECRET_KEY             (required — boot fails without it)
//!   STRIPE_WEBHOOK_SECRET         (required)
//!   STRIPE_PRICE_MONTHLY          (required)
//!   STRIPE_PRICE_YEARLY           (required)

mod entitlements;
mod stripe;

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Redirect;
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;

use entitlements::EntStore;
use stripe::{create_checkout_session, valid_vault_id, verify_signature, StripeEvent};

pub struct Config {
    pub bind: String,
    pub entitlements_path: PathBuf,
    pub pro_quota: u64,
    pub success_url: String,
    pub cancel_url: String,
    pub stripe_url: String,
    pub stripe_secret_key: String,
    pub stripe_webhook_secret: String,
    pub price_monthly: String,
    pub price_yearly: String,
}

impl Config {
    /// Optional knobs default; the four STRIPE_* vars FAIL FAST, all missing
    /// names in one panic. A half-configured billing daemon is worse than a
    /// visible crash-loop: without the webhook secret it would silently drop
    /// paid upgrades.
    fn from_env() -> Self {
        let mut pro_quota: u64 = 1024 * 1024 * 1024;
        if let Ok(raw) = std::env::var("TACITUS_BILLING_PRO_QUOTA") {
            match raw.parse::<u64>() {
                Ok(bytes) if bytes > 0 => pro_quota = bytes,
                _ => tracing::warn!(
                    "TACITUS_BILLING_PRO_QUOTA={raw} is not a positive byte count; keeping {pro_quota}"
                ),
            }
        }
        let mut missing = Vec::new();
        // Empty counts as missing: docker-compose interpolation turns an
        // unset host var into "", which must not slip past the fail-fast.
        let mut required = |name: &'static str| match std::env::var(name) {
            Ok(value) if !value.is_empty() => value,
            _ => {
                missing.push(name);
                String::new()
            }
        };
        let config = Self {
            bind: std::env::var("TACITUS_BILLING_BIND").unwrap_or_else(|_| "127.0.0.1:8092".into()),
            entitlements_path: std::env::var("TACITUS_BILLING_ENTITLEMENTS")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("./relay-data/entitlements.json")),
            pro_quota,
            success_url: std::env::var("TACITUS_BILLING_SUCCESS_URL")
                .unwrap_or_else(|_| "https://tacitus.md".into()),
            cancel_url: std::env::var("TACITUS_BILLING_CANCEL_URL")
                .unwrap_or_else(|_| "https://tacitus.md".into()),
            stripe_url: std::env::var("TACITUS_BILLING_STRIPE_URL")
                .unwrap_or_else(|_| "https://api.stripe.com".into()),
            stripe_secret_key: required("STRIPE_SECRET_KEY"),
            stripe_webhook_secret: required("STRIPE_WEBHOOK_SECRET"),
            price_monthly: required("STRIPE_PRICE_MONTHLY"),
            price_yearly: required("STRIPE_PRICE_YEARLY"),
        };
        assert!(
            missing.is_empty(),
            "tacitus-billing cannot start — missing required env vars: {}",
            missing.join(", ")
        );
        config
    }
}

pub struct AppState {
    config: Config,
    store: EntStore,
    agent: ureq::Agent,
}

impl AppState {
    fn new(config: Config) -> Self {
        let store = EntStore::new(config.entitlements_path.clone());
        // http_status_as_error(false): we branch on the status ourselves so
        // Stripe's error JSON reaches our logs instead of vanishing.
        let agent = ureq::config::Config::builder()
            .http_status_as_error(false)
            .build()
            .new_agent();
        Self {
            config,
            store,
            agent,
        }
    }
}

pub fn app(state: Arc<AppState>) -> Router {
    // Full public paths — nginx proxies /billing/ verbatim (URI-less
    // proxy_pass), so there is no rewrite to get wrong and the webhook body
    // arrives untouched.
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/billing/checkout", get(checkout))
        .route("/billing/webhook", post(webhook))
        .with_state(state)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[derive(Deserialize)]
struct CheckoutParams {
    vault_id: String,
    plan: String,
}

async fn checkout(
    State(state): State<Arc<AppState>>,
    Query(params): Query<CheckoutParams>,
) -> Result<Redirect, (StatusCode, String)> {
    if !valid_vault_id(&params.vault_id) {
        return Err((
            StatusCode::BAD_REQUEST,
            "vault_id must be 32 lowercase hex chars".into(),
        ));
    }
    let price_id = match params.plan.as_str() {
        "monthly" => state.config.price_monthly.clone(),
        "yearly" => state.config.price_yearly.clone(),
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                "plan must be \"monthly\" or \"yearly\"".into(),
            ))
        }
    };
    let task_state = state.clone();
    let url = tokio::task::spawn_blocking(move || {
        create_checkout_session(
            &task_state.agent,
            &task_state.config,
            &params.vault_id,
            &price_id,
        )
    })
    .await
    .map_err(|e| {
        tracing::error!("checkout task failed: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
    })?
    .map_err(|e| {
        // Full Stripe detail stays in our logs — browsers get a generic line.
        tracing::error!("checkout session failed: {e}");
        (
            StatusCode::BAD_GATEWAY,
            "could not reach the payment provider — please try again shortly".into(),
        )
    })?;
    // axum's Redirect::to is 303 See Other — exactly right after a GET that
    // triggers an action elsewhere.
    Ok(Redirect::to(&url))
}

/// SECURITY: the Stripe signature is HMAC over the RAW bytes on the wire.
/// The body is extracted as `Bytes` (the last extractor, after HeaderMap)
/// and parsed ONLY after `verify_signature` passes — a `Json`/`String`
/// extractor here would verify a re-serialization instead of the wire bytes.
async fn webhook(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let Some(signature) = headers
        .get("stripe-signature")
        .and_then(|v| v.to_str().ok())
    else {
        return StatusCode::BAD_REQUEST;
    };
    let now = unix_now();
    if !verify_signature(&state.config.stripe_webhook_secret, signature, &body, now) {
        return StatusCode::BAD_REQUEST;
    }

    // Probe only {id, type} first: signed NON-subscription payloads (an
    // invoice, a future event class) have shapes the Subscription parse
    // would refuse — those are 200-ignored, never errors.
    #[derive(Deserialize)]
    struct Probe {
        id: String,
        #[serde(rename = "type")]
        kind: String,
    }
    let Ok(probe) = serde_json::from_slice::<Probe>(&body) else {
        tracing::warn!("signed webhook with an unparseable envelope — ignoring");
        return StatusCode::OK;
    };
    if !probe.kind.starts_with("customer.subscription.") {
        return StatusCode::OK;
    }
    let event: StripeEvent = match serde_json::from_slice(&body) {
        Ok(event) => event,
        Err(e) => {
            // A Stripe retry re-sends the same bytes — it can't fix a shape
            // mismatch, so don't ask for one.
            tracing::warn!(
                "subscription event {} has an unexpected shape: {e}",
                probe.id
            );
            return StatusCode::OK;
        }
    };
    match event.data.object.metadata.get("vault_id") {
        Some(vault_id) if valid_vault_id(vault_id) => {}
        _ => {
            // A subscription we didn't create (no vault_id) — e.g. a future
            // Teams price. None of our business.
            tracing::warn!(
                "subscription event {} without a valid vault_id — ignoring",
                event.id
            );
            return StatusCode::OK;
        }
    }

    let event_id = event.id.clone();
    let pro_quota = state.config.pro_quota;
    let task_state = state.clone();
    match tokio::task::spawn_blocking(move || task_state.store.apply(&event, pro_quota, now)).await
    {
        Ok(Ok(changed)) => {
            if changed {
                tracing::info!("event {event_id} applied to entitlements");
            }
            StatusCode::OK
        }
        // 500 makes Stripe retry — for transient disk trouble the retry IS
        // the recovery path; for a corrupt/foreign-versioned file the
        // retries keep failing loudly until a human intervenes (the relay
        // keeps serving its last good set meanwhile).
        Ok(Err(e)) => {
            tracing::error!("entitlements write for {event_id} failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
        Err(e) => {
            tracing::error!("webhook task failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_target(false).init();
    let config = Config::from_env();
    let bind = config.bind.clone();
    let state = Arc::new(AppState::new(config));

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .unwrap_or_else(|e| panic!("cannot bind {bind}: {e}"));
    tracing::info!("tacitus-billing listening on {bind}");
    axum::serve(listener, app(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("server error");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stripe::tests::sign_header;
    use std::sync::Mutex;

    fn temp_ent_path(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "tacitus-billingtest-{tag}-{nanos}/entitlements.json"
        ))
    }

    const WEBHOOK_SECRET: &str = "whsec_testsecret";

    fn test_config(entitlements_path: PathBuf, stripe_url: &str) -> Config {
        Config {
            bind: String::new(),
            entitlements_path,
            pro_quota: 1024, // byte-scale, per repo convention
            success_url: "https://tacitus.md/thanks".into(),
            cancel_url: "https://tacitus.md/cancel".into(),
            stripe_url: stripe_url.trim_end_matches('/').into(),
            stripe_secret_key: "sk_test_dummy".into(),
            stripe_webhook_secret: WEBHOOK_SECRET.into(),
            price_monthly: "price_month_1".into(),
            price_yearly: "price_year_1".into(),
        }
    }

    async fn spawn_billing(config: Config) -> String {
        let state = Arc::new(AppState::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app(state)).await.unwrap();
        });
        format!("http://{addr}")
    }

    /// In-process fake Stripe: records (authorization, form body) and
    /// replies with a fixed (status, body) — the same in-process-server
    /// style as the relay's tests.
    struct FakeStripe {
        status: u16,
        reply: String,
        seen: Mutex<Vec<(String, String)>>,
    }

    async fn spawn_fake_stripe(status: u16, reply: &str) -> (String, Arc<FakeStripe>) {
        let fake = Arc::new(FakeStripe {
            status,
            reply: reply.to_string(),
            seen: Mutex::new(Vec::new()),
        });
        async fn handler(
            State(fake): State<Arc<FakeStripe>>,
            headers: HeaderMap,
            body: String,
        ) -> (StatusCode, String) {
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            fake.seen.lock().unwrap().push((auth, body));
            (
                StatusCode::from_u16(fake.status).unwrap(),
                fake.reply.clone(),
            )
        }
        let router = Router::new()
            .route("/v1/checkout/sessions", post(handler))
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (format!("http://{addr}"), fake)
    }

    /// Blocking ureq client for tests: no redirect following (we assert the
    /// 303 itself), statuses returned instead of erroring.
    fn test_agent() -> ureq::Agent {
        ureq::config::Config::builder()
            .max_redirects(0)
            .http_status_as_error(false)
            .build()
            .new_agent()
    }

    async fn http_get(url: String) -> (u16, Option<String>, String) {
        tokio::task::spawn_blocking(move || {
            let mut resp = test_agent().get(&url).call().unwrap();
            let location = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(String::from);
            let status = resp.status().as_u16();
            let body = resp.body_mut().read_to_string().unwrap_or_default();
            (status, location, body)
        })
        .await
        .unwrap()
    }

    async fn post_webhook(url: String, signature: Option<String>, body: Vec<u8>) -> u16 {
        tokio::task::spawn_blocking(move || {
            let mut req = test_agent().post(&url);
            if let Some(sig) = signature {
                req = req.header("Stripe-Signature", sig);
            }
            req.send(&body[..]).unwrap().status().as_u16()
        })
        .await
        .unwrap()
    }

    fn sub_event_body(kind: &str, status: &str, vault_id: &str, created: i64) -> Vec<u8> {
        serde_json::json!({
            "id": format!("evt_{created}"),
            "type": kind,
            "created": created,
            "data": { "object": {
                "id": "sub_test", "customer": "cus_test", "status": status,
                "metadata": { "vault_id": vault_id },
            }},
        })
        .to_string()
        .into_bytes()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn healthz_ok() {
        let base = spawn_billing(test_config(temp_ent_path("healthz"), "http://127.0.0.1:9")).await;
        let (status, _, body) = http_get(format!("{base}/healthz")).await;
        assert_eq!((status, body.as_str()), (200, "ok"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn checkout_redirects_303_to_stripe_session_url() {
        let session_url = "https://checkout.stripe.com/c/pay/cs_test_1";
        let (stripe_url, fake) = spawn_fake_stripe(
            200,
            &format!(r#"{{"id":"cs_test_1","url":"{session_url}"}}"#),
        )
        .await;
        let base = spawn_billing(test_config(temp_ent_path("checkout"), &stripe_url)).await;
        let vid = "a1".repeat(16);

        for (plan, price) in [("monthly", "price_month_1"), ("yearly", "price_year_1")] {
            let (status, location, _) = http_get(format!(
                "{base}/billing/checkout?vault_id={vid}&plan={plan}"
            ))
            .await;
            assert_eq!(status, 303, "{plan}");
            assert_eq!(location.as_deref(), Some(session_url));

            let (auth, form) = fake.seen.lock().unwrap().last().unwrap().clone();
            assert_eq!(auth, "Bearer sk_test_dummy");
            assert!(form.contains("mode=subscription"), "{form}");
            assert!(
                form.contains(&format!("line_items%5B0%5D%5Bprice%5D={price}")),
                "{form}"
            );
            assert!(
                form.contains(&format!(
                    "subscription_data%5Bmetadata%5D%5Bvault_id%5D={vid}"
                )),
                "{form}"
            );
            assert!(
                form.contains("success_url=https%3A%2F%2Ftacitus.md%2Fthanks"),
                "{form}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn checkout_rejects_bad_vault_id_and_bad_plan() {
        let (stripe_url, fake) = spawn_fake_stripe(200, "{}").await;
        let base = spawn_billing(test_config(temp_ent_path("badreq"), &stripe_url)).await;
        let vid = "a1".repeat(16);

        for bad in [
            format!("{base}/billing/checkout?vault_id=UPPER&plan=monthly"),
            format!("{base}/billing/checkout?vault_id={vid}&plan=weekly"),
            format!("{base}/billing/checkout?vault_id=..%2F..%2Fetc&plan=monthly"),
        ] {
            let (status, location, _) = http_get(bad).await;
            assert_eq!(status, 400);
            assert!(location.is_none());
        }
        assert!(
            fake.seen.lock().unwrap().is_empty(),
            "Stripe never contacted"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn checkout_maps_stripe_failure_to_502() {
        let (stripe_url, _fake) =
            spawn_fake_stripe(402, r#"{"error":{"message":"Your card was declined-ish"}}"#).await;
        let base = spawn_billing(test_config(temp_ent_path("stripefail"), &stripe_url)).await;
        let vid = "b2".repeat(16);

        let (status, location, body) = http_get(format!(
            "{base}/billing/checkout?vault_id={vid}&plan=monthly"
        ))
        .await;
        assert_eq!(status, 502);
        assert!(location.is_none());
        assert!(
            !body.contains("declined-ish"),
            "Stripe internals never reach the browser: {body}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn webhook_full_cycle_writes_and_removes_entitlement() {
        let ent = temp_ent_path("cycle");
        let base = spawn_billing(test_config(ent.clone(), "http://127.0.0.1:9")).await;
        let vid = "c3".repeat(16);
        let url = format!("{base}/billing/webhook");

        // A signed created(active) writes the entitlement…
        let body = sub_event_body("customer.subscription.created", "active", &vid, 100);
        let sig = sign_header(WEBHOOK_SECRET, unix_now(), &body);
        assert_eq!(post_webhook(url.clone(), Some(sig), body).await, 200);
        let raw = std::fs::read_to_string(&ent).unwrap();
        assert!(
            raw.contains(&vid) && raw.contains("\"quota_bytes\":1024"),
            "{raw}"
        );

        // …and a signed deleted removes it (tombstone stays).
        let body = sub_event_body("customer.subscription.deleted", "canceled", &vid, 200);
        let sig = sign_header(WEBHOOK_SECRET, unix_now(), &body);
        assert_eq!(post_webhook(url, Some(sig), body).await, 200);
        let raw = std::fs::read_to_string(&ent).unwrap();
        assert!(!raw.contains("\"quota_bytes\":1024"), "{raw}");
        assert!(raw.contains("billing_removed"), "{raw}");
        std::fs::remove_dir_all(ent.parent().unwrap()).ok();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn webhook_rejects_bad_signature_and_missing_header() {
        let ent = temp_ent_path("badsig");
        let base = spawn_billing(test_config(ent.clone(), "http://127.0.0.1:9")).await;
        let vid = "d4".repeat(16);
        let url = format!("{base}/billing/webhook");
        let body = sub_event_body("customer.subscription.created", "active", &vid, 100);

        let wrong = sign_header("whsec_WRONG", unix_now(), &body);
        assert_eq!(
            post_webhook(url.clone(), Some(wrong), body.clone()).await,
            400
        );
        assert_eq!(post_webhook(url, None, body).await, 400);
        assert!(!ent.exists(), "file untouched on rejected webhooks");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn webhook_ignores_unrelated_and_foreign_events() {
        let ent = temp_ent_path("foreign");
        let base = spawn_billing(test_config(ent.clone(), "http://127.0.0.1:9")).await;
        let url = format!("{base}/billing/webhook");

        // A correctly signed event of a class we don't subscribe to.
        let invoice = br#"{"id":"evt_inv","type":"invoice.paid","created":1,"data":{"object":{"amount_due":500}}}"#.to_vec();
        let sig = sign_header(WEBHOOK_SECRET, unix_now(), &invoice);
        assert_eq!(post_webhook(url.clone(), Some(sig), invoice).await, 200);

        // A subscription event that isn't ours (no vault_id metadata).
        let foreign = br#"{"id":"evt_f","type":"customer.subscription.created","created":2,"data":{"object":{"id":"sub_x","customer":"cus_x","status":"active","metadata":{}}}}"#.to_vec();
        let sig = sign_header(WEBHOOK_SECRET, unix_now(), &foreign);
        assert_eq!(post_webhook(url, Some(sig), foreign).await, 200);

        assert!(!ent.exists(), "nothing was ever written");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn webhook_replay_is_idempotent() {
        let ent = temp_ent_path("replay");
        let base = spawn_billing(test_config(ent.clone(), "http://127.0.0.1:9")).await;
        let vid = "e5".repeat(16);
        let url = format!("{base}/billing/webhook");

        let body = sub_event_body("customer.subscription.created", "active", &vid, 100);
        let sig = sign_header(WEBHOOK_SECRET, unix_now(), &body);
        assert_eq!(
            post_webhook(url.clone(), Some(sig.clone()), body.clone()).await,
            200
        );
        let first = std::fs::read(&ent).unwrap();
        // Stripe retries deliver the identical signed bytes.
        assert_eq!(post_webhook(url, Some(sig), body).await, 200);
        let second = std::fs::read(&ent).unwrap();
        assert_eq!(first, second, "replay leaves the file byte-identical");
        std::fs::remove_dir_all(ent.parent().unwrap()).ok();
    }
}
