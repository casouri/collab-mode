use super::*;
use crate::config_man::{ConfigManager, Permission};
use crate::signaling;
use std::time::Duration;
// use tokio::net::TcpListener;
use rand::Rng;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
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

#[cfg(any(test, feature = "test-runner"))]
pub struct TestEnvironment {
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
    #[cfg(any(test, feature = "test-runner"))]
    pub async fn new() -> anyhow::Result<Self> {
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

    #[cfg(any(test, feature = "test-runner"))]
    pub fn signaling_url(&self) -> &str {
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
    // format!("{}-{}", prefix, Uuid::new_v4())
    prefix.to_string()
}

#[cfg(any(test, feature = "test-runner"))]
pub struct MockEditor {
    tx: mpsc::Sender<lsp_server::Message>,
    rx: mpsc::Receiver<lsp_server::Message>,
    next_request_id: Arc<AtomicI32>,
}

impl MockEditor {
    #[cfg(any(test, feature = "test-runner"))]
    pub fn new() -> (
        Self,
        mpsc::Sender<lsp_server::Message>,
        mpsc::Receiver<lsp_server::Message>,
    ) {
        let (editor_to_server_tx, editor_to_server_rx) = mpsc::channel(100);
        let (server_to_editor_tx, server_to_editor_rx) = mpsc::channel(100);

        let mock = MockEditor {
            tx: editor_to_server_tx,
            rx: server_to_editor_rx,
            next_request_id: Arc::new(AtomicI32::new(1)),
        };

        (mock, server_to_editor_tx, editor_to_server_rx)
    }

    fn next_request_id(&self) -> i32 {
        self.next_request_id.fetch_add(1, Ordering::SeqCst)
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

    async fn expect_connected_notification(
        &mut self,
        timeout_secs: u64,
    ) -> anyhow::Result<(ServerId, String)> {
        let params = self
            .wait_for_notification(&NotificationCode::Connected.to_string(), timeout_secs)
            .await?;
        let host_id = params
            .get("hostId")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing hostId in Connected notification"))?
            .to_string();
        let message = params
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Ok((host_id, message))
    }

    // Helper method: Send request and wait for response in one call
    async fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        self.request_expect(method, params, 5).await
    }

    // Helper method: Send request and wait for response with custom timeout
    async fn request_expect(
        &mut self,
        method: &str,
        params: serde_json::Value,
        timeout_secs: u64,
    ) -> anyhow::Result<serde_json::Value> {
        let req_id = self.next_request_id();
        self.send_request(req_id, method, params).await?;
        self.wait_for_response(req_id, timeout_secs).await
    }

    // Helper method: Drain any pending response without waiting
    async fn drain_response(&mut self) -> Option<serde_json::Value> {
        match self.rx.try_recv() {
            Ok(lsp_server::Message::Response(resp)) => {
                tracing::debug!("Drained response: {:?}", resp);
                if resp.error.is_some() {
                    None
                } else {
                    resp.result
                }
            }
            Ok(msg) => {
                tracing::debug!("Drained non-response message: {:?}", msg);
                None
            }
            Err(_) => None,
        }
    }

    // Test-specific helper methods

    /// Helper: Open a file and return doc_id, site_id, and content
    #[cfg(any(test, feature = "test-runner"))]
    pub async fn open_file(
        &mut self,
        file_desc: serde_json::Value,
    ) -> anyhow::Result<(u32, u32, String)> {
        let resp = self
            .request(
                "OpenFile",
                serde_json::json!({
                    "fileDesc": file_desc,
                    "mode": "open",
                }),
            )
            .await?;

        let doc_id = resp["docId"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("Missing docId in response"))?
            as u32;
        let site_id = resp["siteId"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("Missing siteId in response"))?
            as u32;
        let content = resp["content"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing content in response"))?
            .to_string();

        Ok((doc_id, site_id, content))
    }

    /// Helper: Send ops to a document
    #[cfg(any(test, feature = "test-runner"))]
    pub async fn send_ops(
        &mut self,
        doc_id: u32,
        host_id: &str,
        ops: Vec<serde_json::Value>,
    ) -> anyhow::Result<serde_json::Value> {
        self.request(
            "SendOps",
            serde_json::json!({
                "docId": doc_id,
                "hostId": host_id,
                "ops": ops,
            }),
        )
        .await
    }

    /// Helper: Send undo/redo request
    #[cfg(any(test, feature = "test-runner"))]
    pub async fn send_undo(
        &mut self,
        doc_id: u32,
        host_id: &str,
        kind: &str,
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        let resp = self
            .request(
                "Undo",
                serde_json::json!({
                    "docId": doc_id,
                    "hostId": host_id,
                    "kind": kind,
                }),
            )
            .await?;

        resp["ops"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("Missing ops in undo response"))
            .map(|ops| ops.clone())
    }

    /// Helper: Declare a project
    async fn declare_project(
        &mut self,
        project_path: &str,
        project_name: &str,
    ) -> anyhow::Result<()> {
        self.request(
            "DeclareProjects",
            serde_json::json!({
                "projects": [{
                    "path": project_path,
                    "name": project_name,
                }],
            }),
        )
        .await?;
        Ok(())
    }

    #[cfg(any(test, feature = "test-runner"))]
    pub async fn share_file(
        &mut self,
        filename: &str,
        content: &str,
        meta: serde_json::Value,
    ) -> anyhow::Result<(u32, u32)> {
        let resp = self
            .request(
                "ShareFile",
                serde_json::json!({
                    "filename": filename,
                    "content": content,
                    "meta": meta,
                }),
            )
            .await?;

        let doc_id = resp["docId"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("Missing docId in response"))?
            as u32;
        let site_id = resp["siteId"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("Missing siteId in response"))?
            as u32;

        Ok((doc_id, site_id))
    }
}

#[test]
fn test_share_file_duplicate_via_symlink() {
    // Create a temp project dir with a file and a symlink to it.
    let test_dir = tempfile::tempdir().unwrap();
    let real = test_dir.path().join("file.txt");
    std::fs::write(&real, "hello").unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs as unix_fs;
        let link = test_dir.path().join("link.txt");
        unix_fs::symlink(&real, &link).unwrap();

        // Spin up a server with config rooted in the temp dir (to avoid restricted locations).
        let config = crate::config_man::ConfigManager::new(Some(test_dir.path().to_path_buf()), None).unwrap();
        let mut server = super::Server::new("self".to_string(), config).unwrap();

        // Share the real path first.
        let resp1 = server.handle_share_file_from_editor(crate::message::ShareFileParams {
            filename: real.to_string_lossy().to_string(),
            content: "hello".into(),
            meta: serde_json::Map::new(),
        });
        assert!(resp1.is_ok());

        // Sharing the symlink to the same file should be rejected (duplicate).
        let resp2 = server.handle_share_file_from_editor(crate::message::ShareFileParams {
            filename: link.to_string_lossy().to_string(),
            content: "hello".into(),
            meta: serde_json::Map::new(),
        });
        assert!(resp2.is_err());
    }
}

#[cfg(any(test, feature = "test-runner"))]
pub struct ServerSetup {
    pub id: ServerId,
    pub handle: tokio::task::JoinHandle<()>,
    pub editor: MockEditor,
    pub _temp_dir: tempfile::TempDir,
}

#[cfg(any(test, feature = "test-runner"))]
pub struct HubAndSpokeSetup {
    pub hub: ServerSetup,
    pub spokes: Vec<ServerSetup>,
}

impl HubAndSpokeSetup {
    #[cfg(any(test, feature = "test-runner"))]
    pub fn cleanup(self) {
        self.hub.handle.abort();
        for spoke in self.spokes {
            spoke.handle.abort();
        }
    }
}

/// Creates a hub-and-spoke topology with one central server and multiple spoke servers.
/// The hub server accepts connections and all spoke servers connect to it.
/// Returns the hub and spoke setups with established connections.
///
/// If spoke_permissions is provided, it should contain exactly num_spokes permissions,
/// one for each spoke. If not provided, all spokes get full permissions.
#[cfg(any(test, feature = "test-runner"))]
pub async fn setup_hub_and_spoke_servers(
    env: &TestEnvironment,
    num_spokes: usize,
    spoke_permissions: Option<Vec<Permission>>,
) -> anyhow::Result<HubAndSpokeSetup> {
    // Validate spoke_permissions if provided
    if let Some(ref perms) = spoke_permissions {
        if perms.len() != num_spokes {
            return Err(anyhow::anyhow!(
                "spoke_permissions must have exactly {} entries, got {}",
                num_spokes,
                perms.len()
            ));
        }
    }

    // Create hub server
    let hub_id = create_test_id("hub");
    let hub_temp_dir = tempfile::TempDir::new()?;
    let mut hub_config = ConfigManager::new(Some(hub_temp_dir.path().to_path_buf()), None)?;

    // Generate spoke IDs upfront so we can use them consistently
    let spoke_ids: Vec<ServerId> = (0..num_spokes)
        .map(|i| create_test_id(&format!("spoke{}", i + 1)))
        .collect();

    // Set the host_id in the config so permission checks work
    let mut config = hub_config.config();
    config.host_id = Some(hub_id.clone());

    // Pre-add permissions for all spokes that will connect
    for (i, spoke_id) in spoke_ids.iter().enumerate() {
        let permission = if let Some(ref perms) = spoke_permissions {
            perms[i].clone()
        } else {
            // Default to full permissions
            Permission {
                write: true,
                create: true,
                delete: true,
            }
        };
        config.permission.insert(spoke_id.clone(), permission);
    }

    hub_config.replace_and_save(config)?;

    let mut hub_server = Server::new(hub_id.clone(), hub_config)?;

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

    for (i, spoke_id) in spoke_ids.iter().enumerate() {
        let spoke_temp_dir = tempfile::TempDir::new()?;
        let mut spoke_config = ConfigManager::new(Some(spoke_temp_dir.path().to_path_buf()), None)?;

        // Set the host_id in the config and add permission for hub
        let mut config = spoke_config.config();
        config.host_id = Some(spoke_id.clone());
        // Add permission for hub to have full access to this spoke
        config.permission.insert(
            hub_id.clone(),
            Permission {
                write: true,
                create: true,
                delete: true,
            },
        );
        spoke_config.replace_and_save(config)?;

        let mut spoke_server = Server::new(spoke_id.clone(), spoke_config)?;

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
            id: spoke_id.clone(),
            handle: spoke_handle,
            editor: spoke_editor,
            _temp_dir: spoke_temp_dir,
        });
    }

    // Wait for all connections to be established
    tracing::info!("Waiting for all connections to be established");

    // Hub should receive Connected from each spoke
    for _i in 0..num_spokes {
        let (host_id, _message) = hub_editor.expect_connected_notification(10).await?;
        tracing::info!("Hub received Connected from spoke: {}", host_id);
    }

    // Each spoke should receive Connected from hub
    for spoke in &mut spokes {
        let (host_id, _message) = spoke.editor.expect_connected_notification(10).await?;
        tracing::info!(
            "Spoke {} received Connected from hub: {}",
            spoke.id,
            host_id
        );
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
    // 5. Each editor receives a Connected notification from the remote server
    // Expected: Both editors receive Connected messages indicating successful connection

    let env = TestEnvironment::new().await.unwrap();

    let id1 = create_test_id("server1");
    let id2 = create_test_id("server2");

    tracing::info!("Creating servers with IDs: {} and {}", id1, id2);

    let temp_dir1 = tempfile::TempDir::new().unwrap();
    let config1 = ConfigManager::new(Some(temp_dir1.path().to_path_buf()), None).unwrap();
    let mut server1 = Server::new(id1.clone(), config1).unwrap();

    let temp_dir2 = tempfile::TempDir::new().unwrap();
    let config2 = ConfigManager::new(Some(temp_dir2.path().to_path_buf()), None).unwrap();
    let mut server2 = Server::new(id2.clone(), config2).unwrap();

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

    // Wait for Connected messages from both sides
    let timeout_duration = Duration::from_secs(30);
    let start_time = std::time::Instant::now();

    let mut connected_received_count = 0;
    let connection_progress_method = NotificationCode::ConnectionProgress.to_string();
    let connected_method = NotificationCode::Connected.to_string();

    while connected_received_count < 2 && start_time.elapsed() < timeout_duration {
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
                } else if notif.method == connected_method {
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
                    tracing::info!("Editor1 received Connected from {}: {}", host_id, message);
                    assert_eq!(host_id, id2, "Connected should be from server2");
                    connected_received_count += 1;
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
                } else if notif.method == connected_method {
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
                    tracing::info!("Editor2 received Connected from {}: {}", host_id, message);
                    assert_eq!(host_id, id1, "Connected should be from server1");
                    connected_received_count += 1;
                }
            }
            _ => {}
        }

        sleep(Duration::from_millis(10)).await;
    }

    assert_eq!(
        connected_received_count, 2,
        "Should receive Connected messages from both sides indicating connection established"
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
    let mut setup = setup_hub_and_spoke_servers(&env, 0, None).await.unwrap();

    // Create test project
    let project_dir = create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Hub declares project
    setup
        .hub
        .editor
        .declare_project(&project_path, "TestProject")
        .await
        .unwrap();

    sleep(Duration::from_millis(100)).await;

    // Request to open a file
    let (doc_id, site_id, content) = setup
        .hub
        .editor
        .open_file(serde_json::json!({
            "type": "projectFile",
            "hostId": setup.hub.id.clone(),
            "project": "TestProject",
            "file": "test.txt"
        }))
        .await
        .unwrap();

    // Verify response
    assert_eq!(content, "Hello from test.txt");
    assert!(doc_id > 0);
    assert!(site_id >= 0); // site_id is 0 for the hub itself

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
    let mut setup = setup_hub_and_spoke_servers(&env, 1, None).await.unwrap();

    // Create test project
    let project_dir = create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Hub declares project
    setup
        .hub
        .editor
        .declare_project(&project_path, "TestProject")
        .await
        .unwrap();

    sleep(Duration::from_millis(100)).await;

    // Spoke requests to open a file from hub
    let (doc_id, site_id, content) = setup.spokes[0]
        .editor
        .open_file(serde_json::json!({
            "type": "projectFile",
            "hostId": setup.hub.id.clone(),
            "project": "TestProject",
            "file": "src/main.rs"
        }))
        .await
        .unwrap();

    // Verify response
    assert_eq!(content, "fn main() {\n    println!(\"Hello\");\n}");
    assert!(doc_id > 0);
    assert!(site_id > 0);

    tracing::info!("test_open_file_from_remote completed successfully");
    setup.cleanup();
}

#[tokio::test]
async fn test_open_file_create_mode() {
    // Test: Create a new file using OpenMode::Create
    // Flow:
    // 1. Create a project
    // 2. Request to open a non-existent file with mode: "create"
    // 3. Verify file is created with empty content
    // 4. Send ops to add content
    // 5. Open again to verify content persists
    // Expected: File is created and can be modified

    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 0, None).await.unwrap();

    // Create test project
    let project_dir = create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Hub declares project
    setup
        .hub
        .editor
        .declare_project(&project_path, "TestProject")
        .await
        .unwrap();

    sleep(Duration::from_millis(100)).await;

    // Request to create a new file
    let req_id = 1;
    setup
        .hub
        .editor
        .send_request(
            req_id,
            "OpenFile",
            serde_json::json!({
                "fileDesc": {
                    "type": "projectFile",
                    "hostId": setup.hub.id.clone(),
                    "project": "TestProject",
                    "file": "new_file.txt"
                },
                "mode": "create"
            }),
        )
        .await
        .unwrap();

    // Should succeed and return empty content
    let resp = setup.hub.editor.wait_for_response(req_id, 5).await.unwrap();
    let doc_id = resp["docId"].as_u64().unwrap() as u32;
    let content = resp["content"].as_str().unwrap();
    assert_eq!(content, "", "New file should have empty content");
    assert!(doc_id > 0, "Should have valid doc_id");

    // Send an insert operation to add content
    let ops = vec![serde_json::json!({
        "op": {"Ins": [0, "This is a newly created file!"]},
        "groupSeq": 1
    })];

    let _ = setup
        .hub
        .editor
        .send_ops(doc_id, &setup.hub.id, ops)
        .await
        .unwrap();

    sleep(Duration::from_millis(100)).await;

    // Open the file again with "open" mode to verify it exists and has content
    let req_id2 = 2;
    setup
        .hub
        .editor
        .send_request(
            req_id2,
            "OpenFile",
            serde_json::json!({
                "fileDesc": {
                    "type": "projectFile",
                    "hostId": setup.hub.id.clone(),
                    "project": "TestProject",
                    "file": "new_file.txt"
                },
                "mode": "open"
            }),
        )
        .await
        .unwrap();

    let resp2 = setup
        .hub
        .editor
        .wait_for_response(req_id2, 5)
        .await
        .unwrap();
    let doc_id2 = resp2["docId"].as_u64().unwrap() as u32;

    // Should return the same doc_id since it's already open
    assert_eq!(
        doc_id, doc_id2,
        "Should return same doc_id for already open file"
    );

    // Verify the file was actually created on disk
    let file_path = project_dir.path().join("new_file.txt");
    assert!(file_path.exists(), "File should exist on disk");

    tracing::info!("test_open_file_create_mode completed successfully");
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
    let mut setup = setup_hub_and_spoke_servers(&env, 0, None).await.unwrap();

    // Create test project
    let project_dir = create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Hub declares project
    setup
        .hub
        .editor
        .request(
            "DeclareProjects",
            serde_json::json!({
                "projects": [{
                    "path": project_path.clone(),
                    "name": "TestProject",
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
                "fileDesc": {
                    "type": "projectFile",
                    "hostId": setup.hub.id.clone(),
                    "project": "TestProject",
                    "file": "non_existent.txt"
                },
                "mode": "open"
            }),
        )
        .await
        .unwrap();

    // Wait for error response
    let result = setup.hub.editor.wait_for_response(req_id, 5).await;
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("Failed to") || err_msg.contains("not found") || err_msg.contains("105")
    );

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
    let mut setup = setup_hub_and_spoke_servers(&env, 0, None).await.unwrap();

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
                "fileDesc": {
                    "type": "project",
                    "hostId": setup.hub.id.clone(),
                    "id": "TestProject"
                },
                "mode": "open"
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
    let mut setup = setup_hub_and_spoke_servers(&env, 0, None).await.unwrap();

    // Create test project
    let project_dir = create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Hub declares project
    setup
        .hub
        .editor
        .request(
            "DeclareProjects",
            serde_json::json!({
                "projects": [{
                    "path": project_path.clone(),
                    "name": "TestProject",
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
                "fileDesc": {
                    "type": "projectFile",
                    "hostId": setup.hub.id.clone(),
                    "project": "TestProject",
                    "file": "test.txt"
                },
                "mode": "open"
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
                "fileDesc": {
                    "type": "projectFile",
                    "hostId": setup.hub.id.clone(),
                    "project": "TestProject",
                    "file": "test.txt"
                },
                "mode": "open"
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
    let mut setup = setup_hub_and_spoke_servers(&env, 0, None).await.unwrap();

    // Request to open a non-existent doc by filename
    let req_id = 1;
    setup
        .hub
        .editor
        .send_request(
            req_id,
            "OpenFile",
            serde_json::json!({
                "fileDesc": {
                    "type": "file",
                    "hostId": setup.hub.id.clone(),
                    "id": "nonexistent.txt"
                },
                "mode": "open"
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
    // Test: List top-level projects and _buffers virtual project
    // Flow:
    // 1. Hub declares multiple projects
    // 2. Spoke requests top-level listing (dir = None)
    // Expected: Returns list of all projects and _buffers virtual project

    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 1, None).await.unwrap();

    // Create two test projects
    let project1_dir = create_test_project().unwrap();
    let project1_path = project1_dir.path().to_string_lossy().to_string();

    let project2_dir = create_complex_project().unwrap();
    let project2_path = project2_dir.path().to_string_lossy().to_string();

    // Hub declares projects
    setup
        .hub
        .editor
        .request(
            "DeclareProjects",
            serde_json::json!({
                "projects": [
                    {
                        "path": project1_path.clone(),
                        "name": "Project1",
                    },
                    {
                        "path": project2_path.clone(),
                        "name": "Project2",
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
            "ListProjects",
            serde_json::json!({
                "hostId": setup.hub.id.clone(),
                "signalingAddr": env.signaling_url()
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

    // Verify we have 3 items (2 projects + _buffers virtual project)
    assert_eq!(files.len(), 3);

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

    // Check _buffers virtual project
    let has_buffers = files.iter().any(|f| {
        f["filename"].as_str() == Some("_buffers")
            && f["isDirectory"].as_bool() == Some(true)
            && f["file"]["type"].as_str() == Some("project")
    });
    assert!(has_buffers, "Should have _buffers virtual project");

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
    let mut setup = setup_hub_and_spoke_servers(&env, 1, None).await.unwrap();

    // Create complex project
    let project_dir = create_complex_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Hub declares project
    setup
        .hub
        .editor
        .request(
            "DeclareProjects",
            serde_json::json!({
                "projects": [{
                    "path": project_path.clone(),
                    "name": "ComplexProject",
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
                "dir": {
                    "type": "projectFile",
                    "hostId": setup.hub.id.clone(),
                    "project": "ComplexProject",
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
                "dir": {
                    "type": "projectFile",
                    "hostId": setup.hub.id.clone(),
                    "project": "ComplexProject",
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
    let mut setup = setup_hub_and_spoke_servers(&env, 2, None).await.unwrap();

    // Create test project
    let project_dir = create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Hub declares project
    setup
        .hub
        .editor
        .request(
            "DeclareProjects",
            serde_json::json!({
                "projects": [{
                    "path": project_path.clone(),
                    "name": "SharedProject",
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
                "dir": {
                    "type": "project",
                    "hostId": setup.hub.id.clone(),
                    "id": "SharedProject"
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
                "dir": {
                    "type": "project",
                    "hostId": setup.hub.id.clone(),
                    "id": "SharedProject"
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
    let mut setup = setup_hub_and_spoke_servers(&env, 1, None).await.unwrap();

    // Request listing with non-existent project
    let req_id = 1;
    setup.spokes[0]
        .editor
        .send_request(
            req_id,
            "ListFiles",
            serde_json::json!({
                "dir": {
                    "type": "projectFile",
                    "hostId": setup.hub.id.clone(),
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
    let mut setup = setup_hub_and_spoke_servers(&env, 1, None).await.unwrap();

    // Create test project
    let project_dir = create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Hub declares project
    setup
        .hub
        .editor
        .request(
            "DeclareProjects",
            serde_json::json!({
                "projects": [{
                    "path": project_path.clone(),
                    "name": "TestProject",
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
                "dir": {
                    "type": "projectFile",
                    "hostId": setup.hub.id.clone(),
                    "project": "TestProject",
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
    let mut setup = setup_hub_and_spoke_servers(&env, 1, None).await.unwrap();

    // Create project with empty directory
    let project_dir = create_complex_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Hub declares project
    setup
        .hub
        .editor
        .request(
            "DeclareProjects",
            serde_json::json!({
                "projects": [{
                    "path": project_path.clone(),
                    "name": "ProjectWithEmpty",
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
                "dir": {
                    "type": "projectFile",
                    "hostId": setup.hub.id.clone(),
                    "project": "ProjectWithEmpty",
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
    let mut setup = setup_hub_and_spoke_servers(&env, 1, None).await.unwrap();

    // Create complex project
    let project_dir = create_complex_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Hub declares project
    setup
        .hub
        .editor
        .request(
            "DeclareProjects",
            serde_json::json!({
                "projects": [{
                    "path": project_path.clone(),
                    "name": "NestedProject",
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
                "dir": {
                    "type": "project",
                    "hostId": setup.hub.id.clone(),
                    "id": "NestedProject"
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
                "dir": {
                    "type": "projectFile",
                    "hostId": setup.hub.id.clone(),
                    "project": "NestedProject",
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
async fn test_delete_file() {
    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 2, None).await.unwrap();

    // Create a test project with some files.
    let test_dir = tempfile::tempdir().unwrap();
    let project_path = test_dir.path().to_path_buf();

    // Create directory structure:
    // project/
    //   file1.txt
    //   dir1/
    //     file2.txt
    //     file3.txt
    std::fs::write(project_path.join("file1.txt"), "Content 1").unwrap();
    std::fs::create_dir(project_path.join("dir1")).unwrap();
    std::fs::write(project_path.join("dir1/file2.txt"), "Content 2").unwrap();
    std::fs::write(project_path.join("dir1/file3.txt"), "Content 3").unwrap();

    // Declare the project.
    let req_id = 1;
    setup
        .hub
        .editor
        .send_request(
            req_id,
            "DeclareProjects",
            serde_json::json!({
                "projects": [{
                    "name": "TestProject",
                    "path": project_path.display().to_string(),
                }]
            }),
        )
        .await
        .unwrap();

    setup.hub.editor.wait_for_response(req_id, 5).await.unwrap();

    // Share file1.txt from hub.
    let req_id = 2;
    setup
        .hub
        .editor
        .send_request(
            req_id,
            "ShareFile",
            serde_json::json!({
                "filename": project_path.join("file1.txt").display().to_string(),
                "content": "Content 1",
                "meta": {}
            }),
        )
        .await
        .unwrap();

    let resp = setup.hub.editor.wait_for_response(req_id, 5).await.unwrap();
    let _doc_id_1 = resp["docId"].as_u64().unwrap() as DocId;

    // Share dir1/file2.txt from hub.
    let req_id = 3;
    setup
        .hub
        .editor
        .send_request(
            req_id,
            "ShareFile",
            serde_json::json!({
                "filename": project_path.join("dir1/file2.txt").display().to_string(),
                "content": "Content 2",
                "meta": {}
            }),
        )
        .await
        .unwrap();

    let resp = setup.hub.editor.wait_for_response(req_id, 5).await.unwrap();
    let _doc_id_2 = resp["docId"].as_u64().unwrap() as DocId;

    // Share dir1/file3.txt from hub.
    let req_id = 4;
    setup
        .hub
        .editor
        .send_request(
            req_id,
            "ShareFile",
            serde_json::json!({
                "filename": project_path.join("dir1/file3.txt").display().to_string(),
                "content": "Content 3",
                "meta": {}
            }),
        )
        .await
        .unwrap();

    let resp = setup.hub.editor.wait_for_response(req_id, 5).await.unwrap();
    let _doc_id_3 = resp["docId"].as_u64().unwrap() as DocId;

    // Connect spoke 0 to hub.
    setup.spokes[0]
        .editor
        .send_notification(
            "Connect",
            serde_json::json!({
                "hostId": setup.hub.id.clone(),
                "signalingAddr": env.signaling_url(),
                "transportType": "websocket"
            }),
        )
        .await
        .unwrap();

    // Wait for connection to be established.
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Open file1.txt from spoke 0.
    let req_id = 5;
    setup.spokes[0]
        .editor
        .send_request(
            req_id,
            "OpenFile",
            serde_json::json!({
                "fileDesc": {
                    "type": "projectFile",
                    "hostId": setup.hub.id.clone(),
                    "project": "TestProject",
                    "file": "file1.txt"
                },
                "mode": "open"
            }),
        )
        .await
        .unwrap();

    let resp = setup.spokes[0]
        .editor
        .wait_for_response(req_id, 5)
        .await
        .unwrap();
    let _remote_doc_id_1 = resp["docId"].as_u64().unwrap() as DocId;

    // Open dir1/file2.txt from spoke 0.
    let req_id = 6;
    setup.spokes[0]
        .editor
        .send_request(
            req_id,
            "OpenFile",
            serde_json::json!({
                "fileDesc": {
                    "type": "projectFile",
                    "hostId": setup.hub.id.clone(),
                    "project": "TestProject",
                    "file": "dir1/file2.txt"
                },
                "mode": "open"
            }),
        )
        .await
        .unwrap();

    let resp = setup.spokes[0]
        .editor
        .wait_for_response(req_id, 5)
        .await
        .unwrap();
    let _remote_doc_id_2 = resp["docId"].as_u64().unwrap() as DocId;

    // Test 1: Delete a single file (file1.txt).
    let req_id = 7;
    setup
        .hub
        .editor
        .send_request(
            req_id,
            "DeleteFile",
            serde_json::json!({
                "file": {
                    "type": "projectFile",
                    "hostId": setup.hub.id.clone(),
                    "project": "TestProject",
                    "file": "file1.txt"
                }
            }),
        )
        .await
        .unwrap();

    setup.hub.editor.wait_for_response(req_id, 5).await.unwrap();

    // Spoke 0 should receive FileDeleted notification for the file.
    let notification = setup.spokes[0]
        .editor
        .wait_for_notification("FileDeleted", 5)
        .await
        .unwrap();
    // Check that we got the correct file descriptor.
    assert_eq!(
        notification["file"]["type"].as_str().unwrap(),
        "projectFile"
    );
    assert_eq!(
        notification["file"]["project"].as_str().unwrap(),
        "TestProject"
    );
    assert_eq!(notification["file"]["file"].as_str().unwrap(), "file1.txt");

    // Verify file1.txt is deleted from filesystem.
    assert!(!project_path.join("file1.txt").exists());

    // Test 2: Delete a directory (dir1).
    let req_id = 8;
    setup
        .hub
        .editor
        .send_request(
            req_id,
            "DeleteFile",
            serde_json::json!({
                "file": {
                    "type": "projectFile",
                    "hostId": setup.hub.id.clone(),
                    "project": "TestProject",
                    "file": "dir1"
                }
            }),
        )
        .await
        .unwrap();

    setup.hub.editor.wait_for_response(req_id, 5).await.unwrap();

    // Spoke 0 should receive FileDeleted notification for the directory.
    let notification = setup.spokes[0]
        .editor
        .wait_for_notification("FileDeleted", 5)
        .await
        .unwrap();
    // Check that we got the correct file descriptor for the directory.
    assert_eq!(
        notification["file"]["type"].as_str().unwrap(),
        "projectFile"
    );
    assert_eq!(
        notification["file"]["project"].as_str().unwrap(),
        "TestProject"
    );
    assert_eq!(notification["file"]["file"].as_str().unwrap(), "dir1");

    // Verify dir1 and its contents are deleted from filesystem.
    assert!(!project_path.join("dir1").exists());

    tracing::info!("test_delete_file completed successfully");
    setup.cleanup();
}

#[tokio::test]
async fn test_send_ops_e2e() {
    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 2, None).await.unwrap();

    // Create test project with a file
    let project_dir = create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Hub declares project
    setup
        .hub
        .editor
        .declare_project(&project_path, "TestProject")
        .await
        .unwrap();

    sleep(Duration::from_millis(100)).await;

    // Hub opens the file first (to create it in both docs and remote_docs)
    let (hub_doc_id, _hub_site_id, _hub_content) = setup
        .hub
        .editor
        .open_file(serde_json::json!({
            "type": "projectFile",
            "hostId": setup.hub.id.clone(),
            "project": "TestProject",
            "file": "test.txt"
        }))
        .await
        .unwrap();

    // Spoke 1 requests the file
    let (spoke1_doc_id, _spoke1_site_id, spoke1_content) = setup.spokes[0]
        .editor
        .open_file(serde_json::json!({
            "type": "projectFile",
            "hostId": setup.hub.id.clone(),
            "project": "TestProject",
            "file": "test.txt"
        }))
        .await
        .unwrap();
    assert_eq!(spoke1_content, "Hello from test.txt");

    // Spoke 2 requests the file
    let (spoke2_doc_id, _spoke2_site_id, spoke2_content) = setup.spokes[1]
        .editor
        .open_file(serde_json::json!({
            "type": "projectFile",
            "hostId": setup.hub.id.clone(),
            "project": "TestProject",
            "file": "test.txt"
        }))
        .await
        .unwrap();
    assert_eq!(spoke2_content, "Hello from test.txt");

    //
    // Step 1: Hub sends op
    //

    tracing::info!("\n\nStep 1: Hub sends op\n\n");

    // Hub sends ops to insert text at the beginning (hub acts as a client to itself)
    let ops = vec![serde_json::json!({
                "op": {
                    "Ins": [0, "Modified: "]
                },
                "groupSeq": 1
    })];
    let _ = setup
        .hub
        .editor
        .send_ops(hub_doc_id, &setup.hub.id, ops)
        .await
        .unwrap();

    // Spoke 2 should receive RemoteOpsArrived notification
    let notification = setup.spokes[1]
        .editor
        .wait_for_notification("RemoteOpsArrived", 5)
        .await
        .unwrap();

    assert_eq!(notification["hostId"], setup.hub.id.clone());
    assert_eq!(notification["docId"], spoke2_doc_id);

    // Spoke 2 sends SendOps with empty ops to fetch remote ops
    let fetch_ops_resp = setup.spokes[1]
        .editor
        .send_ops(spoke2_doc_id, &setup.hub.id, vec![])
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
    assert!(serde_json::to_string(op).unwrap() == serde_json::to_string(&expected_op).unwrap());

    // Spoke 1 should also receive RemoteOpsArrived notification and fetch the op
    let notification_spoke1 = setup.spokes[0]
        .editor
        .wait_for_notification("RemoteOpsArrived", 5)
        .await
        .unwrap();

    assert_eq!(notification_spoke1["hostId"], setup.hub.id.clone());

    // Spoke 1 fetches remote ops
    let fetch_ops_resp_spoke1 = setup.spokes[0]
        .editor
        .send_ops(spoke1_doc_id, &setup.hub.id, vec![])
        .await
        .unwrap();
    let remote_ops = fetch_ops_resp_spoke1["ops"].as_array().unwrap();
    assert_eq!(remote_ops.len(), 1);

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

    //
    // Step 2: Spoke 1 sends op
    //

    tracing::info!("\n\nStep 2: Spoke 1 sends op\n\n");

    // Now Spoke 1 sends its own op
    // Insert at position 10 (after "Modified: ")
    let _send_ops_resp_spoke1 = setup.spokes[0]
        .editor
        .send_ops(
            spoke1_doc_id,
            &setup.hub.id,
            vec![serde_json::json!({
                "op": {
                    "Ins": [10, "[Spoke1]"]  // Insert after "Modified: "
                },
                "groupSeq": 1
            })],
        )
        .await
        .unwrap();

    //
    // Step 3: Spoke 2 sends op in the same time
    //

    tracing::info!("\n\nStep 3: Spoke 2 sends op in the same time\n\n");

    // Spoke 2 sends its own op before processing spoke 1’s op. This
    // will also fetch Spoke1's op in the response.

    // First wait until spoke 2 receives spoke 1’s op.
    setup.spokes[1]
        .editor
        .wait_for_notification("RemoteOpsArrived", 5)
        .await
        .unwrap();
    let send_ops_resp = setup.spokes[1]
        .editor
        .send_ops(
            spoke2_doc_id,
            &setup.hub.id,
            vec![serde_json::json!({
                "op": {
                    "Ins": [10, "[Spoke2]"]  // Also insert after "Modified: "
                },
                "groupSeq": 1
            })],
        )
        .await
        .unwrap();
    // The response should contain Spoke1's op that was buffered
    tracing::info!("Spoke2 SendOps response: {:?}", send_ops_resp);
    let remote_ops = send_ops_resp["ops"].as_array().unwrap();

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

    //
    // Step 4: In spoke 1, should receive its op’s ack and spoke 2’s op.
    //

    tracing::info!("\n\nStep 4: In spoke 1, should receive its op’s ack and spoke 2’s op.\n\n");

    // Wait for RemoteOpsArrived notification in Spoke1
    setup.spokes[0]
        .editor
        .wait_for_notification("RemoteOpsArrived", 5)
        .await
        .unwrap();

    // Spoke1 sends empty SendOps to fetch the new ops
    let send_ops_resp = setup.spokes[0]
        .editor
        .send_ops(spoke1_doc_id, &setup.hub.id, vec![])
        .await
        .unwrap();

    // Print out the ops received for debugging
    let ops_from_spoke2 = send_ops_resp["ops"].as_array().unwrap();
    tracing::info!("Spoke1 received ops from Spoke2: {:?}", ops_from_spoke2);

    // Verify we received Spoke2's op
    assert_eq!(ops_from_spoke2.len(), 1);
    let op = &ops_from_spoke2[0];

    // The op should be Spoke2's insertion transformed
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
    let mut setup = setup_hub_and_spoke_servers(&env, 1, None).await.unwrap();

    // Create test project with a file
    let project_dir = create_test_project().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    // Hub declares project
    setup
        .hub
        .editor
        .declare_project(&project_path, "TestProject")
        .await
        .unwrap();

    sleep(Duration::from_millis(100)).await;

    // Spoke opens the file
    let (doc_id, _site_id, content) = setup.spokes[0]
        .editor
        .open_file(serde_json::json!({
            "type": "projectFile",
            "hostId": setup.hub.id.clone(),
            "project": "TestProject",
            "file": "test.txt"
        }))
        .await
        .unwrap();
    assert_eq!(content, "Hello from test.txt");

    sleep(Duration::from_millis(200)).await;

    // Send an insert operation
    let send_ops_resp = setup.spokes[0]
        .editor
        .send_ops(
            doc_id,
            &setup.hub.id,
            vec![serde_json::json!({
                "op": {
                    "Ins": [0, "UNDO_TEST: "]
                },
                "groupSeq": 1
            })],
        )
        .await
        .unwrap();
    tracing::info!("SendOps response: {:?}", send_ops_resp);

    sleep(Duration::from_millis(100)).await;

    // Send an undo request and get the undo operations
    let undo_ops = setup.spokes[0]
        .editor
        .send_undo(doc_id, &setup.hub.id, "Undo")
        .await
        .unwrap();

    tracing::info!("Undo operations: {:?}", undo_ops);

    // Verify that the undo response contains the delete operation
    assert!(
        !undo_ops.is_empty(),
        "Undo should return at least one operation"
    );

    // The undo operation should be a deletion of the inserted text
    let actual_op = &undo_ops[0];
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
    let send_undo_resp = setup.spokes[0]
        .editor
        .send_ops(
            doc_id,
            &setup.hub.id,
            vec![serde_json::json!({
                "op": "Undo",
                "groupSeq": 2
            })],
        )
        .await
        .unwrap();
    tracing::info!("SendOps (undo) response: {:?}", send_undo_resp);

    sleep(Duration::from_millis(100)).await;

    // Test redo
    let redo_ops = setup.spokes[0]
        .editor
        .send_undo(doc_id, &setup.hub.id, "Redo")
        .await
        .unwrap();

    tracing::info!("Redo operations: {:?}", redo_ops);

    // Verify that the redo response contains the insert operation
    assert!(
        !redo_ops.is_empty(),
        "Redo should return at least one operation"
    );

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

#[tokio::test]
async fn test_share_file_e2e() {
    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 0, None).await.unwrap();

    // Share a file with some content
    let (doc_id, site_id) = setup
        .hub
        .editor
        .share_file(
            "test_file.txt",
            "Hello, this is shared content!",
            serde_json::json!({"author": "test"}),
        )
        .await
        .unwrap();

    // Verify we got a valid doc_id
    assert!(doc_id > 0, "Should receive a valid doc_id");
    assert_eq!(site_id, 0, "Hub should have site_id 0");

    // Test that we can send ops to the shared doc
    // Content is "Hello, this is shared content!" (30 chars), so insert at position 30
    let ops = vec![serde_json::json!({
        "op": {"Ins": [30, " More text."]},
        "groupSeq": 1
    })];

    let _ = setup
        .hub
        .editor
        .send_ops(doc_id, &setup.hub.id, ops)
        .await
        .unwrap();

    // Share another file and verify it gets a different doc_id
    let (doc_id2, site_id2) = setup
        .hub
        .editor
        .share_file(
            "another_file.txt",
            "Different content",
            serde_json::json!({}),
        )
        .await
        .unwrap();

    assert_ne!(doc_id, doc_id2, "Second file should have different doc_id");
    assert_eq!(site_id2, 0, "Hub should always have site_id 0");

    setup.cleanup();
}

// *** Tests for expand_project_paths feature ***

#[tokio::test]
async fn test_expand_project_paths_home_directory() {
    // Test: Paths starting with ~ should be expanded to home directory
    use crate::config_man::ConfigProject;
    use std::env;

    let home_dir = env::var("HOME").unwrap_or_else(|_| "CAN’t GET HOME DIR".to_string());

    let mut projects = vec![
        ConfigProject {
            name: "home_project".to_string(),
            path: "~/my_project".to_string(),
        },
        ConfigProject {
            name: "nested_home".to_string(),
            path: "~/Documents/code/project".to_string(),
        },
    ];

    // Call the function under test
    let result = super::expand_project_paths(&mut projects);
    assert!(
        result.is_ok(),
        "expand_project_paths should succeed for ~ paths"
    );

    // Verify paths are expanded
    assert_eq!(
        projects[0].path,
        format!("{}/my_project", home_dir),
        "~ should be expanded to home directory"
    );
    assert_eq!(
        projects[1].path,
        format!("{}/Documents/code/project", home_dir),
        "~/Documents path should be expanded correctly"
    );

    // Verify all paths are now absolute
    for project in &projects {
        assert!(
            std::path::Path::new(&project.path).is_absolute(),
            "Expanded path {} should be absolute",
            project.path
        );
    }
}

#[tokio::test]
async fn test_expand_project_paths_relative_error() {
    // Test: Relative paths should cause an error
    use crate::config_man::ConfigProject;

    let mut projects = vec![ConfigProject {
        name: "relative_project".to_string(),
        path: "./my_project".to_string(),
    }];

    // Call the function under test
    let result = super::expand_project_paths(&mut projects);
    assert!(
        result.is_err(),
        "expand_project_paths should fail for relative paths"
    );

    // Verify error message
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("is not absolute"),
        "Error should mention path is not absolute"
    );
    assert!(
        err.to_string()
            .contains("All project paths must be absolute"),
        "Error should mention requirement for absolute paths"
    );
}

#[tokio::test]
async fn test_declare_projects_relative_path_error() {
    // Test: DeclareProjects should return error for relative paths

    let env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&env, 0, None).await.unwrap();

    // Try to declare a project with a relative path
    let result = setup
        .hub
        .editor
        .request(
            "DeclareProjects",
            serde_json::json!({
                "projects": [{
                    "name": "relative_project",
                    "path": "./my_relative_project"
                }]
            }),
        )
        .await;

    // Should return an error
    assert!(
        result.is_err(),
        "DeclareProjects should fail for relative paths"
    );

    setup.cleanup();
}

#[tokio::test]
async fn test_list_files_sorted_alphanumerically() {
    let mut env = TestEnvironment::new().await.unwrap();
    let project_dir = tempfile::TempDir::new().unwrap();

    // Create files with names that would be out of order if not sorted
    std::fs::write(project_dir.path().join("zebra.txt"), "content").unwrap();
    std::fs::write(project_dir.path().join("apple.txt"), "content").unwrap();
    std::fs::write(project_dir.path().join("banana.txt"), "content").unwrap();
    std::fs::write(project_dir.path().join("1_first.txt"), "content").unwrap();
    std::fs::write(project_dir.path().join("10_tenth.txt"), "content").unwrap();
    std::fs::write(project_dir.path().join("2_second.txt"), "content").unwrap();

    let project_path = project_dir.path().to_string_lossy().to_string();

    let mut setup = setup_hub_and_spoke_servers(&mut env, 1, None)
        .await
        .unwrap();

    // Declare project on hub
    setup
        .hub
        .editor
        .request(
            "DeclareProjects",
            serde_json::json!({
                "projects": [
                    {
                        "path": project_path.clone(),
                        "name": "TestProject",
                    }
                ]
            }),
        )
        .await
        .unwrap();

    sleep(Duration::from_millis(100)).await;

    // List files in the project directory
    let req_id = 1;
    setup.spokes[0]
        .editor
        .send_request(
            req_id,
            "ListFiles",
            serde_json::json!({
                "dir": {
                    "type": "projectFile",
                    "hostId": setup.hub.id.clone(),
                    "project": "TestProject",
                    "file": "",
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

    // Verify we have 6 files
    assert_eq!(files.len(), 6);

    // Extract filenames to check order
    let filenames: Vec<String> = files
        .iter()
        .map(|f| f["filename"].as_str().unwrap().to_string())
        .collect();

    // Verify files are sorted alphanumerically
    assert_eq!(filenames[0], "10_tenth.txt");
    assert_eq!(filenames[1], "1_first.txt");
    assert_eq!(filenames[2], "2_second.txt");
    assert_eq!(filenames[3], "apple.txt");
    assert_eq!(filenames[4], "banana.txt");
    assert_eq!(filenames[5], "zebra.txt");
}

#[tokio::test]
async fn test_open_binary_file_rejected() {
    let mut env = TestEnvironment::new().await.unwrap();
    let project_dir = tempfile::TempDir::new().unwrap();

    // Create a binary file
    let binary_file_path = project_dir.path().join("test.bin");
    let binary_content: Vec<u8> = vec![0x00, 0xFF, 0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x42];
    std::fs::write(&binary_file_path, binary_content).unwrap();

    let project_path = project_dir.path().to_string_lossy().to_string();

    let mut setup = setup_hub_and_spoke_servers(&mut env, 1, None)
        .await
        .unwrap();

    // Declare project on hub
    setup
        .hub
        .editor
        .request(
            "DeclareProjects",
            serde_json::json!({
                "projects": [
                    {
                        "path": project_path.clone(),
                        "name": "TestProject",
                    }
                ]
            }),
        )
        .await
        .unwrap();

    sleep(Duration::from_millis(100)).await;

    // Try to open the binary file
    let req_id = 1;
    setup
        .hub
        .editor
        .send_request(
            req_id,
            "OpenFile",
            serde_json::json!({
                "fileDesc": {
                    "type": "projectFile",
                    "hostId": setup.hub.id.clone(),
                    "project": "TestProject",
                    "file": "test.bin",
                },
                "mode": "open"
            }),
        )
        .await
        .unwrap();

    let resp = setup.hub.editor.wait_for_response(req_id, 5).await;

    // Verify that we get an error response
    assert!(resp.is_err() || resp.as_ref().unwrap().get("error").is_some());

    if let Ok(response) = resp {
        if let Some(error) = response.get("error") {
            let error_message = error.get("message").unwrap().as_str().unwrap();
            assert!(error_message.contains("Cannot open binary file"));
        }
    } else if let Err(err) = resp {
        let error_str = err.to_string();
        assert!(error_str.contains("Cannot open binary file"));
    }

    setup.cleanup();
}

#[tokio::test]
async fn test_doc_project_handling() {
    let mut env = TestEnvironment::new().await.unwrap();
    let mut setup = setup_hub_and_spoke_servers(&mut env, 1, None)
        .await
        .unwrap();

    // Test 1: Attempt to declare a project named "_buffers" should be rejected.
    let resp = setup
        .hub
        .editor
        .request(
            "DeclareProjects",
            serde_json::json!({
                "projects": [
                    {
                        "name": "_buffers",
                        "path": "/tmp/test",
                    }
                ]
            }),
        )
        .await;

    // Should get an error.
    assert!(resp.is_err());
    if let Err(err) = resp {
        let error_str = err.to_string();
        assert!(error_str.contains("reserved"));
    }

    // Test 2: Create a shared file and access it via _buffers project.
    let share_resp = setup
        .hub
        .editor
        .request(
            "ShareFile",
            serde_json::json!({
                "filename": "test_shared.txt",
                "content": "This is a shared document",
                "meta": {}
            }),
        )
        .await
        .unwrap();

    let doc_id = share_resp["docId"].as_u64().unwrap();

    sleep(Duration::from_millis(100)).await;

    // Test 3: Open the shared file using _buffers project.
    let req_id = 1;
    setup
        .hub
        .editor
        .send_request(
            req_id,
            "OpenFile",
            serde_json::json!({
                "fileDesc": {
                    "type": "projectFile",
                    "hostId": setup.hub.id.clone(),
                    "project": "_buffers",
                    "file": "test_shared.txt",
                },
                "mode": "open"
            }),
        )
        .await
        .unwrap();

    let open_resp = setup.hub.editor.wait_for_response(req_id, 5).await.unwrap();

    // Verify the file opened successfully.
    assert_eq!(
        open_resp["content"].as_str().unwrap(),
        "This is a shared document"
    );
    assert_eq!(open_resp["filename"].as_str().unwrap(), "test_shared.txt");
    assert_eq!(open_resp["docId"].as_u64().unwrap(), doc_id);

    // Test 4: List files in _buffers project.
    let req_id = 2;
    setup
        .hub
        .editor
        .send_request(
            req_id,
            "ListFiles",
            serde_json::json!({
                "dir": {
                    "type": "project",
                    "hostId": setup.hub.id.clone(),
                    "id": "_buffers",
                },
                "signalingAddr": env.signaling_url(),
                "credential": "test"
            }),
        )
        .await
        .unwrap();

    let list_resp = setup.hub.editor.wait_for_response(req_id, 5).await.unwrap();

    let files = list_resp["files"].as_array().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["filename"].as_str().unwrap(), "test_shared.txt");
    assert_eq!(files[0]["isDirectory"].as_bool().unwrap(), false);

    // Test 5: Try to open a non-existent file in _doc.
    let req_id = 3;
    setup
        .hub
        .editor
        .send_request(
            req_id,
            "OpenFile",
            serde_json::json!({
                "fileDesc": {
                    "type": "projectFile",
                    "hostId": setup.hub.id.clone(),
                    "project": "_doc",
                    "file": "nonexistent.txt",
                },
                "mode": "open"
            }),
        )
        .await
        .unwrap();

    let error_resp = setup.hub.editor.wait_for_response(req_id, 5).await;

    // Should get an error.
    assert!(error_resp.is_err() || error_resp.as_ref().unwrap().get("error").is_some());

    setup.cleanup();
}

#[tokio::test]
async fn test_server_run_config_projects_expansion() {
    // Test: Projects from config should be expanded when server starts
    use crate::config_man::{AcceptMode, Config, ConfigManager, ConfigProject};
    use std::collections::HashMap;

    init_test_tracing();

    // Create a temp directory for config
    let temp_dir = tempfile::TempDir::new().unwrap();
    let config_path = temp_dir.path().to_path_buf();

    // Create a test project directory
    let project_dir = create_test_project().unwrap();

    // Create config with projects using ~ path
    let home_dir = std::env::var("HOME").unwrap_or_else(|_| "/home/user".to_string());
    let config = Config {
        projects: vec![
            ConfigProject {
                name: "home_project".to_string(),
                path: "~/my_test_project".to_string(),
            },
            ConfigProject {
                name: "absolute_project".to_string(),
                path: project_dir.path().to_string_lossy().to_string(),
            },
        ],
        trusted_hosts: HashMap::new(),
        accept_mode: AcceptMode::All,
        host_id: Some("test-server".to_string()),
        permission: HashMap::new(),
    };

    // Create ConfigManager
    let mut config_manager = ConfigManager::new(Some(config_path), None).unwrap();
    config_manager.replace_and_save(config).unwrap();

    // Create and run server
    let host_id = "test-server".to_string();

    let mut server = Server::new(host_id.clone(), config_manager).unwrap();

    // Create channels for server
    let (editor_to_server_tx, editor_to_server_rx) = mpsc::channel(100);
    let (server_to_editor_tx, mut server_to_editor_rx) = mpsc::channel(100);

    // Run server in background task
    let server_task =
        tokio::spawn(async move { server.run(server_to_editor_tx, editor_to_server_rx).await });

    // Give server time to initialize
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Send Initialize request to verify server is running
    let init_request = lsp_server::Request {
        id: lsp_server::RequestId::from(1),
        method: "Initialize".to_string(),
        params: serde_json::json!({}),
    };

    editor_to_server_tx
        .send(lsp_server::Message::Request(init_request))
        .await
        .unwrap();

    // Wait for response
    let timeout_duration = Duration::from_secs(1);
    let response = tokio::time::timeout(timeout_duration, server_to_editor_rx.recv()).await;

    assert!(response.is_ok(), "Should receive response from server");

    // Verify the response contains the host_id
    if let Ok(Some(lsp_server::Message::Response(resp))) = response {
        assert!(resp.error.is_none(), "Initialize should succeed");
        let result = resp.result.unwrap();
        assert_eq!(result["hostId"], "test-server");
    } else {
        panic!("Expected Initialize response");
    }

    // Clean up
    server_task.abort();
}

#[tokio::test]
async fn test_send_ops_permission_denied() {
    let env = TestEnvironment::new().await.unwrap();

    // Set up hub and spoke with write permission denied
    let mut setup = setup_hub_and_spoke_servers(
        &env,
        1,
        Some(vec![Permission {
            write: false,
            create: true,
            delete: false,
        }]),
    )
    .await
    .unwrap();

    // Share a file on the hub
    let (doc_id, _site_id) = setup
        .hub
        .editor
        .share_file("test.txt", "initial content", serde_json::json!({}))
        .await
        .unwrap();

    // Spoke opens the file first
    let _open_result = setup.spokes[0]
        .editor
        .request(
            "OpenFile",
            serde_json::json!({
                "fileDesc": {
                    "type": "file",
                    "hostId": setup.hub.id.clone(),
                    "id": "test.txt",
                },
                "mode": "open",
            }),
        )
        .await
        .unwrap();

    // Wait a bit for the connection to stabilize
    sleep(Duration::from_millis(100)).await;

    // Try to send ops from spoke (should be denied)
    // SendOps succeeds locally, but permission check happens async on hub
    let result = setup.spokes[0]
        .editor
        .send_ops(
            doc_id,
            &setup.hub.id,
            vec![serde_json::json!({
                "op": {"Ins": [0, "new text"]},
                "groupSeq": 0,
            })],
        )
        .await;

    // SendOps should succeed locally (it just queues the op)
    assert!(
        result.is_ok(),
        "SendOps should succeed locally, got error: {:?}",
        result
    );

    // Wait for InternalError notification about permission denial
    let notification = setup.spokes[0]
        .editor
        .wait_for_notification(&NotificationCode::InternalError.to_string(), 5)
        .await;

    assert!(
        notification.is_ok(),
        "Expected InternalError notification for permission denial, timeout waiting"
    );

    // Verify the error message contains "denied"
    if let Ok(params) = notification {
        let message = params
            .as_object()
            .and_then(|obj| obj.get("message"))
            .and_then(|msg| msg.as_str())
            .unwrap_or("");
        assert!(
            message.contains("denied"),
            "Expected message containing 'denied', got: {}",
            message
        );
    }
}

#[tokio::test]
async fn test_create_file_permission_denied() {
    let env = TestEnvironment::new().await.unwrap();

    // Set up hub and spoke with create permission denied
    let mut setup = setup_hub_and_spoke_servers(
        &env,
        1,
        Some(vec![Permission {
            write: true,
            create: false,
            delete: false,
        }]),
    )
    .await
    .unwrap();

    // Try to create a new file from spoke
    // This should return an error response due to permission denial
    let result = setup.spokes[0]
        .editor
        .request(
            "OpenFile",
            serde_json::json!({
                "fileDesc": {
                    "type": "projectFile",
                    "hostId": setup.hub.id.clone(),
                    "project": "test-project",
                    "file": "new-file.txt",
                },
                "mode": "create",
            }),
        )
        .await;

    // Should receive error response
    assert!(
        result.is_err(),
        "Expected error response for create permission denial, got: {:?}",
        result
    );

    // Verify the error is permission-related
    if let Err(e) = result {
        let error_str = e.to_string();
        assert!(
            error_str.contains("Permission")
                || error_str.contains("permission")
                || error_str.contains("denied"),
            "Expected permission error, got: {}",
            error_str
        );
    }
}

#[tokio::test]
async fn test_delete_file_permission_denied() {
    let env = TestEnvironment::new().await.unwrap();

    // Set up hub and spoke with delete permission denied
    let mut setup = setup_hub_and_spoke_servers(
        &env,
        1,
        Some(vec![Permission {
            write: true,
            create: true,
            delete: false,
        }]),
    )
    .await
    .unwrap();

    // First, share a file on the hub
    let (_doc_id, _site_id) = setup
        .hub
        .editor
        .share_file("test.txt", "test content", serde_json::json!({}))
        .await
        .unwrap();

    // Try to delete the file from spoke
    // This should return an error response due to permission denial
    let result = setup.spokes[0]
        .editor
        .request(
            "DeleteFile",
            serde_json::json!({
                "file": {
                    "type": "projectFile",
                    "hostId": setup.hub.id.clone(),
                    "project": "test-project",
                    "file": "test.txt",
                },
            }),
        )
        .await;

    // Should receive error response
    assert!(
        result.is_err(),
        "Expected error response for delete permission denial, got: {:?}",
        result
    );

    // Verify the error is permission-related
    if let Err(e) = result {
        let error_str = e.to_string();
        assert!(
            error_str.contains("Permission")
                || error_str.contains("permission")
                || error_str.contains("denied"),
            "Expected permission error, got: {}",
            error_str
        );
    }
}
