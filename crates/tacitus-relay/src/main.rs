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
    use tacitus_sync::live::{run_live, LiveConfig, LiveEvent};
    use tacitus_sync::{SyncEngine as LiveEngine, SyncError, VaultCode as LiveCode};

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
        nudge: tokio::sync::mpsc::Sender<()>,
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
        a.nudge.send(()).await.unwrap();
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
        a.nudge.send(()).await.unwrap();
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
        a.nudge.send(()).await.unwrap();

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
    async fn rejects_malformed_vault_id_over_ws() {
        let url = spawn_relay(temp_dir("badid")).await;
        let mut ws = connect(&url).await;
        send_json(&mut ws, hello("../escape", "tok", 0)).await;
        let err = recv_json(&mut ws).await;
        assert_eq!(err["t"], "err");
        assert_eq!(err["code"], "bad_vault_id");
    }
}
