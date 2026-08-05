//! The Stripe-facing pieces: webhook signature verification (pure, clock
//! injected), the minimal event shapes we read, the event → action mapping,
//! and the one outbound call (Checkout Session creation, blocking ureq —
//! always behind spawn_blocking).

use std::collections::BTreeMap;

use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;

use crate::Config;

/// Webhooks older or newer than this many seconds are rejected — Stripe's
/// own recommended replay-protection window.
const SIGNATURE_TOLERANCE_SECS: i64 = 300;

/// `vault_id` is 32 lowercase hex chars — anything else never reaches Stripe
/// or the entitlements file. Mirrored from tacitus-relay's hub.rs: the relay
/// is deliberately not a lib (same precedent as it mirroring the wire enums).
pub fn valid_vault_id(vault_id: &str) -> bool {
    vault_id.len() == 32
        && vault_id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Decode a hex string; None on odd length or a non-hex char. (The repo
/// hand-rolls hex — encode side is `{byte:02x}` in tacitus-core — so no hex
/// crate here either.)
fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Parse `Stripe-Signature: t=1712345678,v1=abc…,v1=old…,v0=ignored` into
/// (timestamp, every v1 signature). None when t or any v1 is missing.
fn parse_sig_header(header: &str) -> Option<(i64, Vec<String>)> {
    let mut t = None;
    let mut v1s = Vec::new();
    for part in header.split(',') {
        match part.trim().split_once('=') {
            Some(("t", raw)) => t = raw.parse::<i64>().ok(),
            Some(("v1", sig)) => v1s.push(sig.to_string()),
            _ => {} // v0 and future schemes are deliberately ignored
        }
    }
    match (t, v1s.is_empty()) {
        (Some(t), false) => Some((t, v1s)),
        _ => None,
    }
}

/// Verify a Stripe webhook signature over the RAW request body bytes:
/// HMAC-SHA256 of `"{t}.{payload}"` with the endpoint's signing secret.
/// Any one matching v1 passes (Stripe sends several during key rotation).
/// `now` is injected so tests never sleep; `verify_slice` is constant-time.
pub fn verify_signature(secret: &str, header: &str, payload: &[u8], now: i64) -> bool {
    let Some((t, v1s)) = parse_sig_header(header) else {
        tracing::warn!("webhook signature header unparseable");
        return false;
    };
    if (now - t).abs() > SIGNATURE_TOLERANCE_SECS {
        // The delta matters: a consistently large value means NTP drift on
        // one side, not an attack.
        tracing::warn!(
            "webhook timestamp outside tolerance (now - t = {}s)",
            now - t
        );
        return false;
    }
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac accepts any key length");
    mac.update(t.to_string().as_bytes());
    mac.update(b".");
    mac.update(payload);
    v1s.iter()
        .any(|v1| hex_decode(v1).is_some_and(|sig| mac.clone().verify_slice(&sig).is_ok()))
}

/// The only fields we read from a Stripe event; serde ignores the rest of
/// the (large) payload.
#[derive(Debug, Deserialize)]
pub struct StripeEvent {
    /// evt_… — logging only.
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    /// Unix seconds — drives the out-of-order guard in entitlements.rs.
    pub created: i64,
    pub data: EventData,
}

#[derive(Debug, Deserialize)]
pub struct EventData {
    pub object: Subscription,
}

#[derive(Debug, Deserialize)]
pub struct Subscription {
    /// sub_…
    pub id: String,
    /// cus_… (a plain string id on subscription events).
    pub customer: String,
    /// active | trialing | past_due | canceled | …
    pub status: String,
    /// Carries `vault_id`, set at checkout via subscription_data.
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Upsert,
    Remove,
    Ignore,
}

/// Map an event to what it does to the vault's entitlement. Upsert is an
/// ALLOWLIST: any subscription state we don't recognize removes — removal
/// of an absent entitlement is a no-op, so unknown future statuses fail
/// safe toward "not entitled", never toward free Pro.
pub fn action_for(event_kind: &str, status: &str) -> Action {
    if !event_kind.starts_with("customer.subscription.") {
        return Action::Ignore;
    }
    if event_kind == "customer.subscription.deleted" {
        return Action::Remove;
    }
    match status {
        "active" | "trialing" | "past_due" => Action::Upsert,
        _ => Action::Remove,
    }
}

/// Create a subscription Checkout Session; returns the hosted checkout URL.
/// Blocking (ureq) — callers wrap in tokio::task::spawn_blocking. The agent
/// is built with http_status_as_error(false) so Stripe's error JSON reaches
/// our logs instead of vanishing into a bare status code.
pub fn create_checkout_session(
    agent: &ureq::Agent,
    config: &Config,
    vault_id: &str,
    price_id: &str,
) -> Result<String, String> {
    let mut resp = agent
        .post(format!("{}/v1/checkout/sessions", config.stripe_url))
        .header(
            "Authorization",
            format!("Bearer {}", config.stripe_secret_key),
        )
        .send_form([
            ("mode", "subscription"),
            ("line_items[0][price]", price_id),
            ("line_items[0][quantity]", "1"),
            // On the SUBSCRIPTION, so every lifecycle webhook repeats it…
            ("subscription_data[metadata][vault_id]", vault_id),
            // …and on the session too, for dashboard visibility.
            ("metadata[vault_id]", vault_id),
            ("success_url", &config.success_url),
            ("cancel_url", &config.cancel_url),
        ])
        .map_err(|e| format!("stripe unreachable: {e}"))?;
    if !resp.status().is_success() {
        let body = resp.body_mut().read_to_string().unwrap_or_default();
        return Err(format!("stripe {}: {body}", resp.status()));
    }
    let session: serde_json::Value = resp
        .body_mut()
        .read_json()
        .map_err(|e| format!("stripe returned malformed JSON: {e}"))?;
    session
        .get("url")
        .and_then(|u| u.as_str())
        .map(String::from)
        .ok_or_else(|| "checkout session has no url".into())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Compose a valid Stripe-Signature header the way Stripe does — shared
    /// with the HTTP-level tests in main.rs.
    pub(crate) fn sign_header(secret: &str, t: i64, payload: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(t.to_string().as_bytes());
        mac.update(b".");
        mac.update(payload);
        let sig = mac.finalize().into_bytes();
        use std::fmt::Write;
        let mut hex = String::new();
        for byte in sig {
            write!(hex, "{byte:02x}").unwrap();
        }
        format!("t={t},v1={hex}")
    }

    #[test]
    fn verify_signature_accepts_valid_and_rejects_tampered() {
        let payload = br#"{"id":"evt_1","type":"customer.subscription.updated"}"#;
        let header = sign_header("whsec_test", 1000, payload);
        assert!(verify_signature("whsec_test", &header, payload, 1000));

        // A flipped payload byte fails…
        let mut tampered = payload.to_vec();
        tampered[10] ^= 1;
        assert!(!verify_signature("whsec_test", &header, &tampered, 1000));
        // …so does a flipped signature nibble…
        let bad_header = header.replace("v1=", "v1=0");
        assert!(!verify_signature("whsec_test", &bad_header, payload, 1000));
        // …and the wrong secret.
        assert!(!verify_signature("whsec_other", &header, payload, 1000));
    }

    #[test]
    fn verify_signature_rejects_outside_tolerance() {
        let payload = b"body";
        let header = sign_header("whsec_test", 1000, payload);
        assert!(verify_signature("whsec_test", &header, payload, 1000 + 299));
        assert!(verify_signature("whsec_test", &header, payload, 1000 - 299));
        assert!(!verify_signature(
            "whsec_test",
            &header,
            payload,
            1000 + 301
        ));
        assert!(!verify_signature(
            "whsec_test",
            &header,
            payload,
            1000 - 301
        ));
    }

    #[test]
    fn verify_signature_accepts_rotated_v1_and_ignores_v0() {
        let payload = b"rotating";
        let good = sign_header("whsec_new", 500, payload);
        let good_sig = good.split("v1=").nth(1).unwrap();
        // Key rotation: a stale v1 from the old secret plus the valid one,
        // and a v0 that must never be consulted.
        let header = format!("t=500,v0=deadbeef,v1=00ff00ff,v1={good_sig}");
        assert!(verify_signature("whsec_new", &header, payload, 500));
    }

    #[test]
    fn parse_sig_header_and_hex_decode_reject_malformed() {
        assert!(parse_sig_header("v1=abcd").is_none(), "missing t");
        assert!(parse_sig_header("t=notanumber,v1=abcd").is_none());
        assert!(parse_sig_header("t=100").is_none(), "missing v1");
        assert_eq!(
            parse_sig_header("t=100, v1=aa, v0=bb").map(|(t, v)| (t, v.len())),
            Some((100, 1)),
            "whitespace tolerated, v0 not collected"
        );
        assert!(hex_decode("abc").is_none(), "odd length");
        assert!(hex_decode("zz").is_none(), "non-hex");
        assert_eq!(hex_decode("00ff"), Some(vec![0, 255]));
    }

    #[test]
    fn action_for_maps_subscription_states() {
        use Action::*;
        // deleted removes regardless of the status snapshot it carries.
        assert_eq!(
            action_for("customer.subscription.deleted", "active"),
            Remove
        );
        // The Upsert allowlist.
        for status in ["active", "trialing", "past_due"] {
            assert_eq!(action_for("customer.subscription.created", status), Upsert);
            assert_eq!(action_for("customer.subscription.updated", status), Upsert);
        }
        // Everything else — incl. unknown future statuses — fails safe.
        for status in [
            "canceled",
            "unpaid",
            "incomplete",
            "incomplete_expired",
            "paused",
            "quantum",
        ] {
            assert_eq!(action_for("customer.subscription.updated", status), Remove);
        }
        // Non-subscription events are none of our business.
        assert_eq!(action_for("invoice.paid", "active"), Ignore);
        assert_eq!(action_for("checkout.session.completed", "active"), Ignore);
    }

    #[test]
    fn valid_vault_id_mirrors_relay_rules() {
        assert!(valid_vault_id(&"a1".repeat(16)));
        assert!(!valid_vault_id("../../etc/passwd"));
        assert!(!valid_vault_id(&"A1".repeat(16)));
        assert!(!valid_vault_id(&"a1".repeat(15)));
        assert!(!valid_vault_id(""));
    }
}
