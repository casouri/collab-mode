use super::*;
use crate::config_man::ConfigManager;
use crate::server::{Server, SignalingChannelFactory, WebChannelFactory};
use crate::signaling::client::SignalingChannel;
use crate::webchannel::WebChannel;

/// Tempdirs and channels held by an envoy e2e test. Drops
/// `mock_editor` first (signaling shutdown), then the host server,
/// then the tempdirs.
struct EnvoyHarness {
    mock_editor: MockEditor,
    server_handle: tokio::task::JoinHandle<()>,
    /// The shared project directory; tests assert on disk contents
    /// inside it.
    project_dir: tempfile::TempDir,
    /// Held to keep host config alive.
    _host_config_dir: tempfile::TempDir,
}

/// Spin up a host `Server`, mock editor, and create a real ssh
/// connection to `yuan@localhost` running this same crate's envoy
/// binary. Returns once the host has received `Connected`.
async fn connect_envoy_via_ssh(host_id: &str) -> EnvoyHarness {
    init_test_tracing();

    let host_config_dir = tempfile::TempDir::new().unwrap();
    let project_dir = tempfile::TempDir::new().unwrap();
    let project_path = project_dir.path().to_string_lossy().to_string();

    let host_config = ConfigManager::new(Some(host_config_dir.path().to_owned()), None).unwrap();
    let mut host_server = Server::new(host_id.to_string(), host_config).unwrap();

    let (mut mock_editor, server_to_editor_tx, editor_to_server_rx) = MockEditor::new();

    // NOTE: envoy tests are `#[ignore]`d — they need Transport::Ssh,
    // which the actor-model WebChannel doesn’t handle yet (the SshRemote
    // refactor will restore it). This factory just hands back a vanilla
    // WebChannel so the file compiles.
    let host_id_for_factory = host_id.to_string();
    let web_factory: WebChannelFactory =
        Box::new(move |msg_tx, self_tx| WebChannel::new(host_id_for_factory, msg_tx, self_tx));
    let sig_factory: SignalingChannelFactory = Box::new(SignalingChannel::new);

    let server_handle = tokio::spawn(async move {
        let shutdown = std::sync::Arc::new(tokio::sync::Notify::new());
        if let Err(e) = host_server
            .run(
                server_to_editor_tx,
                editor_to_server_rx,
                web_factory,
                sig_factory,
                shutdown,
            )
            .await
        {
            tracing::error!("Host server error: {}", e);
        }
    });

    let manifest_path = format!("{}/Cargo.toml", env!("CARGO_MANIFEST_DIR"));
    let command = vec![
        "cargo".to_string(),
        "run".to_string(),
        "--quiet".to_string(),
        "--manifest-path".to_string(),
        manifest_path,
        "--bin".to_string(),
        "collab_mode".to_string(),
        "--".to_string(),
        "envoy".to_string(),
    ];

    mock_editor
        .send_notification(
            "Connect",
            serde_json::json!({
                "hostId": "envoy-peer",
                "transportConfig": {
                    "SshStdio": {
                        "sshHost": "yuan@localhost",
                        "command": command,
                        "projects": [
                            { "name": "demo", "path": project_path }
                        ]
                    }
                }
            }),
        )
        .await
        .unwrap();

    // Cargo build could be slow; allow generous timeout.
    let connected = mock_editor
        .wait_for_notification(&NotificationCode::Connected.to_string(), 120)
        .await;
    assert!(
        connected.is_ok(),
        "Expected Connected notification within 120s, got {:?}",
        connected
    );

    EnvoyHarness {
        mock_editor,
        server_handle,
        project_dir,
        _host_config_dir: host_config_dir,
    }
}

/// Tear down: drop the mock editor so editor_rx's held tx is gone;
/// the server task then exits, the `SshRemote` actor drops, the ssh
/// session closes, and the remote envoy is killed.
async fn teardown(harness: EnvoyHarness) {
    drop(harness.mock_editor);
    let _ = tokio::time::timeout(Duration::from_secs(5), harness.server_handle).await;
}

/// End-to-end test that opens a ssh connection to `yuan@localhost`
/// and runs `cargo run --bin collab_mode -- envoy` as the remote.
///
/// Run with: `cargo test envoy_e2e_ssh_localhost -- --ignored --nocapture`.
#[tokio::test]
#[ignore]
async fn envoy_e2e_ssh_localhost() {
    let harness = connect_envoy_via_ssh("envoy-test-host").await;
    tracing::info!("Envoy connected — handshake completed");
    teardown(harness).await;
}

/// End-to-end test that creates a file on the envoy, writes content,
/// saves it, then verifies the content on disk.
///
/// Run with:
/// `cargo test envoy_e2e_create_write_save -- --ignored --nocapture`.
#[tokio::test]
#[ignore]
async fn envoy_e2e_create_write_save() {
    let mut harness = connect_envoy_via_ssh("envoy-test-host-create").await;

    let file_desc = serde_json::json!({
        "hostId": "envoy-peer",
        "project": "demo",
        "file": "created.txt"
    });

    // Create a new file on the envoy.
    let create_resp = harness
        .mock_editor
        .request_expect(
            "OpenFile",
            serde_json::json!({
                "file": file_desc,
                "mode": "create",
            }),
            10,
        )
        .await
        .expect("OpenFile create should succeed");
    let file = create_resp["file"].clone();
    let content = create_resp["content"].as_str().unwrap();
    assert_eq!(content, "", "newly created file should be empty");

    // Insert text via an op.
    let text = "hello envoy world";
    let ops = vec![serde_json::json!({
        "op": {"kind": "Ins", "pos": 0, "content": text},
        "groupSeq": 1,
    })];
    harness
        .mock_editor
        .send_ops(file.clone(), ops)
        .await
        .expect("SendOps should succeed");

    // Save the file.
    harness
        .mock_editor
        .request_expect("SaveFile", serde_json::json!({ "file": file }), 10)
        .await
        .expect("SaveFile should succeed");

    // Verify on disk. Since envoy runs on localhost, the project_dir
    // tempdir is the same path on both ends.
    let saved_path = harness.project_dir.path().join("created.txt");
    let saved_content =
        std::fs::read_to_string(&saved_path).expect("created.txt should exist on disk");
    assert_eq!(
        saved_content, text,
        "file content on disk should match what we sent"
    );

    teardown(harness).await;
}
