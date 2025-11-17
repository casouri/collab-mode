#[cfg(test)]
mod tests {
    use super::super::client_new::*;
    use crate::config_man::create_key_cert;
    use crate::message::Msg;
    use crate::signaling::{SignalingError, SignalingMessage};
    use std::path::Path;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    /// Helper to create a test signaling server and return its address
    async fn create_test_signaling_server() -> String {
        let db_path = Path::new("/tmp/collab-signal-test.sqlite3");
        let _ = std::fs::remove_file(&db_path);

        tokio::spawn(crate::signaling::server::run_signaling_server(
            "127.0.0.1:0",
            &db_path,
        ));

        // Wait a bit for server to start
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        "ws://127.0.0.1:9001".to_string()
    }

    #[tokio::test]
    async fn test_signaling_client_bind() {
        let addr = create_test_signaling_server().await;
        let id = "test-client-1".to_string();
        let key_cert = Arc::new(create_key_cert(&id));
        let (msg_tx, mut msg_rx) = mpsc::channel(10);

        // Create and bind client
        let client = SignalingClient::bind(addr.clone(), id.clone(), key_cert, msg_tx)
            .await
            .expect("Failed to bind");

        // Should be bound
        assert!(client.is_bound().await);

        // Should receive Bound message
        if let Some(msg) = msg_rx.recv().await {
            match msg {
                Msg::SignalingMsg(recv_addr, SignalingMessage::Bound(recv_id)) => {
                    assert_eq!(recv_addr, addr);
                    assert_eq!(recv_id, id);
                }
                _ => panic!("Expected SignalingMsg(Bound), got {:?}", msg),
            }
        } else {
            panic!("Expected to receive Bound message");
        }
    }

    #[tokio::test]
    async fn test_signaling_client_create_sock() {
        let addr = create_test_signaling_server().await;
        let id = "test-client-2".to_string();
        let key_cert = Arc::new(create_key_cert(&id));
        let (msg_tx, _msg_rx) = mpsc::channel(10);

        let client = SignalingClient::bind(addr.clone(), id.clone(), key_cert.clone(), msg_tx)
            .await
            .expect("Failed to bind");

        // Create a sock for a peer
        let peer_id = "test-peer-1".to_string();
        let peer_cert = key_cert.cert_der_hash();
        let peer_sdp = Some("peer-sdp".to_string());

        let sock = client
            .create_sock(peer_id.clone(), peer_cert.clone(), peer_sdp.clone())
            .await;

        // Verify sock properties
        assert_eq!(sock.id(), peer_id);
        assert_eq!(sock.cert_hash(), peer_cert);
    }

    #[tokio::test]
    async fn test_sock_sdp_exchange() {
        let addr = create_test_signaling_server().await;
        let id = "test-client-3".to_string();
        let key_cert = Arc::new(create_key_cert(&id));
        let (msg_tx, _msg_rx) = mpsc::channel(10);

        let client = SignalingClient::bind(addr.clone(), id.clone(), key_cert.clone(), msg_tx)
            .await
            .expect("Failed to bind");

        let peer_id = "test-peer-2".to_string();
        let peer_cert = key_cert.cert_der_hash();

        // Create sock without initial SDP
        let mut sock = client
            .create_sock(peer_id.clone(), peer_cert, None)
            .await;

        // Simulate receiving SDP
        // This would normally come through send_receive_stream
        // For now, we'll test the send_sdp method
        let my_sdp = "my-test-sdp".to_string();
        sock.send_sdp(my_sdp.clone()).await.expect("Failed to send SDP");
    }

    #[tokio::test]
    async fn test_sock_candidate_exchange() {
        let addr = create_test_signaling_server().await;
        let id = "test-client-4".to_string();
        let key_cert = Arc::new(create_key_cert(&id));
        let (msg_tx, _msg_rx) = mpsc::channel(10);

        let client = SignalingClient::bind(addr.clone(), id.clone(), key_cert.clone(), msg_tx)
            .await
            .expect("Failed to bind");

        let peer_id = "test-peer-3".to_string();
        let peer_cert = key_cert.cert_der_hash();

        let sock = client
            .create_sock(peer_id.clone(), peer_cert, None)
            .await;

        // Test sending candidate
        let candidate = "test-candidate".to_string();
        sock.send_candidate(candidate.clone())
            .await
            .expect("Failed to send candidate");
    }

    #[tokio::test]
    async fn test_error_message_handling() {
        let addr = "ws://127.0.0.1:9999".to_string(); // Non-existent server
        let id = "test-client-5".to_string();
        let key_cert = Arc::new(create_key_cert(&id));
        let (msg_tx, mut msg_rx) = mpsc::channel(10);

        // Try to bind to non-existent server
        let result = SignalingClient::bind(addr.clone(), id.clone(), key_cert, msg_tx).await;

        // Should fail to connect
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_connection_deduplication() {
        // This test verifies that the Server's pending_connections
        // mechanism works correctly. It's more of an integration test
        // and will be implemented when we integrate with Server.
    }

    #[tokio::test]
    async fn test_signaling_client_send() {
        let addr = create_test_signaling_server().await;
        let id = "test-client-6".to_string();
        let key_cert = Arc::new(create_key_cert(&id));
        let (msg_tx, _msg_rx) = mpsc::channel(10);

        let client = SignalingClient::bind(addr.clone(), id.clone(), key_cert.clone(), msg_tx)
            .await
            .expect("Failed to bind");

        // Send a Connect message
        let peer_id = "test-peer-4".to_string();
        let my_cert = key_cert.cert_der_hash();
        let sdp = "test-sdp".to_string();
        let msg = SignalingMessage::Connect(
            id.clone(),
            peer_id,
            sdp,
            my_cert,
            true, // initiator
        );

        client.send(msg).await.expect("Failed to send message");
    }
}
