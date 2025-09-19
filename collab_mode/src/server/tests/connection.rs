use super::*;

#[tokio::test]
async fn test_accept_connect() {
    // Test: Basic connection establishment between two servers.
    // Expected: Both editors receive Connected messages indicating successful connection.

    let env = TestEnvironment::new().await.unwrap();

    let id1 = "server1".to_string();
    let id2 = "server2".to_string();

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

    // Wait for Connected messages from both sides.
    let timeout_duration = Duration::from_secs(30);
    let start_time = std::time::Instant::now();

    let mut connected_received_count = 0;
    let connection_progress_method = NotificationCode::ConnectionProgress.to_string();
    let connected_method = NotificationCode::Connected.to_string();

    while connected_received_count < 2 && start_time.elapsed() < timeout_duration {
        // Check editor1 for messages.
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

        // Check editor2 for messages.
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
    }

    assert_eq!(
        connected_received_count, 2,
        "Both sides should be connected"
    );

    tracing::info!("Test completed successfully");

    server1_handle.abort();
    server2_handle.abort();
}
