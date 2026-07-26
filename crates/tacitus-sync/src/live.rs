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

use crate::apply::ApplyReport;
use crate::client::{ensure_crypto_provider, ws_url};
use crate::engine::SyncEngine;
use crate::presence::{Peer, PeerTracker, PresenceState};
use crate::protocol::{parse_server_msg, ClientMsg, ServerMsg, CAP_PRESENCE};
use crate::SyncError;

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

/// Fold local disk state into the CRDT and push whatever changed.
async fn fold_and_push<S, F>(
    engine: &mut SyncEngine,
    sink: &mut S,
    on_event: &mut F,
) -> Result<Option<String>, SyncError>
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
    F: FnMut(LiveEvent),
{
    let pushes = engine.tick_scan()?;
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
    let mut presence = PresenceCtx {
        state: PresenceState::default(),
        peers: PeerTracker::new(config.presence_ttl),
    };
    loop {
        match session(
            engine,
            writer,
            config,
            cmds,
            &mut backoff,
            &mut presence,
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
                                // TTL); anything received but unapplied is
                                // in the persisted queue — flush it now.
                                flush_pending(engine, writer, &mut on_event)?;
                                return Ok(());
                            }
                            Some(LiveCmd::Presence(state)) => {
                                // Remember the newest state for the next
                                // session's announce, then retry right away.
                                presence.state = state;
                            }
                            Some(LiveCmd::Nudge) => {
                                // A local change while down → retry now.
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
    presence: &mut PresenceCtx,
    on_event: &mut F,
) -> Result<SessionEnd, SyncError>
where
    F: FnMut(LiveEvent),
{
    let ws = match tokio_tungstenite::connect_async(ws_url(&config.relay_url)).await {
        Ok((ws, _)) => ws,
        Err(e) => return Ok(SessionEnd::Network(e.to_string())),
    };
    let (mut sink, mut stream) = ws.split();

    // Hello first, then fold local changes so the outbox is complete before
    // the Welcome asks us to (re)send everything unacked — same order as
    // `run_once`.
    let hello = [engine.hello()];
    if let Some(reason) = send_all(&mut sink, &hello).await? {
        return Ok(SessionEnd::Network(reason));
    }
    if let Some(reason) = fold_and_push(engine, &mut sink, on_event).await? {
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

    loop {
        let apply_at = apply_deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(3600));
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
                // Presence is ephemeral and never touches the engine: decode,
                // fold into the peer map, surface changes, move on.
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
                        if presence.peers.observe(&payload, std::time::Instant::now()) {
                            on_event(LiveEvent::Peers(presence.peers.snapshot()));
                        }
                    }
                    continue;
                }
                if let ServerMsg::Welcome { latest_seq, caps } = &msg {
                    *backoff = None;
                    target = Some(*latest_seq);
                    presence_on = caps.iter().any(|c| c == CAP_PRESENCE);
                    on_event(LiveEvent::Connected {
                        latest_seq: *latest_seq,
                    });
                    if presence_on {
                        // Announce ourselves; hello=true asks peers to
                        // answer so a fresh device discovers everyone fast.
                        let frame = engine.seal_presence(&presence.state, true, false)?;
                        if let Some(reason) = send_all(&mut sink, &[frame]).await? {
                            return Ok(SessionEnd::Network(reason));
                        }
                        last_presence_sent = Some(Instant::now());
                        next_presence = Instant::now() + config.presence_interval;
                    }
                    // Unconditional snapshot: hosts repaint after reconnect
                    // or webview reload even when nothing changed.
                    on_event(LiveEvent::Peers(presence.peers.snapshot()));
                }
                // Fold-before-apply: local disk state enters the CRDT before
                // this update does, so concurrent edits merge instead of the
                // stale snapshot erasing the remote edit later.
                if matches!(msg, ServerMsg::Update { .. }) && last_scan.elapsed() >= config.fold_guard {
                    if let Some(reason) = fold_and_push(engine, &mut sink, on_event).await? {
                        return Ok(SessionEnd::Network(reason));
                    }
                    last_scan = Instant::now();
                }
                let effect = engine.on_server_msg(msg)?;
                if !effect.dirty_items.is_empty() {
                    apply_deadline = Some(Instant::now() + config.apply_debounce);
                }
                if let Some(reason) = send_all(&mut sink, &effect.outbound).await? {
                    return Ok(SessionEnd::Network(reason));
                }
                if !caught_up {
                    if let Some(t) = target {
                        if engine.pending_pushes().is_empty() && engine.last_seq() >= t {
                            caught_up = true;
                            on_event(LiveEvent::CaughtUp);
                        }
                    }
                }
            }
            cmd = cmds.recv() => {
                match cmd {
                    None => {
                        // Goodbye first (peers drop us immediately instead
                        // of waiting out the TTL); errors don't matter,
                        // we're leaving. flush_pending never touches the
                        // sink, so the ordering is safe.
                        if presence_on {
                            if let Ok(frame) = engine.seal_presence(&presence.state, false, true) {
                                let _ = send_all(&mut sink, &[frame]).await;
                            }
                        }
                        flush_pending(engine, writer, on_event)?;
                        let _ = sink.send(Message::Close(None)).await;
                        return Ok(SessionEnd::Shutdown);
                    }
                    Some(first) => {
                        // Coalesce the burst WITHOUT swallowing presence:
                        // fold every queued cmd into (nudged, newest state).
                        let mut nudged = false;
                        let mut new_state: Option<PresenceState> = None;
                        for cmd in std::iter::once(first)
                            .chain(std::iter::from_fn(|| cmds.try_recv().ok()))
                        {
                            match cmd {
                                LiveCmd::Nudge => nudged = true,
                                LiveCmd::Presence(state) => new_state = Some(state),
                            }
                        }
                        if let Some(state) = new_state {
                            if state != presence.state {
                                presence.state = state;
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
                                fold_and_push(engine, &mut sink, on_event).await?
                            {
                                return Ok(SessionEnd::Network(reason));
                            }
                            last_scan = Instant::now();
                        }
                    }
                }
            }
            _ = tokio::time::sleep_until(presence_send_at), if presence_on && presence_dirty => {
                presence_dirty = false;
                let frame = engine.seal_presence(&presence.state, false, false)?;
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
                let frame = engine.seal_presence(&presence.state, false, false)?;
                if let Some(reason) = send_all(&mut sink, &[frame]).await? {
                    return Ok(SessionEnd::Network(reason));
                }
                last_presence_sent = Some(Instant::now());
                if presence.peers.sweep(std::time::Instant::now()) {
                    on_event(LiveEvent::Peers(presence.peers.snapshot()));
                }
            }
            _ = tokio::time::sleep_until(apply_at), if apply_deadline.is_some() => {
                apply_deadline = None;
                flush_pending(engine, writer, on_event)?;
            }
            _ = tokio::time::sleep_until(next_scan) => {
                next_scan = Instant::now() + config.scan_interval;
                if let Some(reason) = fold_and_push(engine, &mut sink, on_event).await? {
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
