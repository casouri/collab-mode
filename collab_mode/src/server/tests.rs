use super::*;
use crate::config_man::ConfigManager;
use crate::signaling;
use std::time::Duration;
// use tokio::net::TcpListener;
use rand::Rng;
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

fn get_random_port() -> u16 {
    let mut rng = rand::thread_rng();
    // Ephemeral port range (49152-65535)
    rng.gen_range(49152..=65535)
}

impl TestEnvironment {
    async fn new() -> anyhow::Result<Self> {
        init_test_tracing();

        tracing::info!("Creating new test environment");

        let temp_dir = tempfile::TempDir::new()?;
        let db_path = temp_dir.path().join("test-signal.db");
        tracing::debug!("Test database path: {:?}", db_path);

        let port = get_random_port();
        let addr = format!("127.0.0.1:{}", port);
        // let listener = TcpListener::bind("127.0.0.1:0").await?;
        // let addr = listener.local_addr()?;
        // drop(listener);
        // tracing::info!("Found available port: {}", addr);

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

    sleep(Duration::from_millis(1000)).await;

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

/// Creates a test project directory with some sample files
fn create_test_project() -> anyhow::Result<tempfile::TempDir> {
    let temp_dir = tempfile::TempDir::new()?;
    let project_path = temp_dir.path();

    // Create test files
    std::fs::write(project_path.join("test.txt"), "Hello from test.txt")?;
    std::fs::write(
        project_path.join("readme.md"),
        "# Test Project\nThis is a test",
    )?;

    // Create a subdirectory with files
    let subdir = project_path.join("src");
    std::fs::create_dir(&subdir)?;
    std::fs::write(
        subdir.join("main.rs"),
        "fn main() {\n    println!(\"Hello\");\n}",
    )?;
    std::fs::write(
        subdir.join("lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}",
    )?;

    Ok(temp_dir)
}

/// Creates a complex project directory with nested structure
fn create_complex_project() -> anyhow::Result<tempfile::TempDir> {
    let temp_dir = tempfile::TempDir::new()?;
    let project_path = temp_dir.path();

    // Root level files
    std::fs::write(project_path.join("README.md"), "# Complex Project")?;
    std::fs::write(
        project_path.join("Cargo.toml"),
        "[package]\nname = \"test\"",
    )?;
    std::fs::write(project_path.join(".gitignore"), "target/\n*.swp")?;

    // src directory
    let src_dir = project_path.join("src");
    std::fs::create_dir(&src_dir)?;
    std::fs::write(src_dir.join("main.rs"), "fn main() {}")?;
    std::fs::write(src_dir.join("lib.rs"), "pub mod modules;")?;

    // src/modules directory
    let modules_dir = src_dir.join("modules");
    std::fs::create_dir(&modules_dir)?;
    std::fs::write(modules_dir.join("mod1.rs"), "pub fn func1() {}")?;
    std::fs::write(modules_dir.join("mod2.rs"), "pub fn func2() {}")?;
    std::fs::write(modules_dir.join("mod.rs"), "pub mod mod1;\npub mod mod2;")?;

    // tests directory
    let tests_dir = project_path.join("tests");
    std::fs::create_dir(&tests_dir)?;
    std::fs::write(
        tests_dir.join("integration_test.rs"),
        "#[test]\nfn test() {}",
    )?;

    // docs directory
    let docs_dir = project_path.join("docs");
    std::fs::create_dir(&docs_dir)?;
    std::fs::write(docs_dir.join("api.md"), "# API Documentation")?;
    std::fs::write(docs_dir.join("guide.md"), "# User Guide")?;

    // Empty directory
    let empty_dir = project_path.join("empty");
    std::fs::create_dir(&empty_dir)?;

    Ok(temp_dir)
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

#[tokio::test]
async fn test_open_file_basic() {
    // Test: Basic file opening from local editor
    // Flow:
    // 1. Create a test project with files
    // 2. Server declares the project
    // 3. Editor requests to open a file from the project
    // 4. Server opens the file and returns content
    // Expected: Editor receives file content and metadata

    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 0).await.unwrap();

    // Create test project
    let project_dir = create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Hub declares project
    setup
        .hub
        .editor
        .send_notification(
            "DeclareProjects",
            serde_json::json!({
                "projects": [{
                    "filename": project_path.clone(),
                    "name": "TestProject",
                    "meta": {}
                }]
            }),
        )
        .await
        .unwrap();

    sleep(Duration::from_millis(100)).await;

    // Request to open a file
    let req_id = 1;
    setup
        .hub
        .editor
        .send_request(
            req_id,
            "OpenFile",
            serde_json::json!({
                "hostId": setup.hub.id.clone(),
                "fileDesc": {
                    "type": "projectFile",
                    "project": project_path.clone(),
                    "file": "test.txt"
                }
            }),
        )
        .await
        .unwrap();

    // Wait for response
    let resp = setup.hub.editor.wait_for_response(req_id, 5).await.unwrap();

    // Verify response
    assert_eq!(resp["content"], "Hello from test.txt");
    assert_eq!(resp["filename"], "test.txt");
    assert!(resp["docId"].is_number());
    assert!(resp["siteId"].is_number());

    tracing::info!("test_open_file_basic completed successfully");
    setup.cleanup();
}

#[tokio::test]
async fn test_open_file_from_remote() {
    // Test: Remote server requests file from hub
    // Flow:
    // 1. Hub declares a project with files
    // 2. Spoke connects to hub
    // 3. Spoke requests to open a file from hub's project
    // 4. Hub serves the file content to spoke
    // Expected: Spoke receives file content from hub

    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 1).await.unwrap();

    // Create test project
    let project_dir = create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Hub declares project
    setup
        .hub
        .editor
        .send_notification(
            "DeclareProjects",
            serde_json::json!({
                "projects": [{
                    "filename": project_path.clone(),
                    "name": "TestProject",
                    "meta": {}
                }]
            }),
        )
        .await
        .unwrap();

    sleep(Duration::from_millis(100)).await;

    // Spoke requests to open a file from hub
    let req_id = 1;
    setup.spokes[0]
        .editor
        .send_request(
            req_id,
            "OpenFile",
            serde_json::json!({
                "hostId": setup.hub.id.clone(),
                "fileDesc": {
                    "type": "projectFile",
                    "project": project_path.clone(),
                    "file": "src/main.rs"
                }
            }),
        )
        .await
        .unwrap();

    // Wait for response
    let resp = setup.spokes[0]
        .editor
        .wait_for_response(req_id, 5)
        .await
        .unwrap();

    // Verify response
    assert_eq!(resp["content"], "fn main() {\n    println!(\"Hello\");\n}");
    assert_eq!(resp["filename"], "main.rs");
    assert!(resp["docId"].is_number());
    assert!(resp["siteId"].is_number());

    tracing::info!("test_open_file_from_remote completed successfully");
    setup.cleanup();
}

#[tokio::test]
async fn test_open_file_not_found() {
    // Test: Handle file not found error
    // Flow:
    // 1. Create a project
    // 2. Request to open a non-existent file
    // Expected: Error response with FileNotFound code

    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 0).await.unwrap();

    // Create test project
    let project_dir = create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Hub declares project
    setup
        .hub
        .editor
        .send_notification(
            "DeclareProjects",
            serde_json::json!({
                "projects": [{
                    "filename": project_path.clone(),
                    "name": "TestProject",
                    "meta": {}
                }]
            }),
        )
        .await
        .unwrap();

    sleep(Duration::from_millis(100)).await;

    // Request to open a non-existent file
    let req_id = 1;
    setup
        .hub
        .editor
        .send_request(
            req_id,
            "OpenFile",
            serde_json::json!({
                "hostId": setup.hub.id.clone(),
                "fileDesc": {
                    "type": "projectFile",
                    "project": project_path.clone(),
                    "file": "non_existent.txt"
                }
            }),
        )
        .await
        .unwrap();

    // Wait for error response
    let result = setup.hub.editor.wait_for_response(req_id, 5).await;
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("112") || err_msg.contains("Failed to read file"));

    tracing::info!("test_open_file_not_found completed successfully");
    setup.cleanup();
}

#[tokio::test]
async fn test_open_file_bad_request() {
    // Test: Handle bad request (trying to open a directory)
    // Flow:
    // 1. Request to open a Project (directory) instead of a file
    // Expected: Error response with BadRequest code

    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 0).await.unwrap();

    // Create test project
    let project_dir = create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Request to open a project directory
    let req_id = 1;
    setup
        .hub
        .editor
        .send_request(
            req_id,
            "OpenFile",
            serde_json::json!({
                "hostId": setup.hub.id.clone(),
                "fileDesc": {
                    "type": "project",
                    "id": project_path.clone()
                }
            }),
        )
        .await
        .unwrap();

    // Wait for error response
    let result = setup.hub.editor.wait_for_response(req_id, 5).await;
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("113") || err_msg.contains("Cannot open a project directory"));

    tracing::info!("test_open_file_bad_request completed successfully");
    setup.cleanup();
}

#[tokio::test]
async fn test_open_file_already_open() {
    // Test: Handle already opened file
    // Flow:
    // 1. Open a file successfully
    // 2. Try to open the same file again
    // Expected: Should return the same content without error

    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 0).await.unwrap();

    // Create test project
    let project_dir = create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Hub declares project
    setup
        .hub
        .editor
        .send_notification(
            "DeclareProjects",
            serde_json::json!({
                "projects": [{
                    "filename": project_path.clone(),
                    "name": "TestProject",
                    "meta": {}
                }]
            }),
        )
        .await
        .unwrap();

    sleep(Duration::from_millis(100)).await;

    // First request to open a file
    let req_id = 1;
    setup
        .hub
        .editor
        .send_request(
            req_id,
            "OpenFile",
            serde_json::json!({
                "hostId": setup.hub.id.clone(),
                "fileDesc": {
                    "type": "projectFile",
                    "project": project_path.clone(),
                    "file": "test.txt"
                }
            }),
        )
        .await
        .unwrap();

    let resp1 = setup.hub.editor.wait_for_response(req_id, 5).await.unwrap();
    let doc_id1 = resp1["docId"].as_u64().unwrap();

    // Second request to open the same file
    let req_id = 2;
    setup
        .hub
        .editor
        .send_request(
            req_id,
            "OpenFile",
            serde_json::json!({
                "hostId": setup.hub.id.clone(),
                "fileDesc": {
                    "type": "projectFile",
                    "project": project_path.clone(),
                    "file": "test.txt"
                }
            }),
        )
        .await
        .unwrap();

    let resp2 = setup.hub.editor.wait_for_response(req_id, 5).await.unwrap();
    let doc_id2 = resp2["docId"].as_u64().unwrap();

    // Should return the same doc_id
    assert_eq!(doc_id1, doc_id2);
    assert_eq!(resp2["content"], "Hello from test.txt");

    tracing::info!("test_open_file_already_open completed successfully");
    setup.cleanup();
}

#[tokio::test]
async fn test_open_file_doc_id_not_found() {
    // Test: Handle request for non-existent doc by ID
    // Flow:
    // 1. Request to open a file by doc_id that doesn't exist
    // Expected: Error response with FileNotFound code

    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 0).await.unwrap();

    // Request to open a non-existent doc by ID
    let req_id = 1;
    setup
        .hub
        .editor
        .send_request(
            req_id,
            "OpenFile",
            serde_json::json!({
                "hostId": setup.hub.id.clone(),
                "fileDesc": {
                    "type": "file",
                    "id": 9999
                }
            }),
        )
        .await
        .unwrap();

    // Wait for error response
    let result = setup.hub.editor.wait_for_response(req_id, 5).await;
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("112") || err_msg.contains("not found"));

    tracing::info!("test_open_file_doc_id_not_found completed successfully");
    setup.cleanup();
}

#[tokio::test]
async fn test_list_files_top_level() {
    // Test: List top-level projects and docs
    // Flow:
    // 1. Hub declares multiple projects
    // 2. Hub shares some docs
    // 3. Spoke requests top-level listing (dir = None)
    // Expected: Returns list of all projects and shared docs

    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 1).await.unwrap();

    // Create two test projects
    let project1_dir = create_test_project().unwrap();
    let project1_path = project1_dir.path().to_string_lossy().to_string();

    let project2_dir = create_complex_project().unwrap();
    let project2_path = project2_dir.path().to_string_lossy().to_string();

    // Hub declares projects
    setup
        .hub
        .editor
        .send_notification(
            "DeclareProjects",
            serde_json::json!({
                "projects": [
                    {
                        "filename": project1_path.clone(),
                        "name": "Project1",
                        "meta": {"type": "simple"}
                    },
                    {
                        "filename": project2_path.clone(),
                        "name": "Project2",
                        "meta": {"type": "complex"}
                    }
                ]
            }),
        )
        .await
        .unwrap();

    sleep(Duration::from_millis(100)).await;

    // Spoke requests top-level listing
    let req_id = 1;
    setup.spokes[0]
        .editor
        .send_request(
            req_id,
            "ListFiles",
            serde_json::json!({
                "hostId": setup.hub.id.clone(),
                "dir": null,
                "signalingAddr": env.signaling_url(),
                "credential": "test"
            }),
        )
        .await
        .unwrap();

    // Wait for response
    let resp = setup.spokes[0]
        .editor
        .wait_for_response(req_id, 5)
        .await
        .unwrap();
    let files = resp["files"].as_array().unwrap();

    // Verify we have 2 projects
    assert_eq!(files.len(), 2);

    // Check first project
    let has_project1 = files.iter().any(|f| {
        f["filename"].as_str() == Some("Project1")
            && f["isDirectory"].as_bool() == Some(true)
            && f["file"]["type"].as_str() == Some("project")
    });
    assert!(has_project1, "Should have Project1");

    // Check second project
    let has_project2 = files.iter().any(|f| {
        f["filename"].as_str() == Some("Project2")
            && f["isDirectory"].as_bool() == Some(true)
            && f["file"]["type"].as_str() == Some("project")
    });
    assert!(has_project2, "Should have Project2");

    tracing::info!("test_list_files_top_level completed successfully");
    setup.cleanup();
}

#[tokio::test]
async fn test_list_files_project_directory() {
    // Test: List files in a specific project directory
    // Flow:
    // 1. Hub declares a complex project
    // 2. Spoke requests listing of src directory
    // 3. Spoke requests listing of src/modules directory
    // Expected: Returns correct files for each directory level

    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 1).await.unwrap();

    // Create complex project
    let project_dir = create_complex_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Hub declares project
    setup
        .hub
        .editor
        .send_notification(
            "DeclareProjects",
            serde_json::json!({
                "projects": [{
                    "filename": project_path.clone(),
                    "name": "ComplexProject",
                    "meta": {}
                }]
            }),
        )
        .await
        .unwrap();

    sleep(Duration::from_millis(100)).await;

    // Request listing of src directory
    let req_id = 1;
    setup.spokes[0]
        .editor
        .send_request(
            req_id,
            "ListFiles",
            serde_json::json!({
                "hostId": setup.hub.id.clone(),
                "dir": {
                    "type": "projectFile",
                    "project": project_path.clone(),
                    "file": "src"
                },
                "signalingAddr": env.signaling_url(),
                "credential": "test"
            }),
        )
        .await
        .unwrap();

    let resp = setup.spokes[0]
        .editor
        .wait_for_response(req_id, 5)
        .await
        .unwrap();
    let files = resp["files"].as_array().unwrap();

    // Should have main.rs, lib.rs, and modules directory
    assert_eq!(files.len(), 3);

    let has_main = files.iter().any(|f| {
        f["filename"].as_str() == Some("main.rs") && f["isDirectory"].as_bool() == Some(false)
    });
    assert!(has_main, "Should have main.rs");

    let has_lib = files.iter().any(|f| {
        f["filename"].as_str() == Some("lib.rs") && f["isDirectory"].as_bool() == Some(false)
    });
    assert!(has_lib, "Should have lib.rs");

    let has_modules = files.iter().any(|f| {
        f["filename"].as_str() == Some("modules") && f["isDirectory"].as_bool() == Some(true)
    });
    assert!(has_modules, "Should have modules directory");

    // Request listing of src/modules directory
    let req_id = 2;
    setup.spokes[0]
        .editor
        .send_request(
            req_id,
            "ListFiles",
            serde_json::json!({
                "hostId": setup.hub.id.clone(),
                "dir": {
                    "type": "projectFile",
                    "project": project_path.clone(),
                    "file": "src/modules"
                },
                "signalingAddr": env.signaling_url(),
                "credential": "test"
            }),
        )
        .await
        .unwrap();

    let resp = setup.spokes[0]
        .editor
        .wait_for_response(req_id, 5)
        .await
        .unwrap();
    let files = resp["files"].as_array().unwrap();

    // Should have mod.rs, mod1.rs, and mod2.rs
    assert_eq!(files.len(), 3);

    tracing::info!("test_list_files_project_directory completed successfully");
    setup.cleanup();
}

#[tokio::test]
async fn test_list_files_from_remote() {
    // Test: Remote server requests file listing from hub
    // Flow:
    // 1. Hub declares project
    // 2. Spoke1 requests file listing from hub
    // 3. Hub serves the file list
    // Expected: Spoke receives correct file list from hub

    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 2).await.unwrap();

    // Create test project
    let project_dir = create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Hub declares project
    setup
        .hub
        .editor
        .send_notification(
            "DeclareProjects",
            serde_json::json!({
                "projects": [{
                    "filename": project_path.clone(),
                    "name": "SharedProject",
                    "meta": {}
                }]
            }),
        )
        .await
        .unwrap();

    sleep(Duration::from_millis(100)).await;

    // Spoke1 requests root directory listing
    let req_id = 1;
    setup.spokes[0]
        .editor
        .send_request(
            req_id,
            "ListFiles",
            serde_json::json!({
                "hostId": setup.hub.id.clone(),
                "dir": {
                    "type": "project",
                    "id": project_path.clone()
                },
                "signalingAddr": env.signaling_url(),
                "credential": "test"
            }),
        )
        .await
        .unwrap();

    let resp = setup.spokes[0]
        .editor
        .wait_for_response(req_id, 5)
        .await
        .unwrap();
    let files = resp["files"].as_array().unwrap();

    // Should have test.txt, readme.md, and src directory
    assert_eq!(files.len(), 3);

    // Spoke2 also requests the same listing
    let req_id = 2;
    setup.spokes[1]
        .editor
        .send_request(
            req_id,
            "ListFiles",
            serde_json::json!({
                "hostId": setup.hub.id.clone(),
                "dir": {
                    "type": "project",
                    "id": project_path.clone()
                },
                "signalingAddr": env.signaling_url(),
                "credential": "test"
            }),
        )
        .await
        .unwrap();

    let resp = setup.spokes[1]
        .editor
        .wait_for_response(req_id, 5)
        .await
        .unwrap();
    let files2 = resp["files"].as_array().unwrap();

    // Both spokes should get the same file list
    assert_eq!(files.len(), files2.len());

    tracing::info!("test_list_files_from_remote completed successfully");
    setup.cleanup();
}

#[tokio::test]
async fn test_list_files_project_not_found() {
    // Test: Request listing with non-existent project ID
    // Flow:
    // 1. Request listing with invalid project ID
    // Expected: Error response

    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 1).await.unwrap();

    // Request listing with non-existent project
    let req_id = 1;
    setup.spokes[0]
        .editor
        .send_request(
            req_id,
            "ListFiles",
            serde_json::json!({
                "hostId": setup.hub.id.clone(),
                "dir": {
                    "type": "projectFile",
                    "project": "/non/existent/project",
                    "file": "src"
                },
                "signalingAddr": env.signaling_url(),
                "credential": "test"
            }),
        )
        .await
        .unwrap();

    // Should get an error response
    let result = setup.spokes[0].editor.wait_for_response(req_id, 5).await;
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("500") || err_msg.contains("Project") || err_msg.contains("not found")
    );

    tracing::info!("test_list_files_project_not_found completed successfully");
    setup.cleanup();
}

#[tokio::test]
async fn test_list_files_not_directory() {
    // Test: Request listing of a file path (not directory)
    // Flow:
    // 1. Hub declares project
    // 2. Request listing of a file instead of directory
    // Expected: Error response "Not a directory"

    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 1).await.unwrap();

    // Create test project
    let project_dir = create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Hub declares project
    setup
        .hub
        .editor
        .send_notification(
            "DeclareProjects",
            serde_json::json!({
                "projects": [{
                    "filename": project_path.clone(),
                    "name": "TestProject",
                    "meta": {}
                }]
            }),
        )
        .await
        .unwrap();

    sleep(Duration::from_millis(100)).await;

    // Request listing of a file (not directory)
    let req_id = 1;
    setup.spokes[0]
        .editor
        .send_request(
            req_id,
            "ListFiles",
            serde_json::json!({
                "hostId": setup.hub.id.clone(),
                "dir": {
                    "type": "projectFile",
                    "project": project_path.clone(),
                    "file": "test.txt"
                },
                "signalingAddr": env.signaling_url(),
                "credential": "test"
            }),
        )
        .await
        .unwrap();

    // Should get an error response
    let result = setup.spokes[0].editor.wait_for_response(req_id, 5).await;
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("Not a directory") || err_msg.contains("500"));

    tracing::info!("test_list_files_not_directory completed successfully");
    setup.cleanup();
}

#[tokio::test]
async fn test_list_files_empty_directory() {
    // Test: List an empty directory
    // Flow:
    // 1. Hub declares project with empty directory
    // 2. Request listing of empty directory
    // Expected: Returns empty list

    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 1).await.unwrap();

    // Create project with empty directory
    let project_dir = create_complex_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Hub declares project
    setup
        .hub
        .editor
        .send_notification(
            "DeclareProjects",
            serde_json::json!({
                "projects": [{
                    "filename": project_path.clone(),
                    "name": "ProjectWithEmpty",
                    "meta": {}
                }]
            }),
        )
        .await
        .unwrap();

    sleep(Duration::from_millis(100)).await;

    // Request listing of empty directory
    let req_id = 1;
    setup.spokes[0]
        .editor
        .send_request(
            req_id,
            "ListFiles",
            serde_json::json!({
                "hostId": setup.hub.id.clone(),
                "dir": {
                    "type": "projectFile",
                    "project": project_path.clone(),
                    "file": "empty"
                },
                "signalingAddr": env.signaling_url(),
                "credential": "test"
            }),
        )
        .await
        .unwrap();

    let resp = setup.spokes[0]
        .editor
        .wait_for_response(req_id, 5)
        .await
        .unwrap();
    let files = resp["files"].as_array().unwrap();

    // Should be empty
    assert_eq!(files.len(), 0);

    tracing::info!("test_list_files_empty_directory completed successfully");
    setup.cleanup();
}

#[tokio::test]
async fn test_list_files_nested_structure() {
    // Test: Navigate through nested directory structure
    // Flow:
    // 1. Hub declares complex project
    // 2. Request listings at different levels
    // 3. Verify correct file paths and types
    // Expected: Correct listings at each level with proper relative paths

    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 1).await.unwrap();

    // Create complex project
    let project_dir = create_complex_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Hub declares project
    setup
        .hub
        .editor
        .send_notification(
            "DeclareProjects",
            serde_json::json!({
                "projects": [{
                    "filename": project_path.clone(),
                    "name": "NestedProject",
                    "meta": {}
                }]
            }),
        )
        .await
        .unwrap();

    sleep(Duration::from_millis(100)).await;

    // Test root level
    let req_id = 1;
    setup.spokes[0]
        .editor
        .send_request(
            req_id,
            "ListFiles",
            serde_json::json!({
                "hostId": setup.hub.id.clone(),
                "dir": {
                    "type": "project",
                    "id": project_path.clone()
                },
                "signalingAddr": env.signaling_url(),
                "credential": "test"
            }),
        )
        .await
        .unwrap();

    let resp = setup.spokes[0]
        .editor
        .wait_for_response(req_id, 5)
        .await
        .unwrap();
    let files = resp["files"].as_array().unwrap();

    // Should have README.md, Cargo.toml, .gitignore, src/, tests/, docs/, empty/
    assert_eq!(files.len(), 7);

    // Verify file paths are correct
    let readme = files
        .iter()
        .find(|f| f["filename"].as_str() == Some("README.md"))
        .unwrap();
    assert_eq!(readme["file"]["file"].as_str(), Some("README.md"));
    assert_eq!(readme["isDirectory"].as_bool(), Some(false));

    let src_dir = files
        .iter()
        .find(|f| f["filename"].as_str() == Some("src"))
        .unwrap();
    assert_eq!(src_dir["file"]["file"].as_str(), Some("src"));
    assert_eq!(src_dir["isDirectory"].as_bool(), Some(true));

    // Test nested level - src/modules
    let req_id = 2;
    setup.spokes[0]
        .editor
        .send_request(
            req_id,
            "ListFiles",
            serde_json::json!({
                "hostId": setup.hub.id.clone(),
                "dir": {
                    "type": "projectFile",
                    "project": project_path.clone(),
                    "file": "src/modules"
                },
                "signalingAddr": env.signaling_url(),
                "credential": "test"
            }),
        )
        .await
        .unwrap();

    let resp = setup.spokes[0]
        .editor
        .wait_for_response(req_id, 5)
        .await
        .unwrap();
    let files = resp["files"].as_array().unwrap();

    // Should have mod.rs, mod1.rs, mod2.rs
    assert_eq!(files.len(), 3);

    // Verify nested file paths
    let mod1 = files
        .iter()
        .find(|f| f["filename"].as_str() == Some("mod1.rs"))
        .unwrap();
    assert_eq!(mod1["file"]["file"].as_str(), Some("src/modules/mod1.rs"));
    assert_eq!(mod1["file"]["type"].as_str(), Some("projectFile"));

    tracing::info!("test_list_files_nested_structure completed successfully");
    setup.cleanup();
}

#[tokio::test]
async fn test_send_ops_e2e() {
    // Test: End-to-end test for SendOps functionality
    // Note: This test currently only tests the hub sending ops to itself
    // because of an architectural limitation where OpFromServer messages
    // contain doc_ids that are specific to the sending server, and
    // receiving servers can't map these to their own doc_ids.
    // A full test would require refactoring to track doc mappings
    // between servers or use file descriptors instead of doc_ids.

    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 2).await.unwrap();

    // Create test project with a file
    let project_dir = create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Hub declares project
    setup
        .hub
        .editor
        .send_notification(
            "DeclareProjects",
            serde_json::json!({
                "projects": [{
                    "filename": project_path.clone(),
                    "name": "TestProject",
                    "meta": {}
                }]
            }),
        )
        .await
        .unwrap();

    sleep(Duration::from_millis(100)).await;

    // Hub opens the file first (to create it in both docs and remote_docs)
    let hub_req_id = 1;
    setup
        .hub
        .editor
        .send_request(
            hub_req_id,
            "OpenFile",
            serde_json::json!({
                "hostId": setup.hub.id.clone(),
                "fileDesc": {
                    "type": "projectFile",
                    "project": project_path.clone(),
                    "file": "test.txt"
                }
            }),
        )
        .await
        .unwrap();

    let hub_resp = setup
        .hub
        .editor
        .wait_for_response(hub_req_id, 5)
        .await
        .unwrap();
    let hub_doc_id = hub_resp["docId"].as_u64().unwrap() as u32;

    // Spoke 1 requests the file
    let spoke1_req_id = 2;
    setup.spokes[0]
        .editor
        .send_request(
            spoke1_req_id,
            "OpenFile",
            serde_json::json!({
                "hostId": setup.hub.id.clone(),
                "fileDesc": {
                    "type": "projectFile",
                    "project": project_path.clone(),
                    "file": "test.txt"
                }
            }),
        )
        .await
        .unwrap();

    let spoke1_resp = setup.spokes[0]
        .editor
        .wait_for_response(spoke1_req_id, 5)
        .await
        .unwrap();
    assert_eq!(spoke1_resp["content"], "Hello from test.txt");
    let spoke1_doc_id = spoke1_resp["docId"].as_u64().unwrap() as u32;
    let spoke1_site_id = spoke1_resp["siteId"].as_u64().unwrap() as u32;

    // Spoke 2 requests the file
    let spoke2_req_id = 3;
    setup.spokes[1]
        .editor
        .send_request(
            spoke2_req_id,
            "OpenFile",
            serde_json::json!({
                "hostId": setup.hub.id.clone(),
                "fileDesc": {
                    "type": "projectFile",
                    "project": project_path.clone(),
                    "file": "test.txt"
                }
            }),
        )
        .await
        .unwrap();

    let spoke2_resp = setup.spokes[1]
        .editor
        .wait_for_response(spoke2_req_id, 5)
        .await
        .unwrap();
    assert_eq!(spoke2_resp["content"], "Hello from test.txt");
    let spoke2_doc_id = spoke2_resp["docId"].as_u64().unwrap() as u32;
    let spoke2_site_id = spoke2_resp["siteId"].as_u64().unwrap() as u32;

    sleep(Duration::from_millis(200)).await;

    // Hub sends ops to insert text at the beginning (hub acts as a client to itself)
    let send_ops_req_id = 4;
    setup
        .hub
        .editor
        .send_request(
            send_ops_req_id,
            "SendOps",
            serde_json::json!({
                "docId": hub_doc_id,
                "hostId": setup.hub.id.clone(),
                "ops": [{
                    "op": {
                        "Ins": [0, "Modified: "]
                    },
                    "groupSeq": 1
                }]
            }),
        )
        .await
        .unwrap();

    // Wait for SendOps response
    let send_ops_resp = setup
        .hub
        .editor
        .wait_for_response(send_ops_req_id, 5)
        .await
        .unwrap();

    // Hub shouldn't receive any remote ops in response initially (ops are from itself)
    assert!(send_ops_resp["ops"].as_array().unwrap().is_empty());

    // Give time for ops to propagate
    sleep(Duration::from_millis(500)).await;

    // Spoke 2 should receive RemoteOpsArrived notification
    let notification = setup.spokes[1]
        .editor
        .wait_for_notification("RemoteOpsArrived", 5)
        .await
        .unwrap();

    assert_eq!(notification["hostId"], setup.hub.id.clone());
    assert_eq!(notification["docId"], spoke2_doc_id);

    // Spoke 2 sends SendOps with empty ops to fetch remote ops
    let fetch_ops_req_id = 5;
    setup.spokes[1]
        .editor
        .send_request(
            fetch_ops_req_id,
            "SendOps",
            serde_json::json!({
                "docId": spoke2_doc_id,
                "hostId": setup.hub.id.clone(),
                "ops": []
            }),
        )
        .await
        .unwrap();

    // Wait for response with remote ops
    let fetch_ops_resp = setup.spokes[1]
        .editor
        .wait_for_response(fetch_ops_req_id, 5)
        .await
        .unwrap();
    let remote_ops = fetch_ops_resp["ops"].as_array().unwrap();

    // Should have received the insert op
    assert_eq!(remote_ops.len(), 1);
    let op = &remote_ops[0];
    let expected_op = serde_json::json!({
                "op": {
                    "Ins": [
                        [0, "Modified: "]
                    ]
                },
                "siteId": 0, // site is 0 because the op is from the hub
    });
    dbg!(serde_json::to_string(op).unwrap());
    dbg!(serde_json::to_string(&expected_op).unwrap());
    assert!(serde_json::to_string(op).unwrap() == serde_json::to_string(&expected_op).unwrap());

    // Spoke 1 should also receive RemoteOpsArrived notification and fetch the op
    let notification_spoke1 = setup.spokes[0]
        .editor
        .wait_for_notification("RemoteOpsArrived", 5)
        .await
        .unwrap();

    assert_eq!(notification_spoke1["hostId"], setup.hub.id.clone());

    // Spoke 1 fetches remote ops
    let fetch_ops_req_id_spoke1 = 6;
    setup.spokes[0]
        .editor
        .send_request(
            fetch_ops_req_id_spoke1,
            "SendOps",
            serde_json::json!({
                "docId": spoke1_doc_id,
                "hostId": setup.hub.id.clone(),
                "ops": []
            }),
        )
        .await
        .unwrap();

    let fetch_ops_resp_spoke1 = setup.spokes[0]
        .editor
        .wait_for_response(fetch_ops_req_id_spoke1, 5)
        .await
        .unwrap();
    assert_eq!(fetch_ops_resp_spoke1["ops"].as_array().unwrap().len(), 1);
    let remote_ops = fetch_ops_resp_spoke1["ops"].as_array().unwrap();

    // Should have received the insert op
    assert_eq!(remote_ops.len(), 1);
    let op = &remote_ops[0];
    let expected_op = serde_json::json!({
                "op": {
                    "Ins": [
                        [0, "Modified: "]
                    ]
                },
                "siteId": 0, // site is 0 because the op is from the hub
    });
    assert!(serde_json::to_string(op).unwrap() == serde_json::to_string(&expected_op).unwrap());

    // Now Spoke 1 sends its own op
    // Insert at position 10 (after "Modified: ")
    let send_ops_req_id_spoke1 = 7;
    setup.spokes[0]
        .editor
        .send_request(
            send_ops_req_id_spoke1,
            "SendOps",
            serde_json::json!({
                "docId": spoke1_doc_id,
                "hostId": setup.hub.id.clone(),
                "ops": [{
                    "op": {
                        "Ins": [10, "[Spoke1]"]  // Insert after "Modified: "
                    },
                    "groupSeq": 1
                }]
            }),
        )
        .await
        .unwrap();

    let _send_ops_resp_spoke1 = setup.spokes[0]
        .editor
        .wait_for_response(send_ops_req_id_spoke1, 5)
        .await
        .unwrap();

    // Give time for ops to propagate and for Spoke2 to receive notification
    sleep(Duration::from_millis(1000)).await;

    // Spoke 2 sends its own op - this will also fetch Spoke1's op in the response
    // Insert at position 10 (also after "Modified: ", will be transformed by OT)
    let send_ops_req_id_spoke2 = 8;
    setup.spokes[1]
        .editor
        .send_request(
            send_ops_req_id_spoke2,
            "SendOps",
            serde_json::json!({
                "docId": spoke2_doc_id,
                "hostId": setup.hub.id.clone(),
                "ops": [{
                    "op": {
                        "Ins": [10, "[Spoke2]"]  // Also insert after "Modified: "
                    },
                    "groupSeq": 1
                }]
            }),
        )
        .await
        .unwrap();

    let send_ops_resp_spoke2 = setup.spokes[1]
        .editor
        .wait_for_response(send_ops_req_id_spoke2, 5)
        .await
        .unwrap();
    // The response should contain Spoke1's op that was buffered
    tracing::info!("Spoke2 SendOps response: {:?}", send_ops_resp_spoke2);
    let remote_ops = send_ops_resp_spoke2["ops"].as_array().unwrap();

    // Should have received the insert op from spoke 1
    assert_eq!(remote_ops.len(), 1);
    let op = &remote_ops[0];
    let expected_op = serde_json::json!({
                "op": {
                    "Ins": [
                        [10, "[Spoke1]"]
                    ]
                },
                "siteId": 1, // site is 1 because the op is from spoke 1
    });
    assert!(serde_json::to_string(op).unwrap() == serde_json::to_string(&expected_op).unwrap());

    // Wait for RemoteOpsArrived notification in Spoke1
    let notification_spoke1_from_spoke2 = setup.spokes[0]
        .editor
        .wait_for_notification("RemoteOpsArrived", 5)
        .await
        .unwrap();

    assert_eq!(
        notification_spoke1_from_spoke2["hostId"],
        setup.hub.id.clone()
    );
    assert_eq!(notification_spoke1_from_spoke2["docId"], spoke1_doc_id);

    // Spoke1 sends empty SendOps to fetch the new ops
    let fetch_ops_req_id_spoke1_2 = 9;
    setup.spokes[0]
        .editor
        .send_request(
            fetch_ops_req_id_spoke1_2,
            "SendOps",
            serde_json::json!({
                "docId": spoke1_doc_id,
                "hostId": setup.hub.id.clone(),
                "ops": []
            }),
        )
        .await
        .unwrap();

    let fetch_ops_resp_spoke1_2 = setup.spokes[0]
        .editor
        .wait_for_response(fetch_ops_req_id_spoke1_2, 5)
        .await
        .unwrap();

    // Print out the ops received for debugging
    let ops_from_spoke2 = fetch_ops_resp_spoke1_2["ops"].as_array().unwrap();
    tracing::info!("Spoke1 received ops from Spoke2: {:?}", ops_from_spoke2);

    // Verify we received Spoke2's op
    assert_eq!(ops_from_spoke2.len(), 1);
    let op = &ops_from_spoke2[0];

    // The op should be Spoke2's insertion, transformed if necessary
    // The exact position might be transformed due to OT
    tracing::info!(
        "Spoke1 received op details: {}",
        serde_json::to_string_pretty(op).unwrap()
    );

    let expected_op = serde_json::json!({
                "op": {
                    "Ins": [
                        [18, "[Spoke2]"]
                    ]
                },
                "siteId": 2, // site is 2 because the op is from spoke 2
    });
    assert!(serde_json::to_string(op).unwrap() == serde_json::to_string(&expected_op).unwrap());

    setup.cleanup();
}

#[tokio::test]
async fn test_undo_e2e() {
    // Test: End-to-end test for Undo functionality
    // Sends an op, then sends an undo request, and verifies the returned undo op

    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 1).await.unwrap();

    // Create test project with a file
    let project_dir = create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Hub declares project
    setup
        .hub
        .editor
        .send_notification(
            "DeclareProjects",
            serde_json::json!({
                "projects": [{
                    "filename": project_path.clone(),
                    "name": "TestProject",
                    "meta": {}
                }]
            }),
        )
        .await
        .unwrap();

    sleep(Duration::from_millis(100)).await;

    // Spoke opens the file
    let spoke_req_id = 1;
    setup.spokes[0]
        .editor
        .send_request(
            spoke_req_id,
            "OpenFile",
            serde_json::json!({
                "hostId": setup.hub.id.clone(),
                "fileDesc": {
                    "type": "projectFile",
                    "project": project_path.clone(),
                    "file": "test.txt"
                }
            }),
        )
        .await
        .unwrap();

    let spoke_resp = setup.spokes[0]
        .editor
        .wait_for_response(spoke_req_id, 5)
        .await
        .unwrap();
    assert_eq!(spoke_resp["content"], "Hello from test.txt");
    let doc_id = spoke_resp["docId"].as_u64().unwrap() as u32;

    sleep(Duration::from_millis(200)).await;

    // Send an insert operation
    let send_ops_req_id = 2;
    setup.spokes[0]
        .editor
        .send_request(
            send_ops_req_id,
            "SendOps",
            serde_json::json!({
                "docId": doc_id,
                "hostId": setup.hub.id.clone(),
                "ops": [{
                    "op": {
                        "Ins": [0, "UNDO_TEST: "]
                    },
                    "groupSeq": 1
                }]
            }),
        )
        .await
        .unwrap();

    // Wait for SendOps response
    let send_ops_resp = setup.spokes[0]
        .editor
        .wait_for_response(send_ops_req_id, 5)
        .await
        .unwrap();
    tracing::info!("SendOps response: {:?}", send_ops_resp);

    sleep(Duration::from_millis(100)).await;

    // Send an undo request
    let undo_req_id = 3;
    setup.spokes[0]
        .editor
        .send_request(
            undo_req_id,
            "Undo",
            serde_json::json!({
                "docId": doc_id,
                "hostId": setup.hub.id.clone(),
                "kind": "Undo"
            }),
        )
        .await
        .unwrap();

    // Wait for Undo response
    let undo_resp = setup.spokes[0]
        .editor
        .wait_for_response(undo_req_id, 5)
        .await
        .unwrap();
    
    tracing::info!("Undo response: {:?}", undo_resp);
    
    // Verify that the undo response contains the delete operation
    let ops = undo_resp["ops"].as_array().unwrap();
    assert!(!ops.is_empty(), "Undo should return at least one operation");
    
    // The undo operation should be a deletion of the inserted text
    let actual_op = &ops[0];
    let expected_op = serde_json::json!({
        "Del": [[0, "UNDO_TEST: "]]
    });
    
    assert_eq!(
        serde_json::to_string(&actual_op).unwrap(),
        serde_json::to_string(&expected_op).unwrap(),
        "Undo operation should delete the inserted text"
    );

    // Send the undo operation back as a SendOps request  
    // The undo operation needs to be sent as "Undo" not as a delete
    let send_undo_req_id = 4;
    setup.spokes[0]
        .editor
        .send_request(
            send_undo_req_id,
            "SendOps",
            serde_json::json!({
                "docId": doc_id,
                "hostId": setup.hub.id.clone(),
                "ops": [{
                    "op": "Undo",
                    "groupSeq": 2
                }]
            }),
        )
        .await
        .unwrap();

    // Wait for SendOps response
    let send_undo_resp = setup.spokes[0]
        .editor
        .wait_for_response(send_undo_req_id, 5)
        .await
        .unwrap();
    tracing::info!("SendOps (undo) response: {:?}", send_undo_resp);

    sleep(Duration::from_millis(100)).await;

    // Test redo
    let redo_req_id = 5;
    setup.spokes[0]
        .editor
        .send_request(
            redo_req_id,
            "Undo",
            serde_json::json!({
                "docId": doc_id,
                "hostId": setup.hub.id.clone(),
                "kind": "Redo"
            }),
        )
        .await
        .unwrap();

    // Wait for Redo response
    let redo_resp = setup.spokes[0]
        .editor
        .wait_for_response(redo_req_id, 5)
        .await
        .unwrap();
    
    tracing::info!("Redo response: {:?}", redo_resp);
    
    // Verify that the redo response contains the insert operation
    let redo_ops = redo_resp["ops"].as_array().unwrap();
    assert!(!redo_ops.is_empty(), "Redo should return at least one operation");
    
    // The redo operation should be an insertion of the text back
    let actual_redo_op = &redo_ops[0];
    let expected_redo_op = serde_json::json!({
        "Ins": [[0, "UNDO_TEST: "]]
    });
    
    assert_eq!(
        serde_json::to_string(&actual_redo_op).unwrap(),
        serde_json::to_string(&expected_redo_op).unwrap(),
        "Redo operation should reinsert the text"
    );

    setup.cleanup();
}
