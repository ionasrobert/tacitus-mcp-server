//! tacitus-relay — the dumb half of Tacitus Sync.
//!
//! Clients speak the JSON protocol from `tacitus-sync` over a WebSocket at
//! `/ws`. The relay authenticates a vault (TOFU bearer token), replays the
//! backlog after `since_seq`, appends pushed blobs to a per-vault
//! append-only log, and fans every update out to ALL of the vault's live
//! connections (pusher included). It never sees plaintext: blobs are
//! end-to-end encrypted by the clients.
//!
//!   TACITUS_RELAY_BIND           (default 127.0.0.1:8091)
//!   TACITUS_RELAY_DATA           (default ./relay-data)
//!   TACITUS_RELAY_QUOTA_DEFAULT  (bytes; default 512 MiB) — per-vault quota
//!                                for vaults with no entitlement
//!   TACITUS_RELAY_ENTITLEMENTS   (default <data>/entitlements.json) —
//!                                per-vault quota overrides, hot-reloaded

mod hub;
mod log;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use serde::{Deserialize, Serialize};

use hub::{valid_vault_id, RelayConfig, RelayState, VaultHub};

// The wire protocol, mirrored from tacitus-sync/src/protocol.rs (the relay
// deliberately does not depend on the sync crate — it must never be able to
// read payloads, and the compiler enforcing that is worth a few lines).
/// Extensions this relay speaks; advertised in every Welcome. Extension
/// frames are only ever SENT to connections whose Hello asked for the cap —
/// an old client can never receive a tag it can't parse. A client below a
/// compaction snapshot WITHOUT the compact cap is rejected honestly
/// (`err compacted`) instead of silently missing the compacted prefix.
/// "quota" gates only the `quota_exceeded` err code — clients without it
/// get the legacy `log_full` (an unknown err code is fatal to old clients).
const RELAY_CAPS: [&str; 4] = ["presence", "compact", "compact2", "quota"];

/// Presence blobs are ephemeral (never logged) so the log cap doesn't bound
/// them — this does. Real payloads are ~300 bytes.
const PRESENCE_MAX_B64_LEN: usize = 8 * 1024;

#[derive(Debug, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum ClientMsg {
    Hello {
        vault_id: String,
        token: String,
        since_seq: u64,
        #[serde(default)]
        caps: Vec<String>,
    },
    Push {
        blob: String, // base64 — kept opaque, decoded only for storage
    },
    /// Ephemeral: fanned out to the vault's presence-capable connections,
    /// never logged, never sequenced, never acked.
    Presence { blob: String },
    /// A sealed full-state snapshot covering the log up to `upto_seq` —
    /// the relay truncates beneath it (still opaque ciphertext).
    Compact { upto_seq: u64, blob: String },
    /// One part of a chunked snapshot offer (compact2), staged on disk and
    /// committed when the last part arrives. Parts must come in order on
    /// one connection; a violation discards the staging (advisory error).
    CompactPart {
        upto_seq: u64,
        idx: u32,
        of: u32,
        blob: String,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum ServerMsg {
    Welcome {
        latest_seq: u64,
        caps: Vec<String>,
        log_bytes: u64,
        /// The vault's effective quota — always the enforced number, whether
        /// it comes from an entitlement or the default. (Clients read 0 as
        /// "relay predates quotas"; a real quota is never 0.)
        quota_bytes: u64,
    },
    Update {
        seq: u64,
        blob: String,
    },
    Ack {
        seq: u64,
    },
    Err {
        code: String,
        msg: String,
    },
    Presence {
        blob: String,
    },
    Snapshot {
        upto_seq: u64,
        blob: String,
    },
    /// One part of a chunked snapshot, served in place of `snapshot` (only
    /// to compact2 connections) when the stored snapshot is multi-part.
    SnapshotPart {
        upto_seq: u64,
        idx: u32,
        of: u32,
        blob: String,
    },
    Compacted {
        upto_seq: u64,
    },
}

fn b64_encode(bytes: &[u8]) -> String {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    STANDARD.encode(bytes)
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    STANDARD.decode(s).ok()
}

pub fn app(state: Arc<RelayState>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/ws", get(ws_handler))
        .with_state(state)
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<RelayState>>,
) -> impl IntoResponse {
    // Compaction snapshots can exceed the 16 MiB default frame cap
    // (tungstenite never fragments outgoing messages) — allow up to the
    // b64-inflated SNAPSHOT_MAX in both directions.
    ws.max_message_size(64 * 1024 * 1024)
        .max_frame_size(64 * 1024 * 1024)
        .on_upgrade(move |socket| connection(socket, state))
}

async fn send(socket: &mut WebSocket, msg: &ServerMsg) -> bool {
    match serde_json::to_string(msg) {
        Ok(json) => socket.send(Message::Text(json.into())).await.is_ok(),
        Err(_) => false,
    }
}

async fn reject(mut socket: WebSocket, code: &str, msg: &str) {
    let _ = send(
        &mut socket,
        &ServerMsg::Err {
            code: code.into(),
            msg: msg.into(),
        },
    )
    .await;
}

/// Map a compaction refusal onto its structured wire code and send it.
/// Rejections are advisory — the log is untouched and the connection lives
/// on. Returns false only when the socket died.
async fn send_compact_refusal(socket: &mut WebSocket, e: &std::io::Error) -> bool {
    let reason = e.to_string();
    let code = [
        "compact_stale",
        "compact_ahead",
        "snapshot_too_large",
        "compact_part_too_large",
        "compact_too_large",
        "compact_part_order",
    ]
    .into_iter()
    .find(|known| reason.contains(known))
    .unwrap_or("storage");
    if code == "storage" {
        tracing::error!("compact failed: {e}");
    }
    send(
        socket,
        &ServerMsg::Err {
            code: code.into(),
            msg: reason,
        },
    )
    .await
}

/// Serve the stored snapshot to a lagging client, one part per short lock —
/// resident memory stays at one part no matter the vault size. A single
/// part goes as the legacy `snapshot` frame (0.22 serving unchanged);
/// multi-part as `snapshot_part` 0..of-1. Returns false when the socket
/// died or a newer compaction swapped the snapshot mid-serve — the caller
/// closes, and the reconnect gets the fresh one.
async fn send_snapshot(socket: &mut WebSocket, hub: &VaultHub, seq: u64, parts: u32) -> bool {
    for idx in 0..parts {
        let part = {
            let log = hub.log.lock().await;
            if log.snapshot_seq() != Some(seq) {
                return false;
            }
            log.snapshot_part(idx)
        };
        let Ok(blob) = part else { return false };
        let msg = if parts == 1 {
            ServerMsg::Snapshot {
                upto_seq: seq,
                blob: b64_encode(&blob),
            }
        } else {
            ServerMsg::SnapshotPart {
                upto_seq: seq,
                idx,
                of: parts,
                blob: b64_encode(&blob),
            }
        };
        if !send(socket, &msg).await {
            return false;
        }
    }
    true
}

/// Await a presence frame when subscribed; pend forever when not. Lets the
/// select! below stay guard-free (an unsubscribed connection simply never
/// completes this arm).
async fn recv_presence(
    rx: &mut Option<tokio::sync::broadcast::Receiver<String>>,
) -> Result<String, tokio::sync::broadcast::error::RecvError> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn connection(mut socket: WebSocket, state: Arc<RelayState>) {
    // First frame must be a Hello.
    let hello = loop {
        match socket.recv().await {
            Some(Ok(Message::Text(text))) => match serde_json::from_str::<ClientMsg>(&text) {
                Ok(ClientMsg::Hello {
                    vault_id,
                    token,
                    since_seq,
                    caps,
                }) => break (vault_id, token, since_seq, caps),
                Ok(_) => return reject(socket, "protocol", "hello must come first").await,
                Err(_) => return reject(socket, "protocol", "malformed frame").await,
            },
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            _ => return,
        }
    };
    let (vault_id, token, since_seq, caps) = hello;
    let wants_presence = caps.iter().any(|c| c == "presence");
    let wants_compact = caps.iter().any(|c| c == "compact");
    let wants_compact2 = caps.iter().any(|c| c == "compact2");
    let wants_quota = caps.iter().any(|c| c == "quota");

    if !valid_vault_id(&vault_id) {
        return reject(
            socket,
            "bad_vault_id",
            "vault_id must be 32 lowercase hex chars",
        )
        .await;
    }
    let hub: Arc<VaultHub> = match state.vault(&vault_id).await {
        Ok(hub) => hub,
        Err(e) => {
            tracing::error!("vault open failed: {e}");
            return reject(socket, "storage", "cannot open vault storage").await;
        }
    };
    match hub.log.lock().await.check_or_register_token(&token) {
        Ok(true) => {}
        Ok(false) => return reject(socket, "auth", "wrong token for this vault").await,
        Err(e) => {
            tracing::error!("token check failed: {e}");
            return reject(socket, "storage", "token storage failed").await;
        }
    }

    // Subscribe BEFORE reading the backlog so nothing lands in the gap;
    // `last_sent` dedups the overlap. Presence subscription doubles as the
    // capability gate: unsubscribed connections can never receive the tag.
    let mut rx = hub.tx.subscribe();
    let mut presence_rx = wants_presence.then(|| hub.presence.subscribe());
    // Quota is sampled before the log lock, never under it.
    let quota_bytes = state.quota_for(&vault_id);
    let (latest_seq, log_bytes, snapshot, backlog) = {
        let log = hub.log.lock().await;
        // A cursor below the snapshot replays snapshot-then-tail; everyone
        // else replays a plain tail from where they left off. Only (seq,
        // parts) is captured — the bytes are read per-part while serving.
        let snapshot = match log.snapshot_seq() {
            Some(seq) if since_seq < seq => Some((seq, log.snapshot_parts())),
            _ => None,
        };
        let from = snapshot.map(|(seq, _)| seq).unwrap_or(since_seq);
        (
            log.last_seq(),
            log.log_bytes(),
            snapshot,
            log.read_since(from),
        )
    };
    if let Some((_, parts)) = snapshot {
        // The compacted prefix no longer exists as entries, and this client
        // can't parse frames it didn't ask for — fail honestly. Multi-part
        // needs compact2; compact-only clients get the same treatment 0.21
        // clients get below any snapshot.
        if !wants_compact || (parts > 1 && !wants_compact2) {
            return reject(
                socket,
                "compacted",
                "log compacted past your cursor; upgrade this client",
            )
            .await;
        }
    }
    let backlog = match backlog {
        Ok(backlog) => backlog,
        Err(e) => {
            tracing::error!("backlog read failed: {e}");
            return reject(socket, "storage", "backlog read failed").await;
        }
    };
    let welcome = ServerMsg::Welcome {
        latest_seq,
        caps: RELAY_CAPS.iter().map(|c| c.to_string()).collect(),
        log_bytes,
        quota_bytes,
    };
    if !send(&mut socket, &welcome).await {
        return;
    }
    let mut last_sent = since_seq;
    if let Some((snapshot_seq, parts)) = snapshot {
        if !send_snapshot(&mut socket, &hub, snapshot_seq, parts).await {
            return;
        }
        last_sent = snapshot_seq;
    }
    for (seq, blob) in backlog {
        if !send(
            &mut socket,
            &ServerMsg::Update {
                seq,
                blob: b64_encode(&blob),
            },
        )
        .await
        {
            return;
        }
        last_sent = seq;
    }

    let mut ping = tokio::time::interval(Duration::from_secs(30));
    ping.tick().await; // first tick fires immediately — skip it

    // This connection's in-progress chunked upload; dropped (dir removed)
    // on disconnect or on any ordering violation.
    let mut staging: Option<log::PartStaging> = None;

    loop {
        tokio::select! {
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ClientMsg>(&text) {
                            Ok(ClientMsg::Push { blob }) => {
                                let Some(bytes) = b64_decode(&blob) else {
                                    let _ = send(&mut socket, &ServerMsg::Err {
                                        code: "protocol".into(),
                                        msg: "push blob is not base64".into(),
                                    }).await;
                                    continue;
                                };
                                // Sampled per push (before the log lock) so
                                // an entitlement upgrade unblocks a refused
                                // vault without a reconnect.
                                let quota = state.quota_for(&vault_id);
                                let appended = hub.log.lock().await.append_within(&bytes, quota);
                                match appended {
                                    Ok(seq) => {
                                        if !send(&mut socket, &ServerMsg::Ack { seq }).await {
                                            return;
                                        }
                                        let _ = hub.tx.send((seq, bytes));
                                    }
                                    Err(e) if e.to_string().contains("quota_exceeded") => {
                                        // Old clients treat any unknown err
                                        // code as fatal — they keep getting
                                        // the code they already know.
                                        let code = if wants_quota { "quota_exceeded" } else { "log_full" };
                                        let _ = send(&mut socket, &ServerMsg::Err {
                                            code: code.into(),
                                            msg: format!("vault storage quota exceeded ({quota} bytes)"),
                                        }).await;
                                    }
                                    Err(e) => {
                                        tracing::error!("append failed: {e}");
                                        let _ = send(&mut socket, &ServerMsg::Err {
                                            code: "storage".into(),
                                            msg: "append failed".into(),
                                        }).await;
                                    }
                                }
                            }
                            Ok(ClientMsg::Presence { blob }) => {
                                // Ephemeral: no log, no fsync, no seq, no
                                // ack — fan out (sender included; clients
                                // filter their own device) and forget.
                                if blob.len() > PRESENCE_MAX_B64_LEN {
                                    let _ = send(&mut socket, &ServerMsg::Err {
                                        code: "protocol".into(),
                                        msg: "presence blob too large".into(),
                                    }).await;
                                } else {
                                    let _ = hub.presence.send(blob);
                                }
                            }
                            Ok(ClientMsg::Compact { upto_seq, blob }) => {
                                let Some(bytes) = b64_decode(&blob) else {
                                    let _ = send(&mut socket, &ServerMsg::Err {
                                        code: "protocol".into(),
                                        msg: "compact blob is not base64".into(),
                                    }).await;
                                    continue;
                                };
                                let quota = state.quota_for(&vault_id);
                                let result =
                                    hub.log.lock().await.compact_within(upto_seq, &bytes, quota);
                                match result {
                                    Ok(()) => {
                                        if !send(&mut socket, &ServerMsg::Compacted { upto_seq }).await {
                                            return;
                                        }
                                    }
                                    Err(e) => {
                                        if !send_compact_refusal(&mut socket, &e).await {
                                            return;
                                        }
                                    }
                                }
                            }
                            Ok(ClientMsg::CompactPart { upto_seq, idx, of, blob }) => {
                                let Some(bytes) = b64_decode(&blob) else {
                                    staging = None;
                                    let _ = send(&mut socket, &ServerMsg::Err {
                                        code: "protocol".into(),
                                        msg: "compact_part blob is not base64".into(),
                                    }).await;
                                    continue;
                                };
                                // Parts must arrive in order and agree on
                                // (upto, of); anything else discards the
                                // staging — advisory, like every refusal.
                                let in_order = match &staging {
                                    None => idx == 0,
                                    Some(s) => {
                                        idx == s.next_idx()
                                            && upto_seq == s.upto_seq()
                                            && of == s.of()
                                    }
                                };
                                if !in_order {
                                    staging = None;
                                    let _ = send(&mut socket, &ServerMsg::Err {
                                        code: "compact_part_order".into(),
                                        msg: "parts must arrive in order for one offer".into(),
                                    }).await;
                                    continue;
                                }
                                if staging.is_none() {
                                    let quota = state.quota_for(&vault_id);
                                    match hub.log.lock().await.stage_parts_within(upto_seq, of, quota) {
                                        Ok(s) => staging = Some(s),
                                        Err(e) => {
                                            if !send_compact_refusal(&mut socket, &e).await {
                                                return;
                                            }
                                            continue;
                                        }
                                    }
                                }
                                let st = staging.as_mut().expect("staged above");
                                if let Err(e) = st.add(&bytes) {
                                    staging = None;
                                    if !send_compact_refusal(&mut socket, &e).await {
                                        return;
                                    }
                                    continue;
                                }
                                if st.next_idx() == st.of() {
                                    let full = staging.take().expect("staged above");
                                    let result = hub.log.lock().await.commit_parts(full);
                                    match result {
                                        Ok(()) => {
                                            if !send(&mut socket, &ServerMsg::Compacted { upto_seq }).await {
                                                return;
                                            }
                                        }
                                        Err(e) => {
                                            if !send_compact_refusal(&mut socket, &e).await {
                                                return;
                                            }
                                        }
                                    }
                                }
                            }
                            Ok(ClientMsg::Hello { .. }) => {
                                let _ = send(&mut socket, &ServerMsg::Err {
                                    code: "protocol".into(),
                                    msg: "already said hello".into(),
                                }).await;
                            }
                            Err(_) => {
                                let _ = send(&mut socket, &ServerMsg::Err {
                                    code: "protocol".into(),
                                    msg: "malformed frame".into(),
                                }).await;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Ok(_)) => {} // ping/pong/binary ignored
                    Some(Err(_)) => return,
                }
            }
            update = rx.recv() => {
                match update {
                    Ok((seq, blob)) => {
                        if seq <= last_sent {
                            continue;
                        }
                        if !send(&mut socket, &ServerMsg::Update {
                            seq,
                            blob: b64_encode(&blob),
                        }).await {
                            return;
                        }
                        last_sent = seq;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Fell behind the channel: resync from the log. A
                        // compaction may have raced past us — same treatment
                        // as at hello: snapshot-then-tail (or an honest
                        // error for a client that can't parse snapshots).
                        let (snapshot, resync) = {
                            let log = hub.log.lock().await;
                            match log.snapshot_seq() {
                                Some(seq) if last_sent < seq => (
                                    Some((seq, log.snapshot_parts())),
                                    log.read_since(seq),
                                ),
                                _ => (None, log.read_since(last_sent)),
                            }
                        };
                        if let Some((snapshot_seq, parts)) = snapshot {
                            if !wants_compact || (parts > 1 && !wants_compact2) {
                                let _ = send(&mut socket, &ServerMsg::Err {
                                    code: "compacted".into(),
                                    msg: "log compacted past your cursor; upgrade this client".into(),
                                }).await;
                                return;
                            }
                            if !send_snapshot(&mut socket, &hub, snapshot_seq, parts).await {
                                return;
                            }
                            last_sent = snapshot_seq;
                        }
                        if let Ok(entries) = resync {
                            for (seq, blob) in entries {
                                if !send(&mut socket, &ServerMsg::Update {
                                    seq,
                                    blob: b64_encode(&blob),
                                }).await {
                                    return;
                                }
                                last_sent = seq;
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
            presence = recv_presence(&mut presence_rx) => {
                match presence {
                    Ok(blob) => {
                        if !send(&mut socket, &ServerMsg::Presence { blob }).await {
                            return;
                        }
                    }
                    // Ephemeral: nothing to resync on lag — just drop.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
            _ = ping.tick() => {
                if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                    return;
                }
            }
        }
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_target(false).init();
    let bind = std::env::var("TACITUS_RELAY_BIND").unwrap_or_else(|_| "127.0.0.1:8091".into());
    let data = std::env::var("TACITUS_RELAY_DATA").unwrap_or_else(|_| "./relay-data".into());
    let mut config = RelayConfig::new(PathBuf::from(&data));
    if let Ok(raw) = std::env::var("TACITUS_RELAY_QUOTA_DEFAULT") {
        match raw.parse::<u64>() {
            Ok(bytes) if bytes > 0 => config.default_quota_bytes = bytes,
            _ => tracing::warn!(
                "TACITUS_RELAY_QUOTA_DEFAULT={raw} is not a positive byte count; keeping {}",
                config.default_quota_bytes
            ),
        }
    }
    if let Ok(path) = std::env::var("TACITUS_RELAY_ENTITLEMENTS") {
        config.entitlements_path = PathBuf::from(path);
    }
    let state = Arc::new(RelayState::with_config(config));

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .unwrap_or_else(|e| panic!("cannot bind {bind}: {e}"));
    tracing::info!("tacitus-relay listening on {bind}, data in {data}");
    axum::serve(listener, app(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("server error");
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut dir = std::env::temp_dir();
        dir.push(format!("tacitus-relaytest-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn spawn_relay(data_dir: PathBuf) -> String {
        spawn_relay_with(data_dir, |_| {}).await
    }

    /// Like `spawn_relay`, with a config tweak — quota tests inject byte-
    /// scale quotas here (the `spawn_live` pattern, relay-side).
    async fn spawn_relay_with(data_dir: PathBuf, tweak: impl FnOnce(&mut RelayConfig)) -> String {
        let mut config = RelayConfig::new(data_dir);
        tweak(&mut config);
        let state = Arc::new(RelayState::with_config(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app(state)).await.unwrap();
        });
        format!("ws://{addr}/ws")
    }

    type Client = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    async fn connect(url: &str) -> Client {
        let (ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        ws
    }

    async fn send_json(ws: &mut Client, value: serde_json::Value) {
        ws.send(WsMessage::Text(value.to_string().into()))
            .await
            .unwrap();
    }

    /// Next JSON text frame (skips ping/pong), with a test timeout.
    async fn recv_json(ws: &mut Client) -> serde_json::Value {
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(15), ws.next())
                .await
                .expect("timed out waiting for a frame")
                .expect("stream ended")
                .expect("ws error");
            match frame {
                WsMessage::Text(text) => return serde_json::from_str(&text).unwrap(),
                WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
                other => panic!("unexpected frame: {other:?}"),
            }
        }
    }

    fn hello(vault_id: &str, token: &str, since_seq: u64) -> serde_json::Value {
        serde_json::json!({ "t": "hello", "vault_id": vault_id, "token": token, "since_seq": since_seq })
    }

    fn hello_caps(vault_id: &str, token: &str, since_seq: u64) -> serde_json::Value {
        serde_json::json!({
            "t": "hello", "vault_id": vault_id, "token": token,
            "since_seq": since_seq, "caps": ["presence"],
        })
    }

    fn presence_frame(payload: &[u8]) -> serde_json::Value {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;
        serde_json::json!({ "t": "presence", "blob": STANDARD.encode(payload) })
    }

    fn push(blob: &[u8]) -> serde_json::Value {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;
        serde_json::json!({ "t": "push", "blob": STANDARD.encode(blob) })
    }

    fn hello_compact(vault_id: &str, token: &str, since_seq: u64) -> serde_json::Value {
        serde_json::json!({
            "t": "hello", "vault_id": vault_id, "token": token,
            "since_seq": since_seq, "caps": ["presence", "compact"],
        })
    }

    fn compact_frame(upto_seq: u64, blob: &[u8]) -> serde_json::Value {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;
        serde_json::json!({ "t": "compact", "upto_seq": upto_seq, "blob": STANDARD.encode(blob) })
    }

    fn hello_compact2(vault_id: &str, token: &str, since_seq: u64) -> serde_json::Value {
        serde_json::json!({
            "t": "hello", "vault_id": vault_id, "token": token,
            "since_seq": since_seq, "caps": ["presence", "compact", "compact2"],
        })
    }

    fn compact_part_frame(upto_seq: u64, idx: u32, of: u32, blob: &[u8]) -> serde_json::Value {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;
        serde_json::json!({
            "t": "compact_part", "upto_seq": upto_seq, "idx": idx, "of": of,
            "blob": STANDARD.encode(blob),
        })
    }

    fn hello_quota(vault_id: &str, token: &str, since_seq: u64) -> serde_json::Value {
        serde_json::json!({
            "t": "hello", "vault_id": vault_id, "token": token,
            "since_seq": since_seq, "caps": ["presence", "compact", "compact2", "quota"],
        })
    }

    /// Push 3 blobs and compact up to seq 2; returns (url, data_dir, vault).
    async fn compacted_vault(tag: &str) -> (String, PathBuf, String) {
        let data = temp_dir(tag);
        let url = spawn_relay(data.clone()).await;
        let vault = "d".repeat(32);

        let mut writer = connect(&url).await;
        send_json(&mut writer, hello_compact(&vault, "tok", 0)).await;
        recv_json(&mut writer).await; // welcome
        for blob in [b"one".as_slice(), b"two", b"three"] {
            send_json(&mut writer, push(blob)).await;
            recv_json(&mut writer).await; // ack
            recv_json(&mut writer).await; // own echo
        }
        send_json(&mut writer, compact_frame(2, b"snapshot-of-1-and-2")).await;
        let reply = recv_json(&mut writer).await;
        assert_eq!(reply["t"], "compacted");
        assert_eq!(reply["upto_seq"], 2);
        (url, data, vault)
    }

    #[tokio::test]
    async fn welcome_advertises_compact_cap_and_log_bytes() {
        let url = spawn_relay(temp_dir("caps-bytes")).await;
        let vault = "a".repeat(32);

        let mut writer = connect(&url).await;
        send_json(&mut writer, hello_compact(&vault, "tok", 0)).await;
        let welcome = recv_json(&mut writer).await;
        let caps = welcome["caps"].as_array().unwrap();
        assert!(caps.iter().any(|c| c == "presence"));
        assert!(caps.iter().any(|c| c == "compact"));
        assert!(caps.iter().any(|c| c == "compact2"));
        assert_eq!(welcome["log_bytes"], 0, "empty vault, empty log");

        send_json(&mut writer, push(b"some-blob")).await;
        recv_json(&mut writer).await; // ack
        recv_json(&mut writer).await; // echo

        let mut second = connect(&url).await;
        send_json(&mut second, hello_compact(&vault, "tok", 1)).await;
        let welcome = recv_json(&mut second).await;
        assert!(
            welcome["log_bytes"].as_u64().unwrap() > 0,
            "log_bytes reflects the on-disk tail"
        );
    }

    #[tokio::test]
    async fn late_joiner_below_snapshot_gets_snapshot_then_tail_with_cap() {
        let (url, data, vault) = compacted_vault("snap-tail").await;

        // Below the snapshot, capable → snapshot first, then the tail.
        let mut joiner = connect(&url).await;
        send_json(&mut joiner, hello_compact(&vault, "tok", 0)).await;
        let welcome = recv_json(&mut joiner).await;
        assert_eq!(welcome["latest_seq"], 3);
        let snap = recv_json(&mut joiner).await;
        assert_eq!(snap["t"], "snapshot");
        assert_eq!(snap["upto_seq"], 2);
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;
        assert_eq!(
            STANDARD.decode(snap["blob"].as_str().unwrap()).unwrap(),
            b"snapshot-of-1-and-2"
        );
        let tail = recv_json(&mut joiner).await;
        assert_eq!(tail["t"], "update");
        assert_eq!(tail["seq"], 3);

        // At or past the snapshot, capable → no snapshot frame, plain tail.
        let mut caught = connect(&url).await;
        send_json(&mut caught, hello_compact(&vault, "tok", 2)).await;
        recv_json(&mut caught).await; // welcome
        assert_eq!(recv_json(&mut caught).await["seq"], 3);

        // And the prefix is truly gone from disk (one tail line left).
        let raw = std::fs::read_to_string(data.join(&vault).join("log.jsonl")).unwrap();
        assert_eq!(raw.lines().count(), 1);
    }

    #[tokio::test]
    async fn late_joiner_below_snapshot_without_cap_gets_err_compacted() {
        let (url, _data, vault) = compacted_vault("snap-nocap").await;

        // A 0.21 client below the snapshot: honest fatal error, never a
        // silent gap (it can't parse a snapshot frame it didn't ask for).
        let mut old = connect(&url).await;
        send_json(&mut old, hello(&vault, "tok", 0)).await;
        let err = recv_json(&mut old).await;
        assert_eq!(err["t"], "err");
        assert_eq!(err["code"], "compacted");

        // The same old client at or past the snapshot keeps working.
        let mut old_caught = connect(&url).await;
        send_json(&mut old_caught, hello(&vault, "tok", 2)).await;
        assert_eq!(recv_json(&mut old_caught).await["t"], "welcome");
        assert_eq!(recv_json(&mut old_caught).await["seq"], 3);
    }

    #[tokio::test]
    async fn compact_with_bad_upto_returns_err_not_close() {
        let url = spawn_relay(temp_dir("compact-bad")).await;
        let vault = "e".repeat(32);

        let mut ws = connect(&url).await;
        send_json(&mut ws, hello_compact(&vault, "tok", 0)).await;
        recv_json(&mut ws).await; // welcome
        send_json(&mut ws, push(b"only-entry")).await;
        recv_json(&mut ws).await; // ack
        recv_json(&mut ws).await; // echo

        // Claiming coverage beyond the log's head is refused…
        send_json(&mut ws, compact_frame(99, b"snap")).await;
        let err = recv_json(&mut ws).await;
        assert_eq!(err["t"], "err");
        assert_eq!(err["code"], "compact_ahead");

        // …but the connection survives and keeps accepting pushes.
        send_json(&mut ws, push(b"still-alive")).await;
        assert_eq!(recv_json(&mut ws).await["t"], "ack");
    }

    // ---- per-vault quotas (mon-m1)
    //
    // Quotas in these tests are tens of bytes: one JSON log line (~40 bytes
    // for a 3-byte blob) is enough to cross them — no MB fixtures.

    #[tokio::test]
    async fn welcome_advertises_quota_cap_and_effective_quota_bytes() {
        let data = temp_dir("quota-welcome");
        let url = spawn_relay_with(data.clone(), |c| c.default_quota_bytes = 140).await;
        let vault = "a".repeat(32);

        let mut ws = connect(&url).await;
        send_json(&mut ws, hello(&vault, "tok", 0)).await;
        let welcome = recv_json(&mut ws).await;
        assert!(welcome["caps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "quota"));
        assert_eq!(
            welcome["quota_bytes"], 140,
            "the effective quota is sent even when it equals the default"
        );

        // An entitlement granted while the relay runs shows up on the next
        // hello — the file is hot-reloaded, no restart.
        std::fs::write(
            data.join("entitlements.json"),
            format!(r#"{{"version":1,"vaults":{{"{vault}":{{"quota_bytes":5000}}}}}}"#),
        )
        .unwrap();
        let mut second = connect(&url).await;
        send_json(&mut second, hello(&vault, "tok", 0)).await;
        assert_eq!(recv_json(&mut second).await["quota_bytes"], 5000);
    }

    #[tokio::test]
    async fn push_over_quota_errs_quota_exceeded_but_compaction_heals() {
        let url = spawn_relay_with(temp_dir("quota-push"), |c| c.default_quota_bytes = 30).await;
        let vault = "b".repeat(32);

        let mut ws = connect(&url).await;
        send_json(&mut ws, hello_quota(&vault, "tok", 0)).await;
        recv_json(&mut ws).await; // welcome
        send_json(&mut ws, push(b"one")).await;
        assert_eq!(recv_json(&mut ws).await["t"], "ack");
        recv_json(&mut ws).await; // own echo

        // The first line put the log over 30 bytes — refused, no seq burned.
        send_json(&mut ws, push(b"two")).await;
        let err = recv_json(&mut ws).await;
        assert_eq!(err["t"], "err");
        assert_eq!(err["code"], "quota_exceeded");

        // Compaction is the cure: allowed while over quota…
        send_json(&mut ws, compact_frame(1, b"snap")).await;
        assert_eq!(recv_json(&mut ws).await["t"], "compacted");
        // …and the truncated log accepts pushes again, seqs continuous.
        send_json(&mut ws, push(b"two")).await;
        let ack = recv_json(&mut ws).await;
        assert_eq!(ack["t"], "ack");
        assert_eq!(ack["seq"], 2);
    }

    #[tokio::test]
    async fn legacy_client_over_quota_still_gets_log_full() {
        let url = spawn_relay_with(temp_dir("quota-legacy"), |c| c.default_quota_bytes = 30).await;
        let vault = "c".repeat(32);

        // No "quota" in the hello caps: an unknown err code is fatal to old
        // clients, so they keep receiving the one they already know.
        let mut ws = connect(&url).await;
        send_json(&mut ws, hello_compact(&vault, "tok", 0)).await;
        recv_json(&mut ws).await; // welcome
        send_json(&mut ws, push(b"one")).await;
        recv_json(&mut ws).await; // ack
        recv_json(&mut ws).await; // echo
        send_json(&mut ws, push(b"two")).await;
        let err = recv_json(&mut ws).await;
        assert_eq!(err["t"], "err");
        assert_eq!(err["code"], "log_full");
        // Still advisory relay-side: the connection keeps working.
        send_json(&mut ws, compact_frame(1, b"snap")).await;
        assert_eq!(recv_json(&mut ws).await["t"], "compacted");
    }

    #[tokio::test]
    async fn entitlements_raise_quota_per_vault() {
        let data = temp_dir("quota-ent");
        let entitled = "a1".repeat(16);
        std::fs::write(
            data.join("entitlements.json"),
            format!(
                r#"{{"version":1,"vaults":{{"{entitled}":{{"quota_bytes":10000,"tier":"pro"}}}}}}"#
            ),
        )
        .unwrap();
        let url = spawn_relay_with(data, |c| c.default_quota_bytes = 30).await;

        // The entitled vault sails past the default…
        let mut pro = connect(&url).await;
        send_json(&mut pro, hello_quota(&entitled, "tok", 0)).await;
        assert_eq!(recv_json(&mut pro).await["quota_bytes"], 10000);
        for blob in [b"one".as_slice(), b"two"] {
            send_json(&mut pro, push(blob)).await;
            assert_eq!(recv_json(&mut pro).await["t"], "ack");
            recv_json(&mut pro).await; // echo
        }

        // …while an unentitled one is refused at 30 bytes.
        let free = "b2".repeat(16);
        let mut ws = connect(&url).await;
        send_json(&mut ws, hello_quota(&free, "tok", 0)).await;
        recv_json(&mut ws).await; // welcome
        send_json(&mut ws, push(b"one")).await;
        recv_json(&mut ws).await; // ack
        recv_json(&mut ws).await; // echo
        send_json(&mut ws, push(b"two")).await;
        assert_eq!(recv_json(&mut ws).await["code"], "quota_exceeded");
    }

    // ---- chunked compaction (collab-m5)

    /// Push 3 blobs and chunk-compact up to seq 2 in two parts.
    async fn chunked_vault(tag: &str) -> (String, PathBuf, String) {
        let data = temp_dir(tag);
        let url = spawn_relay(data.clone()).await;
        let vault = "f".repeat(32);

        let mut writer = connect(&url).await;
        send_json(&mut writer, hello_compact2(&vault, "tok", 0)).await;
        recv_json(&mut writer).await; // welcome
        for blob in [b"one".as_slice(), b"two", b"three"] {
            send_json(&mut writer, push(blob)).await;
            recv_json(&mut writer).await; // ack
            recv_json(&mut writer).await; // own echo
        }
        send_json(&mut writer, compact_part_frame(2, 0, 2, b"part-zero")).await;
        send_json(&mut writer, compact_part_frame(2, 1, 2, b"part-one")).await;
        let reply = recv_json(&mut writer).await;
        assert_eq!(reply["t"], "compacted");
        assert_eq!(reply["upto_seq"], 2);
        (url, data, vault)
    }

    #[tokio::test]
    async fn chunked_upload_commits_on_last_part_and_serves_parts_to_compact2() {
        let (url, data, vault) = chunked_vault("chunk-commit").await;
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;

        // A compact2 joiner below the snapshot gets the parts in order,
        // then the tail.
        let mut joiner = connect(&url).await;
        send_json(&mut joiner, hello_compact2(&vault, "tok", 0)).await;
        let welcome = recv_json(&mut joiner).await;
        assert_eq!(welcome["latest_seq"], 3);
        for (idx, expected) in [(0, b"part-zero".as_slice()), (1, b"part-one")] {
            let part = recv_json(&mut joiner).await;
            assert_eq!(part["t"], "snapshot_part");
            assert_eq!(part["upto_seq"], 2);
            assert_eq!(part["idx"], idx);
            assert_eq!(part["of"], 2);
            assert_eq!(
                STANDARD.decode(part["blob"].as_str().unwrap()).unwrap(),
                expected
            );
        }
        let tail = recv_json(&mut joiner).await;
        assert_eq!(tail["t"], "update");
        assert_eq!(tail["seq"], 3);

        // v2 layout on disk, prefix truly gone from the log.
        let vdir = data.join(&vault);
        let raw = std::fs::read_to_string(vdir.join("snapshot.json")).unwrap();
        assert!(raw.contains("\"parts\":2"));
        assert!(vdir.join("snapshot.parts.2").join("1").is_file());
        let raw = std::fs::read_to_string(vdir.join("log.jsonl")).unwrap();
        assert_eq!(raw.lines().count(), 1);
    }

    #[tokio::test]
    async fn chunked_upload_out_of_order_discards_and_errs_but_connection_survives() {
        let url = spawn_relay(temp_dir("chunk-order")).await;
        let vault = "f".repeat(32);

        let mut ws = connect(&url).await;
        send_json(&mut ws, hello_compact2(&vault, "tok", 0)).await;
        recv_json(&mut ws).await; // welcome
        for blob in [b"one".as_slice(), b"two"] {
            send_json(&mut ws, push(blob)).await;
            recv_json(&mut ws).await; // ack
            recv_json(&mut ws).await; // echo
        }

        // First part must be idx 0…
        send_json(&mut ws, compact_part_frame(2, 1, 2, b"part-one")).await;
        let err = recv_json(&mut ws).await;
        assert_eq!(err["t"], "err");
        assert_eq!(err["code"], "compact_part_order");

        // …a mid-offer mismatch discards the staging…
        send_json(&mut ws, compact_part_frame(2, 0, 2, b"part-zero")).await;
        send_json(&mut ws, compact_part_frame(2, 0, 3, b"regressed")).await;
        let err = recv_json(&mut ws).await;
        assert_eq!(err["code"], "compact_part_order");

        // …the connection survives, and a clean retry commits.
        send_json(&mut ws, push(b"still-alive")).await;
        assert_eq!(recv_json(&mut ws).await["t"], "ack");
        recv_json(&mut ws).await; // echo
        send_json(&mut ws, compact_part_frame(3, 0, 2, b"p0")).await;
        send_json(&mut ws, compact_part_frame(3, 1, 2, b"p1")).await;
        let reply = recv_json(&mut ws).await;
        assert_eq!(reply["t"], "compacted");
        assert_eq!(reply["upto_seq"], 3);
    }

    #[tokio::test]
    async fn multi_part_snapshot_below_cursor_is_err_compacted_for_compact_only_client() {
        let (url, _data, vault) = chunked_vault("chunk-oldcap").await;

        // A 0.22-style client (compact, no compact2) below a multi-part
        // snapshot gets the same honest fatal a 0.21 client gets below any
        // snapshot: err compacted, upgrade.
        let mut old = connect(&url).await;
        send_json(&mut old, hello_compact(&vault, "tok", 0)).await;
        let err = recv_json(&mut old).await;
        assert_eq!(err["t"], "err");
        assert_eq!(err["code"], "compacted");

        // At or past the snapshot the same client works untouched.
        let mut caught = connect(&url).await;
        send_json(&mut caught, hello_compact(&vault, "tok", 2)).await;
        recv_json(&mut caught).await; // welcome
        assert_eq!(recv_json(&mut caught).await["seq"], 3);
    }

    #[tokio::test]
    async fn single_part_snapshot_still_served_as_legacy_snapshot_to_compact2() {
        // Compat guard: chunking must not change how snapshots that fit one
        // frame are served — 0.22 clients keep working below them.
        let (url, _data, vault) = compacted_vault("chunk-legacy").await;

        let mut joiner = connect(&url).await;
        send_json(&mut joiner, hello_compact2(&vault, "tok", 0)).await;
        recv_json(&mut joiner).await; // welcome
        let snap = recv_json(&mut joiner).await;
        assert_eq!(snap["t"], "snapshot", "one part → the legacy frame");
        assert_eq!(snap["upto_seq"], 2);
        assert_eq!(recv_json(&mut joiner).await["seq"], 3);
    }

    #[tokio::test]
    async fn disconnect_mid_upload_discards_staging() {
        let data = temp_dir("chunk-drop");
        let url = spawn_relay(data.clone()).await;
        let vault = "f".repeat(32);

        let mut ws = connect(&url).await;
        send_json(&mut ws, hello_compact2(&vault, "tok", 0)).await;
        recv_json(&mut ws).await; // welcome
        send_json(&mut ws, push(b"one")).await;
        recv_json(&mut ws).await; // ack
        recv_json(&mut ws).await; // echo

        send_json(&mut ws, compact_part_frame(1, 0, 2, b"part-zero")).await;
        // Half an offer, then the connection dies.
        drop(ws);

        let vdir = data.join(&vault);
        wait_until("staging dir cleaned up after disconnect", || {
            !std::fs::read_dir(&vdir).is_ok_and(|entries| {
                entries.flatten().any(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with("snapshot.staging.")
                })
            })
        })
        .await;
        assert!(
            !vdir.join("snapshot.json").exists(),
            "nothing was committed"
        );

        // A fresh, complete upload succeeds afterwards.
        let mut ws = connect(&url).await;
        send_json(&mut ws, hello_compact2(&vault, "tok", 1)).await;
        recv_json(&mut ws).await; // welcome
        send_json(&mut ws, compact_part_frame(1, 0, 2, b"p0")).await;
        send_json(&mut ws, compact_part_frame(1, 1, 2, b"p1")).await;
        assert_eq!(recv_json(&mut ws).await["t"], "compacted");
    }

    #[tokio::test]
    async fn hello_flow_welcome_push_ack_and_fanout_to_all() {
        let url = spawn_relay(temp_dir("flow")).await;
        let vault = "a".repeat(32);

        let mut one = connect(&url).await;
        send_json(&mut one, hello(&vault, "tok", 0)).await;
        assert_eq!(recv_json(&mut one).await["t"], "welcome");

        let mut two = connect(&url).await;
        send_json(&mut two, hello(&vault, "tok", 0)).await;
        assert_eq!(recv_json(&mut two).await["t"], "welcome");

        send_json(&mut one, push(b"encrypted-blob")).await;
        let ack = recv_json(&mut one).await;
        assert_eq!(ack["t"], "ack");
        assert_eq!(ack["seq"], 1);

        // Fanout reaches the OTHER client…
        let update = recv_json(&mut two).await;
        assert_eq!(update["t"], "update");
        assert_eq!(update["seq"], 1);
        // …and the pusher gets its own echo too (cursor advances via updates).
        let echo = recv_json(&mut one).await;
        assert_eq!(echo["t"], "update");
        assert_eq!(echo["seq"], 1);
    }

    #[tokio::test]
    async fn backlog_replays_from_since_seq_over_ws() {
        let data = temp_dir("ws-backlog");
        let url = spawn_relay(data).await;
        let vault = "b".repeat(32);

        let mut writer = connect(&url).await;
        send_json(&mut writer, hello(&vault, "tok", 0)).await;
        recv_json(&mut writer).await; // welcome
        for blob in [b"one".as_slice(), b"two", b"three"] {
            send_json(&mut writer, push(blob)).await;
            recv_json(&mut writer).await; // ack
            recv_json(&mut writer).await; // own echo
        }

        let mut reader = connect(&url).await;
        send_json(&mut reader, hello(&vault, "tok", 1)).await;
        let welcome = recv_json(&mut reader).await;
        assert_eq!(welcome["latest_seq"], 3);
        assert_eq!(recv_json(&mut reader).await["seq"], 2);
        assert_eq!(recv_json(&mut reader).await["seq"], 3);
    }

    #[tokio::test]
    async fn hello_with_wrong_token_is_rejected() {
        let url = spawn_relay(temp_dir("auth")).await;
        let vault = "c".repeat(32);

        let mut first = connect(&url).await;
        send_json(&mut first, hello(&vault, "the-right-token", 0)).await;
        assert_eq!(recv_json(&mut first).await["t"], "welcome");

        let mut wrong = connect(&url).await;
        send_json(&mut wrong, hello(&vault, "not-the-token", 0)).await;
        let err = recv_json(&mut wrong).await;
        assert_eq!(err["t"], "err");
        assert_eq!(err["code"], "auth");
    }

    #[tokio::test]
    async fn driver_reconnects_and_resumes_with_cursor() {
        use tacitus_sync::{client, SyncEngine, VaultCode};
        let url = spawn_relay(temp_dir("driver-data")).await;

        let va = temp_dir("driver-va");
        let vb = temp_dir("driver-vb");
        std::fs::write(va.join("note.md"), "hello from A\n").unwrap();
        let code = VaultCode::generate();

        let mut a = SyncEngine::open(&va, &code).unwrap();
        let report = client::run_once(&mut a, &url).await.unwrap();
        assert_eq!(report.pushed, 1);

        let mut b = SyncEngine::open(&vb, &code).unwrap();
        client::run_once(&mut b, &url).await.unwrap();
        assert_eq!(
            b.materialize("n:note").unwrap().as_deref(),
            Some("hello from A\n")
        );
        let cursor = b.last_seq();
        assert!(cursor >= 1);
        drop(b);

        // Reconnect: resumes from the persisted cursor, applies nothing new.
        let mut b2 = SyncEngine::open(&vb, &code).unwrap();
        let report = client::run_once(&mut b2, &url).await.unwrap();
        assert_eq!(report.applied, 0);
        assert_eq!(b2.last_seq(), cursor);
    }

    #[tokio::test]
    async fn e2e_two_vaults_converge_over_in_process_relay() {
        use tacitus_core::vault::{NoteWriter, PermissionScope};
        use tacitus_sync::{client, SyncEngine, VaultCode};
        let url = spawn_relay(temp_dir("e2e-data")).await;

        let va = temp_dir("e2e-va");
        let vb = temp_dir("e2e-vb");
        std::fs::write(va.join("plan.md"), "# Plan\n\nwritten on A\n").unwrap();
        let code = VaultCode::generate();

        let mut a = SyncEngine::open(&va, &code).unwrap();
        let mut b = SyncEngine::open(&vb, &code).unwrap();
        let mut wa = NoteWriter::new(&va, PermissionScope::ReadWrite);
        let mut wb = NoteWriter::new(&vb, PermissionScope::ReadWrite);
        wa.set_origin("sync");
        wb.set_origin("sync");

        // A pushes; B pulls — the FILE lands on B's disk, byte-identical.
        client::sync_pass(&mut a, &mut wa, &url).await.unwrap();
        let report = client::sync_pass(&mut b, &mut wb, &url).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(vb.join("plan.md")).unwrap(),
            "# Plan\n\nwritten on A\n"
        );

        // The sync batch is revertible via the existing revert (audited).
        let version_id = report.apply.version_id.expect("through NoteWriter");
        let audit = wb.read_audit(1).unwrap();
        assert_eq!(audit[0].origin.as_deref(), Some("sync"));
        wb.revert(&version_id).unwrap();
        assert!(!vb.join("plan.md").exists());

        // B edits after re-pulling; the edit flows back to A's disk.
        client::sync_pass(&mut b, &mut wb, &url).await.unwrap();
        std::fs::write(vb.join("plan.md"), "# Plan\n\nwritten on A\nplus B\n").unwrap();
        client::sync_pass(&mut b, &mut wb, &url).await.unwrap();
        client::sync_pass(&mut a, &mut wa, &url).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(va.join("plan.md")).unwrap(),
            "# Plan\n\nwritten on A\nplus B\n"
        );
    }

    // ---- live sessions (collab-m1) -------------------------------------
    // These drive the persistent `run_live` loop end-to-end through a real
    // in-process relay: sub-second convergence, the fold-before-apply merge
    // invariant, reconnect backoff, and clean shutdown on sender drop.

    use std::sync::mpsc as std_mpsc;
    use tacitus_core::vault::{NoteWriter as LiveWriter, PermissionScope as LiveScope};
    use tacitus_sync::live::{run_live, LiveCmd, LiveConfig, LiveEvent};
    use tacitus_sync::{
        Peer, PresenceState, SyncEngine as LiveEngine, SyncError, VaultCode as LiveCode,
    };

    /// Poll until `cond` holds — deadline-capped, never a fixed sleep.
    async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while !cond() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for: {what}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    struct Live {
        nudge: tokio::sync::mpsc::Sender<LiveCmd>,
        events: std_mpsc::Receiver<LiveEvent>,
        task: tokio::task::JoinHandle<Result<(), SyncError>>,
    }

    fn spawn_live(
        vault: &std::path::Path,
        code: &LiveCode,
        url: &str,
        tweak: impl FnOnce(&mut LiveConfig),
    ) -> Live {
        let mut engine = LiveEngine::open(vault, code).unwrap();
        let mut writer = LiveWriter::new(vault, LiveScope::ReadWrite);
        writer.set_origin("sync");
        let mut config = LiveConfig::new(url);
        // Test-friendly timings; individual tests override further.
        config.apply_debounce = Duration::from_millis(50);
        tweak(&mut config);
        let (ntx, mut nrx) = tokio::sync::mpsc::channel(4);
        let (etx, erx) = std_mpsc::channel();
        let task = tokio::spawn(async move {
            run_live(&mut engine, &mut writer, &config, &mut nrx, |ev| {
                let _ = etx.send(ev);
            })
            .await
        });
        Live {
            nudge: ntx,
            events: erx,
            task,
        }
    }

    #[tokio::test]
    async fn live_sessions_converge_subsecond_via_nudge() {
        let url = spawn_relay(temp_dir("live-data")).await;
        let va = temp_dir("live-va");
        let vb = temp_dir("live-vb");
        let code = LiveCode::generate();

        let a = spawn_live(&va, &code, &url, |_| {});
        let b = spawn_live(&vb, &code, &url, |_| {});

        // Type on A, nudge — B's DISK converges with no pass cadence at all.
        std::fs::write(va.join("note.md"), "typed on A\n").unwrap();
        a.nudge.send(LiveCmd::Nudge).await.unwrap();
        wait_until("note lands on B", || vb.join("note.md").exists()).await;
        assert_eq!(
            std::fs::read_to_string(vb.join("note.md")).unwrap(),
            "typed on A\n"
        );

        // Dropping every sender ends both sessions cleanly.
        drop(a.nudge);
        drop(b.nudge);
        a.task.await.unwrap().unwrap();
        b.task.await.unwrap().unwrap();

        // B's event order: Connected first, CaughtUp before the Applied.
        let evs: Vec<LiveEvent> = b.events.try_iter().collect();
        assert!(
            matches!(evs.first(), Some(LiveEvent::Connected { .. })),
            "first event is Connected, got {evs:?}"
        );
        let caught = evs
            .iter()
            .position(|e| matches!(e, LiveEvent::CaughtUp))
            .expect("CaughtUp fired");
        let applied = evs
            .iter()
            .position(|e| matches!(e, LiveEvent::Applied(_)))
            .expect("Applied fired");
        assert!(caught < applied, "CaughtUp precedes Applied: {evs:?}");

        // A pushed, but its own echo must never come back as an Applied.
        let evs: Vec<LiveEvent> = a.events.try_iter().collect();
        assert!(evs
            .iter()
            .any(|e| matches!(e, LiveEvent::Pushed { count } if *count > 0)));
        assert!(
            !evs.iter().any(|e| matches!(e, LiveEvent::Applied(_))),
            "own echo applied: {evs:?}"
        );
    }

    #[tokio::test]
    async fn live_fold_before_apply_merges_concurrent_disk_edit() {
        let url = spawn_relay(temp_dir("fold-data")).await;
        let va = temp_dir("fold-va");
        let vb = temp_dir("fold-vb");
        let code = LiveCode::generate();

        // fold_guard ZERO pins the mechanism under test (fold on every
        // incoming Update); a long scan_interval rules the periodic scan out.
        let pin = |c: &mut LiveConfig| {
            c.fold_guard = Duration::ZERO;
            c.scan_interval = Duration::from_secs(120);
        };
        let a = spawn_live(&va, &code, &url, pin);
        let b = spawn_live(&vb, &code, &url, pin);

        // Converge on a shared baseline.
        std::fs::write(va.join("doc.md"), "baseline\n").unwrap();
        a.nudge.send(LiveCmd::Nudge).await.unwrap();
        wait_until("baseline lands on B", || {
            std::fs::read_to_string(vb.join("doc.md")).ok().as_deref() == Some("baseline\n")
        })
        .await;

        // B edits its DISK without any nudge; A pushes a concurrent edit.
        // The incoming Update must fold B's local line first, so the CRDT
        // merges both — instead of the remote apply skipping (or a later
        // stale fold erasing A's line).
        std::fs::write(vb.join("doc.md"), "baseline\nB's line\n").unwrap();
        std::fs::write(va.join("doc.md"), "A's line\nbaseline\n").unwrap();
        a.nudge.send(LiveCmd::Nudge).await.unwrap();

        wait_until("both edits merge on B", || {
            let doc = std::fs::read_to_string(vb.join("doc.md")).unwrap_or_default();
            doc.contains("A's line") && doc.contains("B's line")
        })
        .await;
        // B's fold pushed its line too — A converges to the same bytes.
        wait_until("replicas converge", || {
            std::fs::read_to_string(va.join("doc.md")).ok()
                == std::fs::read_to_string(vb.join("doc.md")).ok()
        })
        .await;

        drop(a.nudge);
        drop(b.nudge);
        a.task.await.unwrap().unwrap();
        b.task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn live_session_reconnects_with_backoff() {
        // Reserve an address with no listener yet: the session must keep
        // retrying with growing backoff instead of dying.
        let reserved = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = reserved.local_addr().unwrap();
        drop(reserved);
        let url = format!("ws://{addr}/ws");

        let va = temp_dir("reconn-va");
        std::fs::write(va.join("note.md"), "arrives late\n").unwrap();
        let code = LiveCode::generate();
        let a = spawn_live(&va, &code, &url, |c| {
            c.backoff_min = Duration::from_millis(50);
            c.backoff_max = Duration::from_millis(200);
        });

        let mut evs: Vec<LiveEvent> = Vec::new();
        wait_until("two Disconnected events", || {
            evs.extend(a.events.try_iter());
            evs.iter()
                .filter(|e| matches!(e, LiveEvent::Disconnected { .. }))
                .count()
                >= 2
        })
        .await;
        let retries: Vec<Duration> = evs
            .iter()
            .filter_map(|e| match e {
                LiveEvent::Disconnected { retry_in, .. } => Some(*retry_in),
                _ => None,
            })
            .collect();
        assert!(
            retries.windows(2).all(|w| w[0] <= w[1]),
            "backoff never shrinks: {retries:?}"
        );
        assert!(retries[1] > retries[0], "backoff grows: {retries:?}");

        // The relay comes up on that very address — the session recovers and
        // a second device receives the note pushed before any connection.
        let state = Arc::new(RelayState::new(temp_dir("reconn-data")));
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app(state)).await.unwrap();
        });

        let vb = temp_dir("reconn-vb");
        let b = spawn_live(&vb, &code, &url, |_| {});
        wait_until("late note lands on B", || vb.join("note.md").exists()).await;

        wait_until("A reconnected", || {
            evs.extend(a.events.try_iter());
            evs.iter().any(|e| matches!(e, LiveEvent::Connected { .. }))
        })
        .await;

        drop(a.nudge);
        drop(b.nudge);
        a.task.await.unwrap().unwrap();
        b.task.await.unwrap().unwrap();
    }

    fn viewing(note: &str, editing: bool) -> PresenceState {
        PresenceState {
            note_id: Some(note.into()),
            editing,
        }
    }

    /// Drain a Live's events, keeping the newest Peers snapshot.
    fn latest_peers(live: &Live, current: &mut Vec<Peer>) {
        for event in live.events.try_iter() {
            if let LiveEvent::Peers(peers) = event {
                *current = peers;
            }
        }
    }

    #[tokio::test]
    async fn live_presence_discovers_tracks_and_says_goodbye() {
        let url = spawn_relay(temp_dir("pres-live")).await;
        let va = temp_dir("pres-live-a");
        let vb = temp_dir("pres-live-b");
        let code = LiveCode::generate();
        // Heartbeats deliberately SLOW: discovery must come from the
        // hello-reply, goodbye from the explicit gone — never from cadence.
        let slow_beat = |c: &mut LiveConfig| {
            c.presence_interval = Duration::from_secs(10);
            c.presence_ttl = Duration::from_secs(60);
            c.presence_debounce = Duration::from_millis(20);
        };

        let a = spawn_live(&va, &code, &url, slow_beat);
        a.nudge
            .send(LiveCmd::Presence(viewing("plan", true)))
            .await
            .unwrap();

        // B joins later — the announce/hello-reply handshake makes A
        // visible fast, long before any heartbeat.
        let b = spawn_live(&vb, &code, &url, slow_beat);
        let mut peers: Vec<Peer> = Vec::new();
        wait_until("B discovers A editing plan", || {
            latest_peers(&b, &mut peers);
            peers.len() == 1 && peers[0].note_id.as_deref() == Some("plan") && peers[0].editing
        })
        .await;

        // A burst of note switches converges to the last one.
        for i in 0..5 {
            a.nudge
                .send(LiveCmd::Presence(viewing(&format!("n{i}"), false)))
                .await
                .unwrap();
        }
        wait_until("B converges to the last state", || {
            latest_peers(&b, &mut peers);
            peers.len() == 1 && peers[0].note_id.as_deref() == Some("n4") && !peers[0].editing
        })
        .await;

        // Clean shutdown sends gone — B empties long before the 60s TTL.
        drop(a.nudge);
        a.task.await.unwrap().unwrap();
        wait_until("A's goodbye empties B's peers", || {
            latest_peers(&b, &mut peers);
            peers.is_empty()
        })
        .await;

        drop(b.nudge);
        b.task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn crashed_peer_expires_via_ttl_sweep() {
        let url = spawn_relay(temp_dir("pres-ttl")).await;
        let va = temp_dir("pres-ttl-a");
        let vb = temp_dir("pres-ttl-b");
        let code = LiveCode::generate();
        let fast = |c: &mut LiveConfig| {
            c.presence_interval = Duration::from_millis(100);
            c.presence_ttl = Duration::from_millis(400);
            c.presence_debounce = Duration::from_millis(10);
        };

        let a = spawn_live(&va, &code, &url, fast);
        let b = spawn_live(&vb, &code, &url, fast);
        a.nudge
            .send(LiveCmd::Presence(viewing("x", false)))
            .await
            .unwrap();
        let mut peers: Vec<Peer> = Vec::new();
        wait_until("B sees A", || {
            latest_peers(&b, &mut peers);
            peers.len() == 1
        })
        .await;

        // Hard crash: no goodbye ever arrives — only the TTL can clean up.
        a.task.abort();
        let _ = a.task.await;
        wait_until("TTL sweep drops the silent peer", || {
            latest_peers(&b, &mut peers);
            peers.is_empty()
        })
        .await;

        drop(b.nudge);
        b.task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn old_relay_receives_zero_presence_frames() {
        // A hand-rolled 0.19-style relay: Welcome WITHOUT caps. The client
        // must never send a presence frame at it, no matter what the host
        // asks for.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (seen_tx, seen_rx) = std_mpsc::channel::<String>();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut sink, mut frames) = ws.split();
            while let Some(Ok(frame)) = frames.next().await {
                if let WsMessage::Text(text) = frame {
                    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
                    let tag = v["t"].as_str().unwrap_or("?").to_string();
                    let is_hello = tag == "hello";
                    let _ = seen_tx.send(tag);
                    if is_hello {
                        let welcome = r#"{"t":"welcome","latest_seq":0}"#;
                        sink.send(WsMessage::Text(welcome.to_string().into()))
                            .await
                            .unwrap();
                    }
                }
            }
        });

        let va = temp_dir("old-relay-va");
        std::fs::write(va.join("note.md"), "content\n").unwrap();
        let code = LiveCode::generate();
        let a = spawn_live(&va, &code, &format!("ws://{addr}/ws"), |c| {
            // Aggressive timings: if presence WERE wrongly enabled, frames
            // would show up within the assertion window many times over.
            c.presence_interval = Duration::from_millis(50);
            c.presence_debounce = Duration::from_millis(10);
        });
        a.nudge
            .send(LiveCmd::Presence(viewing("secret", true)))
            .await
            .unwrap();

        let mut seen: Vec<String> = Vec::new();
        wait_until("stub saw hello + the initial push", || {
            seen.extend(seen_rx.try_iter());
            seen.contains(&"hello".to_string()) && seen.contains(&"push".to_string())
        })
        .await;
        // Negative window: ~6 would-be heartbeat intervals of silence.
        let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
        while tokio::time::Instant::now() < deadline {
            seen.extend(seen_rx.try_iter());
            assert!(
                !seen.iter().any(|t| t == "presence"),
                "presence leaked to an old relay: {seen:?}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }

        drop(a.nudge);
        a.task.await.unwrap().unwrap();
    }

    // ---- co-editing rooms (collab-m3) ----------------------------------
    // The tests play the desktop frontend with a raw yrs doc: RoomState
    // bootstraps it, local edits become v2 diffs sent as LiveCmd, remote
    // LiveEvents apply back into it.

    /// Stand-in for the frontend's Y.Doc (byte offsets, like the engine's).
    struct FrontDoc {
        doc: yrs::Doc,
    }

    impl FrontDoc {
        fn new(state: &[u8]) -> Self {
            use yrs::updates::decoder::Decode;
            use yrs::Transact;
            let doc = yrs::Doc::with_options(yrs::Options {
                offset_kind: yrs::OffsetKind::Bytes,
                ..Default::default()
            });
            let update = yrs::Update::decode_v2(state).expect("valid room state");
            doc.transact_mut().apply_update(update).unwrap();
            Self { doc }
        }

        fn text(&self) -> String {
            use yrs::{GetString, Transact};
            let text = self.doc.get_or_insert_text("c");
            let txn = self.doc.transact();
            text.get_string(&txn)
        }

        /// Insert at byte offset; returns the v2 diff to send upstream.
        fn insert(&mut self, index: u32, chunk: &str) -> Vec<u8> {
            use yrs::{ReadTxn, Text, Transact};
            let text = self.doc.get_or_insert_text("c");
            let mut txn = self.doc.transact_mut();
            let before = txn.state_vector();
            text.insert(&mut txn, index, chunk);
            txn.encode_state_as_update_v2(&before)
        }

        fn apply(&mut self, update: &[u8]) {
            use yrs::updates::decoder::Decode;
            use yrs::Transact;
            let parsed = yrs::Update::decode_v2(update).expect("valid update");
            self.doc.transact_mut().apply_update(parsed).unwrap();
        }
    }

    /// Drain a Live's events into room-shaped state.
    #[derive(Default)]
    struct RoomFeed {
        states: Vec<(String, Vec<u8>)>,
        updates: Vec<(String, Vec<u8>)>,
        awareness: Vec<(String, Vec<u8>)>,
    }

    fn drain_room(live: &Live, feed: &mut RoomFeed) {
        for event in live.events.try_iter() {
            match event {
                LiveEvent::RoomState { note_id, state } => feed.states.push((note_id, state)),
                LiveEvent::CoeditUpdate { note_id, update } => feed.updates.push((note_id, update)),
                LiveEvent::CoeditAwareness { note_id, data } => {
                    feed.awareness.push((note_id, data))
                }
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn coedit_rooms_exchange_keystrokes_and_materialize_everywhere() {
        let url = spawn_relay(temp_dir("coedit-data")).await;
        let va = temp_dir("coedit-va");
        let vb = temp_dir("coedit-vb");
        let vc = temp_dir("coedit-vc");
        std::fs::write(va.join("doc.md"), "base\n").unwrap();
        let code = LiveCode::generate();
        let fast = |c: &mut LiveConfig| {
            c.apply_debounce = Duration::from_millis(40);
            c.coedit_durable_debounce = Duration::from_millis(150);
        };
        let a = spawn_live(&va, &code, &url, fast);
        let b = spawn_live(&vb, &code, &url, fast);
        // C: headless live device, never opens a room.
        let c = spawn_live(&vc, &code, &url, fast);

        wait_until("baseline lands on B", || vb.join("doc.md").exists()).await;

        a.nudge
            .send(LiveCmd::RoomEnter {
                note_id: "doc".into(),
            })
            .await
            .unwrap();
        b.nudge
            .send(LiveCmd::RoomEnter {
                note_id: "doc".into(),
            })
            .await
            .unwrap();

        let (mut feed_a, mut feed_b) = (RoomFeed::default(), RoomFeed::default());
        wait_until("both get RoomState", || {
            drain_room(&a, &mut feed_a);
            drain_room(&b, &mut feed_b);
            !feed_a.states.is_empty() && !feed_b.states.is_empty()
        })
        .await;
        let mut front_a = FrontDoc::new(&feed_a.states[0].1);
        let mut front_b = FrontDoc::new(&feed_b.states[0].1);
        assert_eq!(front_a.text(), "base\n");
        assert_eq!(front_b.text(), "base\n");

        // A types at the top, B types at the bottom — concurrently.
        let update_a = front_a.insert(0, "A> ");
        let update_b = front_b.insert(5, "B!\n");
        a.nudge
            .send(LiveCmd::CoeditUpdate {
                note_id: "doc".into(),
                update: update_a,
            })
            .await
            .unwrap();
        b.nudge
            .send(LiveCmd::CoeditUpdate {
                note_id: "doc".into(),
                update: update_b,
            })
            .await
            .unwrap();

        wait_until("frontends converge with both edits", || {
            drain_room(&a, &mut feed_a);
            drain_room(&b, &mut feed_b);
            for (note, update) in feed_a.updates.drain(..) {
                assert_eq!(note, "doc");
                front_a.apply(&update);
            }
            for (note, update) in feed_b.updates.drain(..) {
                assert_eq!(note, "doc");
                front_b.apply(&update);
            }
            let (ta, tb) = (front_a.text(), front_b.text());
            ta == tb && ta.contains("A> ") && ta.contains("B!\n")
        })
        .await;

        // The backend is the only disk writer — both disks materialize…
        wait_until("disks materialize the merge", || {
            let da = std::fs::read_to_string(va.join("doc.md")).unwrap_or_default();
            let db = std::fs::read_to_string(vb.join("doc.md")).unwrap_or_default();
            da == db && da.contains("A> ") && da.contains("B!\n")
        })
        .await;
        // …and so does the ROOMLESS headless device (from the ephemeral
        // frames it applies + the debounced materialize).
        wait_until("headless C converges too", || {
            std::fs::read_to_string(vc.join("doc.md"))
                .map(|t| t.contains("A> ") && t.contains("B!\n"))
                .unwrap_or(false)
        })
        .await;

        // Awareness reaches the peer's room, never the roomless device.
        a.nudge
            .send(LiveCmd::CoeditAwareness {
                note_id: "doc".into(),
                data: vec![1, 2, 3],
            })
            .await
            .unwrap();
        wait_until("B sees A's awareness", || {
            drain_room(&b, &mut feed_b);
            feed_b
                .awareness
                .iter()
                .any(|(note, data)| note == "doc" && data.as_slice() == [1, 2, 3])
        })
        .await;
        let mut feed_c = RoomFeed::default();
        drain_room(&c, &mut feed_c);
        assert!(
            feed_c.awareness.is_empty() && feed_c.states.is_empty(),
            "roomless devices get no room events"
        );

        drop(a.nudge);
        drop(b.nudge);
        drop(c.nudge);
        a.task.await.unwrap().unwrap();
        b.task.await.unwrap().unwrap();
        c.task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn room_edits_land_durably_and_oversize_falls_back_to_the_log() {
        let url = spawn_relay(temp_dir("durable-data")).await;
        let va = temp_dir("durable-va");
        std::fs::write(va.join("doc.md"), "start\n").unwrap();
        let code = LiveCode::generate();
        let a = spawn_live(&va, &code, &url, |c| {
            c.apply_debounce = Duration::from_millis(40);
            c.coedit_durable_debounce = Duration::from_millis(120);
        });
        a.nudge
            .send(LiveCmd::RoomEnter {
                note_id: "doc".into(),
            })
            .await
            .unwrap();
        let mut feed = RoomFeed::default();
        wait_until("room state", || {
            drain_room(&a, &mut feed);
            !feed.states.is_empty()
        })
        .await;
        let mut front = FrontDoc::new(&feed.states[0].1);

        // A normal keystroke batch…
        let small = front.insert(0, "typed ");
        a.nudge
            .send(LiveCmd::CoeditUpdate {
                note_id: "doc".into(),
                update: small,
            })
            .await
            .unwrap();
        // …and a paste far past the ephemeral frame budget (~4.4KB raw).
        let huge_chunk = "x".repeat(7 * 1024);
        let huge = front.insert(6, &huge_chunk);
        assert!(huge.len() > 6 * 1024);
        a.nudge
            .send(LiveCmd::CoeditUpdate {
                note_id: "doc".into(),
                update: huge,
            })
            .await
            .unwrap();

        // A pass-based device (no rooms, no ephemeral frames — run_once
        // reads ONLY the durable log) must converge on everything.
        let vd = temp_dir("durable-vd");
        let mut engine = LiveEngine::open(&vd, &code).unwrap();
        let mut writer = LiveWriter::new(&vd, LiveScope::ReadWrite);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            tacitus_sync::client::sync_pass(&mut engine, &mut writer, &url)
                .await
                .unwrap();
            let text = std::fs::read_to_string(vd.join("doc.md")).unwrap_or_default();
            if text.starts_with("typed x") && text.contains(&huge_chunk) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "durable tier never delivered; got: {:?}…",
                text.chars().take(40).collect::<String>()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        drop(a.nudge);
        a.task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn external_fold_forwards_to_the_room_and_offline_enter_reemits() {
        // Reserve an addr; the relay starts LATER — the room opens offline.
        let reserved = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = reserved.local_addr().unwrap();
        drop(reserved);
        let url = format!("ws://{addr}/ws");

        let va = temp_dir("fold-room-va");
        std::fs::write(va.join("doc.md"), "offline base\n").unwrap();
        let code = LiveCode::generate();
        let a = spawn_live(&va, &code, &url, |c| {
            c.backoff_min = Duration::from_millis(50);
            c.backoff_max = Duration::from_millis(200);
            c.apply_debounce = Duration::from_millis(40);
            c.coedit_durable_debounce = Duration::from_millis(120);
        });

        // Entering a room while disconnected still yields a usable state.
        let mut feed = RoomFeed::default();
        wait_until("offline RoomState", || {
            a.nudge
                .try_send(LiveCmd::RoomEnter {
                    note_id: "doc".into(),
                })
                .ok();
            drain_room(&a, &mut feed);
            !feed.states.is_empty()
        })
        .await;
        assert_eq!(FrontDoc::new(&feed.states[0].1).text(), "offline base\n");
        let states_before_connect = feed.states.len();

        // The relay appears → reconnect → the room resyncs its frontend.
        let state = Arc::new(RelayState::new(temp_dir("fold-room-data")));
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app(state)).await.unwrap();
        });
        wait_until("RoomState re-emitted after connect", || {
            drain_room(&a, &mut feed);
            feed.states.len() > states_before_connect
        })
        .await;

        // An EXTERNAL writer (agent/plugin/editor) hits the disk + a nudge:
        // the fold's exact bytes reach the room frontend.
        let mut front = FrontDoc::new(&feed.states.last().unwrap().1);
        std::fs::write(va.join("doc.md"), "offline base\nagent line\n").unwrap();
        a.nudge.send(LiveCmd::Nudge).await.unwrap();
        wait_until("fold forwarded to the room", || {
            drain_room(&a, &mut feed);
            for (_, update) in feed.updates.drain(..) {
                front.apply(&update);
            }
            front.text() == "offline base\nagent line\n"
        })
        .await;

        drop(a.nudge);
        a.task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn old_relay_gets_no_coedit_frames_but_durable_pushes_flow() {
        // 0.19-style stub: welcome WITHOUT caps → the fast tier stays off,
        // yet room edits still reach the log through durable pushes.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (seen_tx, seen_rx) = std_mpsc::channel::<String>();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let (mut sink, mut frames) = ws.split();
            while let Some(Ok(frame)) = frames.next().await {
                if let WsMessage::Text(text) = frame {
                    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
                    let tag = v["t"].as_str().unwrap_or("?").to_string();
                    let is_hello = tag == "hello";
                    let _ = seen_tx.send(tag);
                    if is_hello {
                        let welcome = r#"{"t":"welcome","latest_seq":0}"#;
                        sink.send(WsMessage::Text(welcome.to_string().into()))
                            .await
                            .unwrap();
                    }
                }
            }
        });

        let va = temp_dir("old-coedit-va");
        std::fs::write(va.join("doc.md"), "base\n").unwrap();
        let code = LiveCode::generate();
        let a = spawn_live(&va, &code, &format!("ws://{addr}/ws"), |c| {
            c.coedit_durable_debounce = Duration::from_millis(80);
        });
        a.nudge
            .send(LiveCmd::RoomEnter {
                note_id: "doc".into(),
            })
            .await
            .unwrap();
        let mut feed = RoomFeed::default();
        wait_until("room state against the old relay", || {
            drain_room(&a, &mut feed);
            !feed.states.is_empty()
        })
        .await;
        let mut front = FrontDoc::new(&feed.states[0].1);
        let update = front.insert(0, "quietly ");
        a.nudge
            .send(LiveCmd::CoeditUpdate {
                note_id: "doc".into(),
                update,
            })
            .await
            .unwrap();

        // The durable flush produces a second push; presence/coedit frames
        // never appear.
        let mut seen: Vec<String> = Vec::new();
        wait_until("two durable pushes seen by the stub", || {
            seen.extend(seen_rx.try_iter());
            seen.iter().filter(|t| *t == "push").count() >= 2
        })
        .await;
        assert!(
            !seen.iter().any(|t| t == "presence"),
            "an extension frame leaked to an old relay: {seen:?}"
        );

        drop(a.nudge);
        a.task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn sync_pass_materializes_crash_leftovers() {
        let url = spawn_relay(temp_dir("leftover-data")).await;
        let va = temp_dir("leftover-va");
        let vb = temp_dir("leftover-vb");
        std::fs::write(va.join("note.md"), "crash survivor\n").unwrap();
        let code = LiveCode::generate();
        let mut a = LiveEngine::open(&va, &code).unwrap();
        tacitus_sync::client::run_once(&mut a, &url).await.unwrap();

        // B's process "crashes" after receipt: run_once advances the cursor
        // but nothing materializes.
        {
            let mut b = LiveEngine::open(&vb, &code).unwrap();
            tacitus_sync::client::run_once(&mut b, &url).await.unwrap();
            assert!(!vb.join("note.md").exists(), "receipt only, no apply yet");
        }

        // The next pass must write the file even though the relay will never
        // redeliver below the persisted cursor.
        let mut b = LiveEngine::open(&vb, &code).unwrap();
        let mut writer = LiveWriter::new(&vb, LiveScope::ReadWrite);
        tacitus_sync::client::sync_pass(&mut b, &mut writer, &url)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(vb.join("note.md")).unwrap(),
            "crash survivor\n"
        );
    }

    #[tokio::test]
    async fn dropping_nudge_sender_flushes_pending_applies() {
        let url = spawn_relay(temp_dir("flush-data")).await;
        let va = temp_dir("flush-va");
        let vb = temp_dir("flush-vb");
        std::fs::write(va.join("note.md"), "must not be lost\n").unwrap();
        let code = LiveCode::generate();

        // Seed the relay log with a plain one-shot pass from A.
        let mut a = LiveEngine::open(&va, &code).unwrap();
        tacitus_sync::client::run_once(&mut a, &url).await.unwrap();

        // B receives the update but the apply debounce is far away — the
        // clean-shutdown path must flush it to disk before returning.
        let b = spawn_live(&vb, &code, &url, |c| {
            c.apply_debounce = Duration::from_secs(3600);
        });
        wait_until("B caught up", || {
            b.events
                .try_iter()
                .any(|e| matches!(e, LiveEvent::CaughtUp))
        })
        .await;

        drop(b.nudge);
        b.task.await.unwrap().unwrap();
        assert_eq!(
            std::fs::read_to_string(vb.join("note.md")).unwrap(),
            "must not be lost\n"
        );
    }

    #[tokio::test]
    async fn presence_fans_out_only_to_capable_and_never_touches_the_log() {
        let url = spawn_relay(temp_dir("pres-fan")).await;
        let vault = "d".repeat(32);

        let mut a = connect(&url).await;
        send_json(&mut a, hello_caps(&vault, "tok", 0)).await;
        let welcome = recv_json(&mut a).await;
        assert_eq!(welcome["t"], "welcome");
        assert!(
            welcome["caps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "presence"),
            "relay advertises the capability: {welcome}"
        );
        let mut b = connect(&url).await;
        send_json(&mut b, hello_caps(&vault, "tok", 0)).await;
        recv_json(&mut b).await;
        // C is a 0.19 client: no caps field at all.
        let mut c = connect(&url).await;
        send_json(&mut c, hello(&vault, "tok", 0)).await;
        recv_json(&mut c).await;

        // A: one presence frame, then one push.
        send_json(&mut a, presence_frame(b"ciphertext-here")).await;
        send_json(&mut a, push(b"real-update")).await;

        // B (capable) sees both the presence frame and the update; their
        // relative order is a select race between the two broadcast
        // channels. The update carries seq 1 — presence consumed no
        // sequence number.
        let one = recv_json(&mut b).await;
        let two = recv_json(&mut b).await;
        let mut tags = [one["t"].as_str().unwrap(), two["t"].as_str().unwrap()];
        tags.sort_unstable();
        assert_eq!(tags, ["presence", "update"]);
        let update = if one["t"] == "update" { &one } else { &two };
        assert_eq!(update["seq"], 1);

        // C (old): the FIRST frame it ever receives is the update — the
        // unknown tag never reaches it.
        let frame = recv_json(&mut c).await;
        assert_eq!(frame["t"], "update");
        assert_eq!(frame["seq"], 1);

        // A gets its own presence echo, the push ack, and its own update
        // echo. The presence echo and the update echo ride two DIFFERENT
        // broadcast channels, so every interleaving is legal (the only
        // fixed point: the inline ack precedes the update echo). Never a
        // presence ACK, though.
        let frames = [
            recv_json(&mut a).await,
            recv_json(&mut a).await,
            recv_json(&mut a).await,
        ];
        let mut tags: Vec<&str> = frames.iter().map(|f| f["t"].as_str().unwrap()).collect();
        tags.sort_unstable();
        assert_eq!(tags, ["ack", "presence", "update"]);
        let ack = frames.iter().find(|f| f["t"] == "ack").unwrap();
        assert_eq!(ack["seq"], 1, "the only ack is the push's");

        // A late joiner replays a log that contains ONLY the push.
        let mut late = connect(&url).await;
        send_json(&mut late, hello(&vault, "tok", 0)).await;
        let welcome = recv_json(&mut late).await;
        assert_eq!(welcome["latest_seq"], 1, "presence never entered the log");
        assert_eq!(recv_json(&mut late).await["t"], "update");
    }

    #[tokio::test]
    async fn presence_before_hello_is_rejected() {
        let url = spawn_relay(temp_dir("pres-early")).await;
        let mut ws = connect(&url).await;
        send_json(&mut ws, presence_frame(b"x")).await;
        let err = recv_json(&mut ws).await;
        assert_eq!(err["t"], "err");
        assert_eq!(err["code"], "protocol");
    }

    #[tokio::test]
    async fn oversized_presence_is_dropped_but_the_connection_survives() {
        let url = spawn_relay(temp_dir("pres-big")).await;
        let vault = "e".repeat(32);
        let mut ws = connect(&url).await;
        send_json(&mut ws, hello_caps(&vault, "tok", 0)).await;
        recv_json(&mut ws).await; // welcome

        let big = vec![0u8; 9 * 1024];
        send_json(&mut ws, presence_frame(&big)).await;
        let err = recv_json(&mut ws).await;
        assert_eq!(err["t"], "err");
        assert_eq!(err["code"], "protocol");

        // Same connection still syncs fine.
        send_json(&mut ws, push(b"still alive")).await;
        // (own presence echo was never sent — the blob was dropped)
        let frame = recv_json(&mut ws).await;
        assert_eq!(frame["t"], "ack");
    }

    #[tokio::test]
    async fn rejects_malformed_vault_id_over_ws() {
        let url = spawn_relay(temp_dir("badid")).await;
        let mut ws = connect(&url).await;
        send_json(&mut ws, hello("../escape", "tok", 0)).await;
        let err = recv_json(&mut ws).await;
        assert_eq!(err["t"], "err");
        assert_eq!(err["code"], "bad_vault_id");
    }

    // ---- compaction (collab-m4) ----------------------------------------

    #[tokio::test]
    async fn live_session_compacts_and_a_fresh_device_converges_from_snapshot() {
        use tacitus_sync::client;
        let data = temp_dir("autocompact-data");
        let url = spawn_relay(data.clone()).await;
        let va = temp_dir("autocompact-va");
        let vc = temp_dir("autocompact-vc");
        let code = LiveCode::generate();

        // Seed real history: several log entries across passes.
        let (vault_id, token) = {
            let mut a = LiveEngine::open(&va, &code).unwrap();
            std::fs::write(va.join("one.md"), "first\n").unwrap();
            client::run_once(&mut a, &url).await.unwrap();
            std::fs::write(va.join("two.md"), "second\n").unwrap();
            client::run_once(&mut a, &url).await.unwrap();
            std::fs::write(va.join("one.md"), "first, edited\n").unwrap();
            client::run_once(&mut a, &url).await.unwrap();
            let tacitus_sync::ClientMsg::Hello {
                vault_id, token, ..
            } = a.hello()
            else {
                panic!("hello is hello");
            };
            (vault_id, token)
        };
        let before = std::fs::read_to_string(data.join(&vault_id).join("log.jsonl"))
            .unwrap()
            .lines()
            .count();
        assert!(before >= 3);

        // A live session with a tiny threshold compacts right after catch-up.
        let a = spawn_live(&va, &code, &url, |c| c.compact_threshold = 1);
        wait_until("snapshot lands on the relay", || {
            data.join(&vault_id).join("snapshot.json").exists()
        })
        .await;
        wait_until("log tail truncated on disk", || {
            std::fs::read_to_string(data.join(&vault_id).join("log.jsonl"))
                .map(|raw| raw.lines().count() < before)
                .unwrap_or(false)
        })
        .await;

        // A fresh device converges from snapshot + tail alone.
        let mut c = LiveEngine::open(&vc, &code).unwrap();
        client::run_once(&mut c, &url).await.unwrap();
        assert_eq!(
            c.materialize("n:one").unwrap().as_deref(),
            Some("first, edited\n")
        );
        assert_eq!(c.materialize("n:two").unwrap().as_deref(), Some("second\n"));

        // An 0.21-style client (no compact cap) below the snapshot gets the
        // honest fatal error instead of a silent gap.
        let mut old = connect(&url).await;
        send_json(&mut old, hello(&vault_id, &token, 0)).await;
        let err = recv_json(&mut old).await;
        assert_eq!(err["t"], "err");
        assert_eq!(err["code"], "compacted");

        drop(a.nudge);
        a.task.await.unwrap().unwrap();
        let compacted = a
            .events
            .try_iter()
            .any(|e| matches!(e, LiveEvent::Compacted { .. }));
        assert!(compacted, "the session surfaced the Compacted event");
    }

    // ---- chunked compaction E2E (collab-m5) ----------------------------

    #[tokio::test]
    async fn live_session_chunks_snapshot_and_fresh_device_converges_from_parts() {
        use tacitus_sync::client;
        let data = temp_dir("chunklive-data");
        let url = spawn_relay(data.clone()).await;
        let va = temp_dir("chunklive-va");
        let vc = temp_dir("chunklive-vc");
        let code = LiveCode::generate();

        // Seed several docs so a tiny part budget forces a multi-part offer.
        let (vault_id, token) = {
            let mut a = LiveEngine::open(&va, &code).unwrap();
            std::fs::write(va.join("one.md"), "first\n").unwrap();
            std::fs::write(va.join("two.md"), "second\n").unwrap();
            client::run_once(&mut a, &url).await.unwrap();
            std::fs::write(va.join("three.md"), "third\n").unwrap();
            client::run_once(&mut a, &url).await.unwrap();
            let tacitus_sync::ClientMsg::Hello {
                vault_id, token, ..
            } = a.hello()
            else {
                panic!("hello is hello");
            };
            (vault_id, token)
        };
        let vdir = data.join(&vault_id);
        let before = std::fs::read_to_string(vdir.join("log.jsonl"))
            .unwrap()
            .lines()
            .count();

        let a = spawn_live(&va, &code, &url, |c| {
            c.compact_threshold = 1;
            c.snapshot_part_budget = 64;
        });
        wait_until("v2 snapshot (parts dir) lands on the relay", || {
            std::fs::read_dir(&vdir).is_ok_and(|entries| {
                entries.flatten().any(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with("snapshot.parts.")
                })
            })
        })
        .await;
        wait_until("log tail truncated on disk", || {
            std::fs::read_to_string(vdir.join("log.jsonl"))
                .map(|raw| raw.lines().count() < before)
                .unwrap_or(false)
        })
        .await;

        // A fresh device converges from the parts + tail alone, through the
        // real client download path.
        let mut c = LiveEngine::open(&vc, &code).unwrap();
        client::run_once(&mut c, &url).await.unwrap();
        assert_eq!(c.materialize("n:one").unwrap().as_deref(), Some("first\n"));
        assert_eq!(c.materialize("n:two").unwrap().as_deref(), Some("second\n"));
        assert_eq!(
            c.materialize("n:three").unwrap().as_deref(),
            Some("third\n")
        );

        // A 0.22-style client (compact, no compact2) below the multi-part
        // snapshot gets the honest fatal instead of frames it can't use.
        let mut old = connect(&url).await;
        send_json(&mut old, hello_compact(&vault_id, &token, 0)).await;
        let err = recv_json(&mut old).await;
        assert_eq!(err["t"], "err");
        assert_eq!(err["code"], "compacted");

        drop(a.nudge);
        a.task.await.unwrap().unwrap();
        let compacted = a
            .events
            .try_iter()
            .any(|e| matches!(e, LiveEvent::Compacted { .. }));
        assert!(compacted, "the session surfaced the Compacted event");
    }

    // ---- per-vault quota E2E (mon-m1) ----------------------------------

    #[tokio::test]
    async fn live_session_heals_quota_refusal_by_compacting_and_resending() {
        use tacitus_sync::client;
        let data = temp_dir("quotaheal-data");
        let url = spawn_relay(data.clone()).await;
        let va = temp_dir("quotaheal-va");
        let vc = temp_dir("quotaheal-vc");
        let code = LiveCode::generate();

        // Seed the SAME note across passes: the log holds every generation,
        // the sealed state only the last — snapshot < quota < log, so the
        // heal can actually work.
        let vault_id = {
            let mut a = LiveEngine::open(&va, &code).unwrap();
            for i in 0..6 {
                std::fs::write(va.join("story.md"), format!("draft {i}\n")).unwrap();
                client::run_once(&mut a, &url).await.unwrap();
            }
            a.vault_id().to_string()
        };
        let vdir = data.join(&vault_id);
        let log_len = std::fs::metadata(vdir.join("log.jsonl")).unwrap().len();
        let before = std::fs::read_to_string(vdir.join("log.jsonl"))
            .unwrap()
            .lines()
            .count();

        // Entitlement downgrade to exactly the current usage: the next push
        // is refused, picked up by the relay without any restart.
        std::fs::write(
            data.join("entitlements.json"),
            format!(r#"{{"version":1,"vaults":{{"{vault_id}":{{"quota_bytes":{log_len}}}}}}}"#),
        )
        .unwrap();

        // A new local note; the default 4 MiB threshold never fires here —
        // only the refusal-driven heal can compact this vault.
        std::fs::write(va.join("fresh.md"), "written over quota\n").unwrap();
        let a = spawn_live(&va, &code, &url, |_| {});

        wait_until("heal snapshot lands on the relay", || {
            vdir.join("snapshot.json").exists()
        })
        .await;
        wait_until("log truncated and the refused push re-sent", || {
            std::fs::read_to_string(vdir.join("log.jsonl"))
                .map(|raw| {
                    let n = raw.lines().count();
                    n >= 1 && n < before
                })
                .unwrap_or(false)
        })
        .await;

        // A fresh device sees the post-quota note — proof the refused push
        // survived the heal (snapshot + re-sent tail, through the real
        // download path).
        let mut c = LiveEngine::open(&vc, &code).unwrap();
        client::run_once(&mut c, &url).await.unwrap();
        assert_eq!(
            c.materialize("n:fresh").unwrap().as_deref(),
            Some("written over quota\n")
        );
        assert_eq!(
            c.materialize("n:story").unwrap().as_deref(),
            Some("draft 5\n")
        );

        // Clean shutdown — the session never died on the refusal — and the
        // events tell the story in order, with the welcome-time numbers.
        drop(a.nudge);
        a.task.await.unwrap().unwrap();
        let evs: Vec<LiveEvent> = a.events.try_iter().collect();
        let quota_at = evs
            .iter()
            .position(|e| matches!(e, LiveEvent::QuotaExceeded { .. }))
            .expect("QuotaExceeded fired");
        let compacted_at = evs
            .iter()
            .position(|e| matches!(e, LiveEvent::Compacted { .. }))
            .expect("Compacted fired");
        assert!(
            quota_at < compacted_at,
            "refusal precedes the heal: {evs:?}"
        );
        assert!(
            evs.iter().any(|e| matches!(
                e,
                LiveEvent::QuotaExceeded { quota_bytes, .. } if *quota_bytes == log_len
            )),
            "the event carries the effective quota from the welcome"
        );
    }

    #[tokio::test]
    async fn live_session_dies_honestly_when_snapshot_cannot_fit_quota() {
        use tacitus_sync::client;
        let data = temp_dir("quotafatal-data");
        let url = spawn_relay(data.clone()).await;
        let va = temp_dir("quotafatal-va");
        let code = LiveCode::generate();

        let vault_id = {
            let mut a = LiveEngine::open(&va, &code).unwrap();
            for i in 0..3 {
                std::fs::write(va.join("story.md"), format!("draft {i}\n")).unwrap();
                client::run_once(&mut a, &url).await.unwrap();
            }
            a.vault_id().to_string()
        };

        // 60 bytes: below any sealed snapshot, so the heal's offer is
        // refused (compact_too_large, advisory) and the heal concludes.
        std::fs::write(
            data.join("entitlements.json"),
            format!(r#"{{"version":1,"vaults":{{"{vault_id}":{{"quota_bytes":60}}}}}}"#),
        )
        .unwrap();

        std::fs::write(va.join("extra.md"), "attempt 0\n").unwrap();
        let a = spawn_live(&va, &code, &url, |_| {});

        // Each edit is a new push and each push a new refusal. The first one
        // starts the heal, the offer's refusal concludes it, and the first
        // refusal AFTER that is honest-fatal — keep editing until it lands.
        // (Deadline-capped poll, same cadence as wait_until.)
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let mut attempt = 0u32;
        let err = loop {
            if a.task.is_finished() {
                break a.task.await.unwrap().unwrap_err();
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for the fatal quota refusal"
            );
            attempt += 1;
            std::fs::write(va.join("extra.md"), format!("attempt {attempt}\n")).unwrap();
            let _ = a.nudge.send(LiveCmd::Nudge).await;
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        assert_eq!(err.code, "RELAY");
        assert!(err.reason.contains("quota_exceeded"), "{}", err.reason);

        // The session surfaced the condition before dying.
        let evs: Vec<LiveEvent> = a.events.try_iter().collect();
        assert!(
            evs.iter()
                .any(|e| matches!(e, LiveEvent::QuotaExceeded { .. })),
            "QuotaExceeded fired before the fatal: {evs:?}"
        );
    }

    #[tokio::test]
    async fn concurrent_live_sessions_survive_compaction_race_and_stay_synced() {
        use tacitus_sync::client;
        let data = temp_dir("race-data");
        let url = spawn_relay(data.clone()).await;
        let va = temp_dir("race-va");
        let vb = temp_dir("race-vb");
        let code = LiveCode::generate();

        // A fat backlog so both sessions are still catching up when they
        // connect — their offers then land close together and one loses.
        // (The losing order is a legal race; every assertion below holds
        // whichever session wins, or even if only one ends up offering.)
        let vault_id = {
            let mut a = LiveEngine::open(&va, &code).unwrap();
            for i in 0..20 {
                std::fs::write(va.join(format!("n{i}.md")), format!("note {i}\n")).unwrap();
                if i % 5 == 0 {
                    client::run_once(&mut a, &url).await.unwrap();
                }
            }
            client::run_once(&mut a, &url).await.unwrap();
            let mut b = LiveEngine::open(&vb, &code).unwrap();
            client::run_once(&mut b, &url).await.unwrap();
            a.vault_id().to_string()
        };

        let a = spawn_live(&va, &code, &url, |c| c.compact_threshold = 1);
        let b = spawn_live(&vb, &code, &url, |c| c.compact_threshold = 1);
        wait_until("one offer wins and the snapshot lands", || {
            data.join(&vault_id).join("snapshot.json").exists()
        })
        .await;

        // Both sessions must have survived the race: an edit still flows
        // A → B through both live loops.
        std::fs::write(va.join("after.md"), "post-compaction\n").unwrap();
        a.nudge.send(LiveCmd::Nudge).await.unwrap();
        wait_until("the loser's session still applies updates", || {
            std::fs::read_to_string(vb.join("after.md"))
                .map(|s| s == "post-compaction\n")
                .unwrap_or(false)
        })
        .await;

        drop(a.nudge);
        drop(b.nudge);
        a.task.await.unwrap().unwrap();
        b.task.await.unwrap().unwrap();
        let compacted = a
            .events
            .try_iter()
            .chain(b.events.try_iter())
            .filter(|e| matches!(e, LiveEvent::Compacted { .. }))
            .count();
        assert_eq!(compacted, 1, "exactly one offer commits");
    }

    // ---- durable mirror + witness (collab-m4) --------------------------

    /// Room-enter both sides and bootstrap their FrontDocs. Assumes the
    /// baseline note already synced to both vaults.
    async fn enter_rooms(a: &Live, b: &Live) -> (FrontDoc, FrontDoc) {
        for live in [a, b] {
            live.nudge
                .send(LiveCmd::RoomEnter {
                    note_id: "doc".into(),
                })
                .await
                .unwrap();
        }
        let (mut feed_a, mut feed_b) = (RoomFeed::default(), RoomFeed::default());
        wait_until("both get RoomState", || {
            drain_room(a, &mut feed_a);
            drain_room(b, &mut feed_b);
            !feed_a.states.is_empty() && !feed_b.states.is_empty()
        })
        .await;
        (
            FrontDoc::new(&feed_a.states[0].1),
            FrontDoc::new(&feed_b.states[0].1),
        )
    }

    fn log_lines(data: &std::path::Path, vault_id: &str) -> usize {
        std::fs::read_to_string(data.join(vault_id).join("log.jsonl"))
            .map(|raw| raw.lines().count())
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn durable_flush_skips_when_peer_echo_already_landed() {
        let data = temp_dir("mirror-data");
        let url = spawn_relay(data.clone()).await;
        let va = temp_dir("mirror-va");
        let vb = temp_dir("mirror-vb");
        std::fs::write(va.join("doc.md"), "base\n").unwrap();
        let code = LiveCode::generate();
        let vault_id = LiveEngine::open(&va, &code).unwrap().vault_id().to_string();

        // A checkpoints fast; B's own debounce is slow, so B's flush (as
        // witness, 3×) fires well AFTER A's durable echo reached it.
        let a = spawn_live(&va, &code, &url, |c| {
            c.apply_debounce = Duration::from_millis(40);
            c.coedit_durable_debounce = Duration::from_millis(60);
        });
        let b = spawn_live(&vb, &code, &url, |c| {
            c.apply_debounce = Duration::from_millis(40);
            c.coedit_durable_debounce = Duration::from_millis(150);
        });
        wait_until("baseline lands on B", || vb.join("doc.md").exists()).await;
        let (mut front_a, _front_b) = enter_rooms(&a, &b).await;

        // A types; its checkpoint lands durably (one new log entry).
        let update = front_a.insert(0, "typed on A ");
        a.nudge
            .send(LiveCmd::CoeditUpdate {
                note_id: "doc".into(),
                update,
            })
            .await
            .unwrap();
        let before = log_lines(&data, &vault_id);
        wait_until("A's checkpoint lands in the log", || {
            log_lines(&data, &vault_id) > before
        })
        .await;
        let after_a = log_lines(&data, &vault_id);

        // B saw the keystrokes ephemerally (witness armed) AND the durable
        // echo (mirror advanced) — its flush must find nothing to push.
        // Negative window: > B's witness deadline (3 × 150ms).
        wait_until("B materializes the edit", || {
            std::fs::read_to_string(vb.join("doc.md"))
                .map(|t| t.contains("typed on A "))
                .unwrap_or(false)
        })
        .await;
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(
            log_lines(&data, &vault_id),
            after_a,
            "the witness re-logged a checkpoint the author already pushed"
        );

        drop(a.nudge);
        drop(b.nudge);
        a.task.await.unwrap().unwrap();
        b.task.await.unwrap().unwrap();
        // Even the shutdown flushes stayed empty — nothing left to log.
        assert_eq!(log_lines(&data, &vault_id), after_a);
    }

    #[tokio::test]
    async fn witness_persists_peer_keystrokes_when_author_dies() {
        let data = temp_dir("witness-data");
        let url = spawn_relay(data.clone()).await;
        let va = temp_dir("witness-va");
        let vb = temp_dir("witness-vb");
        let vc = temp_dir("witness-vc");
        std::fs::write(va.join("doc.md"), "base\n").unwrap();
        let code = LiveCode::generate();
        let vault_id = LiveEngine::open(&va, &code).unwrap().vault_id().to_string();

        // The author's own debounce is effectively infinite — any durable
        // copy of its keystrokes can only come from the witness.
        let a = spawn_live(&va, &code, &url, |c| {
            c.coedit_durable_debounce = Duration::from_secs(600);
        });
        let b = spawn_live(&vb, &code, &url, |c| {
            c.apply_debounce = Duration::from_millis(40);
            c.coedit_durable_debounce = Duration::from_millis(80);
        });
        wait_until("baseline lands on B", || vb.join("doc.md").exists()).await;
        let (mut front_a, _front_b) = enter_rooms(&a, &b).await;

        let before = log_lines(&data, &vault_id);
        let update = front_a.insert(0, "doomed author typed ");
        a.nudge
            .send(LiveCmd::CoeditUpdate {
                note_id: "doc".into(),
                update,
            })
            .await
            .unwrap();
        // B applies the ephemeral frame to its disk…
        wait_until("B materializes the ephemeral edit", || {
            std::fs::read_to_string(vb.join("doc.md"))
                .map(|t| t.contains("doomed author typed "))
                .unwrap_or(false)
        })
        .await;
        // …and the author crashes before ever flushing durably (its own
        // debounce is 600s, so any new log entry is the witness's).
        a.task.abort();
        let _ = a.task.await;

        // The witness's deadline (3 × 80ms) checkpoints the edit for it.
        wait_until("the witness flushes the peer's keystrokes", || {
            log_lines(&data, &vault_id) > before
        })
        .await;

        // A device that reads ONLY the durable log converges with the dead
        // author's text — the edit exists beyond RAM and B's disk.
        let mut c = LiveEngine::open(&vc, &code).unwrap();
        tacitus_sync::client::run_once(&mut c, &url).await.unwrap();
        assert!(c
            .materialize("n:doc")
            .unwrap()
            .expect("doc exists")
            .contains("doomed author typed "));

        drop(b.nudge);
        b.task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn coedit_materialization_is_audited_with_coedit_origin() {
        let url = spawn_relay(temp_dir("attr-data")).await;
        let va = temp_dir("attr-va");
        let vb = temp_dir("attr-vb");
        std::fs::write(va.join("doc.md"), "base\n").unwrap();
        let code = LiveCode::generate();
        let fast = |c: &mut LiveConfig| {
            c.apply_debounce = Duration::from_millis(40);
            c.coedit_durable_debounce = Duration::from_millis(80);
        };
        let a = spawn_live(&va, &code, &url, fast);
        let b = spawn_live(&vb, &code, &url, fast);
        wait_until("baseline lands on B", || vb.join("doc.md").exists()).await;
        let (mut front_a, _front_b) = enter_rooms(&a, &b).await;

        // Room keystrokes materialize on B attributed "coedit"…
        let update = front_a.insert(0, "typed in the room ");
        a.nudge
            .send(LiveCmd::CoeditUpdate {
                note_id: "doc".into(),
                update,
            })
            .await
            .unwrap();
        wait_until("B materializes the room edit", || {
            std::fs::read_to_string(vb.join("doc.md"))
                .map(|t| t.contains("typed in the room "))
                .unwrap_or(false)
        })
        .await;
        let audit_reader = LiveWriter::new(&vb, LiveScope::ReadWrite);
        let audit = audit_reader.read_audit(1).unwrap();
        assert_eq!(audit[0].origin.as_deref(), Some("coedit"));

        // …and a PLAIN note synced afterwards goes back to "sync" (the
        // origin switch restored the writer).
        std::fs::write(va.join("other.md"), "not a room note\n").unwrap();
        a.nudge.send(LiveCmd::Nudge).await.unwrap();
        wait_until("plain note lands on B", || vb.join("other.md").exists()).await;
        let audit = audit_reader.read_audit(1).unwrap();
        assert_eq!(audit[0].origin.as_deref(), Some("sync"));

        drop(a.nudge);
        drop(b.nudge);
        a.task.await.unwrap().unwrap();
        b.task.await.unwrap().unwrap();
    }
}
