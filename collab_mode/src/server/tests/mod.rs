// Shared test utilities and setup for server tests.
// This module re-exports the parent server items so submodules can `use super::*`.

#![allow(unused_imports)]

pub use super::*;

use crate::config_man::{ConfigManager, Permission};
use crate::signaling;
use rand::Rng;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
pub use std::time::Duration;
use tokio::sync::mpsc;
pub use tokio::time::{sleep, timeout};
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

/// Wait for the signaling server to be ready by attempting to connect to it.
/// Returns Ok(()) when connection succeeds, Err if timeout is reached.
async fn wait_for_signaling_server(url: &str, timeout: Duration) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    let retry_interval = Duration::from_millis(10);

    while start.elapsed() < timeout {
        // Try to connect to the WebSocket server
        match tokio_tungstenite::connect_async(url).await {
            Ok((ws_stream, _response)) => {
                // Connection succeeded, close it and return
                drop(ws_stream);
                tracing::debug!("Signaling server is ready at {}", url);
                return Ok(());
            }
            Err(_) => {
                // Server not ready yet, wait a bit and retry
                if start.elapsed() + retry_interval < timeout {
                    sleep(retry_interval).await;
                }
            }
        }
    }

    Err(anyhow::anyhow!(
        "Signaling server failed to start within {:?}",
        timeout
    ))
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

        let signaling_url = format!("ws://{}", addr);

        // Wait for signaling server to be ready
        wait_for_signaling_server(&signaling_url, Duration::from_secs(5)).await?;

        Ok(Self {
            signaling_addr: signaling_url,
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

    // Helper: Send request and wait for response in one call.
    async fn request(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        self.request_expect(method, params, 5).await
    }

    // Helper: Send request and wait for response with custom timeout.
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

    // Helper: Drain any pending response without waiting.
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

    // Helper: Wait for Connected notification and extract fields.
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

    // Test-specific helper methods

    /// Helper: Open a file and return editor file desc, site_id, and content.
    #[cfg(any(test, feature = "test-runner"))]
    pub async fn open_file(
        &mut self,
        file_desc: serde_json::Value,
    ) -> anyhow::Result<(serde_json::Value, u32, String)> {
        let resp = self
            .request(
                "OpenFile",
                serde_json::json!({
                    "file": file_desc,
                    "mode": "open",
                }),
            )
            .await?;
        let file = resp["file"].clone();
        let site_id = resp["siteId"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("Missing siteId in response"))?
            as u32;
        let content = resp["content"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing content in response"))?
            .to_string();

        Ok((file, site_id, content))
    }

    /// Helper: Send ops to a document.
    #[cfg(any(test, feature = "test-runner"))]
    pub async fn send_ops(
        &mut self,
        file: serde_json::Value,
        ops: Vec<serde_json::Value>,
    ) -> anyhow::Result<serde_json::Value> {
        self.request(
            "SendOps",
            serde_json::json!({
                "file": file,
                "ops": ops,
            }),
        )
        .await
    }

    /// Helper: Send undo/redo request.
    #[cfg(any(test, feature = "test-runner"))]
    pub async fn send_undo(
        &mut self,
        file: serde_json::Value,
        kind: &str,
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        let resp = self
            .request(
                "Undo",
                serde_json::json!({
                    "file": file,
                    "kind": kind,
                }),
            )
            .await?;

        resp["ops"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("Missing ops in undo response"))
            .map(|ops| ops.clone())
    }

    /// Helper: Send info to a document.
    #[cfg(any(test, feature = "test-runner"))]
    pub async fn send_info(
        &mut self,
        file: serde_json::Value,
        info: serde_json::Value,
    ) -> anyhow::Result<()> {
        self.send_notification(
            "SendInfo",
            serde_json::json!({
                "file": file,
                "info": info,
            }),
        )
        .await?;
        Ok(())
    }

    /// Helper: Declare a project.
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
    ) -> anyhow::Result<(serde_json::Value, u32)> {
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
        let file = resp["file"].clone();
        let site_id = resp["siteId"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("Missing siteId in response"))?
            as u32;
        Ok((file, site_id))
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
    // Validate spoke_permissions if provided.
    if let Some(ref perms) = spoke_permissions {
        if perms.len() != num_spokes {
            return Err(anyhow::anyhow!(
                "spoke_permissions must have exactly {} entries, got {}",
                num_spokes,
                perms.len()
            ));
        }
    }

    // Create hub server.
    let hub_id = create_test_id("hub");
    let hub_temp_dir = tempfile::TempDir::new()?;
    let mut hub_config = ConfigManager::new(Some(hub_temp_dir.path().to_path_buf()), None)?;

    // Generate spoke IDs upfront so we can use them consistently.
    let spoke_ids: Vec<ServerId> = (0..num_spokes)
        .map(|i| create_test_id(&format!("spoke{}", i + 1)))
        .collect();

    // Set the host_id in the config so permission checks work.
    let mut config = hub_config.config();
    config.host_id = Some(hub_id.clone());

    // Pre-add permissions for all spokes that will connect.
    for (i, spoke_id) in spoke_ids.iter().enumerate() {
        let permission = if let Some(ref perms) = spoke_permissions {
            perms[i].clone()
        } else {
            // Default to full permissions.
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

    // Hub starts accepting connections.
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

    // Create and connect spoke servers.
    let mut spokes = Vec::new();

    for (i, spoke_id) in spoke_ids.iter().enumerate() {
        let spoke_temp_dir = tempfile::TempDir::new()?;
        let mut spoke_config = ConfigManager::new(Some(spoke_temp_dir.path().to_path_buf()), None)?;

        // Set the host_id in the config and add permission for hub.
        let mut config = spoke_config.config();
        config.host_id = Some(spoke_id.clone());
        // Add permission for hub to have full access to this spoke.
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

        // Spoke connects to hub.
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

    // Wait for all connections to be established.
    tracing::info!("Waiting for all connections to be established");

    // Hub should receive Connected from each spoke.
    for _i in 0..num_spokes {
        let (host_id, _message) = hub_editor.expect_connected_notification(10).await?;
        tracing::info!("Hub received Connected from spoke: {}", host_id);
    }

    // Each spoke should receive Connected from hub.
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

/// Creates a test project directory with some sample files.
fn create_test_project() -> anyhow::Result<tempfile::TempDir> {
    let temp_dir = tempfile::TempDir::new()?;
    let project_path = temp_dir.path();

    // Create test files.
    std::fs::write(project_path.join("test.txt"), "Hello from test.txt")?;
    std::fs::write(
        project_path.join("readme.md"),
        "# Test Project\nThis is a test",
    )?;

    // Create a subdirectory with files.
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

/// Creates a complex project directory with nested structure.
fn create_complex_project() -> anyhow::Result<tempfile::TempDir> {
    let temp_dir = tempfile::TempDir::new()?;
    let project_path = temp_dir.path();

    // Root level files.
    std::fs::write(project_path.join("README.md"), "# Complex Project")?;
    std::fs::write(
        project_path.join("Cargo.toml"),
        "[package]\nname = \"test\"",
    )?;
    std::fs::write(project_path.join(".gitignore"), "target/\n*.swp")?;

    // src directory.
    let src_dir = project_path.join("src");
    std::fs::create_dir(&src_dir)?;
    std::fs::write(src_dir.join("main.rs"), "fn main() {}")?;
    std::fs::write(src_dir.join("lib.rs"), "pub mod modules;")?;

    // src/modules directory.
    let modules_dir = src_dir.join("modules");
    std::fs::create_dir(&modules_dir)?;
    std::fs::write(modules_dir.join("mod1.rs"), "pub fn func1() {}")?;
    std::fs::write(modules_dir.join("mod2.rs"), "pub fn func2() {}")?;
    std::fs::write(modules_dir.join("mod.rs"), "pub mod mod1;\npub mod mod2;")?;

    // tests directory.
    let tests_dir = project_path.join("tests");
    std::fs::create_dir(&tests_dir)?;
    std::fs::write(
        tests_dir.join("integration_test.rs"),
        "#[test]\nfn test() {}",
    )?;

    // docs directory.
    let docs_dir = project_path.join("docs");
    std::fs::create_dir(&docs_dir)?;
    std::fs::write(docs_dir.join("api.md"), "# API Documentation")?;
    std::fs::write(docs_dir.join("guide.md"), "# User Guide")?;

    // Empty directory.
    let empty_dir = project_path.join("empty");
    std::fs::create_dir(&empty_dir)?;

    Ok(temp_dir)
}

// Submodules per request/feature under test.
mod config;
mod connection;
mod delete_file;
mod doc_project;
mod list_files;
mod list_projects;
mod local_remote_ops;
mod open_file;
mod reconnection;
mod send_info;
mod send_ops;
mod share_file;
mod shared_file_ops;
mod undo;
