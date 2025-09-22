use super::*;
use std::time::Instant;
use tokio::time::timeout;

#[tokio::test]
async fn test_failed_connection_and_reconnect() {
    // Test: Server attempts to connect to non-existent remote and handles failure with reconnection
    // Expected: Server receives ConnectionBroke, attempts reconnection with exponential backoff

    let env = TestEnvironment::new().await.unwrap();

    let id1 = "server1".to_string();
    let non_existent_id = "non_existent_server".to_string();

    tracing::info!("Creating server with ID: {}", id1);

    let temp_dir1 = tempfile::TempDir::new().unwrap();
    let config1 = ConfigManager::new(Some(temp_dir1.path().to_path_buf()), None).unwrap();
    let mut server1 = Server::new(id1.clone(), config1).unwrap();

    let (mut mock_editor1, server_tx1, server_rx1) = MockEditor::new();

    let server1_handle = {
        tokio::spawn(async move {
            if let Err(e) = server1.run(server_tx1, server_rx1).await {
                tracing::error!("Server1 error: {}", e);
            }
        })
    };

    // Track when we start the connection attempt
    let start_time = Instant::now();

    tracing::info!("Server1 attempting to connect to non-existent server");
    mock_editor1
        .send_notification(
            "Connect",
            serde_json::json!({
                "hostId": non_existent_id.clone(),
                "signalingAddr": env.signaling_url(),
                "transportType": "SCTP",
            }),
        )
        .await
        .unwrap();

    // Should receive Connecting notification immediately
    let connecting_params = mock_editor1
        .wait_for_notification(&NotificationCode::Connecting.to_string(), 2)
        .await;
    assert!(
        connecting_params.is_ok(),
        "Should receive Connecting notification"
    );

    // Should receive ConnectionBroke notification when connection fails
    let connection_broke_params = mock_editor1
        .wait_for_notification(&NotificationCode::ConnectionBroke.to_string(), 10)
        .await;
    assert!(
        connection_broke_params.is_ok(),
        "Should receive ConnectionBroke notification for failed connection"
    );

    let params = connection_broke_params.unwrap();
    assert_eq!(
        params.get("hostId").and_then(|v| v.as_str()),
        Some(non_existent_id.as_str()),
        "ConnectionBroke should be for the non-existent server"
    );

    tracing::info!("Connection failed as expected, now waiting for reconnection attempts");

    // Track reconnection attempts
    let mut reconnect_attempts = Vec::new();
    let mut reconnect_count = 0;

    // Wait for at least 2 reconnection attempts with exponential backoff
    // First reconnect should happen after ~1 second, second after ~2 seconds
    let test_timeout = Duration::from_secs(10);
    let deadline = Instant::now() + test_timeout;

    while reconnect_count < 2 && Instant::now() < deadline {
        match timeout(Duration::from_millis(100), mock_editor1.rx.recv()).await {
            Ok(Some(lsp_server::Message::Notification(notif))) => {
                if notif.method == NotificationCode::Connecting.to_string() {
                    let elapsed = start_time.elapsed();
                    tracing::info!(
                        "Reconnection attempt {} at {:?}",
                        reconnect_count + 1,
                        elapsed
                    );
                    reconnect_attempts.push(elapsed);
                    reconnect_count += 1;

                    // Expect another ConnectionBroke after each failed reconnection
                    let broke_params = mock_editor1
                        .wait_for_notification(&NotificationCode::ConnectionBroke.to_string(), 10)
                        .await;
                    assert!(
                        broke_params.is_ok(),
                        "Should receive ConnectionBroke after failed reconnection attempt {}",
                        reconnect_count
                    );
                }
            }
            _ => {
                // Continue waiting
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }

    assert!(
        reconnect_count >= 2,
        "Should have at least 2 reconnection attempts, got {}",
        reconnect_count
    );

    // Verify exponential backoff timing
    // First reconnection should be around 1-3 seconds (allowing for test delays)
    // Second reconnection should be around 2-5 seconds after the first
    if reconnect_attempts.len() >= 2 {
        let first_attempt = reconnect_attempts[0];
        let second_attempt = reconnect_attempts[1];
        let gap = second_attempt - first_attempt;

        tracing::info!("First reconnection at: {:?}", first_attempt);
        tracing::info!("Second reconnection at: {:?}", second_attempt);
        tracing::info!("Gap between attempts: {:?}", gap);

        // First attempt should happen relatively quickly (within 3 seconds)
        assert!(
            first_attempt.as_secs() <= 3,
            "First reconnection should happen within 3 seconds, was {:?}",
            first_attempt
        );

        // Gap between first and second should be around 2 seconds (1s * 2 for exponential backoff)
        // Allow some tolerance for timing
        assert!(
            gap.as_secs() >= 1 && gap.as_secs() <= 4,
            "Gap between reconnections should be 1-4 seconds (exponential backoff), was {:?}",
            gap
        );
    }

    tracing::info!("Test completed successfully - reconnection with exponential backoff verified");

    server1_handle.abort();
}
