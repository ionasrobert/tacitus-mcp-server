//! Per-vault storage: an append-only JSONL log of encrypted blobs plus a
//! TOFU token file. The relay never parses a blob — seq assignment and
//! replay are its whole job.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};

/// Backstop even with compaction: a vault log stops growing at 512 MB.
pub const LOG_CAP_BYTES: u64 = 512 * 1024 * 1024;

/// A compaction snapshot the relay will accept (raw blob bytes). Bigger
/// vaults can't compact yet (chunked snapshots are future work) — the log
/// then simply keeps growing toward LOG_CAP_BYTES.
pub const SNAPSHOT_MAX: usize = 32 * 1024 * 1024;

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
    /// The compacted prefix: (covered seq, sealed full-state blob). Served
    /// before the tail to anyone whose cursor is below it.
    snapshot: Option<(u64, Vec<u8>)>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl VaultLog {
    /// Open (or create) a vault's storage; recovers `last_seq` from the log
    /// and the snapshot, and heals a compaction interrupted by a crash.
    pub fn open(data_dir: &std::path::Path, vault_id: &str) -> io::Result<Self> {
        let dir = data_dir.join(vault_id);
        fs::create_dir_all(&dir)?;
        let log_path = dir.join("log.jsonl");

        // Crash window between compact()'s two renames: the truncated tail
        // sits in log.jsonl.tmp and log.jsonl is gone — replay the rename.
        let tmp = dir.join("log.jsonl.tmp");
        if !log_path.exists() && tmp.exists() {
            fs::rename(&tmp, &log_path)?;
        }

        let snapshot = match fs::read_to_string(dir.join("snapshot.json")) {
            Ok(raw) => serde_json::from_str::<LogLine>(&raw)
                .ok()
                .and_then(|l| B64.decode(&l.blob).ok().map(|b| (l.seq, b))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };

        let log_last = match fs::read_to_string(&log_path) {
            Ok(raw) => raw
                .lines()
                .rev()
                .find_map(|line| serde_json::from_str::<LogLine>(line).ok())
                .map(|l| l.seq)
                .unwrap_or(0),
            Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e),
        };
        let snapshot_seq = snapshot.as_ref().map(|(seq, _)| *seq).unwrap_or(0);
        let mut log = Self {
            dir,
            log_path,
            last_seq: log_last.max(snapshot_seq),
            snapshot,
        };

        // Crash window between writing snapshot.json and truncating the
        // log: entries at or below the snapshot are still on disk — finish
        // the job (harmless if already clean: rewrite is a no-op filter).
        if snapshot_seq > 0 {
            if let Some(first) = log.read_since(0)?.first() {
                if first.0 <= snapshot_seq {
                    log.rewrite_tail(snapshot_seq)?;
                }
            }
        }
        Ok(log)
    }

    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// Current on-disk size of the tail log — the client's compaction
    /// trigger signal (0 when empty/missing).
    pub fn log_bytes(&self) -> u64 {
        fs::metadata(&self.log_path).map(|m| m.len()).unwrap_or(0)
    }

    /// The compacted prefix, if any: (covered seq, sealed blob).
    pub fn snapshot(&self) -> Option<(u64, &[u8])> {
        self.snapshot.as_ref().map(|(seq, blob)| (*seq, &blob[..]))
    }

    /// Accept a compaction snapshot covering the log up to `upto_seq`:
    /// persist it, then drop every entry at or below it (seqs never
    /// renumber). The old log is kept one generation as log.prev.jsonl.
    pub fn compact(&mut self, upto_seq: u64, blob: &[u8]) -> io::Result<()> {
        if blob.len() > SNAPSHOT_MAX {
            return Err(io::Error::other("snapshot_too_large"));
        }
        let current = self.snapshot.as_ref().map(|(seq, _)| *seq).unwrap_or(0);
        if upto_seq <= current {
            return Err(io::Error::other("compact_stale"));
        }
        if upto_seq > self.last_seq {
            return Err(io::Error::other("compact_ahead"));
        }

        // Snapshot first, fsynced — from this instant a crash leaves at
        // worst a log with entries the snapshot already covers, and open()
        // finishes the truncation.
        let line = serde_json::to_string(&LogLine {
            seq: upto_seq,
            ts: now_secs(),
            blob: B64.encode(blob),
        })
        .map_err(io::Error::other)?;
        let snap_tmp = self.dir.join("snapshot.json.tmp");
        {
            let mut file = fs::File::create(&snap_tmp)?;
            file.write_all(line.as_bytes())?;
            file.sync_data()?;
        }
        fs::rename(&snap_tmp, self.dir.join("snapshot.json"))?;
        self.snapshot = Some((upto_seq, blob.to_vec()));

        self.rewrite_tail(upto_seq)
    }

    /// Rewrite the log keeping only entries with seq > `upto_seq` (atomic:
    /// tmp + fsync + rename; previous log kept as log.prev.jsonl).
    fn rewrite_tail(&mut self, upto_seq: u64) -> io::Result<()> {
        let raw = match fs::read_to_string(&self.log_path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        let tmp = self.dir.join("log.jsonl.tmp");
        {
            let mut file = fs::File::create(&tmp)?;
            for line in raw.lines() {
                let keep = serde_json::from_str::<LogLine>(line)
                    .map(|l| l.seq > upto_seq)
                    .unwrap_or(false);
                if keep {
                    file.write_all(line.as_bytes())?;
                    file.write_all(b"\n")?;
                }
            }
            file.sync_data()?;
        }
        if self.log_path.exists() {
            fs::rename(&self.log_path, self.dir.join("log.prev.jsonl"))?;
        }
        fs::rename(&tmp, &self.log_path)
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

    #[test]
    fn compact_truncates_log_and_preserves_tail() {
        let dir = temp_dir("compact");
        let vid = "e".repeat(32);
        let mut log = VaultLog::open(&dir, &vid).unwrap();
        for i in 1..=5u8 {
            log.append(&[i]).unwrap();
        }

        log.compact(3, b"snapshot-blob").unwrap();

        // The tail keeps its original seqs; the prefix is gone.
        assert_eq!(log.read_since(0).unwrap(), vec![(4, vec![4]), (5, vec![5])]);
        let (seq, blob) = log.snapshot().expect("snapshot stored");
        assert_eq!(seq, 3);
        assert_eq!(blob, b"snapshot-blob");
        // Appends continue the same numbering.
        assert_eq!(log.append(b"six").unwrap(), 6);
        // One generation of the old log is kept for manual recovery.
        assert!(dir.join(&vid).join("log.prev.jsonl").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn compact_rejects_stale_future_and_oversize() {
        let dir = temp_dir("compact-rej");
        let mut log = VaultLog::open(&dir, &"f".repeat(32)).unwrap();
        for i in 1..=4u8 {
            log.append(&[i]).unwrap();
        }
        log.compact(3, b"snap").unwrap();

        // Not beyond the existing snapshot → stale.
        assert!(log.compact(3, b"again").is_err());
        assert!(log.compact(2, b"again").is_err());
        // Beyond the log's head → the pusher claims entries we never assigned.
        assert!(log.compact(99, b"future").is_err());
        // Oversize snapshots are refused before touching disk.
        let huge = vec![0u8; SNAPSHOT_MAX + 1];
        assert!(log.compact(4, &huge).is_err());
        // The valid state is untouched by the rejections.
        assert_eq!(log.snapshot().unwrap().0, 3);
        assert_eq!(log.read_since(0).unwrap(), vec![(4, vec![4])]);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reopen_after_compact_recovers_last_seq_from_snapshot() {
        let dir = temp_dir("compact-reopen");
        let vid = "1".repeat(32);
        {
            let mut log = VaultLog::open(&dir, &vid).unwrap();
            log.append(b"one").unwrap();
            log.append(b"two").unwrap();
            // Compact to the very head — the log file becomes empty.
            log.compact(2, b"snap").unwrap();
            assert!(log.read_since(0).unwrap().is_empty());
        }
        let mut reopened = VaultLog::open(&dir, &vid).unwrap();
        assert_eq!(reopened.last_seq(), 2, "last_seq comes from the snapshot");
        assert_eq!(reopened.snapshot().unwrap().0, 2);
        assert_eq!(reopened.append(b"three").unwrap(), 3);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reopen_heals_interrupted_compact() {
        // Crash window 1: snapshot written, log not yet truncated → open
        // re-truncates.
        let dir = temp_dir("heal-1");
        let vid = "2".repeat(32);
        {
            let mut log = VaultLog::open(&dir, &vid).unwrap();
            for i in 1..=3u8 {
                log.append(&[i]).unwrap();
            }
        }
        let vdir = dir.join(&vid);
        fs::write(
            vdir.join("snapshot.json"),
            format!(
                "{}",
                serde_json::json!({"seq": 2, "ts": 0, "blob": B64.encode(b"snap")})
            ),
        )
        .unwrap();
        let log = VaultLog::open(&dir, &vid).unwrap();
        assert_eq!(
            log.read_since(0).unwrap(),
            vec![(3, vec![3])],
            "entries at or below the snapshot are pruned on open"
        );
        assert_eq!(log.last_seq(), 3);
        fs::remove_dir_all(&dir).ok();

        // Crash window 2: between the two renames — log.jsonl is missing
        // but the truncated tail sits in log.jsonl.tmp.
        let dir = temp_dir("heal-2");
        let vid = "3".repeat(32);
        {
            let mut log = VaultLog::open(&dir, &vid).unwrap();
            for i in 1..=3u8 {
                log.append(&[i]).unwrap();
            }
            log.compact(2, b"snap").unwrap();
        }
        let vdir = dir.join(&vid);
        fs::rename(vdir.join("log.jsonl"), vdir.join("log.jsonl.tmp")).unwrap();
        let mut log = VaultLog::open(&dir, &vid).unwrap();
        assert_eq!(log.read_since(0).unwrap(), vec![(3, vec![3])]);
        assert_eq!(log.last_seq(), 3);
        assert_eq!(log.append(b"four").unwrap(), 4);
        fs::remove_dir_all(&dir).ok();
    }
}
