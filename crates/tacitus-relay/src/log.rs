//! Per-vault storage: an append-only JSONL log of encrypted blobs plus a
//! TOFU token file. The relay never parses a blob — seq assignment and
//! replay are its whole job.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};

/// Beta guard until compaction lands: a vault log stops growing at 512 MB.
pub const LOG_CAP_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct LogLine {
    seq: u64,
    ts: u64,
    blob: String, // base64
}

pub struct VaultLog {
    dir: PathBuf,
    log_path: PathBuf,
    last_seq: u64,
}

impl VaultLog {
    /// Open (or create) a vault's storage; recovers `last_seq` from the log.
    pub fn open(data_dir: &std::path::Path, vault_id: &str) -> io::Result<Self> {
        let dir = data_dir.join(vault_id);
        fs::create_dir_all(&dir)?;
        let log_path = dir.join("log.jsonl");
        let last_seq = match fs::read_to_string(&log_path) {
            Ok(raw) => raw
                .lines()
                .rev()
                .find_map(|line| serde_json::from_str::<LogLine>(line).ok())
                .map(|l| l.seq)
                .unwrap_or(0),
            Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e),
        };
        Ok(Self {
            dir,
            log_path,
            last_seq,
        })
    }

    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// Append a blob; returns its assigned seq. Fsynced — an acked update
    /// survives a relay crash.
    pub fn append(&mut self, blob: &[u8]) -> io::Result<u64> {
        if let Ok(meta) = fs::metadata(&self.log_path) {
            if meta.len() >= LOG_CAP_BYTES {
                return Err(io::Error::other("log_full"));
            }
        }
        let seq = self.last_seq + 1;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let line = serde_json::to_string(&LogLine {
            seq,
            ts,
            blob: B64.encode(blob),
        })
        .map_err(io::Error::other)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)?;
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        self.last_seq = seq;
        Ok(seq)
    }

    /// Everything after `since_seq`, in order.
    pub fn read_since(&self, since_seq: u64) -> io::Result<Vec<(u64, Vec<u8>)>> {
        let raw = match fs::read_to_string(&self.log_path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        Ok(raw
            .lines()
            .filter_map(|line| serde_json::from_str::<LogLine>(line).ok())
            .filter(|l| l.seq > since_seq)
            .filter_map(|l| B64.decode(&l.blob).ok().map(|b| (l.seq, b)))
            .collect())
    }

    /// TOFU auth: the first Hello registers its token (owner-only file);
    /// later Hellos must present the same one. Constant-time compare —
    /// only the (fixed, public) length can leak through timing.
    pub fn check_or_register_token(&self, token: &str) -> io::Result<bool> {
        let token_path = self.dir.join("token");
        match fs::read_to_string(&token_path) {
            Ok(stored) => {
                let stored = stored.trim();
                if stored.len() != token.len() {
                    return Ok(false);
                }
                let mut diff = 0u8;
                for (a, b) in stored.bytes().zip(token.bytes()) {
                    diff |= a ^ b;
                }
                Ok(diff == 0)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::write(&token_path, token)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = fs::set_permissions(&token_path, fs::Permissions::from_mode(0o600));
                }
                Ok(true)
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!("tacitus-relay-{tag}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn push_assigns_monotonic_seqs_and_persists_jsonl() {
        let dir = temp_dir("seqs");
        let mut log = VaultLog::open(&dir, &"a".repeat(32)).unwrap();
        assert_eq!(log.append(b"one").unwrap(), 1);
        assert_eq!(log.append(b"two").unwrap(), 2);
        assert_eq!(log.append(b"three").unwrap(), 3);

        let raw = fs::read_to_string(dir.join("a".repeat(32)).join("log.jsonl")).unwrap();
        assert_eq!(raw.lines().count(), 3);
        assert!(raw.lines().next().unwrap().contains("\"seq\":1"));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn backlog_replays_from_since_seq() {
        let dir = temp_dir("backlog");
        let mut log = VaultLog::open(&dir, &"b".repeat(32)).unwrap();
        log.append(b"one").unwrap();
        log.append(b"two").unwrap();
        log.append(b"three").unwrap();

        let since_1 = log.read_since(1).unwrap();
        assert_eq!(since_1, vec![(2, b"two".to_vec()), (3, b"three".to_vec())]);
        assert!(log.read_since(3).unwrap().is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn restart_recovers_last_seq_from_log() {
        let dir = temp_dir("restart");
        {
            let mut log = VaultLog::open(&dir, &"c".repeat(32)).unwrap();
            log.append(b"one").unwrap();
            log.append(b"two").unwrap();
        }
        let mut reopened = VaultLog::open(&dir, &"c".repeat(32)).unwrap();
        assert_eq!(reopened.last_seq(), 2);
        assert_eq!(reopened.append(b"three").unwrap(), 3);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hello_with_new_vault_registers_token_tofu() {
        let dir = temp_dir("tofu");
        let log = VaultLog::open(&dir, &"d".repeat(32)).unwrap();
        assert!(log.check_or_register_token("secret-token-1").unwrap());
        assert!(log.check_or_register_token("secret-token-1").unwrap());
        assert!(!log.check_or_register_token("wrong-token").unwrap());
        fs::remove_dir_all(&dir).ok();
    }
}
