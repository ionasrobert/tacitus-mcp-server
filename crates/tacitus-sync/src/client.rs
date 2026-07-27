//! The async WebSocket driver around the sans-IO engine — the same loop
//! serves the CLI (`tacitus-mcp sync once|run`) and the desktop app.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

use tacitus_core::vault::NoteWriter;

use crate::apply::ApplyReport;
use crate::engine::{SyncEngine, SNAPSHOT_PART_BUDGET};
use crate::protocol::{parse_server_msg, ClientMsg, ServerMsg, CAP_COMPACT, CAP_COMPACT2};
use crate::SyncError;

/// How long we wait for the relay to say something before deciding the
/// connection went quiet mid-sync.
const QUIET_TIMEOUT: Duration = Duration::from_secs(15);

/// The post-offer wait during `run_compact`: committing a chunked snapshot
/// fsyncs the parts and rewrites a possibly ~512 MiB log inside the relay's
/// select loop, so no frames (not even pings) flow meanwhile.
const COMPACT_QUIET_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Default)]
pub struct RunReport {
    pub pushed: usize,
    pub applied: usize,
    pub dirty_items: Vec<String>,
}

fn net_err(e: impl std::fmt::Display) -> SyncError {
    SyncError {
        code: "NETWORK",
        reason: e.to_string(),
    }
}

pub(crate) fn ws_url(relay_url: &str) -> String {
    let base = relay_url.trim_end_matches('/');
    if base.ends_with("/ws") {
        base.to_string()
    } else {
        format!("{base}/ws")
    }
}

/// rustls 0.23 needs a process-level crypto provider before the first TLS
/// handshake (wss://). Tests use plain ws://, so only real relays hit this.
pub(crate) fn ensure_crypto_provider() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Connect with WS limits matching the relay's: compaction snapshots can
/// exceed tungstenite's 16 MiB default frame cap (outgoing messages are
/// never fragmented), so both directions allow 64 MiB.
pub(crate) async fn connect_ws(
    relay_url: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    SyncError,
> {
    ensure_crypto_provider();
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(64 * 1024 * 1024))
        .max_frame_size(Some(64 * 1024 * 1024));
    let (ws, _) =
        tokio_tungstenite::connect_async_with_config(ws_url(relay_url), Some(config), false)
            .await
            .map_err(net_err)?;
    Ok(ws)
}

/// One full sync pass: connect, push local changes, drain the backlog,
/// return once the outbox is empty and the cursor caught up to the log.
pub async fn run_once(engine: &mut SyncEngine, relay_url: &str) -> Result<RunReport, SyncError> {
    let ws = connect_ws(relay_url).await?;
    let (mut sink, mut stream) = ws.split();

    let mut report = RunReport::default();
    let send = |msg: &ClientMsg| serde_json::to_string(msg).map_err(net_err);

    // Hello first; fold local changes so the outbox is complete before the
    // Welcome asks us to (re)send everything unacked.
    let hello = send(&engine.hello())?;
    let scanned = engine.tick_scan()?;
    report.pushed = scanned.len();
    sink.send(Message::Text(hello.into()))
        .await
        .map_err(net_err)?;

    let mut target: Option<u64> = None;
    loop {
        if let Some(target) = target {
            if engine.pending_pushes().is_empty() && engine.last_seq() >= target {
                break;
            }
        }
        let frame = tokio::time::timeout(QUIET_TIMEOUT, stream.next())
            .await
            .map_err(|_| SyncError {
                code: "NETWORK",
                reason: "relay went quiet mid-sync".into(),
            })?
            .ok_or_else(|| SyncError {
                code: "NETWORK",
                reason: "relay closed the connection mid-sync".into(),
            })?
            .map_err(net_err)?;

        let text = match frame {
            Message::Text(text) => text,
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => {
                return Err(SyncError {
                    code: "NETWORK",
                    reason: "relay closed the connection mid-sync".into(),
                })
            }
            _ => continue,
        };
        let msg: ServerMsg = match parse_server_msg(&text) {
            Ok(Some(msg)) => msg,
            Ok(None) => continue, // a future protocol frame — skip it
            Err(e) => return Err(net_err(e)),
        };
        if let ServerMsg::Welcome { latest_seq, .. } = &msg {
            // Our own pushes land after latest_seq; their echoes advance the
            // cursor past it, so catching up to the pre-push tip suffices.
            target = Some(*latest_seq);
        }
        let effect = engine.on_server_msg(msg)?;
        report.applied += effect.dirty_items.len();
        report.dirty_items.extend(effect.dirty_items);
        for out in &effect.outbound {
            sink.send(Message::Text(send(out)?.into()))
                .await
                .map_err(net_err)?;
        }
    }

    let _ = sink.send(Message::Close(None)).await;
    Ok(report)
}

/// Force one compaction pass: connect, catch up (so the snapshot honestly
/// covers the whole log), offer the snapshot, await the relay's confirm.
/// Returns the covered seq — or 0 when the log is already empty.
pub async fn run_compact(engine: &mut SyncEngine, relay_url: &str) -> Result<u64, SyncError> {
    let ws = connect_ws(relay_url).await?;
    let (mut sink, mut stream) = ws.split();
    let encode = |msg: &ClientMsg| serde_json::to_string(msg).map_err(net_err);

    let hello = encode(&engine.hello())?;
    engine.tick_scan()?; // fold local changes so the snapshot is complete
    sink.send(Message::Text(hello.into()))
        .await
        .map_err(net_err)?;

    let mut target: Option<u64> = None;
    let mut relay_compacts = false;
    let mut relay_compacts2 = false;
    let mut offered = false;
    loop {
        if let Some(t) = target {
            if !offered && engine.pending_pushes().is_empty() && engine.last_seq() >= t {
                if engine.last_seq() == 0 {
                    let _ = sink.send(Message::Close(None)).await;
                    return Ok(0); // empty log — nothing to compact
                }
                if !relay_compacts {
                    return Err(SyncError {
                        code: "UNSUPPORTED",
                        reason: "relay does not advertise compaction — upgrade the relay".into(),
                    });
                }
                let offer = engine.snapshot_state_parts(SNAPSHOT_PART_BUDGET)?;
                if !relay_compacts2
                    && offer
                        .iter()
                        .any(|m| matches!(m, ClientMsg::CompactPart { .. }))
                {
                    return Err(SyncError {
                        code: "UNSUPPORTED",
                        reason: "vault snapshot needs chunked compaction — upgrade the relay"
                            .into(),
                    });
                }
                offered = true;
                for msg in &offer {
                    sink.send(Message::Text(encode(msg)?.into()))
                        .await
                        .map_err(net_err)?;
                }
            }
        }
        let quiet = if offered {
            COMPACT_QUIET_TIMEOUT
        } else {
            QUIET_TIMEOUT
        };
        let frame = tokio::time::timeout(quiet, stream.next())
            .await
            .map_err(|_| SyncError {
                code: "NETWORK",
                reason: "relay went quiet mid-compact".into(),
            })?
            .ok_or_else(|| SyncError {
                code: "NETWORK",
                reason: "relay closed the connection mid-compact".into(),
            })?
            .map_err(net_err)?;
        let text = match frame {
            Message::Text(text) => text,
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => {
                return Err(SyncError {
                    code: "NETWORK",
                    reason: "relay closed the connection mid-compact".into(),
                })
            }
            _ => continue,
        };
        let msg: ServerMsg = match parse_server_msg(&text) {
            Ok(Some(msg)) => msg,
            Ok(None) => continue,
            Err(e) => return Err(net_err(e)),
        };
        if let ServerMsg::Welcome {
            latest_seq, caps, ..
        } = &msg
        {
            target = Some(*latest_seq);
            relay_compacts = caps.iter().any(|c| c == CAP_COMPACT);
            relay_compacts2 = caps.iter().any(|c| c == CAP_COMPACT2);
        }
        if let ServerMsg::Compacted { upto_seq } = &msg {
            let upto = *upto_seq;
            let _ = sink.send(Message::Close(None)).await;
            return Ok(upto);
        }
        let effect = engine.on_server_msg(msg)?;
        for out in &effect.outbound {
            sink.send(Message::Text(encode(out)?.into()))
                .await
                .map_err(net_err)?;
        }
    }
}

#[derive(Debug, Default)]
pub struct PassReport {
    pub run: RunReport,
    pub apply: ApplyReport,
}

/// One complete pass: exchange updates with the relay, then materialize the
/// merged state into the vault through the transactional writer.
pub async fn sync_pass(
    engine: &mut SyncEngine,
    writer: &mut NoteWriter,
    relay_url: &str,
) -> Result<PassReport, SyncError> {
    let run = run_once(engine, relay_url).await?;
    // The persisted apply queue is a superset of this run's dirty items —
    // it also carries anything an earlier crash received but never wrote
    // (the relay won't redeliver below the cursor, so this is the only
    // remaining trigger).
    let dirty = engine.pending_apply();
    let apply = engine.apply_dirty(writer, &dirty)?;
    Ok(PassReport { run, apply })
}

/// Sync forever: a pass now, then one every `interval`, reconnecting on
/// network failure. Returns only on an unrecoverable error.
pub async fn run_forever(
    engine: &mut SyncEngine,
    writer: &mut NoteWriter,
    relay_url: &str,
    interval: Duration,
    mut on_pass: impl FnMut(&PassReport),
) -> Result<(), SyncError> {
    loop {
        match sync_pass(engine, writer, relay_url).await {
            Ok(report) => on_pass(&report),
            Err(e) if e.code == "NETWORK" => {
                eprintln!("sync: {e} — retrying on the next tick");
            }
            Err(e) => return Err(e),
        }
        tokio::time::sleep(interval).await;
    }
}
