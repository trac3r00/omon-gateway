mod common;

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::Router;
use common::{MockWebApi, Recorder};
use omon_gateway::slack::SlackWebClient;
use serde_json::json;
use tokio::sync::oneshot;

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
