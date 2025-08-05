use super::*;
use crate::config_man::ConfigManager;
use crate::signaling;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::{timeout, sleep};
use uuid::Uuid;
use tracing_subscriber::EnvFilter;

fn init_test_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,server=debug,collab_mode=debug"))
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
            let result = signaling::server::run_signaling_server(
                &addr_string,
                &db_path_clone,
            ).await;
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
    fn new() -> (Self, mpsc::Sender<lsp_server::Message>, mpsc::Receiver<lsp_server::Message>) {
        let (editor_to_server_tx, editor_to_server_rx) = mpsc::channel(100);
        let (server_to_editor_tx, server_to_editor_rx) = mpsc::channel(100);
        
        let mock = MockEditor {
            tx: editor_to_server_tx,
            rx: server_to_editor_rx,
        };
        
        (mock, server_to_editor_tx, editor_to_server_rx)
    }

    async fn send_notification(&self, method: &str, params: serde_json::Value) -> anyhow::Result<()> {
        let notif = lsp_server::Notification {
            method: method.to_string(),
            params,
        };
        self.tx.send(lsp_server::Message::Notification(notif)).await?;
        Ok(())
    }

    async fn wait_for_notification(&mut self, method: &str, timeout_secs: u64) -> anyhow::Result<serde_json::Value> {
        let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
        
        while std::time::Instant::now() < deadline {
            match timeout(Duration::from_millis(100), self.rx.recv()).await {
                Ok(Some(lsp_server::Message::Notification(notif))) => {
                    tracing::debug!("Received notification: {} with params: {}", notif.method, notif.params);
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
        
        Err(anyhow::anyhow!("Timeout waiting for notification: {}", method))
    }

    async fn expect_hey_notification(&mut self, timeout_secs: u64) -> anyhow::Result<(ServerId, String)> {
        let params = self.wait_for_notification(&NotificationCode::Hey.to_string(), timeout_secs).await?;
        let host_id = params.get("hostId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing hostId in Hey notification"))?
            .to_string();
        let message = params.get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Ok((host_id, message))
    }
}

#[tokio::test]
async fn test_accept_connect() {
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
    mock_editor1.send_notification(
        "AcceptConnection",
        serde_json::json!({
            "hostId": id1.clone(),
            "signalingAddr": env.signaling_url(),
        })
    ).await.unwrap();
    
    let accepting_params = mock_editor1.wait_for_notification(
        &NotificationCode::AcceptingConnection.to_string(), 
        5
    ).await;
    assert!(accepting_params.is_ok(), "Should receive AcceptingConnection notification");
    
    sleep(Duration::from_millis(500)).await;
    
    tracing::info!("Server2 connecting to Server1");
    mock_editor2.send_notification(
        "Connect",
        serde_json::json!({
            "hostId": id1.clone(),
            "signalingAddr": env.signaling_url(),
        })
    ).await.unwrap();
    
    let connecting_params = mock_editor2.wait_for_notification(
        &NotificationCode::Connecting.to_string(),
        5
    ).await;
    assert!(connecting_params.is_ok(), "Should receive Connecting notification");
    
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
                    tracing::info!("Editor1 progress: {}", notif.params.get("message").and_then(|v| v.as_str()).unwrap_or(""));
                } else if notif.method == hey_method {
                    let host_id = notif.params.get("hostId").and_then(|v| v.as_str()).unwrap_or("");
                    let message = notif.params.get("message").and_then(|v| v.as_str()).unwrap_or("");
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
                    tracing::info!("Editor2 progress: {}", notif.params.get("message").and_then(|v| v.as_str()).unwrap_or(""));
                } else if notif.method == hey_method {
                    let host_id = notif.params.get("hostId").and_then(|v| v.as_str()).unwrap_or("");
                    let message = notif.params.get("message").and_then(|v| v.as_str()).unwrap_or("");
                    tracing::info!("Editor2 received Hey from {}: {}", host_id, message);
                    assert_eq!(host_id, id1, "Hey should be from server1");
                    hey_received_count += 1;
                }
            }
            _ => {}
        }
        
        sleep(Duration::from_millis(10)).await;
    }
    
    assert_eq!(hey_received_count, 2, "Should receive Hey messages from both sides indicating connection established");
    
    tracing::info!("Test completed successfully");
    
    server1_handle.abort();
    server2_handle.abort();
}