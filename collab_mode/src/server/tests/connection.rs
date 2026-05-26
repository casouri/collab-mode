use super::*;
use crate::message::Msg;
use crate::webchannel;

/// A pair of `Server`s wired together through one `TestChannelFactory`,
/// with both editors having already observed `Connected` from the other side.
/// Server2 is the connector, so it’s responsible for reconnect.
struct ConnectedPair {
    factory: TestChannelFactory,
    // Held so server1’s editor_rx stays open for the lifetime of the pair.
    #[allow(dead_code)]
    mock_editor1: MockEditor,
    mock_editor2: MockEditor,
    server1_handle: tokio::task::JoinHandle<()>,
    server2_handle: tokio::task::JoinHandle<()>,
    id1: ServerId,
    #[allow(dead_code)]
    id2: ServerId,
}

impl ConnectedPair {
    fn abort(self) {
        self.server1_handle.abort();
        self.server2_handle.abort();
    }
}

/// Stand up two `Server`s sharing one in-memory factory and signaling
/// state, do the `AcceptConnection` / `Connect` handshake, and return
/// once both editors have seen `Connected` from the other side.
async fn setup_connected_pair() -> ConnectedPair {
    let factory = TestChannelFactory::new();

    let id1 = "server1::test-cert-hash".to_string();
    let id2 = "server2::test-cert-hash".to_string();

    tracing::info!("Creating servers with IDs: {} and {}", id1, id2);

    let temp_dir1 = tempfile::TempDir::new().unwrap();
    let mut config1 = ConfigManager::new(Some(temp_dir1.path().to_path_buf()), None).unwrap();
    {
        let mut cfg = config1.config();
        cfg.trusted_hosts.insert(id2.clone());
        config1.replace_and_save(cfg).unwrap();
    }
    let mut server1 = Server::new(id1.clone(), config1).unwrap();

    let temp_dir2 = tempfile::TempDir::new().unwrap();
    let mut config2 = ConfigManager::new(Some(temp_dir2.path().to_path_buf()), None).unwrap();
    {
        let mut cfg = config2.config();
        cfg.trusted_hosts.insert(id1.clone());
        config2.replace_and_save(cfg).unwrap();
    }
    let mut server2 = Server::new(id2.clone(), config2).unwrap();

    let (mut mock_editor1, server_tx1, server_rx1) = MockEditor::new();
    let (mut mock_editor2, server_tx2, server_rx2) = MockEditor::new();

    let signaling_state = Arc::new(std::sync::Mutex::new(TestFactoryState::default()));

    let server1_handle = {
        let channel_factory = factory.clone();
        let signaling_state = signaling_state.clone();
        let id1 = id1.clone();
        tokio::spawn(async move {
            let web_factory: crate::server::WebChannelFactory =
                Box::new(move |msg_tx, self_tx| channel_factory.get_channel(id1, msg_tx, self_tx));
            let sig_factory: crate::server::SignalingChannelFactory = Box::new(move |sig_msg_tx| {
                SignalingChannel::new_for_test(sig_msg_tx, signaling_state)
            });
            let shutdown = std::sync::Arc::new(tokio::sync::Notify::new());
            let (fw_a_tx, fw_a_rx) =
                tokio::sync::mpsc::channel::<crate::filewatch_receptor::WatchFileMessage>(1);
            let (fw_b_tx, _fw_b_rx) =
                crossbeam_channel::bounded::<crate::filewatch_receptor::WatchFileMessage>(1);
            let _hold_fw_a_tx = fw_a_tx;
            if let Err(e) = server1
                .run(
                    server_tx1,
                    server_rx1,
                    fw_b_tx,
                    fw_a_rx,
                    web_factory,
                    sig_factory,
                    shutdown,
                )
                .await
            {
                tracing::error!("Server1 error: {}", e);
            }
        })
    };

    let server2_handle = {
        let channel_factory = factory.clone();
        let signaling_state = signaling_state.clone();
        let id2 = id2.clone();
        tokio::spawn(async move {
            let web_factory: crate::server::WebChannelFactory =
                Box::new(move |msg_tx, self_tx| channel_factory.get_channel(id2, msg_tx, self_tx));
            let sig_factory: crate::server::SignalingChannelFactory = Box::new(move |sig_msg_tx| {
                SignalingChannel::new_for_test(sig_msg_tx, signaling_state)
            });
            let shutdown = std::sync::Arc::new(tokio::sync::Notify::new());
            let (fw_a_tx, fw_a_rx) =
                tokio::sync::mpsc::channel::<crate::filewatch_receptor::WatchFileMessage>(1);
            let (fw_b_tx, _fw_b_rx) =
                crossbeam_channel::bounded::<crate::filewatch_receptor::WatchFileMessage>(1);
            let _hold_fw_a_tx = fw_a_tx;
            if let Err(e) = server2
                .run(
                    server_tx2,
                    server_rx2,
                    fw_b_tx,
                    fw_a_rx,
                    web_factory,
                    sig_factory,
                    shutdown,
                )
                .await
            {
                tracing::error!("Server2 error: {}", e);
            }
        })
    };

    // Both sides bind to the signaling server.
    mock_editor1
        .send_notification(
            "AcceptConnection",
            serde_json::json!({
                "hostId": id1.clone(),
                "addr": "test",
                "transportType": "SCTP",
            }),
        )
        .await
        .unwrap();
    mock_editor1
        .wait_for_notification(&NotificationCode::AcceptingConnection.to_string(), 5)
        .await
        .expect("server1 should accept");

    mock_editor2
        .send_notification(
            "AcceptConnection",
            serde_json::json!({
                "addr": "test",
                "transportType": "SCTP",
            }),
        )
        .await
        .unwrap();
    mock_editor2
        .wait_for_notification(&NotificationCode::AcceptingConnection.to_string(), 5)
        .await
        .expect("server2 should accept");

    // Server2 initiates the connection to Server1.
    mock_editor2
        .send_notification(
            "Connect",
            serde_json::json!({
                "hostId": id1.clone(),
                "transportConfig": { "SCTP": { "signalingAddr": "test" } },
            }),
        )
        .await
        .unwrap();
    mock_editor2
        .wait_for_notification(&NotificationCode::Connecting.to_string(), 5)
        .await
        .expect("server2 should report Connecting");

    // Both editors should observe Connected from the other side.
    let connected = NotificationCode::Connected.to_string();
    let params1 = mock_editor1
        .wait_for_notification(&connected, 30)
        .await
        .expect("editor1 should receive Connected");
    assert_eq!(
        params1.get("hostId").and_then(|v| v.as_str()).unwrap_or(""),
        id2,
        "editor1 should receive Connected from server2"
    );

    let params2 = mock_editor2
        .wait_for_notification(&connected, 30)
        .await
        .expect("editor2 should receive Connected");
    assert_eq!(
        params2.get("hostId").and_then(|v| v.as_str()).unwrap_or(""),
        id1,
        "editor2 should receive Connected from server1"
    );

    ConnectedPair {
        factory,
        mock_editor1,
        mock_editor2,
        server1_handle,
        server2_handle,
        id1,
        id2,
    }
}

#[tokio::test]
async fn test_accept_connect() {
    // Basic connection establishment between two servers — both editors
    // receive Connected notifications.
    let pair = setup_connected_pair().await;
    tracing::info!("Test completed successfully");
    pair.abort();
}

#[tokio::test]
async fn test_connection_broke_triggers_recovery() {
    // After a healthy Connect, simulate the webchannel surfacing a
    // ConnectionBroke on server2 (the connector) for its peer server1.
    // The server should: (a) forward ConnectionBroke to editor2,
    // (b) mark server1 as Disconnected, and (c) the fix_world tick
    // should re-attempt the connection.
    let mut pair = setup_connected_pair().await;

    let broke = webchannel::Message {
        host: pair.id2.clone(),
        body: Msg::ConnectionBroke {
            peer: pair.id1.clone(),
        },
        req_id: None,
    };
    pair.factory
        .inject_message(&pair.id2, broke)
        .await
        .expect("inject ConnectionBroke");

    let broke_params = pair
        .mock_editor2
        .wait_for_notification(&NotificationCode::ConnectionBroke.to_string(), 5)
        .await
        .expect("editor2 should receive ConnectionBroke");
    assert_eq!(
        broke_params
            .get("hostId")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        pair.id1,
        "ConnectionBroke should be for server1"
    );

    // Initial retry stride is 1s and the fix_world tick runs once a
    // second, so the reconnect should be observed within ~3s.
    let connecting_params = pair
        .mock_editor2
        .wait_for_notification(&NotificationCode::Connecting.to_string(), 5)
        .await
        .expect("editor2 should receive Connecting (reconnect attempt)");
    assert_eq!(
        connecting_params
            .get("hostId")
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        pair.id1,
        "reconnect should target server1"
    );

    pair.abort();
}
