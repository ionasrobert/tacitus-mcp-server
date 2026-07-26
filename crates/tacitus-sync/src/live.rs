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
use crate::protocol::{ClientMsg, ServerMsg};
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
        }
    }
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

/// Run a persistent live session: connect, stream, apply, reconnect with
/// backoff on network failure — forever. Returns `Ok(())` when every nudge
/// sender is dropped (clean shutdown; pending applies are flushed first).
/// Non-network errors (auth, log_full, local IO/write failures) return
/// `Err` — the host decides what to tell the user.
pub async fn run_live<F>(
    engine: &mut SyncEngine,
    writer: &mut NoteWriter,
    config: &LiveConfig,
    nudge: &mut mpsc::Receiver<()>,
    mut on_event: F,
) -> Result<(), SyncError>
where
    F: FnMut(LiveEvent),
{
    ensure_crypto_provider();
    let mut backoff: Option<Duration> = None;
    loop {
        match session(engine, writer, config, nudge, &mut backoff, &mut on_event).await? {
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
                    n = nudge.recv() => {
                        if n.is_none() {
                            // Shutting down while disconnected: anything
                            // received but unapplied is in the persisted
                            // queue — flush it now.
                            flush_pending(engine, writer, &mut on_event)?;
                            return Ok(());
                        }
                        // A local change while down → retry right away.
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
    nudge: &mut mpsc::Receiver<()>,
    backoff: &mut Option<Duration>,
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
                let msg: ServerMsg = match serde_json::from_str(&text) {
                    Ok(msg) => msg,
                    Err(e) => return Ok(SessionEnd::Network(format!("bad frame: {e}"))),
                };
                if let ServerMsg::Welcome { latest_seq } = &msg {
                    *backoff = None;
                    target = Some(*latest_seq);
                    on_event(LiveEvent::Connected {
                        latest_seq: *latest_seq,
                    });
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
            n = nudge.recv() => {
                match n {
                    None => {
                        flush_pending(engine, writer, on_event)?;
                        let _ = sink.send(Message::Close(None)).await;
                        return Ok(SessionEnd::Shutdown);
                    }
                    Some(()) => {
                        while nudge.try_recv().is_ok() {} // coalesce bursts
                        if let Some(reason) = fold_and_push(engine, &mut sink, on_event).await? {
                            return Ok(SessionEnd::Network(reason));
                        }
                        last_scan = Instant::now();
                    }
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
    }
}
