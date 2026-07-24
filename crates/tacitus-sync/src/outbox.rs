//! Crash-safe outbox: sealed blobs waiting for a relay ack. Persisted as
//! JSONL so an interrupted push is retried after restart; `apply_update` is
//! idempotent on every replica, so at-least-once delivery needs no dedup.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::protocol::b64;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingBlob {
    #[serde(with = "b64")]
    blob: Vec<u8>,
}

pub struct Outbox {
    path: PathBuf,
    pending: Vec<PendingBlob>,
}

impl Outbox {
    pub fn load(sync_dir: &Path) -> io::Result<Self> {
        let path = sync_dir.join("outbox.jsonl");
        let pending = match fs::read_to_string(&path) {
            Ok(raw) => raw
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        Ok(Self { path, pending })
    }

    fn persist(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = String::new();
        for entry in &self.pending {
            out.push_str(&serde_json::to_string(entry).map_err(io::Error::other)?);
            out.push('\n');
        }
        let tmp = self.path.with_extension("jsonl.tmp");
        fs::write(&tmp, out)?;
        fs::rename(&tmp, &self.path)
    }

    pub fn push(&mut self, blob: Vec<u8>) -> io::Result<()> {
        self.pending.push(PendingBlob { blob });
        self.persist()
    }

    /// Acks arrive in push order on a connection — drop the oldest.
    pub fn ack_front(&mut self) -> io::Result<()> {
        if !self.pending.is_empty() {
            self.pending.remove(0);
            self.persist()?;
        }
        Ok(())
    }

    pub fn blobs(&self) -> Vec<Vec<u8>> {
        self.pending.iter().map(|p| p.blob.clone()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!("tacitus-outbox-{tag}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn outbox_survives_restart_and_acks_in_order() {
        let dir = temp_dir("restart");
        let mut outbox = Outbox::load(&dir).unwrap();
        outbox.push(vec![1, 1, 1]).unwrap();
        outbox.push(vec![2, 2, 2]).unwrap();
        drop(outbox);

        let mut reloaded = Outbox::load(&dir).unwrap();
        assert_eq!(reloaded.blobs(), vec![vec![1, 1, 1], vec![2, 2, 2]]);
        reloaded.ack_front().unwrap();
        assert_eq!(reloaded.blobs(), vec![vec![2, 2, 2]]);
        reloaded.ack_front().unwrap();
        assert!(reloaded.is_empty());
        fs::remove_dir_all(&dir).ok();
    }
}
