//! Per-vault fanout + storage handles. One `VaultHub` per vault_id, shared
//! across its live connections; the broadcast channel carries (seq, blob)
//! to EVERY subscriber — the pusher included, which is what lets clients
//! advance their cursor purely through the update stream.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;
use tokio::sync::{broadcast, Mutex};

use crate::log::VaultLog;

pub struct VaultHub {
    pub log: Mutex<VaultLog>,
    pub tx: broadcast::Sender<(u64, Vec<u8>)>,
    /// Ephemeral presence fanout: raw base64 blobs, never logged, never
    /// sequenced. Separate from `tx` so presence chatter can't evict update
    /// entries (their Lagged recoveries differ: updates resync from the
    /// log, presence just drops).
    pub presence: broadcast::Sender<String>,
}

/// Operator knobs. Defaults keep a bare relay byte-for-byte compatible with
/// pre-quota deployments: every vault gets the 512 MiB backstop unless an
/// entitlements file says otherwise.
pub struct RelayConfig {
    pub data_dir: PathBuf,
    /// Quota for vaults with no entitlement (`TACITUS_RELAY_QUOTA_DEFAULT`).
    pub default_quota_bytes: u64,
    /// Path to entitlements.json (`TACITUS_RELAY_ENTITLEMENTS`). Missing
    /// file = no entitlements; billing rewrites it and the relay picks the
    /// change up without a restart.
    pub entitlements_path: PathBuf,
}

impl RelayConfig {
    pub fn new(data_dir: PathBuf) -> Self {
        let entitlements_path = data_dir.join("entitlements.json");
        Self {
            data_dir,
            default_quota_bytes: crate::log::LOG_CAP_BYTES,
            entitlements_path,
        }
    }
}

/// `{"version":1,"vaults":{"<vault_id>":{"quota_bytes":N,"tier":"pro"}}}` —
/// `tier` is informational (billing bookkeeping); enforcement only reads
/// `quota_bytes`.
#[derive(Deserialize)]
struct EntitlementsFile {
    version: u32,
    #[serde(default)]
    vaults: HashMap<String, Entitlement>,
}

#[derive(Deserialize)]
struct Entitlement {
    quota_bytes: u64,
    #[serde(default)]
    #[allow(dead_code)]
    tier: String,
}

/// (mtime, len) of the file a map was parsed from. Not mtime alone: second
/// granularity would miss a same-second rewrite, and the pair makes reloads
/// observable in tests without sleeps.
type FileKey = Option<(std::time::SystemTime, u64)>;

#[derive(Default)]
struct EntitlementsCache {
    key: FileKey,
    quotas: HashMap<String, u64>,
    /// Last key we already logged a parse error for — one line per broken
    /// generation of the file, not one per push.
    err_key: FileKey,
}

pub struct RelayState {
    pub config: RelayConfig,
    vaults: Mutex<HashMap<String, Arc<VaultHub>>>,
    entitlements: std::sync::Mutex<EntitlementsCache>,
}

impl RelayState {
    pub fn new(data_dir: PathBuf) -> Self {
        Self::with_config(RelayConfig::new(data_dir))
    }

    pub fn with_config(config: RelayConfig) -> Self {
        Self {
            config,
            vaults: Mutex::new(HashMap::new()),
            entitlements: std::sync::Mutex::new(EntitlementsCache::default()),
        }
    }

    /// The effective quota for a vault, sampled fresh per operation so an
    /// entitlement upgrade takes effect without a reconnect. Synchronous and
    /// cheap (one stat; a reparse only when the file changed) — call it
    /// BEFORE taking the vault's log lock, never while holding it.
    pub fn quota_for(&self, vault_id: &str) -> u64 {
        let mut cache = self.entitlements.lock().unwrap();
        let key = std::fs::metadata(&self.config.entitlements_path)
            .ok()
            .and_then(|m| m.modified().ok().map(|t| (t, m.len())));
        if key != cache.key {
            match key {
                None => {
                    // Deliberately deleted (or never present): no entitlements.
                    cache.quotas = HashMap::new();
                    cache.key = None;
                }
                Some(_) => match std::fs::read_to_string(&self.config.entitlements_path)
                    .map_err(io::Error::other)
                    .and_then(|raw| {
                        serde_json::from_str::<EntitlementsFile>(&raw).map_err(io::Error::other)
                    })
                    .and_then(|file| match file.version {
                        1 => Ok(file.vaults),
                        v => Err(io::Error::other(format!("unknown version {v}"))),
                    }) {
                    Ok(vaults) => {
                        cache.quotas = vaults
                            .into_iter()
                            .map(|(id, e)| (id, e.quota_bytes))
                            .collect();
                        cache.key = key;
                    }
                    Err(e) => {
                        // Keep the last good map; don't store the key so a
                        // fixed file reloads on the next call.
                        if cache.err_key != key {
                            tracing::error!(
                                "entitlements file {} unreadable, keeping last good set: {e}",
                                self.config.entitlements_path.display()
                            );
                            cache.err_key = key;
                        }
                    }
                },
            }
        }
        cache
            .quotas
            .get(vault_id)
            .copied()
            .unwrap_or(self.config.default_quota_bytes)
    }

    pub async fn vault(&self, vault_id: &str) -> io::Result<Arc<VaultHub>> {
        let mut vaults = self.vaults.lock().await;
        if let Some(hub) = vaults.get(vault_id) {
            return Ok(hub.clone());
        }
        let log = VaultLog::open(&self.config.data_dir, vault_id)?;
        let (tx, _) = broadcast::channel(256);
        let (presence, _) = broadcast::channel(64);
        let hub = Arc::new(VaultHub {
            log: Mutex::new(log),
            tx,
            presence,
        });
        vaults.insert(vault_id.to_string(), hub.clone());
        Ok(hub)
    }
}

/// `vault_id` is 32 lowercase hex chars — anything else never touches disk.
pub fn valid_vault_id(vault_id: &str) -> bool {
    vault_id.len() == 32
        && vault_id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!("tacitus-relay-hub-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // Every consecutive file generation below has a different LENGTH, so
    // the (mtime, len) cache key observably changes even when two writes
    // land within one mtime tick — no sleeps needed.
    #[test]
    fn quota_for_defaults_reloads_and_keeps_last_good() {
        let dir = temp_dir("quota");
        let path = dir.join("ent.json");
        let mut config = RelayConfig::new(dir.clone());
        config.default_quota_bytes = 100;
        config.entitlements_path = path.clone();
        let state = RelayState::with_config(config);
        let vid = "a1".repeat(16);

        // Missing file → default for everyone.
        assert_eq!(state.quota_for(&vid), 100);

        std::fs::write(
            &path,
            format!(r#"{{"version":1,"vaults":{{"{vid}":{{"quota_bytes":500}}}}}}"#),
        )
        .unwrap();
        assert_eq!(state.quota_for(&vid), 500);
        assert_eq!(
            state.quota_for(&"b2".repeat(16)),
            100,
            "unentitled vaults keep the default"
        );

        // Garbage keeps the last good set.
        std::fs::write(&path, "definitely-not-json").unwrap();
        assert_eq!(state.quota_for(&vid), 500);

        // A fixed file reloads (tier field parses and is ignored).
        std::fs::write(
            &path,
            format!(r#"{{"version":1,"vaults":{{"{vid}":{{"quota_bytes":900,"tier":"pro"}}}}}}"#),
        )
        .unwrap();
        assert_eq!(state.quota_for(&vid), 900);

        // An unknown version is a parse error → last good set again.
        std::fs::write(&path, r#"{"version":2,"vaults":{}}"#).unwrap();
        assert_eq!(state.quota_for(&vid), 900);

        // Deleting the file deliberately drops every entitlement.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(state.quota_for(&vid), 100);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_malformed_vault_id() {
        assert!(valid_vault_id(&"a1".repeat(16)));
        assert!(!valid_vault_id("../../etc/passwd"));
        assert!(!valid_vault_id(&"A1".repeat(16))); // uppercase
        assert!(!valid_vault_id(&"a1".repeat(15))); // short
        assert!(!valid_vault_id(&"zz".repeat(16))); // non-hex
        assert!(!valid_vault_id(""));
    }
}
