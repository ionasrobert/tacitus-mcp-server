//! The entitlements.json writer. Billing OWNS this file; the relay only ever
//! reads it (hot-reload on (mtime,len) change, keeps its last good set on a
//! parse error). Contract honored here, mirrored from the relay's hub.rs:
//! `version` must be exactly 1, `quota_bytes` is the only enforced field,
//! and unknown fields are tolerated on both levels — which is what lets us
//! keep Stripe bookkeeping in the same file AND preserve hand-written
//! entries an operator comps by editing the file directly.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::stripe::{action_for, Action, StripeEvent};

/// The whole file. BTreeMaps make serialization deterministic: clean diffs,
/// and webhook replays can be asserted byte-identical.
#[derive(Debug, Serialize, Deserialize)]
pub struct EntFile {
    /// Pinned: written as 1, refused (never clobbered) when ≠ 1 on load.
    pub version: u32,
    #[serde(default)]
    pub vaults: BTreeMap<String, Entitlement>,
    /// Tombstones for the resurrection trap: after `deleted` removes an
    /// entry (and its last_event_ts with it), a retried OLDER
    /// `updated(active)` delivery must not recreate Pro for a canceled
    /// subscription. Billing-owned, relay-ignored.
    #[serde(
        default,
        rename = "billing_removed",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub removed: BTreeMap<String, i64>,
    /// Foreign top-level fields survive our rewrite.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl EntFile {
    fn fresh() -> Self {
        Self {
            version: 1,
            vaults: BTreeMap::new(),
            removed: BTreeMap::new(),
            extra: serde_json::Map::new(),
        }
    }
}

/// One vault's entry. Only `quota_bytes` matters to the relay; the rest is
/// billing bookkeeping. Every optional skips serialization when absent so a
/// hand-written `{"quota_bytes":N}` comp round-trips without sprouting
/// nulls.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entitlement {
    pub quota_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stripe_customer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stripe_subscription: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Wall clock at our write — human debugging only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<i64>,
    /// Stripe `event.created` of the last applied event — the ordering
    /// guard (Stripe does not guarantee delivery order).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event_ts: Option<i64>,
    /// Foreign per-entry fields survive our rewrite.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Apply one VERIFIED subscription event to a parsed file. Pure — no IO, no
/// clock reads (`now` is only recorded as updated_at). Returns whether the
/// file changed and must be persisted.
///
/// Ordering rules: an Upsert must beat the entry's last_event_ts AND any
/// tombstone; a Remove must beat the entry's last_event_ts. A winning
/// Upsert prunes its tombstone. Equal timestamps do NOT re-apply — that is
/// what makes webhook retries byte-idempotent.
pub fn apply_event(file: &mut EntFile, ev: &StripeEvent, pro_quota: u64, now: i64) -> bool {
    let sub = &ev.data.object;
    let Some(vault_id) = sub.metadata.get("vault_id") else {
        return false;
    };
    let newer_than_entry = file
        .vaults
        .get(vault_id)
        .and_then(|e| e.last_event_ts)
        .is_none_or(|ts| ev.created > ts);
    match action_for(&ev.kind, &sub.status) {
        Action::Ignore => false,
        Action::Upsert => {
            let newer_than_tombstone = file.removed.get(vault_id).is_none_or(|&ts| ev.created > ts);
            if !newer_than_entry || !newer_than_tombstone {
                tracing::info!("ignoring out-of-order event {} for vault {vault_id}", ev.id);
                return false;
            }
            // Preserve any foreign fields an operator added to the entry.
            let extra = file
                .vaults
                .get(vault_id)
                .map(|e| e.extra.clone())
                .unwrap_or_default();
            file.vaults.insert(
                vault_id.clone(),
                Entitlement {
                    quota_bytes: pro_quota,
                    tier: Some("pro".into()),
                    stripe_customer: Some(sub.customer.clone()),
                    stripe_subscription: Some(sub.id.clone()),
                    status: Some(sub.status.clone()),
                    updated_at: Some(now),
                    last_event_ts: Some(ev.created),
                    extra,
                },
            );
            file.removed.remove(vault_id);
            true
        }
        Action::Remove => {
            if !newer_than_entry {
                tracing::info!(
                    "ignoring out-of-order removal {} for vault {vault_id}",
                    ev.id
                );
                return false;
            }
            let existed = file.vaults.remove(vault_id).is_some();
            let stale_tombstone = file
                .removed
                .get(vault_id)
                .is_some_and(|&ts| ts < ev.created);
            if existed || stale_tombstone || !file.removed.contains_key(vault_id) {
                file.removed.insert(vault_id.clone(), ev.created);
            }
            // Even a no-op removal advances the tombstone — cheap, and it
            // keeps the guard monotonic.
            existed || stale_tombstone
        }
    }
}

/// Locked read-modify-write around the file. One store per process; the
/// mutex serializes concurrent webhook deliveries.
pub struct EntStore {
    path: PathBuf,
    lock: std::sync::Mutex<()>,
}

impl EntStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: std::sync::Mutex::new(()),
        }
    }

    /// Missing file → a fresh `{"version":1}` (created on first persist).
    /// A parse error or unknown version is an Err — we NEVER clobber a file
    /// we can't parse: the webhook 500s (Stripe retries), the relay keeps
    /// serving its last good set, and a human fixes the file.
    fn load(&self) -> io::Result<EntFile> {
        let raw = match fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(EntFile::fresh()),
            Err(e) => return Err(e),
        };
        let file: EntFile = serde_json::from_str(&raw).map_err(io::Error::other)?;
        if file.version != 1 {
            return Err(io::Error::other(format!(
                "entitlements version {} is not 1 — refusing to rewrite",
                file.version
            )));
        }
        Ok(file)
    }

    /// Atomic publish: tmp + fsync + rename in the same directory (same
    /// filesystem, so rename(2) stays atomic — the relay sees either the
    /// complete old file or the complete new one, never a torn read).
    /// Mirrors the relay's publish_snapshot.
    fn persist(&self, file: &EntFile) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let line = serde_json::to_string(file).map_err(io::Error::other)?;
        let tmp = self.path.with_extension("json.tmp");
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(line.as_bytes())?;
            f.sync_data()?;
        }
        fs::rename(&tmp, &self.path)
    }

    /// lock → load → apply → persist-if-changed. Blocking (fsync) — callers
    /// wrap in spawn_blocking.
    pub fn apply(&self, ev: &StripeEvent, pro_quota: u64, now: i64) -> io::Result<bool> {
        let _guard = self.lock.lock().unwrap();
        let mut file = self.load()?;
        let changed = apply_event(&mut file, ev, pro_quota, now);
        if changed {
            self.persist(&file)?;
        }
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("tacitus-billing-{tag}-{nanos}/entitlements.json"))
    }

    fn sub_event(kind: &str, status: &str, vault_id: &str, created: i64) -> StripeEvent {
        serde_json::from_value(serde_json::json!({
            "id": format!("evt_{created}"),
            "type": kind,
            "created": created,
            "data": { "object": {
                "id": "sub_1", "customer": "cus_1", "status": status,
                "metadata": { "vault_id": vault_id },
            }},
        }))
        .unwrap()
    }

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn apply_event_upserts_pro_and_removes_on_delete() {
        let vid = "a1".repeat(16);
        let mut file = EntFile::fresh();

        let ev = sub_event("customer.subscription.created", "active", &vid, 100);
        assert!(apply_event(&mut file, &ev, GIB, 999));
        let ent = &file.vaults[&vid];
        assert_eq!(ent.quota_bytes, GIB);
        assert_eq!(ent.tier.as_deref(), Some("pro"));
        assert_eq!(ent.stripe_subscription.as_deref(), Some("sub_1"));
        assert_eq!(ent.status.as_deref(), Some("active"));
        assert_eq!(ent.last_event_ts, Some(100));
        assert_eq!(ent.updated_at, Some(999));

        let ev = sub_event("customer.subscription.deleted", "canceled", &vid, 200);
        assert!(apply_event(&mut file, &ev, GIB, 999));
        assert!(!file.vaults.contains_key(&vid));
        assert_eq!(file.removed.get(&vid), Some(&200));
    }

    #[test]
    fn apply_event_ignores_out_of_order_deliveries() {
        let vid = "b2".repeat(16);
        let mut file = EntFile::fresh();

        // active(200), then a straggling canceled(100): still entitled.
        assert!(apply_event(
            &mut file,
            &sub_event("customer.subscription.updated", "active", &vid, 200),
            GIB,
            0
        ));
        assert!(!apply_event(
            &mut file,
            &sub_event("customer.subscription.updated", "canceled", &vid, 100),
            GIB,
            0
        ));
        assert!(file.vaults.contains_key(&vid));

        // deleted(300), then a REPLAYED active(250): not resurrected.
        assert!(apply_event(
            &mut file,
            &sub_event("customer.subscription.deleted", "canceled", &vid, 300),
            GIB,
            0
        ));
        assert!(!apply_event(
            &mut file,
            &sub_event("customer.subscription.updated", "active", &vid, 250),
            GIB,
            0
        ));
        assert!(
            !file.vaults.contains_key(&vid),
            "tombstone blocks resurrection"
        );

        // A genuinely NEW subscription (400) re-entitles and prunes the
        // tombstone.
        assert!(apply_event(
            &mut file,
            &sub_event("customer.subscription.created", "active", &vid, 400),
            GIB,
            0
        ));
        assert!(file.vaults.contains_key(&vid));
        assert!(
            !file.removed.contains_key(&vid),
            "winning upsert prunes its tombstone"
        );
    }

    #[test]
    fn apply_event_preserves_foreign_fields_and_vaults() {
        let comped = "c3".repeat(16);
        let paying = "d4".repeat(16);
        // A file with a hand-written comp, an unknown per-entry field, and
        // an unknown top-level key.
        let raw = format!(
            r#"{{"version":1,"note":"hand-managed","vaults":{{"{comped}":{{"quota_bytes":42,"why":"friend"}}}}}}"#
        );
        let mut file: EntFile = serde_json::from_str(&raw).unwrap();

        assert!(apply_event(
            &mut file,
            &sub_event("customer.subscription.created", "active", &paying, 100),
            GIB,
            0
        ));

        let out = serde_json::to_string(&file).unwrap();
        assert!(
            out.contains(r#""note":"hand-managed""#),
            "top-level foreign key survives: {out}"
        );
        assert!(
            out.contains(r#""why":"friend""#),
            "per-entry foreign key survives: {out}"
        );
        assert_eq!(file.vaults[&comped].quota_bytes, 42, "comp untouched");
        assert_eq!(file.vaults[&paying].quota_bytes, GIB);
    }

    #[test]
    fn store_creates_missing_file_atomically_and_relay_shape_parses_it() {
        let path = temp_path("create");
        let store = EntStore::new(path.clone());
        let vid = "e5".repeat(16);

        let ev = sub_event("customer.subscription.created", "active", &vid, 100);
        assert!(store.apply(&ev, GIB, 50).unwrap());

        assert!(path.exists(), "parent dir + file created");
        assert!(
            !path.with_extension("json.tmp").exists(),
            "no tmp left behind"
        );

        // Parse with a local copy of the RELAY's strict shape (hub.rs) —
        // the contract the whole design hangs on.
        #[derive(Deserialize)]
        struct RelayFile {
            version: u32,
            #[serde(default)]
            vaults: std::collections::HashMap<String, RelayEnt>,
        }
        #[derive(Deserialize)]
        struct RelayEnt {
            quota_bytes: u64,
            #[serde(default)]
            #[allow(dead_code)]
            tier: String,
        }
        let raw = fs::read_to_string(&path).unwrap();
        let relay: RelayFile = serde_json::from_str(&raw).unwrap();
        assert_eq!(relay.version, 1);
        assert_eq!(relay.vaults[&vid].quota_bytes, GIB);
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn store_refuses_unknown_version_and_corrupt_file_untouched() {
        let path = temp_path("refuse");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let ev = sub_event(
            "customer.subscription.created",
            "active",
            &"f6".repeat(16),
            100,
        );

        for bad in [r#"{"version":2,"vaults":{}}"#, "definitely-not-json"] {
            fs::write(&path, bad).unwrap();
            let store = EntStore::new(path.clone());
            assert!(store.apply(&ev, GIB, 0).is_err());
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                bad,
                "file bytes untouched"
            );
        }
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
