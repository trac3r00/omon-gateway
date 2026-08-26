mod common;

use common::MockWebApi;
use omon_gateway::slack::{slack_emoji_name, SlackEgress, SlackWebClient};
use omon_gateway::{
    OutboundAction, OutboundDispatcher, SessionKey, StreamChunk,
};
use uuid::Uuid;

fn egress(base: &str) -> SlackEgress {
    SlackEgress::new(SlackWebClient::new(base, "xoxb-test-token"))
}

fn channel_session(thread: Option<&str>) -> SessionKey {
    SessionKey::new("slack", Some("T1"), "C1", thread, "U1").with_bot_id("U0BOT")
}

fn stream_chunk(seq: u64, content: &str, is_final: bool) -> StreamChunk {
    StreamChunk {
        stream_id: Uuid::nil(),
        sequence: seq,
        content: content.into(),
        is_final,
    }
}

#[test]
fn emoji_names_map_discord_reactions() {
    assert_eq!(slack_emoji_name("👀"), "eyes");
    assert_eq!(slack_emoji_name("✅"), "white_check_mark");
    assert_eq!(slack_emoji_name("❌"), "x");
    assert_eq!(slack_emoji_name("thumbsup"), "thumbsup");
}

#[tokio::test]
async fn send_message_posts_threaded_and_chunked() {
    let mock = MockWebApi::start().await;
    let content = "x".repeat(9_000);
    egress(&mock.base)
        .dispatch(OutboundAction::SendMessage {
            session: channel_session(Some("1.5")),
            content,
            reply_to: None,
        })
        .await
        .unwrap();

    let posts = mock.calls_for("chat.postMessage").await;
    assert_eq!(posts.len(), 3, "9000 chars must chunk at 4000, got {posts:?}");
    for post in &posts {
        assert_eq!(post["channel"], "C1");
        assert_eq!(post["thread_ts"], "1.5");
        assert!(post["text"].as_str().unwrap().len() <= 4000);
    }
    mock.stop().await;
}

#[tokio::test]
async fn send_message_uses_reply_to_when_session_has_no_thread() {
    let mock = MockWebApi::start().await;
    egress(&mock.base)
        .dispatch(OutboundAction::SendMessage {
            session: channel_session(None),
            content: "dm reply".into(),
            reply_to: Some("2.2".into()),
        })
        .await
        .unwrap();

    let posts = mock.calls_for("chat.postMessage").await;
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["thread_ts"], "2.2");
    mock.stop().await;
}

#[tokio::test]
async fn edit_and_delete_map_to_chat_methods() {
    let mock = MockWebApi::start().await;
    let egress = egress(&mock.base);
    egress
        .dispatch(OutboundAction::EditMessage {
            session: channel_session(None),
            platform_message_id: "3.3".into(),
            content: "edited".into(),
        })
        .await
        .unwrap();
    egress
        .dispatch(OutboundAction::DeleteMessage {
            session: channel_session(None),
            platform_message_id: "3.4".into(),
        })
        .await
        .unwrap();

    let update = &mock.calls_for("chat.update").await[0];
    assert_eq!(update["ts"], "3.3");
    assert_eq!(update["text"], "edited");
    let delete = &mock.calls_for("chat.delete").await[0];
    assert_eq!(delete["ts"], "3.4");
    mock.stop().await;
}

#[tokio::test]
async fn react_removes_processing_emoji_then_adds_mapped() {
    let mock = MockWebApi::start().await;
    egress(&mock.base)
        .dispatch(OutboundAction::React {
            session: channel_session(None),
            message_id: "4.4".into(),
            emoji: "✅".into(),
            remove_others: true,
        })
        .await
        .unwrap();

    let removes = mock.calls_for("reactions.remove").await;
    assert_eq!(removes.len(), 1);
    assert_eq!(removes[0]["name"], "eyes");
    assert_eq!(removes[0]["timestamp"], "4.4");
    let adds = mock.calls_for("reactions.add").await;
    assert_eq!(adds.len(), 1);
    assert_eq!(adds[0]["name"], "white_check_mark");
    mock.stop().await;
}

#[tokio::test]
async fn stream_posts_once_then_updates_with_final_flush() {
    let mock = MockWebApi::start().await;
    let egress = egress(&mock.base);
    let session = channel_session(Some("5.5"));

    for (seq, text) in ["partial one", "partial two", "partial three"].iter().enumerate() {
        egress
            .dispatch(OutboundAction::Stream {
                session: session.clone(),
                chunk: stream_chunk(seq as u64, text, false),
            })
            .await
            .unwrap();
    }
    egress
        .dispatch(OutboundAction::Stream {
            session: session.clone(),
            chunk: stream_chunk(3, "partial three — done", true),
        })
        .await
        .unwrap();

    let posts = mock.calls_for("chat.postMessage").await;
    assert_eq!(posts.len(), 1, "stream must open exactly one message");
    assert_eq!(posts[0]["channel"], "C1");
    assert_eq!(posts[0]["thread_ts"], "5.5");

    let updates = mock.calls_for("chat.update").await;
    let last = updates.last().expect("final update recorded");
    assert_eq!(last["ts"], "1700.000100", "updates edit the opened message");
    assert_eq!(last["text"], "partial three — done");
    mock.stop().await;
}

#[tokio::test]
async fn approval_request_posts_buttons_and_expire_clears_them() {
    let mock = MockWebApi::start().await;
    let egress = egress(&mock.base);
    let request_id = Uuid::new_v4();

    egress
        .dispatch(OutboundAction::ApprovalRequest {
            session: channel_session(None),
            request_id,
            command: "rm -rf /tmp/x".into(),
            reason: "cleanup".into(),
        })
        .await
        .unwrap();

    let posts = mock.calls_for("chat.postMessage").await;
    assert_eq!(posts.len(), 1);
    let blocks = posts[0]["blocks"].as_array().expect("blocks array");
    let values: Vec<String> = blocks
        .iter()
        .flat_map(|b| b["elements"].as_array().cloned().unwrap_or_default())
        .filter_map(|e| e["value"].as_str().map(str::to_string))
        .collect();
    for decision in ["once", "session", "always", "deny"] {
        let expected = format!("omon:approval:{request_id}:{decision}");
        assert!(values.contains(&expected), "missing button {expected}");
    }

    egress
        .dispatch(OutboundAction::ExpireApproval { request_id })
        .await
        .unwrap();
    let updates = mock.calls_for("chat.update").await;
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0]["ts"], "1700.000100");
    assert_eq!(updates[0]["blocks"].as_array().unwrap().len(), 0);
    mock.stop().await;
}

#[tokio::test]
async fn upload_dispatches_three_step_upload() {
    let mock = MockWebApi::start().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.bin");
    std::fs::write(&path, b"payload").unwrap();

    egress(&mock.base)
        .dispatch(OutboundAction::UploadFile {
            session: channel_session(Some("6.6")),
            path: path.clone(),
        })
        .await
        .unwrap();

    assert_eq!(mock.recorder.uploads.lock().await.len(), 1);
    let complete = &mock.calls_for("files.completeUploadExternal").await[0];
    assert_eq!(complete["channel_id"], "C1");
    assert_eq!(complete["thread_ts"], "6.6");
    mock.stop().await;
}

#[tokio::test]
async fn typing_is_a_successful_noop() {
    let mock = MockWebApi::start().await;
    egress(&mock.base)
        .dispatch(OutboundAction::Typing {
            session: channel_session(None),
            active: true,
        })
        .await
        .unwrap();
    assert!(mock.calls().await.is_empty());
    mock.stop().await;
}
