use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::response::Json;
use axum::routing::post;
use axum::Router;
use serde_json::{json, Value};
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;

#[derive(Default)]
pub struct Recorder {
    pub calls: Mutex<Vec<(String, Value)>>,
    pub auth_headers: Mutex<Vec<(String, String)>>,
    pub uploads: Mutex<Vec<Vec<u8>>>,
    pub base: Mutex<String>,
}

pub struct MockWebApi {
    pub base: String,
    shutdown: Option<oneshot::Sender<()>>,
    handle: JoinHandle<()>,
    pub recorder: Arc<Recorder>,
}

impl MockWebApi {
    pub async fn start() -> Self {
        Self::start_with_error(None).await
    }

    pub async fn start_with_error(failing_method: Option<&'static str>) -> Self {
        let recorder = Arc::new(Recorder::default());
        let state = recorder.clone();
        let fail = failing_method;
        let app = Router::new()
            .route(
                "/{method}",
                post(move |State(state): State<Arc<Recorder>>, request: Request| {
                    let fail = fail;
                    async move {
                        let method = request.uri().path().trim_start_matches('/').to_string();
                        let auth = request
                            .headers()
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_string();
                        state
                            .auth_headers
                            .lock()
                            .await
                            .push((method.clone(), auth));
                        let body = axum::body::to_bytes(request.into_body(), 1 << 20)
                            .await
                            .unwrap_or_default();
                        if Some(method.as_str()) == fail {
                            return Json(json!({"ok": false, "error": "channel_not_found"}));
                        }
                        let value: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
                        state.calls.lock().await.push((method.clone(), value));
                        let response = match method.as_str() {
                            "auth.test" => json!({
                                "ok": true, "user_id": "U0BOT", "team_id": "T1",
                                "bot_id": "B0BOT", "user": "omon"
                            }),
                            "chat.postMessage" => {
                                json!({"ok": true, "ts": "1700.000100", "channel": "C1"})
                            }
                            "apps.connections.open" => {
                                json!({"ok": true, "url": "wss://wss-primary.slack.com/link/?ticket=abc"})
                            }
                            "files.getUploadURLExternal" => json!({
                                "ok": true,
                                "upload_url": format!("{}/upload-raw", *state.base.lock().await),
                                "file_id": "F0UPLOAD"
                            }),
                            "files.completeUploadExternal" => json!({"ok": true}),
                            "conversations.history" | "conversations.replies" => json!({
                                "ok": true,
                                "messages": [
                                    {"type": "message", "user": "U1", "text": "first", "ts": "9.1"},
                                    {"type": "message", "user": "U2", "text": "second", "ts": "9.2"}
                                ]
                            }),
                            _ => json!({"ok": true}),
                        };
                        Json(response)
                    }
                }),
            )
            .route(
                "/upload-raw",
                post(|State(state): State<Arc<Recorder>>, body: Bytes| async move {
                    state.uploads.lock().await.push(body.to_vec());
                    "ok"
                }),
            )
            .with_state(recorder.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        *recorder.base.lock().await = base.clone();
        let (tx, rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await
                .unwrap();
        });
        Self { base, shutdown: Some(tx), handle, recorder }
    }

    pub async fn calls(&self) -> Vec<(String, Value)> {
        self.recorder.calls.lock().await.clone()
    }

    pub async fn calls_for(&self, method: &str) -> Vec<Value> {
        self.recorder
            .calls
            .lock()
            .await
            .iter()
            .filter(|(m, _)| m == method)
            .map(|(_, v)| v.clone())
            .collect()
    }

    pub async fn auth_for(&self, method: &str) -> Option<String> {
        self.recorder
            .auth_headers
            .lock()
            .await
            .iter()
            .find(|(m, _)| m == method)
            .map(|(_, a)| a.clone())
    }

    pub async fn stop(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = (&mut self.handle).await;
    }
}
