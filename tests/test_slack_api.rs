use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::response::Json;
use axum::routing::post;
use axum::Router;
use omon_gateway::slack::SlackWebClient;
use serde_json::{json, Value};
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;

#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<(String, Value)>>,
    auth_headers: Mutex<Vec<(String, String)>>,
    uploads: Mutex<Vec<Vec<u8>>>,
    base: Mutex<String>,
}

struct MockWebApi {
    base: String,
    shutdown: Option<oneshot::Sender<()>>,
    handle: JoinHandle<()>,
    recorder: Arc<Recorder>,
}

impl MockWebApi {
    async fn start() -> Self {
        Self::start_with_error(None).await
    }

    async fn start_with_error(failing_method: Option<&'static str>) -> Self {
        let recorder = Arc::new(Recorder::default());
        let fail = failing_method;
        let app = Router::new()
            .route(
                "/{method}",
                post(move |State(state): State<Arc<Recorder>>, request: Request| {
                    let fail = fail;
                    async move {
                        let method = request
                            .uri()
                            .path()
                            .trim_start_matches('/')
                            .to_string();
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
                        let value: Value =
                            serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
                        state.calls.lock().await.push((method.clone(), value));
                        let response = match method.as_str() {
                            "auth.test" => json!({
                                "ok": true,
                                "user_id": "U0BOT",
                                "team_id": "T1",
                                "bot_id": "B0BOT",
                                "user": "omon"
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
        Self {
            base,
            shutdown: Some(tx),
            handle,
            recorder,
        }
    }

    async fn calls(&self) -> Vec<(String, Value)> {
        self.recorder.calls.lock().await.clone()
    }

    async fn auth_for(&self, method: &str) -> Option<String> {
        self.recorder
            .auth_headers
            .lock()
            .await
            .iter()
            .find(|(m, _)| m == method)
            .map(|(_, a)| a.clone())
    }

    async fn stop(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = (&mut self.handle).await;
    }
}

fn client(base: &str) -> SlackWebClient {
    SlackWebClient::new(base, "xoxb-test-token")
}

#[tokio::test]
async fn auth_test_parses_identity_and_sends_bot_token() {
    let mock = MockWebApi::start().await;
    let identity = client(&mock.base).auth_test().await.unwrap();
    assert_eq!(identity.user_id, "U0BOT");
    assert_eq!(identity.team_id, "T1");
    assert_eq!(identity.bot_id.as_deref(), Some("B0BOT"));
    assert_eq!(
        mock.auth_for("auth.test").await.as_deref(),
        Some("Bearer xoxb-test-token")
    );
    mock.stop().await;
}

#[tokio::test]
async fn post_message_sends_payload_and_returns_ts() {
    let mock = MockWebApi::start().await;
    let ts = client(&mock.base)
        .post_message("C1", "hello world", Some("1700.000001"))
        .await
        .unwrap();
    assert_eq!(ts, "1700.000100");
    let calls = mock.calls().await;
    let (_, body) = calls
        .iter()
        .find(|(m, _)| m == "chat.postMessage")
        .expect("postMessage recorded");
    assert_eq!(body["channel"], "C1");
    assert_eq!(body["text"], "hello world");
    assert_eq!(body["thread_ts"], "1700.000001");
    mock.stop().await;
}

#[tokio::test]
async fn post_message_with_blocks_includes_blocks_json() {
    let mock = MockWebApi::start().await;
    let blocks = json!([{"type": "actions", "elements": []}]);
    client(&mock.base)
        .post_message_blocks("C1", "approve?", blocks.clone(), None)
        .await
        .unwrap();
    let calls = mock.calls().await;
    let (_, body) = calls
        .iter()
        .find(|(m, _)| m == "chat.postMessage")
        .unwrap();
    assert_eq!(body["blocks"], blocks);
    assert!(body.get("thread_ts").is_none());
    mock.stop().await;
}

#[tokio::test]
async fn update_delete_and_reaction_payloads() {
    let mock = MockWebApi::start().await;
    let client = client(&mock.base);
    client.update_message("C1", "1.1", "edited").await.unwrap();
    client.delete_message("C1", "1.2").await.unwrap();
    client.add_reaction("C1", "1.3", "eyes").await.unwrap();
    client.remove_reaction("C1", "1.3", "eyes").await.unwrap();

    let calls = mock.calls().await;
    let find = |m: &str| calls.iter().find(|(name, _)| name == m).unwrap().1.clone();
    assert_eq!(find("chat.update")["text"], "edited");
    assert_eq!(find("chat.update")["ts"], "1.1");
    assert_eq!(find("chat.delete")["ts"], "1.2");
    assert_eq!(find("reactions.add")["name"], "eyes");
    assert_eq!(find("reactions.add")["timestamp"], "1.3");
    assert_eq!(find("reactions.remove")["name"], "eyes");
    mock.stop().await;
}

#[tokio::test]
async fn slack_api_error_surfaces_error_code() {
    let mock = MockWebApi::start_with_error(Some("chat.postMessage")).await;
    let err = client(&mock.base)
        .post_message("C9", "boom", None)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("channel_not_found"),
        "expected slack error code in message, got: {err}"
    );
    mock.stop().await;
}

#[tokio::test]
async fn connections_open_uses_app_token_and_returns_url() {
    let mock = MockWebApi::start().await;
    let url = client(&mock.base)
        .open_socket_connection("xapp-test-token")
        .await
        .unwrap();
    assert!(url.starts_with("wss://"));
    assert_eq!(
        mock.auth_for("apps.connections.open").await.as_deref(),
        Some("Bearer xapp-test-token")
    );
    mock.stop().await;
}

#[tokio::test]
async fn upload_file_runs_three_step_flow() {
    let mock = MockWebApi::start().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("notes.txt");
    std::fs::write(&path, b"hello-bytes").unwrap();

    client(&mock.base)
        .upload_file("C1", Some("1.9"), &path, Some("notes for you"))
        .await
        .unwrap();

    let uploads = mock.recorder.uploads.lock().await.clone();
    assert_eq!(uploads, vec![b"hello-bytes".to_vec()]);

    let calls = mock.calls().await;
    let complete = calls
        .iter()
        .find(|(m, _)| m == "files.completeUploadExternal")
        .map(|(_, v)| v.clone())
        .expect("completeUploadExternal recorded");
    assert_eq!(complete["channel_id"], "C1");
    assert_eq!(complete["thread_ts"], "1.9");
    assert_eq!(complete["initial_comment"], "notes for you");
    assert_eq!(complete["files"][0]["id"], "F0UPLOAD");
    mock.stop().await;
}

#[tokio::test]
async fn history_maps_messages() {
    let mock = MockWebApi::start().await;
    let messages = client(&mock.base).history("C1", 10).await.unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].user.as_deref(), Some("U1"));
    assert_eq!(messages[0].text, "first");
    assert_eq!(messages[1].ts, "9.2");
    mock.stop().await;
}

#[tokio::test]
async fn download_file_uses_bot_token() {
    let recorder = Arc::new(Recorder::default());
    let state = recorder.clone();
    let app = Router::new()
        .route(
            "/files/notes.txt",
            axum::routing::get(|State(state): State<Arc<Recorder>>, request: Request| async move {
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
                    .push(("download".to_string(), auth));
                "file-contents"
            }),
        )
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .unwrap();
    });

    let bytes = client(&format!("http://{addr}"))
        .download_file(&format!("http://{addr}/files/notes.txt"))
        .await
        .unwrap();
    assert_eq!(bytes, b"file-contents".to_vec());
    let headers = recorder.auth_headers.lock().await;
    assert_eq!(headers[0].1, "Bearer xoxb-test-token");
    drop(headers);
    let _ = tx.send(());
    let _ = handle.await;
}
