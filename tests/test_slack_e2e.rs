mod common;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use common::MockWebApi;
use futures_util::{SinkExt, StreamExt};
use omon_gateway::slack::{OwnedSlackFilter, SlackPairingStore, SlackRuntime, SlackRuntimeConfig};
use omon_gateway::{
    AgentRunner, Database, InboundEvent, MultiplexerConfig, OutboundAction, OutboundDispatcher,
    Result, SessionContext, SessionKey, SessionMultiplexer, SmartApprovalGuard,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

struct StubRunner {
    seen: Mutex<Vec<(SessionKey, String)>>,
    dispatcher: RwLock<Option<Arc<dyn OutboundDispatcher>>>,
}

impl StubRunner {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
            dispatcher: RwLock::new(None),
        })
    }
}

#[async_trait]
impl AgentRunner for StubRunner {
    async fn run(&self, session: &mut SessionContext, event: InboundEvent) -> Result<()> {
        self.seen
            .lock()
            .await
            .push((session.key.clone(), event.content.clone()));
        if let Some(dispatcher) = self.dispatcher.read().await.clone() {
            dispatcher
                .dispatch(OutboundAction::SendMessage {
                    session: session.key.clone(),
                    content: "agent reply".to_string(),
                    reply_to: None,
                })
                .await?;
        }
        Ok(())
    }
}

struct E2eRig {
    mock: MockWebApi,
    envelopes: mpsc::Sender<Value>,
    connections: Arc<Mutex<usize>>,
    ws_handle: tokio::task::JoinHandle<()>,
}

impl E2eRig {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (envelope_tx, mut envelope_rx) = mpsc::channel::<Value>(16);
        let connections = Arc::new(Mutex::new(0usize));
        let connections_task = connections.clone();
        let ws_handle = tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            *connections_task.lock().await += 1;
            let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            let _ = ws
                .send(Message::Text(json!({"type": "hello"}).to_string().into()))
                .await;
            loop {
                tokio::select! {
                    frame = ws.next() => {
                        match frame {
                            Some(Ok(Message::Text(_))) => {}
                            Some(Ok(Message::Ping(p))) => {
                                let _ = ws.send(Message::Pong(p)).await;
                            }
                            _ => break,
                        }
                    }
                    Some(envelope) = envelope_rx.recv() => {
                        if ws
                            .send(Message::Text(envelope.to_string().into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });

        let mock = MockWebApi::start().await;
        let ws_url = format!("ws://{addr}/link");
        mock.set_socket_url(&ws_url).await;
        Self {
            mock,
            envelopes: envelope_tx,
            connections,
            ws_handle,
        }
    }

    async fn wait_connected(&self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if *self.connections.lock().await >= 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("runtime connected to socket");
    }

    async fn send_message_event(&self, event_id: &str, ts: &str, thread_ts: Option<&str>, text: &str) {
        let mut event = json!({
            "type": "app_mention",
            "channel": "C1",
            "user": "U1",
            "text": text,
            "ts": ts,
            "channel_type": "channel"
        });
        if let Some(thread_ts) = thread_ts {
            event["thread_ts"] = json!(thread_ts);
        }
        self.envelopes
            .send(json!({
                "envelope_id": format!("env-{event_id}"),
                "type": "events_api",
                "payload": {"team_id": "T1", "event_id": event_id, "event": event}
            }))
            .await
            .unwrap();
    }

    async fn wait_for_post_count(&self, count: usize) -> Vec<Value> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let posts = self.mock.calls_for("chat.postMessage").await;
                if posts.len() >= count {
                    break posts;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("expected chat.postMessage calls")
    }

    async fn stop(self) {
        drop(self.envelopes);
        self.ws_handle.abort();
        self.mock.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn app_mention_flows_through_agent_to_posted_reply() {
    let rig = E2eRig::start().await;
    let workspace = TempDir::new().unwrap();
    let database = Database::connect("sqlite::memory:").await.unwrap();
    let pool = database.pool().clone();

    let runner = StubRunner::new();
    let approval_guard = SmartApprovalGuard::new().with_pool(pool.clone());
    let pairing = SlackPairingStore::new(pool.clone());

    let config = SlackRuntimeConfig {
        bot_token: "xoxb-test".into(),
        app_token: "xapp-test".into(),
        api_base: rig.mock.base.clone(),
        filter: OwnedSlackFilter {
            allow_all_users: true,
            ..OwnedSlackFilter::default()
        },
        processing_reactions: true,
        workspace_root: workspace.path().to_path_buf(),
    };
    let mut runtime = SlackRuntime::new(config, approval_guard, pairing);
    let dispatcher = runtime.egress_dispatcher();
    runner.dispatcher.write().await.replace(dispatcher.clone());
    let multiplexer = SessionMultiplexer::with_dispatcher(
        pool.clone(),
        runner.clone(),
        Some(dispatcher),
        MultiplexerConfig::default(),
    );
    runtime.set_multiplexer(multiplexer.clone());

    let cancel = CancellationToken::new();
    let run_cancel = cancel.clone();
    let run = tokio::spawn(async move { runtime.run(run_cancel).await });

    rig.wait_connected().await;
    rig.send_message_event("Ev1", "100.000001", None, "<@U0BOT> hello agent")
        .await;

    let posts = rig.wait_for_post_count(1).await;
    assert_eq!(posts[0]["channel"], "C1");
    assert_eq!(posts[0]["text"], "agent reply");
    assert_eq!(
        posts[0]["thread_ts"], "100.000001",
        "reply must land in the thread anchored at the triggering message"
    );

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let reactions = rig.mock.calls_for("reactions.add").await;
            if reactions.iter().any(|r| r["name"] == "eyes" && r["timestamp"] == "100.000001") {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("processing reaction recorded");

    rig.send_message_event("Ev2", "100.000002", Some("100.000001"), "follow-up")
        .await;
    rig.wait_for_post_count(2).await;

    let seen = runner.seen.lock().await;
    assert_eq!(seen.len(), 2);
    assert_eq!(
        seen[0].0.storage_key(),
        seen[1].0.storage_key(),
        "thread reply must reuse the root session"
    );
    assert_eq!(seen[0].1, "hello agent");
    assert_eq!(seen[1].1, "follow-up");
    drop(seen);
    assert_eq!(multiplexer.active_sessions(), 1);

    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("runtime exits")
        .expect("runtime joins");
    assert!(result.is_ok(), "graceful shutdown, got {result:?}");
    rig.stop().await;
}
