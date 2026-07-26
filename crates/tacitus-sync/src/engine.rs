//! The sans-IO sync engine: a synchronous state machine the transport drives.
//! `tick_scan` folds local changes into CRDT docs and seals them for the
//! relay; `on_server_msg` decrypts and applies remote updates. No sockets in
//! here — tests drive it through an in-memory fake relay, and the real
//! WebSocket driver (feature "client") is a thin loop around these calls.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use yrs::StateVector;

use crate::coedit::{coedit_aad, CoeditKind, CoeditPayload};
use crate::crypto::{self, DocUpdate, Keys, SyncPayload, VaultCode};
use crate::docs::DocStore;
use crate::outbox::Outbox;
use crate::presence::{presence_aad, PresencePayload, PresenceState};
use crate::protocol::{ClientMsg, ServerMsg, CAP_PRESENCE};
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
/// the raw applied updates (a live room forwards its note's bytes to the
/// frontend doc), and anything worth surfacing to a human.
#[derive(Debug, Default)]
pub struct EngineEffect {
    pub outbound: Vec<ClientMsg>,
    pub dirty_items: Vec<String>,
    pub updates: Vec<DocUpdate>,
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
    /// Items received from the relay but not yet materialized to the vault.
    /// Persisted (apply_queue.json) because the cursor advances on receipt:
    /// the relay never redelivers below it, so a crash between receipt and
    /// apply would otherwise lose the file write forever.
    pending_apply: BTreeSet<String>,
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

        let pending_apply: BTreeSet<String> =
            match fs::read_to_string(sync_dir.join("apply_queue.json")) {
                Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
                Err(e) if e.kind() == io::ErrorKind::NotFound => BTreeSet::new(),
                Err(e) => return Err(SyncError::io(e)),
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
            pending_apply,
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

    fn persist_pending_apply(&self) -> Result<(), SyncError> {
        let json = serde_json::to_string(&self.pending_apply).map_err(|e| SyncError {
            code: "INTERNAL",
            reason: e.to_string(),
        })?;
        let tmp = self.sync_dir.join(".apply_queue.json.tmp");
        fs::write(&tmp, json).map_err(SyncError::io)?;
        fs::rename(&tmp, self.sync_dir.join("apply_queue.json")).map_err(SyncError::io)
    }

    /// Items received but not yet materialized — the caller unions these into
    /// its next apply so a crash between receipt and apply loses nothing.
    pub fn pending_apply(&self) -> Vec<String> {
        self.pending_apply.iter().cloned().collect()
    }

    /// Applied (or deliberately skipped) keys leave the queue; the fold path
    /// owns recovery for skips.
    pub(crate) fn clear_pending_apply(&mut self, keys: &[String]) -> Result<(), SyncError> {
        let before = self.pending_apply.len();
        for key in keys {
            self.pending_apply.remove(key);
        }
        if self.pending_apply.len() != before {
            self.persist_pending_apply()?;
        }
        Ok(())
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
    /// Always advertises the extensions this client understands — the relay
    /// only sends extension frames to connections that asked for them.
    pub fn hello(&self) -> ClientMsg {
        ClientMsg::Hello {
            vault_id: self.keys.vault_id.clone(),
            token: self.keys.auth_token.clone(),
            since_seq: self.last_seq,
            caps: vec![CAP_PRESENCE.to_string()],
        }
    }

    /// Seal our presence for the wire (fills in device id + wall-clock ts).
    /// `hello` asks peers to announce themselves back; `gone` is the goodbye.
    pub fn seal_presence(
        &self,
        state: &PresenceState,
        hello: bool,
        gone: bool,
    ) -> Result<ClientMsg, SyncError> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let payload = PresencePayload {
            v: 1,
            device: self.device_id.clone(),
            ts,
            hello,
            gone,
            state: state.clone(),
        };
        let blob = crypto::seal(
            &self.keys.vault_key,
            &presence_aad(&self.keys.vault_id),
            &payload,
        )?;
        Ok(ClientMsg::Presence { blob })
    }

    /// Seal a co-edit frame (keystroke update or awareness blob) for the
    /// wire — rides the presence frame type under the `#coedit` AAD.
    pub fn seal_coedit(
        &self,
        note_id: &str,
        kind: CoeditKind,
        data: &[u8],
    ) -> Result<ClientMsg, SyncError> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let payload = CoeditPayload {
            v: 1,
            device: self.device_id.clone(),
            ts,
            note_id: note_id.to_string(),
            kind,
            data: data.to_vec(),
        };
        let blob = crypto::seal(
            &self.keys.vault_key,
            &coedit_aad(&self.keys.vault_id),
            &payload,
        )?;
        Ok(ClientMsg::Presence { blob })
    }

    /// Decode a co-edit frame. `None` = our own echo, a presence/sync blob
    /// (different AAD), or anything malformed — never fatal.
    pub fn open_coedit(&self, blob: &[u8]) -> Option<CoeditPayload> {
        let payload: CoeditPayload =
            crypto::open(&self.keys.vault_key, &coedit_aad(&self.keys.vault_id), blob).ok()?;
        (payload.device != self.device_id).then_some(payload)
    }

    /// Apply a peer's keystroke update to the note's doc and queue it for
    /// materialization — same receipt guarantees as a durable Update (the
    /// persisted apply queue survives a crash before the disk write).
    pub fn apply_coedit_update(&mut self, note_id: &str, update: &[u8]) -> Result<(), SyncError> {
        let key = format!("n:{note_id}");
        self.docs
            .apply_remote(&key, update)
            .map_err(SyncError::io)?;
        self.pending_apply.insert(key);
        self.persist_pending_apply()
    }

    /// Seal + queue arbitrary doc updates for the durable log — the co-edit
    /// checkpoint tier. Room updates enter docs via `apply_remote`, never
    /// through `tick_scan`, so without this the log would never carry them
    /// and fresh devices could not converge.
    pub fn push_updates(&mut self, updates: Vec<DocUpdate>) -> Result<Vec<ClientMsg>, SyncError> {
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

    /// Prepare a co-editing room for a note. Returns the doc's full state
    /// (the frontend bootstrap), the durable checkpoint, and — for a note
    /// whose file was never scanned — the bootstrap push that puts the
    /// baseline in the log. The caller must fold the disk first (the live
    /// loop's fold-before invariant); this only bootstraps MISSING docs.
    pub fn coedit_enter_state(
        &mut self,
        note_id: &str,
    ) -> Result<(Vec<u8>, StateVector, Vec<ClientMsg>), SyncError> {
        let key = format!("n:{note_id}");
        let mut bootstrap = Vec::new();
        if self
            .docs
            .full_state_of(&key)
            .map_err(SyncError::io)?
            .is_none()
        {
            let path = self.vault_dir.join(format!("{note_id}.md"));
            let content = match fs::read_to_string(&path) {
                Ok(raw) => raw,
                Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
                Err(e) => return Err(SyncError::io(e)),
            };
            let update = self
                .docs
                .apply_local_text(&key, &content)
                .map_err(SyncError::io)?;
            if !update.is_empty() {
                bootstrap = self.push_updates(vec![DocUpdate {
                    doc: key.clone(),
                    u: update,
                }])?;
            }
        }
        let state = self
            .docs
            .full_state_of(&key)
            .map_err(SyncError::io)?
            .unwrap_or_default();
        let checkpoint = self
            .docs
            .state_vector_of(&key)
            .map_err(SyncError::io)?
            .unwrap_or_default();
        Ok((state, checkpoint, bootstrap))
    }

    /// The note's advance since `since` (the durable co-edit batch) and the
    /// new checkpoint. Empty diff = nothing to push.
    pub fn coedit_diff(
        &mut self,
        note_id: &str,
        since: &StateVector,
    ) -> Result<(Vec<u8>, StateVector), SyncError> {
        let key = format!("n:{note_id}");
        let diff = self.docs.diff_since(&key, since).map_err(SyncError::io)?;
        let checkpoint = self
            .docs
            .state_vector_of(&key)
            .map_err(SyncError::io)?
            .unwrap_or_default();
        Ok((diff, checkpoint))
    }

    /// Decode a presence frame. `None` = our own echo, or anything that
    /// fails to authenticate or parse — presence is ephemeral, a bad frame
    /// is never worth killing a session over.
    pub fn open_presence(&self, blob: &[u8]) -> Option<PresencePayload> {
        let payload: PresencePayload = crypto::open(
            &self.keys.vault_key,
            &presence_aad(&self.keys.vault_id),
            blob,
        )
        .ok()?;
        (payload.device != self.device_id).then_some(payload)
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
        Ok(self.tick_scan_with_updates()?.0)
    }

    /// `tick_scan`, but also exposing the raw per-item updates that were
    /// folded — the live loop forwards the room note's fold to its frontend
    /// with exactly these bytes (no re-diff).
    pub fn tick_scan_with_updates(
        &mut self,
    ) -> Result<(Vec<ClientMsg>, Vec<DocUpdate>), SyncError> {
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
            return Ok((Vec::new(), Vec::new()));
        }
        let payload = SyncPayload {
            v: 1,
            device: self.device_id.clone(),
            updates: updates.clone(),
        };
        let blob = crypto::seal(&self.keys.vault_key, &self.keys.vault_id, &payload)?;
        self.outbox.push(blob.clone()).map_err(SyncError::io)?;
        Ok((vec![ClientMsg::Push { blob }], updates))
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
                let payload: SyncPayload =
                    crypto::open(&self.keys.vault_key, &self.keys.vault_id, &blob)?;
                if payload.device != self.device_id {
                    for update in &payload.updates {
                        self.docs
                            .apply_remote(&update.doc, &update.u)
                            .map_err(SyncError::io)?;
                        effect.dirty_items.push(update.doc.clone());
                    }
                    effect.updates = payload.updates;
                }
                // Queue before cursor: a crash in between redelivers the
                // Update (idempotent); the other order loses it forever.
                if !effect.dirty_items.is_empty() {
                    self.pending_apply
                        .extend(effect.dirty_items.iter().cloned());
                    self.persist_pending_apply()?;
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
            ServerMsg::Presence { .. } => {
                // Ephemeral — the live driver intercepts presence before
                // calling the engine; pass-based drivers simply ignore it.
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
        let payload: SyncPayload = crypto::open(&keys.vault_key, &keys.vault_id, blob).unwrap();
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
    fn hello_advertises_presence_capability() {
        let dir = temp_vault("caps");
        let engine = SyncEngine::open(&dir, &VaultCode::generate()).unwrap();
        let ClientMsg::Hello { caps, .. } = engine.hello() else {
            panic!("hello is hello");
        };
        assert!(caps.iter().any(|c| c == CAP_PRESENCE));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn presence_roundtrips_between_devices_and_filters_own_echo() {
        let da = temp_vault("pres-a");
        let db = temp_vault("pres-b");
        let code = VaultCode::generate();
        let a = SyncEngine::open(&da, &code).unwrap();
        let b = SyncEngine::open(&db, &code).unwrap();

        let state = PresenceState {
            note_id: Some("projects/launch".into()),
            editing: true,
        };
        let ClientMsg::Presence { blob } = a.seal_presence(&state, true, false).unwrap() else {
            panic!("expected a presence frame");
        };

        // The other device reads it…
        let payload = b.open_presence(&blob).expect("decodes on B");
        assert_eq!(payload.device, a.device_id());
        assert!(payload.hello && !payload.gone);
        assert!(payload.ts > 0);
        assert_eq!(payload.state, state);
        // …the sender's own echo is filtered…
        assert!(a.open_presence(&blob).is_none(), "own echo → None");
        // …tampering is rejected quietly…
        let mut bad = blob.clone();
        let last = bad.len() - 1;
        bad[last] ^= 0xff;
        assert!(b.open_presence(&bad).is_none());
        // …and a SYNC blob can't cross into the presence domain (AAD).
        fs::write(da.join("note.md"), "x\n").unwrap();
        let mut a = a;
        let pushes = a.tick_scan().unwrap();
        let ClientMsg::Push { blob: sync_blob } = &pushes[0] else {
            panic!("expected a push");
        };
        assert!(b.open_presence(sync_blob).is_none());
        fs::remove_dir_all(&da).ok();
        fs::remove_dir_all(&db).ok();
    }

    #[test]
    fn coedit_roundtrips_and_filters_own_echo() {
        let da = temp_vault("coed-a");
        let db = temp_vault("coed-b");
        let code = VaultCode::generate();
        let a = SyncEngine::open(&da, &code).unwrap();
        let b = SyncEngine::open(&db, &code).unwrap();

        let ClientMsg::Presence { blob } = a
            .seal_coedit("projects/launch", CoeditKind::Update, &[7, 8, 9])
            .unwrap()
        else {
            panic!("coedit rides the presence frame");
        };
        let payload = b.open_coedit(&blob).expect("decodes on B");
        assert_eq!(payload.device, a.device_id());
        assert_eq!(payload.note_id, "projects/launch");
        assert_eq!(payload.kind, CoeditKind::Update);
        assert_eq!(payload.data, vec![7, 8, 9]);
        assert!(a.open_coedit(&blob).is_none(), "own echo → None");
        let mut bad = blob.clone();
        let last = bad.len() - 1;
        bad[last] ^= 0xff;
        assert!(b.open_coedit(&bad).is_none());
        fs::remove_dir_all(&da).ok();
        fs::remove_dir_all(&db).ok();
    }

    #[test]
    fn presence_client_cannot_open_coedit_frames_and_vice_versa() {
        // The 0.20-compat contract: a coedit blob fails the presence AAD
        // (old clients swallow it silently), and no domain can read another.
        let dir = temp_vault("coed-aad");
        fs::write(dir.join("note.md"), "x\n").unwrap();
        let code = VaultCode::generate();
        let mut engine = SyncEngine::open(&dir, &code).unwrap();

        let ClientMsg::Presence { blob: coedit_blob } = engine
            .seal_coedit("note", CoeditKind::Update, &[1])
            .unwrap()
        else {
            panic!("frame");
        };
        let ClientMsg::Presence {
            blob: presence_blob,
        } = engine
            .seal_presence(&PresenceState::default(), false, false)
            .unwrap()
        else {
            panic!("frame");
        };
        let pushes = engine.tick_scan().unwrap();
        let ClientMsg::Push { blob: sync_blob } = &pushes[0] else {
            panic!("push");
        };

        // Cross-domain opens on a SECOND device (own-echo filter would mask
        // the AAD result on the sender).
        let other_dir = temp_vault("coed-aad-b");
        let other = SyncEngine::open(&other_dir, &code).unwrap();
        assert!(other.open_presence(&coedit_blob).is_none());
        assert!(other.open_coedit(&presence_blob).is_none());
        assert!(other.open_coedit(sync_blob).is_none());
        assert!(other.open_coedit(&coedit_blob).is_some(), "sanity");
        fs::remove_dir_all(&dir).ok();
        fs::remove_dir_all(&other_dir).ok();
    }

    #[test]
    fn apply_coedit_update_applies_and_queues_pending() {
        let da = temp_vault("coed-apply-a");
        let db = temp_vault("coed-apply-b");
        fs::write(da.join("note.md"), "typed live\n").unwrap();
        let code = VaultCode::generate();
        let mut a = SyncEngine::open(&da, &code).unwrap();
        let (_, updates) = a.tick_scan_with_updates().unwrap();
        assert_eq!(updates.len(), 1);

        {
            let mut b = SyncEngine::open(&db, &code).unwrap();
            b.apply_coedit_update("note", &updates[0].u).unwrap();
            assert_eq!(
                b.materialize("n:note").unwrap().as_deref(),
                Some("typed live\n")
            );
            assert_eq!(b.pending_apply(), vec!["n:note"]);
        }
        // The receipt survives a crash, exactly like a durable Update.
        let b = SyncEngine::open(&db, &code).unwrap();
        assert_eq!(b.pending_apply(), vec!["n:note"]);
        fs::remove_dir_all(&da).ok();
        fs::remove_dir_all(&db).ok();
    }

    #[test]
    fn push_updates_lands_durably_and_converges_a_replica() {
        let da = temp_vault("coed-push-a");
        let db = temp_vault("coed-push-b");
        fs::write(da.join("note.md"), "from the room\n").unwrap();
        let code = VaultCode::generate();
        let mut a = SyncEngine::open(&da, &code).unwrap();
        let (_, updates) = a.tick_scan_with_updates().unwrap();
        // Drain A's scan push so only push_updates' blob is in flight.
        let mut relay = FakeRelay::default();
        for msg in a.pending_pushes() {
            relay.push(&msg);
        }
        a.on_server_msg(ServerMsg::Ack { seq: 1 }).unwrap();

        let msgs = a.push_updates(updates).unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(a.pending_pushes().len(), 1, "queued in the outbox");
        let ack = relay.push(&msgs[0]);
        a.on_server_msg(ack).unwrap();
        assert!(a.pending_pushes().is_empty());

        // A fresh device converges from the log alone.
        let mut b = SyncEngine::open(&db, &code).unwrap();
        for msg in relay.updates_since(0) {
            b.on_server_msg(msg).unwrap();
        }
        assert_eq!(
            b.materialize("n:note").unwrap().as_deref(),
            Some("from the room\n")
        );
        // Empty input → no frame, no outbox growth.
        assert!(a.push_updates(Vec::new()).unwrap().is_empty());
        fs::remove_dir_all(&da).ok();
        fs::remove_dir_all(&db).ok();
    }

    #[test]
    fn coedit_enter_state_bootstraps_missing_docs_durably() {
        let dir = temp_vault("coed-enter");
        fs::write(dir.join("draft.md"), "never scanned before\n").unwrap();
        let code = VaultCode::generate();
        let mut engine = SyncEngine::open(&dir, &code).unwrap();

        // Never scanned → the enter bootstraps the doc AND pushes the
        // baseline durably (the log must be able to replay the room).
        let (state, checkpoint, bootstrap) = engine.coedit_enter_state("draft").unwrap();
        assert!(!state.is_empty());
        assert_eq!(bootstrap.len(), 1, "baseline pushed");
        let ddir = temp_vault("coed-enter-replica");
        let mut replica = SyncEngine::open(&ddir, &code).unwrap();
        let ClientMsg::Push { blob } = &bootstrap[0] else {
            panic!("push");
        };
        replica
            .on_server_msg(ServerMsg::Update {
                seq: 1,
                blob: blob.clone(),
            })
            .unwrap();
        assert_eq!(
            replica.materialize("n:draft").unwrap().as_deref(),
            Some("never scanned before\n")
        );

        // Re-enter: doc exists now → same state, NO new bootstrap push.
        let before = engine.pending_pushes().len();
        let (state2, _, bootstrap2) = engine.coedit_enter_state("draft").unwrap();
        assert_eq!(state2, state);
        assert!(bootstrap2.is_empty());
        assert_eq!(engine.pending_pushes().len(), before);

        // The checkpoint diff is empty until the doc advances.
        let (diff, _) = engine.coedit_diff("draft", &checkpoint).unwrap();
        assert!(diff.is_empty());
        engine
            .apply_coedit_update("draft", {
                // Advance via a peer edit built on the replica.
                replica
                    .docs
                    .apply_local_text("n:draft", "never scanned before\nplus a line\n")
                    .unwrap()
                    .as_slice()
            })
            .unwrap();
        let (diff, new_checkpoint) = engine.coedit_diff("draft", &checkpoint).unwrap();
        assert!(!diff.is_empty());
        let (diff_after, _) = engine.coedit_diff("draft", &new_checkpoint).unwrap();
        assert!(diff_after.is_empty(), "checkpoint advanced");
        fs::remove_dir_all(&dir).ok();
        fs::remove_dir_all(&ddir).ok();
    }

    #[test]
    fn presence_frames_are_noops_for_the_engine() {
        let dir = temp_vault("pres-noop");
        let mut engine = SyncEngine::open(&dir, &VaultCode::generate()).unwrap();
        let before = engine.last_seq();
        let effect = engine
            .on_server_msg(ServerMsg::Presence {
                blob: vec![1, 2, 3],
            })
            .unwrap();
        assert!(effect.outbound.is_empty());
        assert!(effect.dirty_items.is_empty());
        assert_eq!(engine.last_seq(), before, "no cursor movement");
        fs::remove_dir_all(&dir).ok();
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
            .on_server_msg(ServerMsg::Welcome {
                latest_seq: 0,
                caps: vec![],
            })
            .unwrap();
        assert_eq!(effect.outbound.len(), 1);
        engine.on_server_msg(ServerMsg::Ack { seq: 1 }).unwrap();
        assert!(engine.pending_pushes().is_empty());
        fs::remove_dir_all(&dir).ok();
    }
}
