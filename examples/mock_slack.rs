use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Json};
use axum::routing::{any, post};
use axum::Router;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::Mutex;

#[derive(Default)]
struct Log {
    lines: Mutex<Vec<String>>,
}

async fn record(log: &Arc<Log>, line: impl Into<String>) {
    let line = line.into();
    println!("[mock-slack] {line}");
    log.lines.lock().await.push(line);
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(9399);
    let log = Arc::new(Log::default());

    let app = Router::new()
        .route(
            "/link",
            any(|State(log): State<Arc<Log>>, ws: WebSocketUpgrade| async move {
                ws.on_upgrade(move |socket| handle_socket(log, socket))
            }),
        )
        .route(
            "/{method}",
            post(move |State(log): State<Arc<Log>>, request: Request| async move {
                let method = request.uri().path().trim_start_matches('/').to_string();
                let body = axum::body::to_bytes(request.into_body(), 1 << 20)
                    .await
                    .unwrap_or_default();
                let payload: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
                record(&log, format!("web-api {method} {payload}")).await;
                let response = match method.as_str() {
                    "auth.test" => json!({
                        "ok": true, "user_id": "U0MOCKBOT", "team_id": "T0MOCK",
                        "bot_id": "B0MOCK", "user": "omon-mock"
                    }),
                    "apps.connections.open" => {
                        json!({"ok": true, "url": format!("ws://127.0.0.1:{port}/link")})
                    }
                    "chat.postMessage" => json!({"ok": true, "ts": "1700.000100"}),
                    "files.getUploadURLExternal" => json!({
                        "ok": true,
                        "upload_url": format!("http://127.0.0.1:{port}/upload-raw"),
                        "file_id": "F0MOCK"
                    }),
                    _ => json!({"ok": true}),
                };
                Json(response).into_response()
            }),
        )
        .route(
            "/upload-raw",
            post(|State(log): State<Arc<Log>>, body: axum::body::Bytes| async move {
                record(&log, format!("upload-raw {} bytes", body.len())).await;
                "ok"
            }),
        )
        .with_state(log);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    println!("[mock-slack] listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn handle_socket(log: Arc<Log>, mut socket: WebSocket) {
    record(&log, "socket connection accepted".to_string()).await;
    let _ = socket
        .send(Message::Text(json!({"type": "hello"}).to_string().into()))
        .await;
    while let Some(Ok(frame)) = socket.next().await {
        if let Message::Text(text) = frame {
            record(&log, format!("socket frame {text}")).await;
        }
    }
    record(&log, "socket closed".to_string()).await;
}
