//! tacitus-relay — the dumb half of Tacitus Sync.
//!
//! Clients speak the JSON protocol from `tacitus-sync` over a WebSocket at
//! `/ws`. The relay authenticates a vault (TOFU bearer token), replays the
//! backlog after `since_seq`, appends pushed blobs to a per-vault
//! append-only log, and fans every update out to ALL of the vault's live
//! connections (pusher included). It never sees plaintext: blobs are
//! end-to-end encrypted by the clients.
//!
//!   TACITUS_RELAY_BIND  (default 127.0.0.1:8091)
//!   TACITUS_RELAY_DATA  (default ./relay-data)

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

use hub::{valid_vault_id, RelayState, VaultHub};

// The wire protocol, mirrored from tacitus-sync/src/protocol.rs (the relay
// deliberately does not depend on the sync crate — it must never be able to
// read payloads, and the compiler enforcing that is worth a few lines).
/// Extensions this relay speaks; advertised in every Welcome. Extension
/// frames are only ever SENT to connections whose Hello asked for the cap —
/// an old client can never receive a tag it can't parse. A client below a
/// compaction snapshot WITHOUT the compact cap is rejected honestly
/// (`err compacted`) instead of silently missing the compacted prefix.
const RELAY_CAPS: [&str; 2] = ["presence", "compact"];

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
}

#[derive(Debug, Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum ServerMsg {
    Welcome {
        latest_seq: u64,
        caps: Vec<String>,
        log_bytes: u64,
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
    let (latest_seq, log_bytes, snapshot, backlog) = {
        let log = hub.log.lock().await;
        // A cursor below the snapshot replays snapshot-then-tail; everyone
        // else replays a plain tail from where they left off.
        let snapshot_seq = log.snapshot().map(|(seq, _)| seq).unwrap_or(0);
        let (snapshot, from) = if since_seq < snapshot_seq {
            (
                log.snapshot().map(|(seq, blob)| (seq, blob.to_vec())),
                snapshot_seq,
            )
        } else {
            (None, since_seq)
        };
        (
            log.last_seq(),
            log.log_bytes(),
            snapshot,
            log.read_since(from),
        )
    };
    if snapshot.is_some() && !wants_compact {
        // The compacted prefix no longer exists as entries, and this client
        // can't parse a snapshot frame it didn't ask for — fail honestly.
        return reject(
            socket,
            "compacted",
            "log compacted past your cursor; upgrade this client",
        )
        .await;
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
    };
    if !send(&mut socket, &welcome).await {
        return;
    }
    let mut last_sent = since_seq;
    if let Some((snapshot_seq, blob)) = snapshot {
        if !send(
            &mut socket,
            &ServerMsg::Snapshot {
                upto_seq: snapshot_seq,
                blob: b64_encode(&blob),
            },
        )
        .await
        {
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
                                let appended = hub.log.lock().await.append(&bytes);
                                match appended {
                                    Ok(seq) => {
                                        if !send(&mut socket, &ServerMsg::Ack { seq }).await {
                                            return;
                                        }
                                        let _ = hub.tx.send((seq, bytes));
                                    }
                                    Err(e) if e.to_string().contains("log_full") => {
                                        let _ = send(&mut socket, &ServerMsg::Err {
                                            code: "log_full".into(),
                                            msg: "vault log reached the beta cap".into(),
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
                                let result = hub.log.lock().await.compact(upto_seq, &bytes);
                                match result {
                                    Ok(()) => {
                                        if !send(&mut socket, &ServerMsg::Compacted { upto_seq }).await {
                                            return;
                                        }
                                    }
                                    Err(e) => {
                                        // Rejections are advisory — the log is
                                        // untouched and the connection lives on.
                                        let reason = e.to_string();
                                        let code = ["compact_stale", "compact_ahead", "snapshot_too_large"]
                                            .into_iter()
                                            .find(|known| reason.contains(known))
                                            .unwrap_or("storage");
                                        if code == "storage" {
                                            tracing::error!("compact failed: {e}");
                                        }
                                        let _ = send(&mut socket, &ServerMsg::Err {
                                            code: code.into(),
                                            msg: reason,
                                        }).await;
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
                            let snapshot_seq = log.snapshot().map(|(seq, _)| seq).unwrap_or(0);
                            if last_sent < snapshot_seq {
                                (
                                    log.snapshot().map(|(seq, blob)| (seq, blob.to_vec())),
                                    log.read_since(snapshot_seq),
                                )
                            } else {
                                (None, log.read_since(last_sent))
                            }
                        };
                        if let Some((snapshot_seq, blob)) = snapshot {
                            if !wants_compact {
                                let _ = send(&mut socket, &ServerMsg::Err {
                                    code: "compacted".into(),
                                    msg: "log compacted past your cursor; upgrade this client".into(),
                                }).await;
                                return;
                            }
                            if !send(&mut socket, &ServerMsg::Snapshot {
                                upto_seq: snapshot_seq,
                                blob: b64_encode(&blob),
                            }).await {
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
    let state = Arc::new(RelayState::new(PathBuf::from(&data)));

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
        let state = Arc::new(RelayState::new(data_dir));
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
            let frame = tokio::time::timeout(Duration::from_secs(5), ws.next())
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
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
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
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
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

        // A gets its own presence echo and the push ack (their relative
        // order is a select race — the ack is sent inline, the echo rides
        // the broadcast arm), but never a presence ACK.
        let one = recv_json(&mut a).await;
        let two = recv_json(&mut a).await;
        let mut tags = [one["t"].as_str().unwrap(), two["t"].as_str().unwrap()];
        tags.sort_unstable();
        assert_eq!(tags, ["ack", "presence"]);
        let ack = if one["t"] == "ack" { &one } else { &two };
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
}
