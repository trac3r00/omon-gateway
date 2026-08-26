use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use omon_gateway::slack::{SlackWebClient, SocketEvent, SocketModeClient};
use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

struct MockSocketServer {
    acks: Arc<Mutex<Vec<String>>>,
    connections: Arc<Mutex<usize>>,
    base: String,
    handle: tokio::task::JoinHandle<()>,
}

impl MockSocketServer {
    async fn start() -> Self {
        let acks = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(Mutex::new(0usize));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let acks_task = acks.clone();
        let connections_task = connections.clone();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                *connections_task.lock().await += 1;
                let acks = acks_task.clone();
                tokio::spawn(async move {
                    let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                        return;
                    };
                    let _ = ws
                        .send(Message::Text(
                            json!({"type": "hello", "num_connections": 1}).to_string().into(),
                        ))
                        .await;
                    let envelope = json!({
                        "envelope_id": "env-1",
                        "type": "events_api",
                        "payload": {
                            "team_id": "T1",
                            "event_id": "Ev1",
                            "event": {"type": "message", "channel": "C1", "user": "U1", "text": "hi", "ts": "1.1"}
                        }
                    });
                    let _ = ws.send(Message::Text(envelope.to_string().into())).await;
                    let duplicate = json!({
                        "envelope_id": "env-1",
                        "type": "events_api",
                        "payload": {
                            "team_id": "T1",
                            "event_id": "Ev1",
                            "event": {"type": "message", "channel": "C1", "user": "U1", "text": "hi", "ts": "1.1"}
                        }
                    });
                    let _ = ws.send(Message::Text(duplicate.to_string().into())).await;

                    while let Some(Ok(frame)) = ws.next().await {
                        let Message::Text(text) = frame else { continue };
                        let Ok(value) = serde_json::from_str::<Value>(&text) else {
                            continue;
                        };
                        if let Some(id) = value.get("envelope_id").and_then(Value::as_str) {
                            acks.lock().await.push(id.to_string());
                            if acks.lock().await.len() >= 2 {
                                let _ = ws
                                    .send(Message::Text(
                                        json!({"type": "disconnect", "reason": "test"})
                                            .to_string()
                                            .into(),
                                    ))
                                    .await;
                                return;
                            }
                        }
                    }
                });
            }
        });

        Self {
            acks,
            connections,
            base: format!("http://{addr}"),
            handle,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn socket_client_acks_dedupes_and_reconnects() {
    let server = MockSocketServer::start().await;
    let wss_url = format!("ws://{}/link", server.base.trim_start_matches("http://"));

    let api = SlackWebClient::new(&server.base, "xoxb-unused");
    let client = SocketModeClient::new(api, "xapp-test").with_socket_url_override(wss_url);
    let (tx, mut rx) = mpsc::channel::<SocketEvent>(16);
    let cancel = CancellationToken::new();
    let run_cancel = cancel.clone();
    let run = tokio::spawn(async move { client.run(tx, run_cancel).await });

    let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("event within timeout")
        .expect("channel open");
    match event {
        SocketEvent::EventsApi { envelope_id, payload } => {
            assert_eq!(envelope_id, "env-1");
            assert_eq!(payload["event"]["text"], "hi");
        }
        other => panic!("expected events_api, got {other:?}"),
    }

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if server.acks.lock().await.len() >= 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both envelopes acked within timeout");

    let duplicate = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await;
    assert!(
        duplicate.is_err(),
        "duplicate envelope must not be forwarded, got {duplicate:?}"
    );

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if *server.connections.lock().await >= 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("client reconnected after disconnect frame");

    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("run future completes")
        .expect("run task joins");
    assert!(result.is_ok(), "graceful shutdown, got {result:?}");

    server.handle.abort();
}
