//! The persistent live-sync driver (collab-m1): one long-lived relay
//! connection per vault instead of pass-per-tick. Remote updates apply
//! within a debounce of receipt; local saves push on a nudge from the host
//! (the desktop app after autosave, the CLI on its scan tick).
//!
//! Correctness invariant (fold-before-apply): `DocStore::apply_local_text`
//! is snapshot-replace — folding a stale disk snapshot AFTER a remote update
//! has merged would splice the remote edit away. So the loop folds local
//! disk state into the CRDT *before* handling each incoming burst of
//! updates; with that ordering, concurrent on-disk edits truly merge.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;

use tacitus_core::vault::NoteWriter;
use yrs::StateVector;

use crate::apply::ApplyReport;
use crate::client::{connect_ws, ensure_crypto_provider};
use crate::coedit::CoeditKind;
use crate::crypto::DocUpdate;
use crate::engine::SyncEngine;
use crate::presence::{Peer, PeerTracker, PresenceState};
use crate::protocol::{parse_server_msg, ClientMsg, ServerMsg, CAP_COMPACT, CAP_PRESENCE};
use crate::SyncError;

/// The relay drops ephemeral frames whose base64 exceeds 8 KiB — anything
/// bigger (huge paste) skips the fast tier and flushes durably instead.
const MAX_COEDIT_FRAME_B64: usize = 8 * 1024;

/// Would this frame survive the relay's ephemeral size cap?
fn frame_fits(msg: &ClientMsg) -> bool {
    match msg {
        ClientMsg::Presence { blob } => blob.len().div_ceil(3) * 4 <= MAX_COEDIT_FRAME_B64,
        _ => true,
    }
}

/// Tunables for a live session. Every timing is injectable so tests never
/// need fixed sleeps.
#[derive(Debug, Clone)]
pub struct LiveConfig {
    pub relay_url: String,
    /// Safety-net local scan for writes nobody nudges about (plugins,
    /// agents, external editors) — there is no file watcher by design.
    pub scan_interval: Duration,
    /// Batch bursts of incoming updates into one apply/materialize.
    pub apply_debounce: Duration,
    /// Skip the fold-before-apply scan when the last scan is fresher than
    /// this (bounds backlog replay to a few scans per second).
    pub fold_guard: Duration,
    /// Client keepalive ping — detects half-open sockets after sleep faster
    /// than the relay's 30s ping alone.
    pub ping_interval: Duration,
    /// No frames at all for this long → the connection is dead, reconnect.
    pub liveness_timeout: Duration,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    /// Presence heartbeat cadence (also sweeps expired peers).
    pub presence_interval: Duration,
    /// A peer silent for this long is considered gone (crashed — no goodbye).
    pub presence_ttl: Duration,
    /// Trailing debounce for state changes (rapid note switching collapses
    /// to the last state; hello-replies ride the same debounce).
    pub presence_debounce: Duration,
    /// Trailing debounce for the durable co-edit checkpoint push — room
    /// keystrokes travel ephemerally, then land in the relay log in one
    /// batched diff after this much quiet.
    pub coedit_durable_debounce: Duration,
    /// When the Welcome reports a relay log bigger than this, a caught-up
    /// session offers a compaction snapshot (once per session). 0 disables.
    pub compact_threshold: u64,
}

impl LiveConfig {
    pub fn new(relay_url: impl Into<String>) -> Self {
        Self {
            relay_url: relay_url.into(),
            scan_interval: Duration::from_secs(20),
            apply_debounce: Duration::from_millis(250),
            fold_guard: Duration::from_millis(250),
            ping_interval: Duration::from_secs(25),
            liveness_timeout: Duration::from_secs(90),
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
            presence_interval: Duration::from_secs(15),
            presence_ttl: Duration::from_secs(45),
            presence_debounce: Duration::from_millis(300),
            coedit_durable_debounce: Duration::from_secs(2),
            compact_threshold: 4 * 1024 * 1024,
        }
    }
}

/// What the host sends into a live session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveCmd {
    /// A local write happened — scan and push now.
    Nudge,
    /// Our presence changed (note opened/closed, editing started/stopped).
    /// Stored even while presence is off (old relay) — it lights up on the
    /// first capable session.
    Presence(PresenceState),
    /// The host opened a note for co-editing: fold the disk, then hand the
    /// frontend the doc's full state (`LiveEvent::RoomState`).
    RoomEnter { note_id: String },
    /// The room note closed — flush the durable checkpoint and forget.
    RoomLeave,
    /// A local keystroke batch from the frontend's binding (yjs update v2).
    CoeditUpdate { note_id: String, update: Vec<u8> },
    /// Local cursor/selection state (yjs awareness blob) — ephemeral only.
    CoeditAwareness { note_id: String, data: Vec<u8> },
}

/// What the session surfaces to its host (status bar, CLI output).
#[derive(Debug)]
pub enum LiveEvent {
    Connected {
        latest_seq: u64,
    },
    /// Outbox drained and cursor caught up to the Welcome tip; the session
    /// stays connected and keeps streaming.
    CaughtUp,
    Pushed {
        count: usize,
    },
    Applied(ApplyReport),
    Disconnected {
        reason: String,
        retry_in: Duration,
    },
    /// The visible peer set changed (join, state change, goodbye, expiry) —
    /// also re-emitted unconditionally after every reconnect so hosts can
    /// repaint. Empty when presence is off (old relay).
    Peers(Vec<Peer>),
    /// The room's full doc state (update v2) — the frontend bootstrap.
    /// Emitted on RoomEnter and re-emitted after every reconnect (apply is
    /// idempotent).
    RoomState {
        note_id: String,
        state: Vec<u8>,
    },
    /// A peer's keystroke batch — or a fold of an external disk write —
    /// for the open room. Apply to the frontend doc with origin 'backend'.
    CoeditUpdate {
        note_id: String,
        update: Vec<u8>,
    },
    /// A peer's cursor/selection blob for the open room.
    CoeditAwareness {
        note_id: String,
        data: Vec<u8>,
    },
    /// The relay accepted our compaction snapshot and truncated its log.
    Compacted {
        upto_seq: u64,
    },
}

/// Pure backoff progression: min on the first failure, doubling to max.
fn next_backoff(prev: Option<Duration>, config: &LiveConfig) -> Duration {
    match prev {
        None => config.backoff_min,
        Some(d) => (d * 2).min(config.backoff_max),
    }
}

enum SessionEnd {
    /// Every nudge sender dropped — the host is shutting down.
    Shutdown,
    /// The connection died; the outer loop reconnects with backoff.
    Network(String),
}

fn internal(e: impl std::fmt::Display) -> SyncError {
    SyncError {
        code: "INTERNAL",
        reason: e.to_string(),
    }
}

/// Send client messages; `Ok(Some(reason))` means the socket died (a
/// NETWORK condition for the session, not an error for the caller).
async fn send_all<S>(sink: &mut S, msgs: &[ClientMsg]) -> Result<Option<String>, SyncError>
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    for msg in msgs {
        let json = serde_json::to_string(msg).map_err(internal)?;
        if let Err(e) = SinkExt::send(sink, Message::Text(json.into())).await {
            return Ok(Some(e.to_string()));
        }
    }
    Ok(None)
}

/// Fold local disk state into the CRDT and push whatever changed. When a
/// room is open, its note's folded bytes are forwarded to the frontend
/// (external writers — plugins, agents, CLI — reach the binding this way),
/// and the durable checkpoint advances past clean folds so the checkpoint
/// tier doesn't re-log what the scan push just logged.
async fn fold_and_push<S, F>(
    engine: &mut SyncEngine,
    sink: &mut S,
    room: Option<&mut RoomCtx>,
    on_event: &mut F,
) -> Result<Option<String>, SyncError>
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
    F: FnMut(LiveEvent),
{
    let (pushes, updates) = engine.tick_scan_with_updates()?;
    if let Some(room) = room {
        let key = room.doc_key();
        let mut folded_room = false;
        for update in &updates {
            if update.doc == key {
                folded_room = true;
                on_event(LiveEvent::CoeditUpdate {
                    note_id: room.note_id.clone(),
                    update: update.u.clone(),
                });
            }
        }
        if folded_room && !room.durable_dirty {
            // The scan push already carries this fold durably.
            let (_, checkpoint) = engine.coedit_diff(&room.note_id, &room.checkpoint)?;
            room.checkpoint = checkpoint;
        }
    }
    if pushes.is_empty() {
        return Ok(None);
    }
    if let Some(reason) = send_all(sink, &pushes).await? {
        return Ok(Some(reason));
    }
    on_event(LiveEvent::Pushed {
        count: pushes.len(),
    });
    Ok(None)
}

/// Materialize everything in the persisted apply queue (crash-safe source
/// of truth for "received but not yet on disk").
fn flush_pending<F>(
    engine: &mut SyncEngine,
    writer: &mut NoteWriter,
    on_event: &mut F,
) -> Result<(), SyncError>
where
    F: FnMut(LiveEvent),
{
    let dirty = engine.pending_apply();
    if !dirty.is_empty() {
        let report = engine.apply_dirty(writer, &dirty)?;
        on_event(LiveEvent::Applied(report));
    }
    Ok(())
}

/// Presence bookkeeping that outlives a single connection: our own state
/// (so it re-announces after a reconnect) and the peer map (so the badge
/// doesn't blank out during a blip).
struct PresenceCtx {
    state: PresenceState,
    peers: PeerTracker,
}

/// An open co-editing room: which note, and how far the durable log has
/// been advanced (checkpoint). Survives reconnects.
struct RoomCtx {
    note_id: String,
    checkpoint: StateVector,
    /// Local room edits not yet flushed to the durable tier.
    durable_dirty: bool,
}

impl RoomCtx {
    fn doc_key(&self) -> String {
        format!("n:{}", self.note_id)
    }
}

/// Everything that outlives one connection, bundled.
struct LiveState {
    presence: PresenceCtx,
    room: Option<RoomCtx>,
}

/// Push the room's advance-since-checkpoint into the durable log (outbox —
/// crash-safe even when the returned frames can't be sent right now).
fn flush_room_durable(
    engine: &mut SyncEngine,
    room: &mut RoomCtx,
) -> Result<Vec<ClientMsg>, SyncError> {
    let (diff, checkpoint) = engine.coedit_diff(&room.note_id, &room.checkpoint)?;
    room.checkpoint = checkpoint;
    room.durable_dirty = false;
    if diff.is_empty() {
        return Ok(Vec::new());
    }
    engine.push_updates(vec![DocUpdate {
        doc: room.doc_key(),
        u: diff,
    }])
}

/// When a debounced presence send should fire.
fn debounced_at(last_sent: Option<Instant>, debounce: Duration) -> Instant {
    match last_sent {
        None => Instant::now(),
        Some(t) => Instant::now().max(t + debounce),
    }
}

/// Run a persistent live session: connect, stream, apply, reconnect with
/// backoff on network failure — forever. Returns `Ok(())` when every cmd
/// sender is dropped (clean shutdown; a goodbye is sent and pending applies
/// are flushed first). Non-network errors (auth, log_full, local IO/write
/// failures) return `Err` — the host decides what to tell the user.
pub async fn run_live<F>(
    engine: &mut SyncEngine,
    writer: &mut NoteWriter,
    config: &LiveConfig,
    cmds: &mut mpsc::Receiver<LiveCmd>,
    mut on_event: F,
) -> Result<(), SyncError>
where
    F: FnMut(LiveEvent),
{
    ensure_crypto_provider();
    let mut backoff: Option<Duration> = None;
    let mut ctx = LiveState {
        presence: PresenceCtx {
            state: PresenceState::default(),
            peers: PeerTracker::new(config.presence_ttl),
        },
        room: None,
    };
    loop {
        match session(
            engine,
            writer,
            config,
            cmds,
            &mut backoff,
            &mut ctx,
            &mut on_event,
        )
        .await?
        {
            SessionEnd::Shutdown => return Ok(()),
            SessionEnd::Network(reason) => {
                let retry = next_backoff(backoff, config);
                backoff = Some(retry);
                on_event(LiveEvent::Disconnected {
                    reason,
                    retry_in: retry,
                });
                tokio::select! {
                    _ = tokio::time::sleep(retry) => {}
                    cmd = cmds.recv() => {
                        match cmd {
                            None => {
                                // Shutting down while disconnected: no
                                // socket for a goodbye (peers expire via
                                // TTL). Durable room edits + received
                                // applies are crash-safe — flush both.
                                if let Some(room) = &mut ctx.room {
                                    let _ = flush_room_durable(engine, room)?;
                                }
                                flush_pending(engine, writer, &mut on_event)?;
                                return Ok(());
                            }
                            Some(LiveCmd::Presence(state)) => {
                                // Remember the newest state for the next
                                // session's announce, then retry right away.
                                ctx.presence.state = state;
                            }
                            Some(LiveCmd::Nudge) => {
                                // A local change while down → retry now.
                            }
                            Some(LiveCmd::RoomEnter { note_id }) => {
                                // Offline room: fold the disk (pushes queue
                                // in the crash-safe outbox), hand the local
                                // doc state over, co-edit locally.
                                let _ = engine.tick_scan_with_updates()?;
                                let (state, checkpoint, _) =
                                    engine.coedit_enter_state(&note_id)?;
                                ctx.room = Some(RoomCtx {
                                    note_id: note_id.clone(),
                                    checkpoint,
                                    durable_dirty: false,
                                });
                                on_event(LiveEvent::RoomState { note_id, state });
                            }
                            Some(LiveCmd::RoomLeave) => {
                                if let Some(mut room) = ctx.room.take() {
                                    let _ = flush_room_durable(engine, &mut room)?;
                                }
                            }
                            Some(LiveCmd::CoeditUpdate { note_id, update }) => {
                                // Apply + flush durably right away (no
                                // debounce offline — everything queues in
                                // the outbox anyway) + materialize.
                                engine.apply_coedit_update(&note_id, &update)?;
                                if let Some(room) = &mut ctx.room {
                                    if room.note_id == note_id {
                                        let _ = flush_room_durable(engine, room)?;
                                    }
                                }
                                flush_pending(engine, writer, &mut on_event)?;
                            }
                            Some(LiveCmd::CoeditAwareness { .. }) => {
                                // Ephemeral — meaningless while offline.
                            }
                        }
                    }
                }
            }
        }
    }
}

async fn session<F>(
    engine: &mut SyncEngine,
    writer: &mut NoteWriter,
    config: &LiveConfig,
    cmds: &mut mpsc::Receiver<LiveCmd>,
    backoff: &mut Option<Duration>,
    ctx: &mut LiveState,
    on_event: &mut F,
) -> Result<SessionEnd, SyncError>
where
    F: FnMut(LiveEvent),
{
    let ws = match connect_ws(&config.relay_url).await {
        Ok(ws) => ws,
        Err(e) => return Ok(SessionEnd::Network(e.reason)),
    };
    let (mut sink, mut stream) = ws.split();

    // Hello first, then fold local changes so the outbox is complete before
    // the Welcome asks us to (re)send everything unacked — same order as
    // `run_once`.
    let hello = [engine.hello()];
    if let Some(reason) = send_all(&mut sink, &hello).await? {
        return Ok(SessionEnd::Network(reason));
    }
    if let Some(reason) = fold_and_push(engine, &mut sink, ctx.room.as_mut(), on_event).await? {
        return Ok(SessionEnd::Network(reason));
    }

    let mut target: Option<u64> = None;
    let mut caught_up = false;
    let mut last_frame = Instant::now();
    let mut last_scan = Instant::now();
    let mut next_scan = Instant::now() + config.scan_interval;
    let mut next_ping = Instant::now() + config.ping_interval;
    // Presence is off until the Welcome advertises the capability (an old
    // relay never does — zero presence bytes leave this client then).
    let mut presence_on = false;
    // Compaction: armed by the Welcome (cap + log size), fired at most once
    // per session, right after catching up.
    let mut compact_on = false;
    let mut welcome_log_bytes = 0u64;
    let mut compact_offered = false;
    let mut next_presence = Instant::now() + config.presence_interval;
    let mut presence_dirty = false;
    let mut presence_send_at = Instant::now();
    let mut last_presence_sent: Option<Instant> = None;
    // Anything left over from a crash (or an earlier failed session) gets
    // applied on the first debounce tick.
    let mut apply_deadline: Option<Instant> = if engine.pending_apply().is_empty() {
        None
    } else {
        Some(Instant::now() + config.apply_debounce)
    };
    // Durable co-edit flush: armed while the room has unflushed local edits.
    let mut durable_at = Instant::now() + config.coedit_durable_debounce;

    loop {
        let apply_at = apply_deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));
        let durable_due = ctx.room.as_ref().is_some_and(|r| r.durable_dirty);
        tokio::select! {
            frame = stream.next() => {
                last_frame = Instant::now();
                let frame = match frame {
                    None => return Ok(SessionEnd::Network("relay closed the connection".into())),
                    Some(Err(e)) => return Ok(SessionEnd::Network(e.to_string())),
                    Some(Ok(f)) => f,
                };
                let text = match frame {
                    Message::Text(text) => text,
                    Message::Ping(_) | Message::Pong(_) => continue, // auto-pong
                    Message::Close(_) => {
                        return Ok(SessionEnd::Network("relay closed the connection".into()))
                    }
                    _ => continue,
                };
                let msg: ServerMsg = match parse_server_msg(&text) {
                    Ok(Some(msg)) => msg,
                    Ok(None) => continue, // a future protocol frame — skip, don't die
                    Err(e) => return Ok(SessionEnd::Network(format!("bad frame: {e}"))),
                };
                // Ephemeral frames never touch on_server_msg: presence folds
                // into the peer map; co-edit updates apply to the doc and
                // stream to the open room's frontend.
                if let ServerMsg::Presence { blob } = &msg {
                    if let Some(payload) = engine.open_presence(blob) {
                        if payload.hello && !payload.gone && presence_on {
                            // A newcomer asked who's here — reply with our
                            // state through the normal debounce (N hellos in
                            // a window collapse to one reply; replies never
                            // set hello, so no loops).
                            presence_dirty = true;
                            presence_send_at =
                                debounced_at(last_presence_sent, config.presence_debounce);
                        }
                        if ctx.presence.peers.observe(&payload, std::time::Instant::now()) {
                            on_event(LiveEvent::Peers(ctx.presence.peers.snapshot()));
                        }
                    } else if let Some(coedit) = engine.open_coedit(blob) {
                        let in_room = ctx
                            .room
                            .as_ref()
                            .is_some_and(|r| r.note_id == coedit.note_id);
                        match coedit.kind {
                            CoeditKind::Update => {
                                // Keystrokes land in the doc + the apply
                                // queue regardless of a room — a headless
                                // live device materializes them too.
                                engine.apply_coedit_update(&coedit.note_id, &coedit.data)?;
                                apply_deadline =
                                    Some(Instant::now() + config.apply_debounce);
                                if in_room {
                                    on_event(LiveEvent::CoeditUpdate {
                                        note_id: coedit.note_id,
                                        update: coedit.data,
                                    });
                                }
                            }
                            CoeditKind::Awareness => {
                                if in_room {
                                    on_event(LiveEvent::CoeditAwareness {
                                        note_id: coedit.note_id,
                                        data: coedit.data,
                                    });
                                }
                            }
                        }
                    }
                    continue;
                }
                if let ServerMsg::Welcome {
                    latest_seq,
                    caps,
                    log_bytes,
                } = &msg
                {
                    *backoff = None;
                    target = Some(*latest_seq);
                    presence_on = caps.iter().any(|c| c == CAP_PRESENCE);
                    compact_on = caps.iter().any(|c| c == CAP_COMPACT);
                    welcome_log_bytes = *log_bytes;
                    on_event(LiveEvent::Connected {
                        latest_seq: *latest_seq,
                    });
                    if presence_on {
                        // Announce ourselves; hello=true asks peers to
                        // answer so a fresh device discovers everyone fast.
                        let frame = engine.seal_presence(&ctx.presence.state, true, false)?;
                        if let Some(reason) = send_all(&mut sink, &[frame]).await? {
                            return Ok(SessionEnd::Network(reason));
                        }
                        last_presence_sent = Some(Instant::now());
                        next_presence = Instant::now() + config.presence_interval;
                    }
                    // Unconditional snapshot: hosts repaint after reconnect
                    // or webview reload even when nothing changed.
                    on_event(LiveEvent::Peers(ctx.presence.peers.snapshot()));
                    // An open room resyncs its frontend the same way (the
                    // full-state apply is idempotent).
                    if let Some(room) = &ctx.room {
                        let (state, _, _) = engine.coedit_enter_state(&room.note_id)?;
                        on_event(LiveEvent::RoomState {
                            note_id: room.note_id.clone(),
                            state,
                        });
                    }
                }
                if let ServerMsg::Compacted { upto_seq } = &msg {
                    on_event(LiveEvent::Compacted { upto_seq: *upto_seq });
                }
                // Fold-before-apply: local disk state enters the CRDT before
                // this update does, so concurrent edits merge instead of the
                // stale snapshot erasing the remote edit later. A snapshot is
                // just a big update — same hazard, same fold.
                if matches!(msg, ServerMsg::Update { .. } | ServerMsg::Snapshot { .. })
                    && last_scan.elapsed() >= config.fold_guard
                {
                    if let Some(reason) =
                        fold_and_push(engine, &mut sink, ctx.room.as_mut(), on_event).await?
                    {
                        return Ok(SessionEnd::Network(reason));
                    }
                    last_scan = Instant::now();
                }
                let effect = engine.on_server_msg(msg)?;
                if !effect.dirty_items.is_empty() {
                    apply_deadline = Some(Instant::now() + config.apply_debounce);
                }
                // Durable updates for the room note (a peer's checkpoint
                // push, a pass-based device's scan) reach the frontend doc
                // too — idempotent for anything it already saw ephemerally.
                if let Some(room) = &ctx.room {
                    let key = room.doc_key();
                    for update in &effect.updates {
                        if update.doc == key {
                            on_event(LiveEvent::CoeditUpdate {
                                note_id: room.note_id.clone(),
                                update: update.u.clone(),
                            });
                        }
                    }
                }
                if let Some(reason) = send_all(&mut sink, &effect.outbound).await? {
                    return Ok(SessionEnd::Network(reason));
                }
                if !caught_up {
                    if let Some(t) = target {
                        if engine.pending_pushes().is_empty() && engine.last_seq() >= t {
                            caught_up = true;
                            on_event(LiveEvent::CaughtUp);
                            // Caught up = our docs cover the whole log — the
                            // moment a compaction snapshot is honest.
                            if compact_on
                                && !compact_offered
                                && config.compact_threshold > 0
                                && welcome_log_bytes > config.compact_threshold
                                && engine.last_seq() > 0
                            {
                                compact_offered = true;
                                let offer = [engine.snapshot_state()?];
                                if let Some(reason) = send_all(&mut sink, &offer).await? {
                                    return Ok(SessionEnd::Network(reason));
                                }
                            }
                        }
                    }
                }
            }
            cmd = cmds.recv() => {
                match cmd {
                    None => {
                        // Leaving: durable room flush first (queues in the
                        // crash-safe outbox even if the send fails), then
                        // the presence goodbye, then materialize. None of
                        // flush_pending touches the sink — ordering is safe.
                        if let Some(room) = &mut ctx.room {
                            let msgs = flush_room_durable(engine, room)?;
                            let _ = send_all(&mut sink, &msgs).await;
                        }
                        if presence_on {
                            if let Ok(frame) =
                                engine.seal_presence(&ctx.presence.state, false, true)
                            {
                                let _ = send_all(&mut sink, &[frame]).await;
                            }
                        }
                        flush_pending(engine, writer, on_event)?;
                        let _ = sink.send(Message::Close(None)).await;
                        return Ok(SessionEnd::Shutdown);
                    }
                    Some(first) => {
                        // Drain the burst, coalescing ONLY the order-free
                        // cmds (nudge, presence). Room/co-edit cmds are
                        // order-sensitive and processed as they came.
                        let mut nudged = false;
                        let mut new_state: Option<PresenceState> = None;
                        for cmd in std::iter::once(first)
                            .chain(std::iter::from_fn(|| cmds.try_recv().ok()))
                        {
                            match cmd {
                                LiveCmd::Nudge => nudged = true,
                                LiveCmd::Presence(state) => new_state = Some(state),
                                LiveCmd::RoomEnter { note_id } => {
                                    // Replacing a room flushes the old one.
                                    if let Some(room) = &mut ctx.room {
                                        let msgs = flush_room_durable(engine, room)?;
                                        if let Some(reason) =
                                            send_all(&mut sink, &msgs).await?
                                        {
                                            return Ok(SessionEnd::Network(reason));
                                        }
                                    }
                                    // Fold-before: the disk enters the CRDT
                                    // before the frontend adopts its state.
                                    if let Some(reason) =
                                        fold_and_push(engine, &mut sink, None, on_event)
                                            .await?
                                    {
                                        return Ok(SessionEnd::Network(reason));
                                    }
                                    last_scan = Instant::now();
                                    let (state, checkpoint, bootstrap) =
                                        engine.coedit_enter_state(&note_id)?;
                                    if let Some(reason) =
                                        send_all(&mut sink, &bootstrap).await?
                                    {
                                        return Ok(SessionEnd::Network(reason));
                                    }
                                    ctx.room = Some(RoomCtx {
                                        note_id: note_id.clone(),
                                        checkpoint,
                                        durable_dirty: false,
                                    });
                                    on_event(LiveEvent::RoomState { note_id, state });
                                }
                                LiveCmd::RoomLeave => {
                                    if let Some(mut room) = ctx.room.take() {
                                        let msgs = flush_room_durable(engine, &mut room)?;
                                        if let Some(reason) =
                                            send_all(&mut sink, &msgs).await?
                                        {
                                            return Ok(SessionEnd::Network(reason));
                                        }
                                    }
                                }
                                LiveCmd::CoeditUpdate { note_id, update } => {
                                    // Our own keystrokes take the same path
                                    // as a peer's: doc + apply queue (the
                                    // backend is the only disk writer).
                                    engine.apply_coedit_update(&note_id, &update)?;
                                    apply_deadline =
                                        Some(Instant::now() + config.apply_debounce);
                                    let mut oversize = false;
                                    if presence_on {
                                        let frame = engine.seal_coedit(
                                            &note_id,
                                            CoeditKind::Update,
                                            &update,
                                        )?;
                                        if frame_fits(&frame) {
                                            if let Some(reason) =
                                                send_all(&mut sink, &[frame]).await?
                                            {
                                                return Ok(SessionEnd::Network(reason));
                                            }
                                        } else {
                                            oversize = true;
                                        }
                                    }
                                    if let Some(room) = &mut ctx.room {
                                        if room.note_id == note_id {
                                            room.durable_dirty = true;
                                            durable_at = if oversize {
                                                // Too big for the fast tier:
                                                // peers get it durably now.
                                                Instant::now()
                                            } else {
                                                Instant::now()
                                                    + config.coedit_durable_debounce
                                            };
                                        }
                                    }
                                }
                                LiveCmd::CoeditAwareness { note_id, data } => {
                                    if presence_on {
                                        let frame = engine.seal_coedit(
                                            &note_id,
                                            CoeditKind::Awareness,
                                            &data,
                                        )?;
                                        if frame_fits(&frame) {
                                            if let Some(reason) =
                                                send_all(&mut sink, &[frame]).await?
                                            {
                                                return Ok(SessionEnd::Network(reason));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if let Some(state) = new_state {
                            if state != ctx.presence.state {
                                ctx.presence.state = state;
                                if presence_on {
                                    presence_dirty = true;
                                    presence_send_at = debounced_at(
                                        last_presence_sent,
                                        config.presence_debounce,
                                    );
                                }
                            }
                        }
                        if nudged {
                            if let Some(reason) =
                                fold_and_push(engine, &mut sink, ctx.room.as_mut(), on_event)
                                    .await?
                            {
                                return Ok(SessionEnd::Network(reason));
                            }
                            last_scan = Instant::now();
                        }
                    }
                }
            }
            _ = tokio::time::sleep_until(durable_at), if durable_due => {
                if let Some(room) = &mut ctx.room {
                    let msgs = flush_room_durable(engine, room)?;
                    if let Some(reason) = send_all(&mut sink, &msgs).await? {
                        return Ok(SessionEnd::Network(reason));
                    }
                    if !msgs.is_empty() {
                        on_event(LiveEvent::Pushed { count: msgs.len() });
                    }
                }
            }
            _ = tokio::time::sleep_until(presence_send_at), if presence_on && presence_dirty => {
                presence_dirty = false;
                let frame = engine.seal_presence(&ctx.presence.state, false, false)?;
                if let Some(reason) = send_all(&mut sink, &[frame]).await? {
                    return Ok(SessionEnd::Network(reason));
                }
                last_presence_sent = Some(Instant::now());
                // A state send doubles as a heartbeat.
                next_presence = Instant::now() + config.presence_interval;
            }
            _ = tokio::time::sleep_until(next_presence), if presence_on => {
                next_presence = Instant::now() + config.presence_interval;
                presence_dirty = false;
                let frame = engine.seal_presence(&ctx.presence.state, false, false)?;
                if let Some(reason) = send_all(&mut sink, &[frame]).await? {
                    return Ok(SessionEnd::Network(reason));
                }
                last_presence_sent = Some(Instant::now());
                if ctx.presence.peers.sweep(std::time::Instant::now()) {
                    on_event(LiveEvent::Peers(ctx.presence.peers.snapshot()));
                }
            }
            _ = tokio::time::sleep_until(apply_at), if apply_deadline.is_some() => {
                apply_deadline = None;
                flush_pending(engine, writer, on_event)?;
            }
            _ = tokio::time::sleep_until(next_scan) => {
                next_scan = Instant::now() + config.scan_interval;
                if let Some(reason) =
                    fold_and_push(engine, &mut sink, ctx.room.as_mut(), on_event).await?
                {
                    return Ok(SessionEnd::Network(reason));
                }
                last_scan = Instant::now();
            }
            _ = tokio::time::sleep_until(next_ping) => {
                next_ping = Instant::now() + config.ping_interval;
                if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                    return Ok(SessionEnd::Network("keepalive ping failed".into()));
                }
            }
            _ = tokio::time::sleep_until(last_frame + config.liveness_timeout) => {
                return Ok(SessionEnd::Network("relay went quiet".into()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_starts_at_min_doubles_and_clamps() {
        let config = LiveConfig::new("ws://x");
        assert_eq!(next_backoff(None, &config), Duration::from_secs(1));
        assert_eq!(
            next_backoff(Some(Duration::from_secs(1)), &config),
            Duration::from_secs(2)
        );
        assert_eq!(
            next_backoff(Some(Duration::from_secs(16)), &config),
            Duration::from_secs(30),
            "clamped at backoff_max"
        );
        assert_eq!(
            next_backoff(Some(Duration::from_secs(30)), &config),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn config_defaults_favor_liveness_over_chatter() {
        let config = LiveConfig::new("wss://sync.tacitus.md");
        assert_eq!(config.relay_url, "wss://sync.tacitus.md");
        assert_eq!(config.apply_debounce, Duration::from_millis(250));
        assert_eq!(config.fold_guard, Duration::from_millis(250));
        assert!(config.scan_interval >= Duration::from_secs(10));
        assert!(
            config.ping_interval < Duration::from_secs(30),
            "client pings faster than the relay's 30s keepalive"
        );
        assert!(config.liveness_timeout >= Duration::from_secs(60));
        assert!(config.backoff_min < config.backoff_max);
        assert!(
            config.presence_ttl >= config.presence_interval * 3,
            "a peer survives two lost heartbeats before expiring"
        );
        assert!(config.presence_debounce < Duration::from_secs(1));
        assert!(
            config.coedit_durable_debounce >= Duration::from_secs(1),
            "keystrokes batch into the log, they don't spam it"
        );
    }

    #[test]
    fn frame_fits_enforces_the_relay_b64_cap() {
        let small = ClientMsg::Presence {
            blob: vec![0; 4 * 1024],
        };
        assert!(frame_fits(&small));
        let big = ClientMsg::Presence {
            blob: vec![0; 7 * 1024],
        };
        assert!(!frame_fits(&big), "7KB raw > 8KiB as base64");
        assert!(frame_fits(&ClientMsg::Push {
            blob: vec![0; 100_000]
        }));
    }

    #[test]
    fn debounced_send_fires_now_when_idle_and_trails_when_busy() {
        let debounce = Duration::from_millis(300);
        let now = Instant::now();
        assert!(debounced_at(None, debounce) <= Instant::now());
        let recent = Some(now);
        assert!(debounced_at(recent, debounce) >= now + debounce);
        let long_ago = Some(now - Duration::from_secs(10));
        assert!(debounced_at(long_ago, debounce) <= Instant::now());
    }
}
