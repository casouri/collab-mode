use super::*;
use crate::config_man::ConfigManager;
use crate::signaling;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

fn init_test_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,server=debug,collab_mode=debug")),
        )
        .with_test_writer()
        .with_target(true)
        .with_thread_ids(true)
        .with_line_number(true)
        .try_init();
}

struct TestEnvironment {
    signaling_addr: String,
    signaling_task: Option<tokio::task::JoinHandle<()>>,
    temp_dir: tempfile::TempDir,
}

impl TestEnvironment {
    async fn new() -> anyhow::Result<Self> {
        init_test_tracing();

        tracing::info!("Creating new test environment");

        let temp_dir = tempfile::TempDir::new()?;
        let db_path = temp_dir.path().join("test-signal.db");
        tracing::debug!("Test database path: {:?}", db_path);

        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        drop(listener);
        tracing::info!("Found available port: {}", addr);

        let db_path_clone = db_path.clone();
        let addr_string = addr.to_string();
        tracing::info!("Starting signaling server on {}", addr_string);
        let signaling_task = tokio::spawn(async move {
            tracing::debug!("Signaling server task started");
            let result =
                signaling::server::run_signaling_server(&addr_string, &db_path_clone).await;
            if let Err(e) = result {
                tracing::error!("Signaling server error: {}", e);
            }
            tracing::debug!("Signaling server task ended");
        });

        sleep(Duration::from_millis(100)).await;

        Ok(Self {
            signaling_addr: format!("ws://{}", addr),
            signaling_task: Some(signaling_task),
            temp_dir,
        })
    }

    fn signaling_url(&self) -> &str {
        &self.signaling_addr
    }
}

impl Drop for TestEnvironment {
    fn drop(&mut self) {
        tracing::debug!("Dropping test environment");
        if let Some(task) = self.signaling_task.take() {
            tracing::debug!("Aborting signaling server task");
            task.abort();
        }
    }
}

fn create_test_id(prefix: &str) -> ServerId {
    format!("{}-{}", prefix, Uuid::new_v4())
}

struct MockEditor {
    tx: mpsc::Sender<lsp_server::Message>,
    rx: mpsc::Receiver<lsp_server::Message>,
}

impl MockEditor {
    fn new() -> (
        Self,
        mpsc::Sender<lsp_server::Message>,
        mpsc::Receiver<lsp_server::Message>,
    ) {
        let (editor_to_server_tx, editor_to_server_rx) = mpsc::channel(100);
        let (server_to_editor_tx, server_to_editor_rx) = mpsc::channel(100);

        let mock = MockEditor {
            tx: editor_to_server_tx,
            rx: server_to_editor_rx,
        };

        (mock, server_to_editor_tx, editor_to_server_rx)
    }

    async fn send_notification(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<()> {
        let notif = lsp_server::Notification {
            method: method.to_string(),
            params,
        };
        self.tx
            .send(lsp_server::Message::Notification(notif))
            .await?;
        Ok(())
    }

    async fn send_request(
        &self,
        req_id: i32,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<()> {
        let req = lsp_server::Request {
            id: lsp_server::RequestId::from(req_id),
            method: method.to_string(),
            params,
        };
        self.tx.send(lsp_server::Message::Request(req)).await?;
        Ok(())
    }

    async fn send_response(
        &self,
        req_id: lsp_server::RequestId,
        result: serde_json::Value,
    ) -> anyhow::Result<()> {
        let resp = lsp_server::Response {
            id: req_id,
            result: Some(result),
            error: None,
        };
        self.tx.send(lsp_server::Message::Response(resp)).await?;
        Ok(())
    }

    async fn send_error_response(
        &self,
        req_id: lsp_server::RequestId,
        code: i32,
        message: String,
    ) -> anyhow::Result<()> {
        let resp = lsp_server::Response {
            id: req_id,
            result: None,
            error: Some(lsp_server::ResponseError {
                code,
                message,
                data: None,
            }),
        };
        self.tx.send(lsp_server::Message::Response(resp)).await?;
        Ok(())
    }

    async fn wait_for_notification(
        &mut self,
        method: &str,
        timeout_secs: u64,
    ) -> anyhow::Result<serde_json::Value> {
        let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);

        while std::time::Instant::now() < deadline {
            match timeout(Duration::from_millis(100), self.rx.recv()).await {
                Ok(Some(lsp_server::Message::Notification(notif))) => {
                    tracing::debug!(
                        "Received notification: {} with params: {}",
                        notif.method,
                        notif.params
                    );
                    if notif.method == method {
                        return Ok(notif.params);
                    }
                }
                Ok(Some(msg)) => {
                    tracing::debug!("Received non-notification message: {:?}", msg);
                }
                Ok(None) => {
                    return Err(anyhow::anyhow!("Channel closed"));
                }
                Err(_) => {
                    continue;
                }
            }
        }

        Err(anyhow::anyhow!(
            "Timeout waiting for notification: {}",
            method
        ))
    }

    async fn wait_for_request(
        &mut self,
        method: &str,
        timeout_secs: u64,
    ) -> anyhow::Result<(lsp_server::RequestId, serde_json::Value)> {
        let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);

        while std::time::Instant::now() < deadline {
            match timeout(Duration::from_millis(100), self.rx.recv()).await {
                Ok(Some(lsp_server::Message::Request(req))) => {
                    tracing::debug!(
                        "Received request: {} with params: {}",
                        req.method,
                        req.params
                    );
                    if req.method == method {
                        return Ok((req.id, req.params));
                    }
                }
                Ok(Some(msg)) => {
                    tracing::debug!("Received non-request message: {:?}", msg);
                }
                Ok(None) => {
                    return Err(anyhow::anyhow!("Channel closed"));
                }
                Err(_) => {
                    continue;
                }
            }
        }

        Err(anyhow::anyhow!("Timeout waiting for request: {}", method))
    }

    async fn wait_for_response(
        &mut self,
        req_id: i32,
        timeout_secs: u64,
    ) -> anyhow::Result<serde_json::Value> {
        let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
        let expected_id = lsp_server::RequestId::from(req_id);

        while std::time::Instant::now() < deadline {
            match timeout(Duration::from_millis(100), self.rx.recv()).await {
                Ok(Some(lsp_server::Message::Response(resp))) => {
                    tracing::debug!("Received response: {:?}", resp);
                    if resp.id == expected_id {
                        if let Some(error) = resp.error {
                            return Err(anyhow::anyhow!(
                                "Response error: {} - {}",
                                error.code,
                                error.message
                            ));
                        }
                        return Ok(resp.result.unwrap_or(serde_json::Value::Null));
                    }
                }
                Ok(Some(msg)) => {
                    tracing::debug!("Received non-response message: {:?}", msg);
                }
                Ok(None) => {
                    return Err(anyhow::anyhow!("Channel closed"));
                }
                Err(_) => {
                    continue;
                }
            }
        }

        Err(anyhow::anyhow!(
            "Timeout waiting for response to request {}",
            req_id
        ))
    }

    async fn expect_hey_notification(
        &mut self,
        timeout_secs: u64,
    ) -> anyhow::Result<(ServerId, String)> {
        let params = self
            .wait_for_notification(&NotificationCode::Hey.to_string(), timeout_secs)
            .await?;
        let host_id = params
            .get("hostId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing hostId in Hey notification"))?
            .to_string();
        let message = params
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Ok((host_id, message))
    }
}

struct ServerSetup {
    id: ServerId,
    handle: tokio::task::JoinHandle<()>,
    editor: MockEditor,
    _temp_dir: tempfile::TempDir,
}

struct HubAndSpokeSetup {
    hub: ServerSetup,
    spokes: Vec<ServerSetup>,
}

impl HubAndSpokeSetup {
    fn cleanup(self) {
        self.hub.handle.abort();
        for spoke in self.spokes {
            spoke.handle.abort();
        }
    }
}

/// Creates a hub-and-spoke topology with one central server and multiple spoke servers.
/// The hub server accepts connections and all spoke servers connect to it.
/// Returns the hub and spoke setups with established connections.
async fn setup_hub_and_spoke_servers(
    env: &TestEnvironment,
    num_spokes: usize,
) -> anyhow::Result<HubAndSpokeSetup> {
    // Create hub server
    let hub_id = create_test_id("hub");
    let hub_temp_dir = tempfile::TempDir::new()?;
    let hub_config = ConfigManager::new(Some(hub_temp_dir.path().to_string_lossy().to_string()));
    let mut hub_server = Server::new(hub_id.clone(), true, hub_config)?;

    let (mut hub_editor, hub_tx, hub_rx) = MockEditor::new();

    let hub_handle = tokio::spawn(async move {
        if let Err(e) = hub_server.run(hub_tx, hub_rx).await {
            tracing::error!("Hub server error: {}", e);
        }
    });

    sleep(Duration::from_millis(100)).await;

    // Hub starts accepting connections
    tracing::info!("Hub server {} starting to accept connections", hub_id);
    hub_editor
        .send_notification(
            "AcceptConnection",
            serde_json::json!({
                "hostId": hub_id.clone(),
                "signalingAddr": env.signaling_url(),
                "transportType": "SCTP",
            }),
        )
        .await?;

    let _ = hub_editor
        .wait_for_notification(&NotificationCode::AcceptingConnection.to_string(), 5)
        .await?;

    sleep(Duration::from_millis(500)).await;

    // Create and connect spoke servers
    let mut spokes = Vec::new();

    for i in 0..num_spokes {
        let spoke_id = create_test_id(&format!("spoke{}", i + 1));
        let spoke_temp_dir = tempfile::TempDir::new()?;
        let spoke_config =
            ConfigManager::new(Some(spoke_temp_dir.path().to_string_lossy().to_string()));
        let mut spoke_server = Server::new(spoke_id.clone(), true, spoke_config)?;

        let (mut spoke_editor, spoke_tx, spoke_rx) = MockEditor::new();

        let spoke_handle = tokio::spawn(async move {
            if let Err(e) = spoke_server.run(spoke_tx, spoke_rx).await {
                tracing::error!("Spoke server {} error: {}", i + 1, e);
            }
        });

        sleep(Duration::from_millis(100)).await;

        // Spoke connects to hub
        tracing::info!("Spoke server {} connecting to hub {}", spoke_id, hub_id);
        spoke_editor
            .send_notification(
                "Connect",
                serde_json::json!({
                    "hostId": hub_id.clone(),
                    "signalingAddr": env.signaling_url(),
                    "transportType": "SCTP",
                }),
            )
            .await?;

        let _ = spoke_editor
            .wait_for_notification(&NotificationCode::Connecting.to_string(), 5)
            .await?;

        spokes.push(ServerSetup {
            id: spoke_id,
            handle: spoke_handle,
            editor: spoke_editor,
            _temp_dir: spoke_temp_dir,
        });
    }

    // Wait for all connections to be established
    tracing::info!("Waiting for all connections to be established");

    // Hub should receive Hey from each spoke
    for i in 0..num_spokes {
        let (host_id, _message) = hub_editor.expect_hey_notification(10).await?;
        tracing::info!("Hub received Hey from spoke: {}", host_id);
    }

    // Each spoke should receive Hey from hub
    for spoke in &mut spokes {
        let (host_id, _message) = spoke.editor.expect_hey_notification(10).await?;
        tracing::info!("Spoke {} received Hey from hub: {}", spoke.id, host_id);
    }

    Ok(HubAndSpokeSetup {
        hub: ServerSetup {
            id: hub_id,
            handle: hub_handle,
            editor: hub_editor,
            _temp_dir: hub_temp_dir,
        },
        spokes,
    })
}

#[tokio::test]
async fn test_accept_connect() {
    // Test: Basic connection establishment between two servers
    // Flow:
    // 1. Editor1 tells Server1 to accept connections on signaling server
    // 2. Editor2 tells Server2 to connect to Server1 via signaling server
    // 3. Both servers establish WebRTC connection through ICE/DTLS/SCTP
    // 4. Once connected, servers exchange "Hey" messages
    // 5. Each editor receives a Hey notification from the remote server
    // Expected: Both editors receive Hey messages indicating successful connection

    let env = TestEnvironment::new().await.unwrap();

    let id1 = create_test_id("server1");
    let id2 = create_test_id("server2");

    tracing::info!("Creating servers with IDs: {} and {}", id1, id2);

    let temp_dir1 = tempfile::TempDir::new().unwrap();
    let config1 = ConfigManager::new(Some(temp_dir1.path().to_string_lossy().to_string()));
    let mut server1 = Server::new(id1.clone(), true, config1).unwrap();

    let temp_dir2 = tempfile::TempDir::new().unwrap();
    let config2 = ConfigManager::new(Some(temp_dir2.path().to_string_lossy().to_string()));
    let mut server2 = Server::new(id2.clone(), true, config2).unwrap();

    let (mut mock_editor1, server_tx1, server_rx1) = MockEditor::new();
    let (mut mock_editor2, server_tx2, server_rx2) = MockEditor::new();

    let server1_handle = {
        tokio::spawn(async move {
            if let Err(e) = server1.run(server_tx1, server_rx1).await {
                tracing::error!("Server1 error: {}", e);
            }
        })
    };

    let server2_handle = {
        tokio::spawn(async move {
            if let Err(e) = server2.run(server_tx2, server_rx2).await {
                tracing::error!("Server2 error: {}", e);
            }
        })
    };

    sleep(Duration::from_millis(100)).await;

    tracing::info!("Server1 starting to accept connections");
    mock_editor1
        .send_notification(
            "AcceptConnection",
            serde_json::json!({
                "hostId": id1.clone(),
                "signalingAddr": env.signaling_url(),
                "transportType": "SCTP",
            }),
        )
        .await
        .unwrap();

    let accepting_params = mock_editor1
        .wait_for_notification(&NotificationCode::AcceptingConnection.to_string(), 5)
        .await;
    assert!(
        accepting_params.is_ok(),
        "Should receive AcceptingConnection notification"
    );

    sleep(Duration::from_millis(500)).await;

    tracing::info!("Server2 connecting to Server1");
    mock_editor2
        .send_notification(
            "Connect",
            serde_json::json!({
                "hostId": id1.clone(),
                "signalingAddr": env.signaling_url(),
                "transportType": "SCTP",
            }),
        )
        .await
        .unwrap();

    let connecting_params = mock_editor2
        .wait_for_notification(&NotificationCode::Connecting.to_string(), 5)
        .await;
    assert!(
        connecting_params.is_ok(),
        "Should receive Connecting notification"
    );

    tracing::info!("Waiting for connection to be established");

    // Wait for Hey messages from both sides
    let timeout_duration = Duration::from_secs(30);
    let start_time = std::time::Instant::now();

    let mut hey_received_count = 0;
    let connection_progress_method = NotificationCode::ConnectionProgress.to_string();
    let hey_method = NotificationCode::Hey.to_string();

    while hey_received_count < 2 && start_time.elapsed() < timeout_duration {
        // Check editor1 for messages
        match timeout(Duration::from_millis(100), mock_editor1.rx.recv()).await {
            Ok(Some(lsp_server::Message::Notification(notif))) => {
                if notif.method == connection_progress_method {
                    tracing::info!(
                        "Editor1 progress: {}",
                        notif
                            .params
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                    );
                } else if notif.method == hey_method {
                    let host_id = notif
                        .params
                        .get("hostId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let message = notif
                        .params
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    tracing::info!("Editor1 received Hey from {}: {}", host_id, message);
                    assert_eq!(host_id, id2, "Hey should be from server2");
                    hey_received_count += 1;
                }
            }
            _ => {}
        }

        // Check editor2 for messages
        match timeout(Duration::from_millis(100), mock_editor2.rx.recv()).await {
            Ok(Some(lsp_server::Message::Notification(notif))) => {
                if notif.method == connection_progress_method {
                    tracing::info!(
                        "Editor2 progress: {}",
                        notif
                            .params
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                    );
                } else if notif.method == hey_method {
                    let host_id = notif
                        .params
                        .get("hostId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let message = notif
                        .params
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    tracing::info!("Editor2 received Hey from {}: {}", host_id, message);
                    assert_eq!(host_id, id1, "Hey should be from server1");
                    hey_received_count += 1;
                }
            }
            _ => {}
        }

        sleep(Duration::from_millis(10)).await;
    }

    assert_eq!(
        hey_received_count, 2,
        "Should receive Hey messages from both sides indicating connection established"
    );

    tracing::info!("Test completed successfully");

    server1_handle.abort();
    server2_handle.abort();
}
