//! Per-vault fanout + storage handles. One `VaultHub` per vault_id, shared
//! across its live connections; the broadcast channel carries (seq, blob)
//! to EVERY subscriber — the pusher included, which is what lets clients
//! advance their cursor purely through the update stream.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{broadcast, Mutex};

use crate::log::VaultLog;

pub struct VaultHub {
    pub log: Mutex<VaultLog>,
    pub tx: broadcast::Sender<(u64, Vec<u8>)>,
}

pub struct RelayState {
    pub data_dir: PathBuf,
    vaults: Mutex<HashMap<String, Arc<VaultHub>>>,
}

impl RelayState {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            vaults: Mutex::new(HashMap::new()),
        }
    }

    pub async fn vault(&self, vault_id: &str) -> io::Result<Arc<VaultHub>> {
        let mut vaults = self.vaults.lock().await;
        if let Some(hub) = vaults.get(vault_id) {
            return Ok(hub.clone());
        }
        let log = VaultLog::open(&self.data_dir, vault_id)?;
        let (tx, _) = broadcast::channel(256);
        let hub = Arc::new(VaultHub {
            log: Mutex::new(log),
            tx,
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
