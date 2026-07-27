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

/// The biggest single snapshot blob the relay accepts — the whole snapshot
/// on the legacy `compact` path, one part on the chunked path (it also
/// keeps the 64 MiB WS frames comfortable: b64(32 MiB) ≈ 43 MiB). A single
/// sealed doc state beyond this still can't ship — see SYNC.md caveats.
pub const SNAPSHOT_MAX: usize = 32 * 1024 * 1024;

/// Total across all parts of a chunked snapshot: a snapshot can never
/// legitimately outgrow the log it replaces. Parts are staged and served
/// from disk, so this is a disk bound, not a RAM bound.
pub const SNAPSHOT_TOTAL_MAX: u64 = LOG_CAP_BYTES;

#[derive(Serialize, Deserialize)]
struct LogLine {
    seq: u64,
    ts: u64,
    blob: String, // base64
}

/// snapshot.json, both generations: v1 carries the blob inline (base64) —
/// the only shape 0.22 knows, kept for every single-part snapshot so a
/// relay rollback stays safe there; v2 records only the part count, with
/// raw part bytes in `snapshot.parts.<seq>/0..count-1`.
#[derive(Serialize, Deserialize)]
struct SnapFile {
    seq: u64,
    ts: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    blob: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parts: Option<u32>,
}

enum SnapshotStore {
    /// v1: the whole sealed blob, resident (≤ SNAPSHOT_MAX).
    Inline(Vec<u8>),
    /// v2: part files on disk, read per-part when serving — RAM stays
    /// bounded to one part no matter how big the vault is.
    Files { count: u32 },
}

struct SnapshotState {
    seq: u64,
    store: SnapshotStore,
}

pub struct VaultLog {
    dir: PathBuf,
    log_path: PathBuf,
    last_seq: u64,
    /// The compacted prefix, served before the tail to anyone below it.
    snapshot: Option<SnapshotState>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Disambiguates staging dirs created within one clock tick.
static STAGING_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Structured refusal for a part that busts a cap (None = fits). Pure so
/// the 512 MiB total bound is testable without fixtures.
fn part_cap_violation(total_so_far: u64, part_len: usize) -> Option<&'static str> {
    if part_len > SNAPSHOT_MAX {
        return Some("compact_part_too_large");
    }
    if total_so_far + part_len as u64 > SNAPSHOT_TOTAL_MAX {
        return Some("compact_too_large");
    }
    None
}

/// An in-progress chunked upload: parts land as files in a private staging
/// dir (fsynced on write, so the commit rename publishes durable bytes).
/// Owned by one connection; dropping it — disconnect, refusal, overwrite —
/// removes the dir.
#[derive(Debug)]
pub struct PartStaging {
    dir: PathBuf,
    upto_seq: u64,
    of: u32,
    next_idx: u32,
    total_bytes: u64,
}

impl PartStaging {
    pub fn upto_seq(&self) -> u64 {
        self.upto_seq
    }

    pub fn of(&self) -> u32 {
        self.of
    }

    pub fn next_idx(&self) -> u32 {
        self.next_idx
    }

    pub fn add(&mut self, blob: &[u8]) -> io::Result<()> {
        if self.next_idx >= self.of {
            return Err(io::Error::other("compact_part_order"));
        }
        if let Some(code) = part_cap_violation(self.total_bytes, blob.len()) {
            return Err(io::Error::other(code));
        }
        let path = self.dir.join(self.next_idx.to_string());
        let mut file = fs::File::create(&path)?;
        file.write_all(blob)?;
        file.sync_data()?;
        self.total_bytes += blob.len() as u64;
        self.next_idx += 1;
        Ok(())
    }
}

impl Drop for PartStaging {
    fn drop(&mut self) {
        // No-op after a successful commit (the dir was renamed away).
        let _ = fs::remove_dir_all(&self.dir);
    }
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
            Ok(raw) => serde_json::from_str::<SnapFile>(&raw).ok().and_then(|s| {
                match (s.blob, s.parts) {
                    (Some(b64), _) => B64.decode(&b64).ok().map(|blob| SnapshotState {
                        seq: s.seq,
                        store: SnapshotStore::Inline(blob),
                    }),
                    // v2 is only readable if every part file is present —
                    // the commit ordering guarantees it, so a gap means
                    // manual tampering: treat as no snapshot rather than
                    // serving a hole.
                    (None, Some(count)) => {
                        let parts = dir.join(format!("snapshot.parts.{}", s.seq));
                        (0..count)
                            .all(|i| parts.join(i.to_string()).is_file())
                            .then_some(SnapshotState {
                                seq: s.seq,
                                store: SnapshotStore::Files { count },
                            })
                    }
                    (None, None) => None,
                }
            }),
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
        let snapshot_seq = snapshot.as_ref().map(|s| s.seq).unwrap_or(0);
        let mut log = Self {
            dir,
            log_path,
            last_seq: log_last.max(snapshot_seq),
            snapshot,
        };

        // Crash leftovers: any staging dir is an interrupted upload, any
        // parts dir for a different seq is a superseded (or never
        // committed) generation. Best-effort sweep.
        log.sweep_part_dirs(snapshot_seq, true);

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

    /// The covered seq of the stored snapshot, if any.
    pub fn snapshot_seq(&self) -> Option<u64> {
        self.snapshot.as_ref().map(|s| s.seq)
    }

    /// How many parts the stored snapshot has (0 = no snapshot; 1 = the
    /// legacy single blob or a one-file chunked commit).
    pub fn snapshot_parts(&self) -> u32 {
        match &self.snapshot {
            None => 0,
            Some(SnapshotState {
                store: SnapshotStore::Inline(_),
                ..
            }) => 1,
            Some(SnapshotState {
                store: SnapshotStore::Files { count },
                ..
            }) => *count,
        }
    }

    /// One part's sealed bytes (idx < snapshot_parts()).
    pub fn snapshot_part(&self, idx: u32) -> io::Result<Vec<u8>> {
        match &self.snapshot {
            Some(SnapshotState {
                store: SnapshotStore::Inline(blob),
                ..
            }) if idx == 0 => Ok(blob.clone()),
            Some(SnapshotState {
                seq,
                store: SnapshotStore::Files { count },
            }) if idx < *count => fs::read(
                self.dir
                    .join(format!("snapshot.parts.{seq}"))
                    .join(idx.to_string()),
            ),
            _ => Err(io::Error::other("no such snapshot part")),
        }
    }

    fn check_compactable(&self, upto_seq: u64) -> io::Result<()> {
        let current = self.snapshot.as_ref().map(|s| s.seq).unwrap_or(0);
        if upto_seq <= current {
            return Err(io::Error::other("compact_stale"));
        }
        if upto_seq > self.last_seq {
            return Err(io::Error::other("compact_ahead"));
        }
        Ok(())
    }

    /// Atomically publish snapshot.json (tmp + fsync + rename) — THE commit
    /// point of every compaction — then drop obsolete part dirs and
    /// truncate the tail.
    fn publish_snapshot(&mut self, file: SnapFile, state: SnapshotState) -> io::Result<()> {
        let upto_seq = state.seq;
        let line = serde_json::to_string(&file).map_err(io::Error::other)?;
        let snap_tmp = self.dir.join("snapshot.json.tmp");
        {
            let mut file = fs::File::create(&snap_tmp)?;
            file.write_all(line.as_bytes())?;
            file.sync_data()?;
        }
        fs::rename(&snap_tmp, self.dir.join("snapshot.json"))?;
        self.snapshot = Some(state);
        self.sweep_part_dirs(upto_seq, false);
        self.rewrite_tail(upto_seq)
    }

    /// Accept a compaction snapshot covering the log up to `upto_seq`:
    /// persist it, then drop every entry at or below it (seqs never
    /// renumber). The old log is kept one generation as log.prev.jsonl.
    pub fn compact(&mut self, upto_seq: u64, blob: &[u8]) -> io::Result<()> {
        if blob.len() > SNAPSHOT_MAX {
            return Err(io::Error::other("snapshot_too_large"));
        }
        self.check_compactable(upto_seq)?;

        // Snapshot first, fsynced — from this instant a crash leaves at
        // worst a log with entries the snapshot already covers, and open()
        // finishes the truncation.
        self.publish_snapshot(
            SnapFile {
                seq: upto_seq,
                ts: now_secs(),
                blob: Some(B64.encode(blob)),
                parts: None,
            },
            SnapshotState {
                seq: upto_seq,
                store: SnapshotStore::Inline(blob.to_vec()),
            },
        )
    }

    /// Start staging a chunked upload. Fail-fast on offers that could never
    /// commit; the definitive stale check re-runs at commit (another
    /// connection may win the race meanwhile).
    pub fn stage_parts(&self, upto_seq: u64, of: u32) -> io::Result<PartStaging> {
        if of == 0 {
            return Err(io::Error::other("compact_part_order"));
        }
        self.check_compactable(upto_seq)?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = STAGING_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = self.dir.join(format!("snapshot.staging.{nanos}-{n}"));
        fs::create_dir_all(&dir)?;
        Ok(PartStaging {
            dir,
            upto_seq,
            of,
            next_idx: 0,
            total_bytes: 0,
        })
    }

    /// Commit a fully-staged chunked upload. One part delegates to the
    /// legacy v1 path (0.22-identical on disk); more become the v2 layout:
    /// staging dir renamed to `snapshot.parts.<seq>`, then snapshot.json.
    /// A crash between the two leaves an unreferenced parts dir that
    /// open() sweeps — never a torn snapshot.
    pub fn commit_parts(&mut self, staging: PartStaging) -> io::Result<()> {
        if staging.next_idx != staging.of {
            return Err(io::Error::other("compact_part_order"));
        }
        if staging.of == 1 {
            let blob = fs::read(staging.dir.join("0"))?;
            return self.compact(staging.upto_seq, &blob);
        }
        self.check_compactable(staging.upto_seq)?;

        let parts_dir = self
            .dir
            .join(format!("snapshot.parts.{}", staging.upto_seq));
        if parts_dir.exists() {
            fs::remove_dir_all(&parts_dir)?;
        }
        fs::rename(&staging.dir, &parts_dir)?;
        self.publish_snapshot(
            SnapFile {
                seq: staging.upto_seq,
                ts: now_secs(),
                blob: None,
                parts: Some(staging.of),
            },
            SnapshotState {
                seq: staging.upto_seq,
                store: SnapshotStore::Files { count: staging.of },
            },
        )
    }

    /// Remove `snapshot.parts.*` dirs for any seq but `keep_seq`, and —
    /// when `sweep_staging` (open-time) — every `snapshot.staging.*` dir
    /// (an interrupted upload; live ones are owned by connections, which
    /// don't survive a restart). Best-effort by design.
    fn sweep_part_dirs(&self, keep_seq: u64, sweep_staging: bool) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let stale = match name.strip_prefix("snapshot.parts.") {
                Some(rest) => rest.parse::<u64>().map(|s| s != keep_seq).unwrap_or(true),
                None => sweep_staging && name.starts_with("snapshot.staging."),
            };
            if stale {
                let _ = fs::remove_dir_all(entry.path());
            }
        }
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
        assert_eq!(log.snapshot_seq(), Some(3));
        assert_eq!(log.snapshot_parts(), 1);
        assert_eq!(log.snapshot_part(0).unwrap(), b"snapshot-blob");
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
        assert_eq!(log.snapshot_seq(), Some(3));
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
        assert_eq!(reopened.snapshot_seq(), Some(2));
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

    /// Stage-and-commit `parts` as one chunked upload covering `upto`.
    fn commit_chunked(log: &mut VaultLog, upto: u64, parts: &[&[u8]]) -> io::Result<()> {
        let mut staging = log.stage_parts(upto, parts.len() as u32)?;
        for part in parts {
            staging.add(part)?;
        }
        log.commit_parts(staging)
    }

    #[test]
    fn chunked_commit_stores_parts_serves_them_and_truncates_log() {
        let dir = temp_dir("chunked");
        let vid = "g".repeat(32);
        let mut log = VaultLog::open(&dir, &vid).unwrap();
        for i in 1..=5u8 {
            log.append(&[i]).unwrap();
        }

        commit_chunked(&mut log, 3, &[b"part-zero", b"part-one"]).unwrap();

        assert_eq!(log.snapshot_seq(), Some(3));
        assert_eq!(log.snapshot_parts(), 2);
        assert_eq!(log.snapshot_part(0).unwrap(), b"part-zero");
        assert_eq!(log.snapshot_part(1).unwrap(), b"part-one");
        assert!(log.snapshot_part(2).is_err());
        // Same truncation semantics as the legacy path.
        assert_eq!(log.read_since(0).unwrap(), vec![(4, vec![4]), (5, vec![5])]);
        assert_eq!(log.append(b"six").unwrap(), 6);
        // v2 on disk: metadata + a parts dir, no inline blob.
        let vdir = dir.join(&vid);
        let raw = fs::read_to_string(vdir.join("snapshot.json")).unwrap();
        assert!(raw.contains("\"parts\":2") && !raw.contains("\"blob\""));
        assert!(vdir.join("snapshot.parts.3").join("1").is_file());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn single_part_commit_writes_v1_format() {
        // One staged part must land exactly like a legacy compact — same
        // file shape, so a 0.22 relay rollback still reads it.
        let dir = temp_dir("chunked-v1");
        let vid = "h".repeat(32);
        let mut log = VaultLog::open(&dir, &vid).unwrap();
        for i in 1..=3u8 {
            log.append(&[i]).unwrap();
        }
        commit_chunked(&mut log, 2, &[b"solo"]).unwrap();

        assert_eq!(log.snapshot_parts(), 1);
        assert_eq!(log.snapshot_part(0).unwrap(), b"solo");
        let vdir = dir.join(&vid);
        let raw = fs::read_to_string(vdir.join("snapshot.json")).unwrap();
        assert!(raw.contains("\"blob\"") && !raw.contains("\"parts\""));
        assert!(!vdir.join("snapshot.parts.2").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn chunked_commit_rejects_stale_ahead_oversize_and_incomplete() {
        let dir = temp_dir("chunked-rej");
        let mut log = VaultLog::open(&dir, &"i".repeat(32)).unwrap();
        for i in 1..=4u8 {
            log.append(&[i]).unwrap();
        }
        commit_chunked(&mut log, 3, &[b"p0", b"p1"]).unwrap();

        // Stale/ahead refusals fail fast at staging time.
        assert!(log
            .stage_parts(3, 2)
            .unwrap_err()
            .to_string()
            .contains("compact_stale"));
        assert!(log
            .stage_parts(99, 2)
            .unwrap_err()
            .to_string()
            .contains("compact_ahead"));
        assert!(log
            .stage_parts(4, 0)
            .unwrap_err()
            .to_string()
            .contains("compact_part_order"));
        // Extra parts beyond the declared count are an ordering violation.
        let mut staging = log.stage_parts(4, 1).unwrap();
        staging.add(b"only").unwrap();
        assert!(staging
            .add(b"extra")
            .unwrap_err()
            .to_string()
            .contains("compact_part_order"));
        // Committing an incomplete set is one too.
        let staging = log.stage_parts(4, 2).unwrap();
        assert!(log
            .commit_parts(staging)
            .unwrap_err()
            .to_string()
            .contains("compact_part_order"));
        // The caps are pure checks — the 512 MiB total needs no fixture.
        assert_eq!(
            part_cap_violation(0, SNAPSHOT_MAX + 1),
            Some("compact_part_too_large")
        );
        assert_eq!(
            part_cap_violation(SNAPSHOT_TOTAL_MAX - 10, 11),
            Some("compact_too_large")
        );
        assert_eq!(part_cap_violation(SNAPSHOT_TOTAL_MAX - 10, 10), None);
        // The valid snapshot is untouched by every rejection.
        assert_eq!(log.snapshot_seq(), Some(3));
        assert_eq!(log.snapshot_parts(), 2);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reopen_after_chunked_compact_recovers_seq_and_parts() {
        let dir = temp_dir("chunked-reopen");
        let vid = "j".repeat(32);
        {
            let mut log = VaultLog::open(&dir, &vid).unwrap();
            log.append(b"one").unwrap();
            log.append(b"two").unwrap();
            commit_chunked(&mut log, 2, &[b"p0", b"p1"]).unwrap();
        }
        let mut reopened = VaultLog::open(&dir, &vid).unwrap();
        assert_eq!(reopened.last_seq(), 2);
        assert_eq!(reopened.snapshot_seq(), Some(2));
        assert_eq!(reopened.snapshot_parts(), 2);
        assert_eq!(reopened.snapshot_part(1).unwrap(), b"p1");
        assert_eq!(reopened.append(b"three").unwrap(), 3);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reopen_sweeps_stale_staging_and_orphan_part_dirs() {
        let dir = temp_dir("chunked-sweep");
        let vid = "k".repeat(32);
        {
            let mut log = VaultLog::open(&dir, &vid).unwrap();
            for i in 1..=3u8 {
                log.append(&[i]).unwrap();
            }
            commit_chunked(&mut log, 2, &[b"p0", b"p1"]).unwrap();
        }
        let vdir = dir.join(&vid);
        // Crash leftovers: an interrupted upload's staging dir, and a parts
        // dir some crashed commit never referenced.
        fs::create_dir_all(vdir.join("snapshot.staging.123-0")).unwrap();
        fs::write(vdir.join("snapshot.staging.123-0").join("0"), b"x").unwrap();
        fs::create_dir_all(vdir.join("snapshot.parts.1")).unwrap();

        let log = VaultLog::open(&dir, &vid).unwrap();
        assert!(!vdir.join("snapshot.staging.123-0").exists());
        assert!(!vdir.join("snapshot.parts.1").exists());
        // The committed generation survives the sweep.
        assert_eq!(log.snapshot_parts(), 2);
        assert_eq!(log.snapshot_part(0).unwrap(), b"p0");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dropped_staging_removes_its_dir_and_v2_with_missing_parts_is_no_snapshot() {
        let dir = temp_dir("chunked-drop");
        let vid = "l".repeat(32);
        let mut log = VaultLog::open(&dir, &vid).unwrap();
        for i in 1..=3u8 {
            log.append(&[i]).unwrap();
        }
        let vdir = dir.join(&vid);
        {
            let mut staging = log.stage_parts(2, 3).unwrap();
            staging.add(b"p0").unwrap();
            let staged: Vec<_> = fs::read_dir(&vdir)
                .unwrap()
                .flatten()
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with("snapshot.staging.")
                })
                .collect();
            assert_eq!(staged.len(), 1);
            // Dropped without commit — disconnect mid-upload.
        }
        let staged = fs::read_dir(&vdir).unwrap().flatten().any(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("snapshot.staging.")
        });
        assert!(!staged, "dropping the staging removes its dir");

        // A v2 snapshot.json whose parts dir is gone (manual tampering)
        // reads as NO snapshot — never a hole served as one.
        commit_chunked(&mut log, 2, &[b"p0", b"p1"]).unwrap();
        drop(log);
        fs::remove_dir_all(vdir.join("snapshot.parts.2")).unwrap();
        let log = VaultLog::open(&dir, &vid).unwrap();
        assert_eq!(log.snapshot_seq(), None);
        assert_eq!(log.snapshot_parts(), 0);
        fs::remove_dir_all(&dir).ok();
    }
}
