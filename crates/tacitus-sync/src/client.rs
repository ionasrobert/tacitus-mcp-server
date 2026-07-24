//! The async WebSocket driver around the sans-IO engine — the same loop
//! serves the CLI (`tacitus-mcp sync once|run`) and the desktop app.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

use tacitus_core::vault::NoteWriter;

use crate::apply::ApplyReport;
use crate::engine::SyncEngine;
use crate::protocol::{ClientMsg, ServerMsg};
use crate::SyncError;

/// How long we wait for the relay to say something before deciding the
/// connection went quiet mid-sync.
const QUIET_TIMEOUT: Duration = Duration::from_secs(15);

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

fn ws_url(relay_url: &str) -> String {
    let base = relay_url.trim_end_matches('/');
    if base.ends_with("/ws") {
        base.to_string()
    } else {
        format!("{base}/ws")
    }
}

/// One full sync pass: connect, push local changes, drain the backlog,
/// return once the outbox is empty and the cursor caught up to the log.
pub async fn run_once(engine: &mut SyncEngine, relay_url: &str) -> Result<RunReport, SyncError> {
    let (ws, _) = tokio_tungstenite::connect_async(ws_url(relay_url))
        .await
        .map_err(net_err)?;
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
        let msg: ServerMsg = serde_json::from_str(&text).map_err(net_err)?;
        if let ServerMsg::Welcome { latest_seq } = &msg {
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
    let apply = engine.apply_dirty(writer, &run.dirty_items)?;
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
