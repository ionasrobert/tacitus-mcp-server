//! Ephemeral presence (collab-m2): who's online in the vault, which note
//! they're on, whether they're editing. Payloads travel E2E-encrypted under
//! the AAD domain `"{vault_id}#presence"`; the relay fans them out to
//! presence-capable connections without logging, sequencing, or acking.
//! Departure is `gone: true` (clean shutdown) or TTL expiry (crash).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// What a device is doing right now. `Default` = online, nothing open.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PresenceState {
    /// UI-level note id ("projects/launch"), not the `n:`-prefixed doc key.
    pub note_id: Option<String>,
    #[serde(default)]
    pub editing: bool,
}

/// What travels inside the ciphertext. Every field except `v`/`device` is
/// `serde(default)` so the payload can grow without breaking old peers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresencePayload {
    pub v: u32,
    pub device: String,
    /// Sender's wall clock, ms since epoch — per-device last-writer-wins so
    /// a replaced session's late `gone` can't erase its successor's hello.
    #[serde(default)]
    pub ts: u64,
    /// "I just joined — please announce yourselves" (receivers reply with
    /// their own state through the normal send debounce; replies never set
    /// `hello`, so there are no loops).
    #[serde(default)]
    pub hello: bool,
    #[serde(default)]
    pub gone: bool,
    #[serde(default)]
    pub state: PresenceState,
}

/// A peer as surfaced to hosts (status bar, CLI).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub device: String,
    pub note_id: Option<String>,
    pub editing: bool,
}

/// The AAD domain for presence blobs. vault_id is 32 hex chars — `#` can
/// never appear in a real vault_id, so the domains are unambiguous.
pub(crate) fn presence_aad(vault_id: &str) -> String {
    format!("{vault_id}#presence")
}

/// Tracks the live peer set. Pure (caller passes `Instant::now()`), so the
/// LWW/TTL logic is unit-testable without a runtime.
pub struct PeerTracker {
    ttl: Duration,
    peers: BTreeMap<String, PeerEntry>,
}

struct PeerEntry {
    state: PresenceState,
    seen: Instant,
    last_ts: u64,
}

impl PeerTracker {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            peers: BTreeMap::new(),
        }
    }

    /// Fold one payload in; returns true when the visible peer set changed.
    /// Stale payloads (`ts` older than the newest seen from that device)
    /// are ignored — including a stale `gone` racing a fresh session.
    pub fn observe(&mut self, payload: &PresencePayload, now: Instant) -> bool {
        if let Some(entry) = self.peers.get(&payload.device) {
            if payload.ts < entry.last_ts {
                return false;
            }
        }
        if payload.gone {
            return self.peers.remove(&payload.device).is_some();
        }
        match self.peers.get_mut(&payload.device) {
            Some(entry) => {
                entry.seen = now;
                entry.last_ts = payload.ts;
                if entry.state == payload.state {
                    return false; // heartbeat with no news
                }
                entry.state = payload.state.clone();
                true
            }
            None => {
                self.peers.insert(
                    payload.device.clone(),
                    PeerEntry {
                        state: payload.state.clone(),
                        seen: now,
                        last_ts: payload.ts,
                    },
                );
                true
            }
        }
    }

    /// Drop peers not heard from within the TTL (crashed — no `gone` came).
    pub fn sweep(&mut self, now: Instant) -> bool {
        let ttl = self.ttl;
        let before = self.peers.len();
        self.peers
            .retain(|_, entry| now.duration_since(entry.seen) < ttl);
        self.peers.len() != before
    }

    /// The visible peers, deterministically ordered by device id.
    pub fn snapshot(&self) -> Vec<Peer> {
        self.peers
            .iter()
            .map(|(device, entry)| Peer {
                device: device.clone(),
                note_id: entry.state.note_id.clone(),
                editing: entry.state.editing,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(device: &str, ts: u64, state: PresenceState) -> PresencePayload {
        PresencePayload {
            v: 1,
            device: device.into(),
            ts,
            hello: false,
            gone: false,
            state,
        }
    }

    fn viewing(note: &str) -> PresenceState {
        PresenceState {
            note_id: Some(note.into()),
            editing: false,
        }
    }

    #[test]
    fn observe_reports_changes_and_swallows_identical_heartbeats() {
        let mut tracker = PeerTracker::new(Duration::from_secs(45));
        let now = Instant::now();
        assert!(tracker.observe(&payload("dev_a", 1, viewing("plan")), now));
        assert!(
            !tracker.observe(&payload("dev_a", 2, viewing("plan")), now),
            "identical heartbeat is not a change"
        );
        assert!(tracker.observe(&payload("dev_a", 3, viewing("notes")), now));
        let snap = tracker.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].note_id.as_deref(), Some("notes"));
    }

    #[test]
    fn gone_removes_but_a_stale_gone_cannot_erase_a_fresh_session() {
        let mut tracker = PeerTracker::new(Duration::from_secs(45));
        let now = Instant::now();
        tracker.observe(&payload("dev_a", 10, viewing("plan")), now);

        // A replaced session's late gone (older ts) must be ignored.
        let mut stale_gone = payload("dev_a", 5, PresenceState::default());
        stale_gone.gone = true;
        assert!(!tracker.observe(&stale_gone, now));
        assert_eq!(tracker.snapshot().len(), 1, "peer survived the stale gone");

        // A current gone removes.
        let mut gone = payload("dev_a", 11, PresenceState::default());
        gone.gone = true;
        assert!(tracker.observe(&gone, now));
        assert!(tracker.snapshot().is_empty());

        // gone for an unknown device is not a change.
        assert!(!tracker.observe(&gone, now));
    }

    #[test]
    fn sweep_expires_only_silent_peers() {
        let mut tracker = PeerTracker::new(Duration::from_millis(100));
        let start = Instant::now();
        tracker.observe(&payload("dev_old", 1, viewing("a")), start);
        let later = start + Duration::from_millis(80);
        tracker.observe(&payload("dev_fresh", 1, viewing("b")), later);

        let at_sweep = start + Duration::from_millis(120);
        assert!(tracker.sweep(at_sweep), "the silent peer expired");
        let snap = tracker.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].device, "dev_fresh");
        assert!(!tracker.sweep(at_sweep), "nothing left to expire");
    }

    #[test]
    fn snapshot_is_deterministically_ordered() {
        let mut tracker = PeerTracker::new(Duration::from_secs(45));
        let now = Instant::now();
        tracker.observe(&payload("dev_zz", 1, viewing("z")), now);
        tracker.observe(&payload("dev_aa", 1, viewing("a")), now);
        let snap = tracker.snapshot();
        let devices: Vec<&str> = snap.iter().map(|p| p.device.as_str()).collect();
        assert_eq!(devices, vec!["dev_aa", "dev_zz"]);
    }

    #[test]
    fn payload_fields_all_default_for_forward_compat() {
        let minimal: PresencePayload = serde_json::from_str(r#"{"v":1,"device":"dev_x"}"#).unwrap();
        assert_eq!(minimal.ts, 0);
        assert!(!minimal.hello && !minimal.gone);
        assert_eq!(minimal.state, PresenceState::default());
    }
}
