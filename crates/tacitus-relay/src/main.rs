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
#[derive(Debug, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum ClientMsg {
    Hello {
        vault_id: String,
        token: String,
        since_seq: u64,
    },
    Push {
        blob: String, // base64 — kept opaque, decoded only for storage
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum ServerMsg {
    Welcome { latest_seq: u64 },
    Update { seq: u64, blob: String },
    Ack { seq: u64 },
    Err { code: String, msg: String },
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
    ws.on_upgrade(move |socket| connection(socket, state))
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

async fn connection(mut socket: WebSocket, state: Arc<RelayState>) {
    // First frame must be a Hello.
    let hello = loop {
        match socket.recv().await {
            Some(Ok(Message::Text(text))) => match serde_json::from_str::<ClientMsg>(&text) {
                Ok(ClientMsg::Hello {
                    vault_id,
                    token,
                    since_seq,
                }) => break (vault_id, token, since_seq),
                Ok(_) => return reject(socket, "protocol", "hello must come first").await,
                Err(_) => return reject(socket, "protocol", "malformed frame").await,
            },
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
            _ => return,
        }
    };
    let (vault_id, token, since_seq) = hello;

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
    // `last_sent` dedups the overlap.
    let mut rx = hub.tx.subscribe();
    let (latest_seq, backlog) = {
        let log = hub.log.lock().await;
        (log.last_seq(), log.read_since(since_seq))
    };
    let backlog = match backlog {
        Ok(backlog) => backlog,
        Err(e) => {
            tracing::error!("backlog read failed: {e}");
            return reject(socket, "storage", "backlog read failed").await;
        }
    };
    if !send(&mut socket, &ServerMsg::Welcome { latest_seq }).await {
        return;
    }
    let mut last_sent = since_seq;
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
                        // Fell behind the channel: resync from the log.
                        let resync = {
                            let log = hub.log.lock().await;
                            log.read_since(last_sent)
                        };
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

    fn push(blob: &[u8]) -> serde_json::Value {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;
        serde_json::json!({ "t": "push", "blob": STANDARD.encode(blob) })
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
    async fn rejects_malformed_vault_id_over_ws() {
        let url = spawn_relay(temp_dir("badid")).await;
        let mut ws = connect(&url).await;
        send_json(&mut ws, hello("../escape", "tok", 0)).await;
        let err = recv_json(&mut ws).await;
        assert_eq!(err["t"], "err");
        assert_eq!(err["code"], "bad_vault_id");
    }
}
