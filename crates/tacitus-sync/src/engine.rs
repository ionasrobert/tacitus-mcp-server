//! The sans-IO sync engine: a synchronous state machine the transport drives.
//! `tick_scan` folds local changes into CRDT docs and seals them for the
//! relay; `on_server_msg` decrypts and applies remote updates. No sockets in
//! here — tests drive it through an in-memory fake relay, and the real
//! WebSocket driver (feature "client") is a thin loop around these calls.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::crypto::{self, DocUpdate, Keys, SyncPayload, VaultCode};
use crate::docs::DocStore;
use crate::outbox::Outbox;
use crate::protocol::{ClientMsg, ServerMsg};
use crate::scan::scan;
use crate::state::ShadowState;
use crate::SyncError;

#[derive(Debug, Serialize, Deserialize, Default)]
struct Cursor {
    device_id: String,
    last_seq: u64,
}

#[derive(Debug, Clone)]
pub struct Flag {
    pub item: String,
    pub reason: String,
}

/// What a server message produced: messages to send back, items whose
/// materialized content changed (the apply layer rewrites those files),
/// and anything worth surfacing to a human.
#[derive(Debug, Default)]
pub struct EngineEffect {
    pub outbound: Vec<ClientMsg>,
    pub dirty_items: Vec<String>,
    pub flagged: Vec<Flag>,
}

pub struct SyncEngine {
    pub(crate) sync_dir: PathBuf,
    pub(crate) vault_dir: PathBuf,
    keys: Keys,
    device_id: String,
    pub(crate) shadow: ShadowState,
    pub(crate) docs: DocStore,
    outbox: Outbox,
    last_seq: u64,
}

fn random_device_id() -> String {
    use chacha20poly1305::aead::rand_core::RngCore;
    let mut bytes = [0u8; 8];
    chacha20poly1305::aead::OsRng.fill_bytes(&mut bytes);
    let mut id = String::from("dev_");
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(id, "{byte:02x}");
    }
    id
}

impl SyncEngine {
    pub fn open(vault_dir: &Path, code: &VaultCode) -> Result<Self, SyncError> {
        let sync_dir = vault_dir.join(".tacitus").join("sync");
        fs::create_dir_all(&sync_dir).map_err(SyncError::io)?;
        let keys = crypto::derive_keys(code);
        let shadow = ShadowState::load(&sync_dir).map_err(SyncError::io)?;
        let docs = DocStore::open(&sync_dir).map_err(SyncError::io)?;
        let outbox = Outbox::load(&sync_dir).map_err(SyncError::io)?;

        let cursor_path = sync_dir.join("cursor.json");
        let cursor: Cursor = match fs::read_to_string(&cursor_path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Cursor::default(),
            Err(e) => return Err(SyncError::io(e)),
        };
        let device_id = if cursor.device_id.is_empty() {
            random_device_id()
        } else {
            cursor.device_id.clone()
        };

        let engine = Self {
            sync_dir,
            vault_dir: vault_dir.to_path_buf(),
            keys,
            device_id,
            shadow,
            docs,
            outbox,
            last_seq: cursor.last_seq,
        };
        engine.persist_cursor()?;
        Ok(engine)
    }

    fn persist_cursor(&self) -> Result<(), SyncError> {
        let cursor = Cursor {
            device_id: self.device_id.clone(),
            last_seq: self.last_seq,
        };
        let json = serde_json::to_string_pretty(&cursor).map_err(|e| SyncError {
            code: "INTERNAL",
            reason: e.to_string(),
        })?;
        let tmp = self.sync_dir.join(".cursor.json.tmp");
        fs::write(&tmp, json).map_err(SyncError::io)?;
        fs::rename(&tmp, self.sync_dir.join("cursor.json")).map_err(SyncError::io)
    }

    pub fn vault_id(&self) -> &str {
        &self.keys.vault_id
    }

    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// The connection opener; `since_seq` resumes from the persisted cursor.
    pub fn hello(&self) -> ClientMsg {
        ClientMsg::Hello {
            vault_id: self.keys.vault_id.clone(),
            token: self.keys.auth_token.clone(),
            since_seq: self.last_seq,
        }
    }

    /// Everything still unacked — re-sent after (re)connecting, in order.
    pub fn pending_pushes(&self) -> Vec<ClientMsg> {
        self.outbox
            .blobs()
            .into_iter()
            .map(|blob| ClientMsg::Push { blob })
            .collect()
    }

    /// Scan the vault; fold local changes into the CRDT docs; seal one
    /// payload and queue it. Returns the new push (if anything changed).
    pub fn tick_scan(&mut self) -> Result<Vec<ClientMsg>, SyncError> {
        let delta = scan(&self.vault_dir, &mut self.shadow).map_err(SyncError::io)?;
        let mut updates: Vec<DocUpdate> = Vec::new();

        for item in delta.created.iter().chain(delta.modified.iter()) {
            let update = self
                .docs
                .apply_local_text(&item.key, &item.content)
                .map_err(SyncError::io)?;
            if !update.is_empty() {
                updates.push(DocUpdate {
                    doc: item.key.clone(),
                    u: update,
                });
            }
        }
        for key in &delta.deleted {
            let update = self.docs.record_delete(key).map_err(SyncError::io)?;
            if !update.is_empty() {
                updates.push(DocUpdate {
                    doc: crate::docs::MANIFEST_KEY.to_string(),
                    u: update,
                });
            }
        }
        self.shadow.save(&self.sync_dir).map_err(SyncError::io)?;

        if updates.is_empty() {
            return Ok(Vec::new());
        }
        let payload = SyncPayload {
            v: 1,
            device: self.device_id.clone(),
            updates,
        };
        let blob = crypto::seal(&self.keys.vault_key, &self.keys.vault_id, &payload)?;
        self.outbox.push(blob.clone()).map_err(SyncError::io)?;
        Ok(vec![ClientMsg::Push { blob }])
    }

    pub fn on_server_msg(&mut self, msg: ServerMsg) -> Result<EngineEffect, SyncError> {
        let mut effect = EngineEffect::default();
        match msg {
            ServerMsg::Welcome { .. } => {
                // Reconnected: everything unacked goes again (idempotent).
                effect.outbound = self.pending_pushes();
            }
            ServerMsg::Ack { seq: _ } => {
                // Ack only confirms persistence. The cursor advances solely
                // through Updates (the relay echoes our own pushes back, we
                // skip applying them via device_id) — advancing on Ack would
                // skip other devices' updates that raced ours into the log.
                self.outbox.ack_front().map_err(SyncError::io)?;
            }
            ServerMsg::Update { seq, blob } => {
                if seq <= self.last_seq {
                    return Ok(effect); // already seen (at-least-once delivery)
                }
                let payload = crypto::open(&self.keys.vault_key, &self.keys.vault_id, &blob)?;
                if payload.device != self.device_id {
                    for update in &payload.updates {
                        self.docs
                            .apply_remote(&update.doc, &update.u)
                            .map_err(SyncError::io)?;
                        effect.dirty_items.push(update.doc.clone());
                    }
                }
                self.last_seq = seq;
                self.persist_cursor()?;
            }
            ServerMsg::Err { code, msg } => {
                return Err(SyncError {
                    code: "RELAY",
                    reason: format!("{code}: {msg}"),
                });
            }
        }
        Ok(effect)
    }

    /// The merged text for an item (None = deleted / never existed).
    pub fn materialize(&mut self, item: &str) -> Result<Option<String>, SyncError> {
        self.docs.materialize(item).map_err(SyncError::io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_vault(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!("tacitus-engine-{tag}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The dumbest possible relay: an ordered log of blobs. Updates fan out
    /// to every subscriber, the pusher included (its own come back and are
    /// skipped by device id) — an ack never moves anyone's cursor.
    #[derive(Default)]
    struct FakeRelay {
        log: Vec<Vec<u8>>,
    }

    impl FakeRelay {
        /// Accept a push; return the ack the pusher would receive.
        fn push(&mut self, msg: &ClientMsg) -> ServerMsg {
            match msg {
                ClientMsg::Push { blob } => {
                    self.log.push(blob.clone());
                    ServerMsg::Ack {
                        seq: self.log.len() as u64,
                    }
                }
                other => panic!("relay only accepts pushes here, got {other:?}"),
            }
        }

        /// The backlog after `since_seq`, as the server would send it.
        fn updates_since(&self, since_seq: u64) -> Vec<ServerMsg> {
            self.log
                .iter()
                .enumerate()
                .skip(since_seq as usize)
                .map(|(i, blob)| ServerMsg::Update {
                    seq: (i + 1) as u64,
                    blob: blob.clone(),
                })
                .collect()
        }
    }

    fn drain(engine: &mut SyncEngine, relay: &FakeRelay) {
        for msg in relay.updates_since(engine.last_seq()) {
            engine.on_server_msg(msg).unwrap();
        }
    }

    fn push_all(engine: &mut SyncEngine, relay: &mut FakeRelay, msgs: Vec<ClientMsg>) {
        for msg in msgs {
            let ack = relay.push(&msg);
            engine.on_server_msg(ack).unwrap();
        }
    }

    #[test]
    fn engine_pushes_local_changes_after_scan() {
        let dir = temp_vault("push");
        fs::write(dir.join("note.md"), "# Note\n").unwrap();
        let code = VaultCode::generate();
        let mut engine = SyncEngine::open(&dir, &code).unwrap();

        let msgs = engine.tick_scan().unwrap();
        assert_eq!(msgs.len(), 1);
        let ClientMsg::Push { blob } = &msgs[0] else {
            panic!("expected a push");
        };
        // The blob decrypts with the vault key and names the note inside.
        let keys = crypto::derive_keys(&code);
        let payload = crypto::open(&keys.vault_key, &keys.vault_id, blob).unwrap();
        assert_eq!(payload.updates.len(), 1);
        assert_eq!(payload.updates[0].doc, "n:note");
        // Nothing changed → nothing pushed.
        assert!(engine.tick_scan().unwrap().is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn two_engines_converge_through_fake_relay() {
        let da = temp_vault("conv-a");
        let db = temp_vault("conv-b");
        fs::write(da.join("shared.md"), "from A\n").unwrap();
        let code = VaultCode::generate();
        let mut a = SyncEngine::open(&da, &code).unwrap();
        let mut b = SyncEngine::open(&db, &code).unwrap();
        let mut relay = FakeRelay::default();

        let pushes = a.tick_scan().unwrap();
        push_all(&mut a, &mut relay, pushes);
        drain(&mut b, &relay);

        assert_eq!(
            b.materialize("n:shared").unwrap().as_deref(),
            Some("from A\n")
        );
        fs::remove_dir_all(&da).ok();
        fs::remove_dir_all(&db).ok();
    }

    #[test]
    fn engine_applies_backlog_then_live() {
        let da = temp_vault("backlog-a");
        let db = temp_vault("backlog-b");
        let code = VaultCode::generate();
        let mut relay = FakeRelay::default();

        fs::write(da.join("one.md"), "first\n").unwrap();
        let mut a = SyncEngine::open(&da, &code).unwrap();
        let pushes = a.tick_scan().unwrap();
        push_all(&mut a, &mut relay, pushes);
        fs::write(da.join("two.md"), "second\n").unwrap();
        let pushes = a.tick_scan().unwrap();
        push_all(&mut a, &mut relay, pushes);

        // B connects later: backlog first…
        let mut b = SyncEngine::open(&db, &code).unwrap();
        drain(&mut b, &relay);
        assert_eq!(b.materialize("n:one").unwrap().as_deref(), Some("first\n"));
        assert_eq!(b.materialize("n:two").unwrap().as_deref(), Some("second\n"));

        // …then live.
        fs::write(da.join("three.md"), "third\n").unwrap();
        let pushes = a.tick_scan().unwrap();
        push_all(&mut a, &mut relay, pushes);
        drain(&mut b, &relay);
        assert_eq!(
            b.materialize("n:three").unwrap().as_deref(),
            Some("third\n")
        );
        fs::remove_dir_all(&da).ok();
        fs::remove_dir_all(&db).ok();
    }

    #[test]
    fn engine_resumes_from_persisted_cursor() {
        let da = temp_vault("cursor-a");
        let db = temp_vault("cursor-b");
        let code = VaultCode::generate();
        let mut relay = FakeRelay::default();

        fs::write(da.join("x.md"), "x\n").unwrap();
        let mut a = SyncEngine::open(&da, &code).unwrap();
        let pushes = a.tick_scan().unwrap();
        push_all(&mut a, &mut relay, pushes);

        let device_id = {
            let mut b = SyncEngine::open(&db, &code).unwrap();
            drain(&mut b, &relay);
            assert_eq!(b.last_seq(), 1);
            b.device_id().to_string()
        };

        let reopened = SyncEngine::open(&db, &code).unwrap();
        let ClientMsg::Hello { since_seq, .. } = reopened.hello() else {
            panic!("hello is hello");
        };
        assert_eq!(since_seq, 1, "cursor survives restart");
        assert_eq!(reopened.device_id(), device_id, "device id survives too");
        fs::remove_dir_all(&da).ok();
        fs::remove_dir_all(&db).ok();
    }

    #[test]
    fn offline_edits_merge_on_reconnect_without_loss() {
        let da = temp_vault("offline-a");
        let db = temp_vault("offline-b");
        let original = "# Notes\n\nshared baseline\n";
        fs::write(da.join("doc.md"), original).unwrap();
        fs::write(db.join("doc.md"), original).unwrap(); // identical copy → dedup bootstrap
        let code = VaultCode::generate();
        let mut a = SyncEngine::open(&da, &code).unwrap();
        let mut b = SyncEngine::open(&db, &code).unwrap();
        let mut relay = FakeRelay::default();

        // First sync while online: identical bootstraps dedup to one doc.
        let pa = a.tick_scan().unwrap();
        let pb = b.tick_scan().unwrap();
        push_all(&mut a, &mut relay, pa);
        push_all(&mut b, &mut relay, pb);
        drain(&mut a, &relay);
        drain(&mut b, &relay);
        assert_eq!(a.materialize("n:doc").unwrap().as_deref(), Some(original));

        // Both go "offline" and edit divergently.
        fs::write(da.join("doc.md"), "# Notes\n\nshared baseline\nA's line\n").unwrap();
        fs::write(db.join("doc.md"), "B's line\n# Notes\n\nshared baseline\n").unwrap();
        let pa = a.tick_scan().unwrap();
        let pb = b.tick_scan().unwrap();

        // Reconnect: both push, both drain.
        push_all(&mut a, &mut relay, pa);
        push_all(&mut b, &mut relay, pb);
        drain(&mut a, &relay);
        drain(&mut b, &relay);

        let ta = a.materialize("n:doc").unwrap().unwrap();
        let tb = b.materialize("n:doc").unwrap().unwrap();
        assert_eq!(ta, tb, "replicas converge");
        assert!(ta.contains("A's line"), "no lost edits");
        assert!(ta.contains("B's line"), "no lost edits");
        assert_eq!(
            ta.matches("shared baseline").count(),
            1,
            "baseline is not duplicated — the offline edits were splices on one shared doc"
        );
        fs::remove_dir_all(&da).ok();
        fs::remove_dir_all(&db).ok();
    }

    #[test]
    fn outbox_repushes_unacked_after_restart() {
        let dir = temp_vault("outbox");
        fs::write(dir.join("pending.md"), "not yet acked\n").unwrap();
        let code = VaultCode::generate();
        {
            let mut engine = SyncEngine::open(&dir, &code).unwrap();
            let msgs = engine.tick_scan().unwrap();
            assert_eq!(msgs.len(), 1);
            // Crash before any ack.
        }
        let mut engine = SyncEngine::open(&dir, &code).unwrap();
        let pending = engine.pending_pushes();
        assert_eq!(pending.len(), 1, "unacked push survives restart");

        // A Welcome after reconnect re-sends it; an ack clears it.
        let effect = engine
            .on_server_msg(ServerMsg::Welcome { latest_seq: 0 })
            .unwrap();
        assert_eq!(effect.outbound.len(), 1);
        engine.on_server_msg(ServerMsg::Ack { seq: 1 }).unwrap();
        assert!(engine.pending_pushes().is_empty());
        fs::remove_dir_all(&dir).ok();
    }
}
