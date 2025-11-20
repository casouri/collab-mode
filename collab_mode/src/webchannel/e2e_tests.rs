#[cfg(test)]
mod e2e_tests {
    use super::super::*;
    use crate::config_man::{create_key_cert, ArcKeyCert};
    use crate::signaling;
    use std::sync::Arc;
    use std::time::Duration;
    // use tokio::net::TcpListener;
    use rand::Rng;
    use tokio::sync::mpsc;
    use tokio::time::{sleep, timeout};
    use tracing_subscriber::EnvFilter;
    use uuid::Uuid;

    // Helper function to initialize tracing for tests
    fn init_test_tracing() {
        // Initialize rustls crypto provider (ring) to avoid ambiguity.
        // webrtc-dtls uses the ring feature of rustls, so there's an
        // ambiguity of which provider to use. Here we just set the
        // default provider to ring explicitly.
        // REF: https://github.com/rustls/rustls/issues/1938
        let _ = rustls::crypto::ring::default_provider().install_default();

        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| EnvFilter::new("info,webchannel=debug,collab_mode=debug")),
            )
            .with_test_writer()
            .with_target(true)
            .with_thread_ids(true)
            .with_line_number(true)
            .try_init();
    }

    // Test infrastructure
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
            // Initialize tracing for this test environment
            init_test_tracing();

            tracing::info!("Creating new test environment");

            // Create temp directory for test database
            let temp_dir = tempfile::TempDir::new()?;
            let db_path = temp_dir.path().join("test-signal.db");
            tracing::debug!("Test database path: {:?}", db_path);

            // Find available port
            let port = get_random_port();
            let addr = format!("127.0.0.1:{}", port);
            // let listener = TcpListener::bind("127.0.0.1:0").await?;
            // let addr = listener.local_addr()?;
            // drop(listener);
            // tracing::info!("Found available port: {}", addr);

            // Start signaling server
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

            // Give server time to start
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

    // Helper to create unique test IDs
    fn create_test_id(prefix: &str) -> ServerId {
        format!("{}-{}", prefix, Uuid::new_v4())
    }

    // Helper to create test key/cert pairs
    fn create_test_key_cert(name: &str) -> ArcKeyCert {
        Arc::new(create_key_cert(name))
    }

    // *** Helper functions for SignalingClient-based connection establishment ***

    /// Wait for Bound message from signaling server
    async fn wait_for_bound(
        sig_rx: &mut mpsc::Receiver<(String, crate::signaling::SignalingMessage)>,
        expected_id: &str,
    ) {
        use crate::signaling::SignalingMessage;
        let (_addr, msg) = timeout(Duration::from_secs(2), sig_rx.recv())
            .await
            .expect("Timeout waiting for Bound message")
            .expect("Expected Bound message");
        if let SignalingMessage::Bound(id) = msg {
            assert_eq!(id, expected_id, "Bound ID mismatch");
        } else {
            panic!("Expected Bound message, got {:?}", msg);
        }
    }

    /// Consume automatic messages from channel (Hey and ICE progress)
    async fn consume_automatic_messages(rx: &mut mpsc::Receiver<Message>) {
        loop {
            if let Ok(Some(msg)) = timeout(Duration::from_millis(500), rx.recv()).await {
                match msg.body {
                    Msg::Hey(_) | Msg::IceProgress(_, _) => continue,
                    _ => break,
                }
            } else {
                break;
            }
        }
    }

    /// Create SignalingClient and wait for it to bind
    async fn create_bound_signaling_client(
        id: String,
        key_cert: ArcKeyCert,
        signaling_url: String,
    ) -> anyhow::Result<(
        crate::signaling::client_new::SignalingClient,
        mpsc::Receiver<(String, crate::signaling::SignalingMessage)>,
    )> {
        let (sig_tx, mut sig_rx) = mpsc::channel(10);
        let client = crate::signaling::client_new::SignalingClient::bind(
            signaling_url,
            id.clone(),
            key_cert,
            sig_tx,
        )
        .await?;

        // Wait for Bound
        wait_for_bound(&mut sig_rx, &id).await;

        Ok((client, sig_rx))
    }

    /// Wait for Connect message and create Sock
    async fn wait_for_connect_and_create_sock(
        sig_rx: &mut mpsc::Receiver<(String, crate::signaling::SignalingMessage)>,
        sig_client: &crate::signaling::client_new::SignalingClient,
        my_id: String,
        my_cert_hash: String,
        expected_peer_id: Option<String>,
    ) -> anyhow::Result<crate::signaling::client_new::Sock> {
        use crate::signaling::SignalingMessage;

        let (_addr, msg) = timeout(Duration::from_secs(5), sig_rx.recv())
            .await
            .map_err(|_| anyhow::anyhow!("Timeout waiting for Connect message"))?
            .ok_or(anyhow::anyhow!("Channel closed"))?;

        if let SignalingMessage::Connect(sender_id, receiver_id, sender_cert, initiator) = msg {
            assert_eq!(receiver_id, my_id, "Connect receiver ID mismatch");
            if let Some(expected) = expected_peer_id {
                assert_eq!(sender_id, expected, "Connect sender ID mismatch");
            }

            // Only send Connect response if this is an initial request (initiator=true)
            // If initiator=false, this is already a response, so don't respond to it
            if initiator {
                let response =
                    SignalingMessage::Connect(receiver_id, sender_id.clone(), my_cert_hash, false);
                sig_client.send(response).await?;
            }

            // Create Sock for peer
            let sock = sig_client.create_sock(sender_id, sender_cert).await;
            Ok(sock)
        } else {
            Err(anyhow::anyhow!("Expected Connect message, got {:?}", msg))
        }
    }

    /// Establish complete bidirectional connection between two WebChannels.
    /// This handles the entire signaling handshake and WebRTC connection setup.
    async fn establish_connection_pair(
        channel_a: &WebChannel,
        id_a: String,
        key_cert_a: ArcKeyCert,
        channel_b: &WebChannel,
        id_b: String,
        key_cert_b: ArcKeyCert,
        signaling_url: &str,
    ) -> anyhow::Result<()> {
        use crate::signaling::SignalingMessage;

        // Create and bind SignalingClients
        let (sig_client_a, mut sig_rx_a) = create_bound_signaling_client(
            id_a.clone(),
            key_cert_a.clone(),
            signaling_url.to_string(),
        )
        .await?;

        let (sig_client_b, mut sig_rx_b) = create_bound_signaling_client(
            id_b.clone(),
            key_cert_b.clone(),
            signaling_url.to_string(),
        )
        .await?;

        let cert_hash_a = key_cert_a.cert_der_hash();
        let cert_hash_b = key_cert_b.cert_der_hash();

        // A initiates connection
        let connect_msg =
            SignalingMessage::Connect(id_a.clone(), id_b.clone(), cert_hash_a.clone(), true);
        sig_client_a.send(connect_msg).await?;

        // B receives Connect and creates Sock
        let sock_b_task = tokio::spawn({
            let sig_client_b = sig_client_b.clone();
            let id_b = id_b.clone();
            let cert_hash_b = cert_hash_b.clone();
            let id_a = id_a.clone();
            async move {
                wait_for_connect_and_create_sock(
                    &mut sig_rx_b,
                    &sig_client_b,
                    id_b,
                    cert_hash_b,
                    Some(id_a),
                )
                .await
            }
        });

        // A receives Connect response and creates Sock
        let sock_a = wait_for_connect_and_create_sock(
            &mut sig_rx_a,
            &sig_client_a,
            id_a.clone(),
            cert_hash_a,
            Some(id_b.clone()),
        )
        .await?;

        let sock_b = sock_b_task.await.unwrap()?;

        // Both sides establish WebRTC connection concurrently
        let connect_a = channel_a.connect(id_b.clone(), Some(sock_a), key_cert_a);
        let connect_b = channel_b.connect(id_a.clone(), Some(sock_b), key_cert_b);

        let (res_a, res_b) = tokio::join!(connect_a, connect_b);
        res_a?;
        res_b?;

        Ok(())
    }

    #[tokio::test]
    async fn test_basic_connection() {
        tracing::info!("Starting test_basic_connection");
        let env = TestEnvironment::new().await.unwrap();

        // Create channels for both sides
        let (tx1, mut rx1) = mpsc::channel::<Message>(100);
        let (tx2, _rx2) = mpsc::channel::<Message>(100);

        // Create unique IDs
        let id1 = create_test_id("host1");
        let id2 = create_test_id("host2");
        tracing::info!("Created test IDs: {} and {}", id1, id2);

        // Create WebChannels
        let channel1 = WebChannel::new(id1.clone(), tx1);
        let channel2 = WebChannel::new(id2.clone(), tx2);

        // Create key/cert pairs
        let key_cert1 = create_test_key_cert(&id1);
        let key_cert2 = create_test_key_cert(&id2);

        // Establish bidirectional connection using helper
        tracing::info!("Establishing connection between {} and {}", id1, id2);
        establish_connection_pair(
            &channel1,
            id1.clone(),
            key_cert1,
            &channel2,
            id2.clone(),
            key_cert2,
            env.signaling_url(),
        )
        .await
        .unwrap();

        // Consume automatic messages (Hey and ICE progress)
        tracing::info!("Consuming automatic messages");
        consume_automatic_messages(&mut rx1).await;

        // Test message exchange
        let test_msg = Msg::FileShared(42);
        tracing::info!("Sending test message from {} to {}", id2, id1);
        let send_result = channel2.send(&id1, None, test_msg.clone()).await;
        assert!(send_result.is_ok(), "Send should succeed");

        // Receive message on channel1
        tracing::info!("Waiting to receive message on {}", id1);
        let received = timeout(Duration::from_secs(5), rx1.recv()).await;
        assert!(received.is_ok(), "Should receive message within timeout");

        let msg = received.unwrap().unwrap();
        tracing::info!("Received message from {}: {:?}", msg.host, msg.body);
        assert_eq!(msg.host, id2);
        match msg.body {
            Msg::FileShared(doc_id) => assert_eq!(doc_id, 42),
            _ => panic!("Unexpected message type: {:?}", msg.body),
        }

        tracing::info!("test_basic_connection completed successfully");
    }

    #[ignore] // Needs rewrite: .accept() and .connect() methods removed, must use SignalingClient API
    #[tokio::test]
    async fn test_large_message_chunking() {
        todo!("Test needs complete rewrite for new SignalingClient-based API")
    }

    #[ignore] // Needs rewrite: .accept() and .connect() methods removed, must use SignalingClient API
    #[tokio::test]
    async fn test_multiple_concurrent_connections() {
        todo!("Test needs complete rewrite for new SignalingClient-based API")
    }

    #[ignore] // No recovery in webchannel.
    #[tokio::test]
    async fn test_connection_failure_recovery() {
        todo!("Test needs complete rewrite for new SignalingClient-based API")
    }

    #[ignore] // Needs rewrite: .accept() and .connect() methods removed, must use SignalingClient API
    #[tokio::test]
    async fn test_certificate_validation() {
        todo!("Test needs complete rewrite for new SignalingClient-based API")
    }

    #[ignore] // Needs rewrite: .accept() and .connect() methods removed, must use SignalingClient API
    #[tokio::test]
    async fn test_high_throughput() {
        todo!("Test needs complete rewrite for new SignalingClient-based API")
    }

    #[ignore] // Needs rewrite: .accept() and .connect() methods removed, must use SignalingClient API
    #[tokio::test]
    async fn test_ice_progress_messages() {
        todo!("Test needs complete rewrite for new SignalingClient-based API")
    }
}
