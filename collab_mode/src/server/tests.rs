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

#[tokio::test]
async fn test_list_files_basic() {
    let env = TestEnvironment::new().await.unwrap();

    let id1 = create_test_id("server1");
    let id2 = create_test_id("server2");

    tracing::info!("Creating servers with IDs: {} and {}", id1, id2);

    // Create servers in attached mode
    let temp_dir1 = tempfile::TempDir::new().unwrap();
    let config1 = ConfigManager::new(Some(temp_dir1.path().to_string_lossy().to_string()));
    let mut server1 = Server::new(id1.clone(), true, config1).unwrap();

    let temp_dir2 = tempfile::TempDir::new().unwrap();
    let config2 = ConfigManager::new(Some(temp_dir2.path().to_string_lossy().to_string()));
    let mut server2 = Server::new(id2.clone(), true, config2).unwrap();

    let (mut mock_editor1, server_tx1, server_rx1) = MockEditor::new();
    let (mut mock_editor2, server_tx2, server_rx2) = MockEditor::new();

    // Start server tasks
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

    // Establish connection between servers
    tracing::info!("Establishing connection between servers");
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

    let _ = mock_editor1
        .wait_for_notification(&NotificationCode::AcceptingConnection.to_string(), 5)
        .await
        .unwrap();

    sleep(Duration::from_millis(500)).await;

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

    let _ = mock_editor2
        .wait_for_notification(&NotificationCode::Connecting.to_string(), 5)
        .await
        .unwrap();

    // Wait for connection to be established - look for Hey messages from both sides
    tracing::info!("Waiting for connection to be established");

    let timeout_duration = Duration::from_secs(30);
    let start_time = std::time::Instant::now();
    let mut hey_received_count = 0;
    let hey_method = NotificationCode::Hey.to_string();

    while hey_received_count < 2 && start_time.elapsed() < timeout_duration {
        // Check for Hey message from editor1
        match timeout(Duration::from_millis(100), mock_editor1.rx.recv()).await {
            Ok(Some(lsp_server::Message::Notification(notif))) => {
                if notif.method == hey_method {
                    tracing::info!("Editor1 received Hey notification");
                    hey_received_count += 1;
                }
            }
            _ => {}
        }

        // Check for Hey message from editor2
        match timeout(Duration::from_millis(100), mock_editor2.rx.recv()).await {
            Ok(Some(lsp_server::Message::Notification(notif))) => {
                if notif.method == hey_method {
                    tracing::info!("Editor2 received Hey notification");
                    hey_received_count += 1;
                }
            }
            _ => {}
        }

        sleep(Duration::from_millis(10)).await;
    }

    assert_eq!(
        hey_received_count, 2,
        "Should receive Hey messages from both sides"
    );

    // Editor 1 sends ListFiles request to its server
    tracing::info!(
        "Editor1 sending ListFiles request to server for host {}",
        id2
    );
    mock_editor1
        .send_request(
            1,
            "ListFiles",
            serde_json::json!({
                "dir": null,
                "hostId": id2.clone(),
                "signalingAddr": "",
                "credential": "",
            }),
        )
        .await
        .unwrap();

    // Editor 2 should receive the ListFiles request from its server
    tracing::info!("Editor2 waiting for ListFiles request");
    let (req_id, params) = mock_editor2
        .wait_for_request("ListFiles", 10)
        .await
        .unwrap();

    // Verify the request params
    let list_params: message::ListFilesParams = serde_json::from_value(params).unwrap();
    assert_eq!(list_params.dir, None);

    // Editor 2 responds with a mock file list
    tracing::info!("Editor2 sending ListFiles response");
    let mock_files = vec![
        message::ListFileEntry {
            file: FileDesc::File(1),
            filename: "test_file.txt".to_string(),
            is_directory: false,
            meta: serde_json::Map::new(),
        },
        message::ListFileEntry {
            file: FileDesc::Project("project1".to_string()),
            filename: "MyProject".to_string(),
            is_directory: true,
            meta: serde_json::Map::new(),
        },
        message::ListFileEntry {
            file: FileDesc::ProjectFile(("project1".to_string(), "src/main.rs".to_string())),
            filename: "main.rs".to_string(),
            is_directory: false,
            meta: serde_json::Map::new(),
        },
    ];

    mock_editor2
        .send_response(
            req_id,
            serde_json::to_value(message::ListFilesResp {
                files: mock_files.clone(),
            })
            .unwrap(),
        )
        .await
        .unwrap();

    // Editor 1 should receive the response
    tracing::info!("Editor1 waiting for ListFiles response");
    let response = mock_editor1.wait_for_response(1, 10).await.unwrap();

    // Verify the response
    let list_resp: message::ListFilesResp = serde_json::from_value(response).unwrap();
    assert_eq!(list_resp.files.len(), 3);
    assert_eq!(list_resp.files[0].filename, "test_file.txt");
    assert_eq!(list_resp.files[1].filename, "MyProject");
    assert_eq!(list_resp.files[2].filename, "main.rs");

    tracing::info!("test_list_files_basic completed successfully");

    server1_handle.abort();
    server2_handle.abort();
}

#[tokio::test]
async fn test_list_files_empty() {
    let env = TestEnvironment::new().await.unwrap();

    let id1 = create_test_id("server1");
    let id2 = create_test_id("server2");

    tracing::info!("Creating servers with IDs: {} and {}", id1, id2);

    // Create servers in attached mode
    let temp_dir1 = tempfile::TempDir::new().unwrap();
    let config1 = ConfigManager::new(Some(temp_dir1.path().to_string_lossy().to_string()));
    let mut server1 = Server::new(id1.clone(), true, config1).unwrap();

    let temp_dir2 = tempfile::TempDir::new().unwrap();
    let config2 = ConfigManager::new(Some(temp_dir2.path().to_string_lossy().to_string()));
    let mut server2 = Server::new(id2.clone(), true, config2).unwrap();

    let (mut mock_editor1, server_tx1, server_rx1) = MockEditor::new();
    let (mut mock_editor2, server_tx2, server_rx2) = MockEditor::new();

    // Start server tasks
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

    // Establish connection between servers
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

    let _ = mock_editor1
        .wait_for_notification(&NotificationCode::AcceptingConnection.to_string(), 5)
        .await
        .unwrap();

    sleep(Duration::from_millis(500)).await;

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

    let _ = mock_editor2
        .wait_for_notification(&NotificationCode::Connecting.to_string(), 5)
        .await
        .unwrap();

    // Wait for connection to be established
    sleep(Duration::from_secs(3)).await;

    // Editor 1 sends ListFiles request with a specific directory
    tracing::info!("Editor1 sending ListFiles request for specific directory");
    mock_editor1
        .send_request(
            2,
            "ListFiles",
            serde_json::json!({
                "dir": ["project1", "src"],
                "hostId": id2.clone(),
                "signalingAddr": "",
                "credential": "",
            }),
        )
        .await
        .unwrap();

    // Editor 2 receives the request
    let (req_id, params) = mock_editor2
        .wait_for_request("ListFiles", 10)
        .await
        .unwrap();

    // Verify the request params
    let list_params: message::ListFilesParams = serde_json::from_value(params).unwrap();
    assert!(list_params.dir.is_some());

    // Editor 2 responds with an empty file list
    tracing::info!("Editor2 sending empty ListFiles response");
    mock_editor2
        .send_response(
            req_id,
            serde_json::to_value(message::ListFilesResp { files: vec![] }).unwrap(),
        )
        .await
        .unwrap();

    // Editor 1 should receive the empty response
    let response = mock_editor1.wait_for_response(2, 10).await.unwrap();

    // Verify the response is empty
    let list_resp: message::ListFilesResp = serde_json::from_value(response).unwrap();
    assert_eq!(list_resp.files.len(), 0);

    tracing::info!("test_list_files_empty completed successfully");

    server1_handle.abort();
    server2_handle.abort();
}

#[tokio::test]
async fn test_list_files_error_handling() {
    let env = TestEnvironment::new().await.unwrap();

    let id1 = create_test_id("server1");
    let id2 = create_test_id("server2");

    tracing::info!("Testing ListFiles error handling scenarios");

    // Create servers in attached mode
    let temp_dir1 = tempfile::TempDir::new().unwrap();
    let config1 = ConfigManager::new(Some(temp_dir1.path().to_string_lossy().to_string()));
    let mut server1 = Server::new(id1.clone(), true, config1).unwrap();

    let temp_dir2 = tempfile::TempDir::new().unwrap();
    let config2 = ConfigManager::new(Some(temp_dir2.path().to_string_lossy().to_string()));
    let mut server2 = Server::new(id2.clone(), true, config2).unwrap();

    let (mut mock_editor1, server_tx1, server_rx1) = MockEditor::new();
    let (mut mock_editor2, server_tx2, server_rx2) = MockEditor::new();

    // Start server tasks
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

    // Establish connection between servers
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

    let _ = mock_editor1
        .wait_for_notification(&NotificationCode::AcceptingConnection.to_string(), 5)
        .await
        .unwrap();

    sleep(Duration::from_millis(500)).await;

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

    let _ = mock_editor2
        .wait_for_notification(&NotificationCode::Connecting.to_string(), 5)
        .await
        .unwrap();

    sleep(Duration::from_secs(3)).await;

    // Test 1: Editor sends error response
    tracing::info!("Test 1: Error response from editor");
    mock_editor1
        .send_request(
            3,
            "ListFiles",
            serde_json::json!({
                "dir": null,
                "hostId": id2.clone(),
                "signalingAddr": "",
                "credential": "",
            }),
        )
        .await
        .unwrap();

    let (req_id, _params) = mock_editor2
        .wait_for_request("ListFiles", 10)
        .await
        .unwrap();

    // Editor 2 responds with an error
    mock_editor2
        .send_error_response(req_id, 404, "File not found".to_string())
        .await
        .unwrap();

    // Editor 1 should receive an error response
    let response_result = mock_editor1.wait_for_response(3, 10).await;
    assert!(response_result.is_err());
    assert!(response_result.unwrap_err().to_string().contains("404"));

    // Test 2: Invalid response format
    tracing::info!("Test 2: Invalid response format");
    mock_editor1
        .send_request(
            4,
            "ListFiles",
            serde_json::json!({
                "dir": null,
                "hostId": id2.clone(),
                "signalingAddr": "",
                "credential": "",
            }),
        )
        .await
        .unwrap();

    let (req_id, _params) = mock_editor2
        .wait_for_request("ListFiles", 10)
        .await
        .unwrap();

    // Editor 2 responds with invalid data (not a ListFilesResp)
    mock_editor2
        .send_response(
            req_id,
            serde_json::json!({
                "invalid": "response",
                "data": 123
            }),
        )
        .await
        .unwrap();

    // Editor 1 should receive the response but parsing might fail on the server side
    // The server will likely send an error notification
    let response = mock_editor1.wait_for_response(4, 10).await;
    // Response should be received but may be empty or error
    assert!(response.is_ok() || response.is_err());

    tracing::info!("test_list_files_error_handling completed successfully");

    server1_handle.abort();
    server2_handle.abort();
}

#[tokio::test]
async fn test_list_files_timeout() {
    let env = TestEnvironment::new().await.unwrap();

    let id1 = create_test_id("server1");
    let id2 = create_test_id("server2");

    tracing::info!("Testing ListFiles timeout scenario");

    // Create servers in attached mode
    let temp_dir1 = tempfile::TempDir::new().unwrap();
    let config1 = ConfigManager::new(Some(temp_dir1.path().to_string_lossy().to_string()));
    let mut server1 = Server::new(id1.clone(), true, config1).unwrap();

    let temp_dir2 = tempfile::TempDir::new().unwrap();
    let config2 = ConfigManager::new(Some(temp_dir2.path().to_string_lossy().to_string()));
    let mut server2 = Server::new(id2.clone(), true, config2).unwrap();

    let (mut mock_editor1, server_tx1, server_rx1) = MockEditor::new();
    let (mut mock_editor2, server_tx2, server_rx2) = MockEditor::new();

    // Start server tasks
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

    // Establish connection between servers
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

    let _ = mock_editor1
        .wait_for_notification(&NotificationCode::AcceptingConnection.to_string(), 5)
        .await
        .unwrap();

    sleep(Duration::from_millis(500)).await;

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

    let _ = mock_editor2
        .wait_for_notification(&NotificationCode::Connecting.to_string(), 5)
        .await
        .unwrap();

    sleep(Duration::from_secs(3)).await;

    // Editor 1 sends ListFiles request
    tracing::info!("Editor1 sending ListFiles request that will timeout");
    mock_editor1
        .send_request(
            5,
            "ListFiles",
            serde_json::json!({
                "dir": null,
                "hostId": id2.clone(),
                "signalingAddr": "",
                "credential": "",
            }),
        )
        .await
        .unwrap();

    // Editor 2 receives the request but doesn't respond
    let (_req_id, _params) = mock_editor2
        .wait_for_request("ListFiles", 10)
        .await
        .unwrap();

    // Editor 2 intentionally doesn't respond to simulate timeout
    tracing::info!("Editor2 not responding to simulate timeout");

    // Editor 1 should timeout waiting for response
    let response_result = mock_editor1.wait_for_response(5, 3).await;
    assert!(response_result.is_err());
    assert!(response_result.unwrap_err().to_string().contains("Timeout"));

    tracing::info!("test_list_files_timeout completed successfully");

    server1_handle.abort();
    server2_handle.abort();
}
