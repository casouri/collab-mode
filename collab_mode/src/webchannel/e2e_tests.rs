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

            let temp_dir = tempfile::TempDir::new()?;

            // Find available port
            let port = get_random_port();
            let addr = format!("127.0.0.1:{}", port);
            let addr_string = addr.to_string();
            tracing::info!("Starting signaling server on {}", addr_string);
            let signaling_task = tokio::spawn(async move {
                let result = signaling::server::run_signaling_server(&addr_string).await;
                if let Err(e) = result {
                    tracing::error!("Signaling server error: {}", e);
                }
            });

            // Wait for the listener.
            for _ in 0..50 {
                if tokio::net::TcpStream::connect(&addr).await.is_ok() {
                    break;
                }
                sleep(Duration::from_millis(20)).await;
            }

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

    // Helper to create test key/cert pairs.
    fn create_test_key_cert(name: &str) -> ArcKeyCert {
        Arc::new(create_key_cert(name))
    }

    // Helper to create a unique test endpoint id whose hash portion
    // matches `key_cert`.
    fn create_test_id(prefix: &str, key_cert: &ArcKeyCert) -> ServerId {
        format!(
            "{}-{}::{}",
            prefix,
            Uuid::new_v4(),
            key_cert.cert_der_hash()
        )
    }

    // *** Helper functions for SignalingClient-based connection establishment ***

    /// Wait for Bound message from signaling server
    async fn wait_for_bound(
        sig_rx: &mut mpsc::Receiver<crate::signaling::SignalingMessage>,
        expected_id: &str,
    ) {
        use crate::signaling::SignalingMsg;
        let signaling_message = timeout(Duration::from_secs(2), sig_rx.recv())
            .await
            .expect("Timeout waiting for Bound message")
            .expect("Expected Bound message");
        if let Ok(SignalingMsg::Bound(id)) = signaling_message.msg {
            assert_eq!(id, expected_id, "Bound ID mismatch");
        } else {
            panic!("Expected Bound message, got {:?}", signaling_message.msg);
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
        mpsc::Receiver<crate::signaling::SignalingMessage>,
    )> {
        let (sig_tx, mut sig_rx) = mpsc::channel(10);
        let client = crate::signaling::client_new::SignalingClient::bind(
            signaling_url,
            id.clone(),
            key_cert,
            sig_tx,
            None,
        )
        .await?;

        // Wait for Bound
        wait_for_bound(&mut sig_rx, &id).await;

        Ok((client, sig_rx))
    }

    /// Wait for Connect message and create Sock
    async fn wait_for_connect_and_create_sock(
        sig_rx: &mut mpsc::Receiver<crate::signaling::SignalingMessage>,
        sig_client: &crate::signaling::client_new::SignalingClient,
        my_id: String,
        expected_peer_id: Option<String>,
    ) -> anyhow::Result<crate::signaling::client_new::Sock> {
        use crate::signaling::SignalingMsg;

        let signaling_message = timeout(Duration::from_secs(5), sig_rx.recv())
            .await
            .map_err(|_| anyhow::anyhow!("Timeout waiting for Connect message"))?
            .ok_or(anyhow::anyhow!("Channel closed"))?;

        if let Ok(SignalingMsg::Connect(sender_id, receiver_id, initiator)) = signaling_message.msg
        {
            assert_eq!(receiver_id, my_id, "Connect receiver ID mismatch");
            if let Some(expected) = expected_peer_id {
                assert_eq!(sender_id, expected, "Connect sender ID mismatch");
            }

            // Only send Connect response if this is an initial request (initiator=true)
            // If initiator=false, this is already a response, so don't respond to it
            if initiator {
                let response = SignalingMsg::Connect(receiver_id, sender_id.clone(), false);
                sig_client.send(response).await?;
            }

            // Create Sock for peer
            let sock = sig_client.create_sock(sender_id).await;
            Ok(sock)
        } else {
            Err(anyhow::anyhow!(
                "Expected Connect message, got {:?}",
                signaling_message.msg
            ))
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
        use crate::signaling::SignalingMsg;

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

        // A initiates connection.
        let connect_msg = SignalingMsg::Connect(id_a.clone(), id_b.clone(), true);
        sig_client_a.send(connect_msg).await?;

        // B receives Connect and creates Sock
        let sock_b_task = tokio::spawn({
            let sig_client_b = sig_client_b.clone();
            let id_b = id_b.clone();
            let id_a = id_a.clone();
            async move {
                wait_for_connect_and_create_sock(&mut sig_rx_b, &sig_client_b, id_b, Some(id_a))
                    .await
            }
        });

        // A receives Connect response and creates Sock
        let sock_a = wait_for_connect_and_create_sock(
            &mut sig_rx_a,
            &sig_client_a,
            id_a.clone(),
            Some(id_b.clone()),
        )
        .await?;

        let sock_b = sock_b_task.await.unwrap()?;

        // Both sides establish WebRTC connection concurrently
        let connect_a = channel_a.connect(id_b.clone(), Transport::Sock(sock_a), key_cert_a);
        let connect_b = channel_b.connect(id_a.clone(), Transport::Sock(sock_b), key_cert_b);

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
        let (self_tx1, _self_rx1) = mpsc::unbounded_channel::<Message>();
        let (self_tx2, _self_rx2) = mpsc::unbounded_channel::<Message>();

        // Create key/cert pairs and matching IDs.
        let key_cert1 = create_test_key_cert("host1");
        let key_cert2 = create_test_key_cert("host2");
        let id1 = create_test_id("host1", &key_cert1);
        let id2 = create_test_id("host2", &key_cert2);
        tracing::info!("Created test IDs: {} and {}", id1, id2);

        // Create WebChannels
        let channel1 = WebChannel::new(id1.clone(), tx1, self_tx1);
        let channel2 = WebChannel::new(id2.clone(), tx2, self_tx2);

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

    #[tokio::test]
    async fn test_large_message_chunking() {
        tracing::info!("Starting test_large_message_chunking");
        let env = TestEnvironment::new().await.unwrap();

        let (tx1, mut rx1) = mpsc::channel::<Message>(100);
        let (tx2, _rx2) = mpsc::channel::<Message>(100);
        let (self_tx1, _self_rx1) = mpsc::unbounded_channel::<Message>();
        let (self_tx2, _self_rx2) = mpsc::unbounded_channel::<Message>();

        let key_cert1 = create_test_key_cert("host1");
        let key_cert2 = create_test_key_cert("host2");
        let id1 = create_test_id("host1", &key_cert1);
        let id2 = create_test_id("host2", &key_cert2);

        let channel1 = WebChannel::new(id1.clone(), tx1, self_tx1);
        let channel2 = WebChannel::new(id2.clone(), tx2, self_tx2);

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

        consume_automatic_messages(&mut rx1).await;

        // Create a large message (100KB of data)
        let large_content = "x".repeat(100 * 1024);
        let test_msg = Msg::ShareSingleFile {
            filename: "large_file.txt".to_string(),
            meta: "{}".to_string(),
            content: crate::types::FileContentOrPath::Content(large_content.clone()),
        };

        tracing::info!("Sending large message ({} bytes)", large_content.len());
        channel2.send(&id1, None, test_msg).await.unwrap();

        // Receive and verify
        let received = timeout(Duration::from_secs(10), rx1.recv()).await;
        assert!(
            received.is_ok(),
            "Should receive large message within timeout"
        );

        let msg = received.unwrap().unwrap();
        assert_eq!(msg.host, id2);
        match msg.body {
            Msg::ShareSingleFile { content, .. } => {
                if let crate::types::FileContentOrPath::Content(c) = content {
                    assert_eq!(c.len(), large_content.len(), "Content length should match");
                    assert_eq!(c, large_content, "Content should match exactly");
                } else {
                    panic!("Expected Content, got Path");
                }
            }
            _ => panic!("Unexpected message type: {:?}", msg.body),
        }

        tracing::info!("test_large_message_chunking completed successfully");
    }

    #[tokio::test]
    async fn test_multiple_concurrent_connections() {
        tracing::info!("Starting test_multiple_concurrent_connections");
        let env = TestEnvironment::new().await.unwrap();

        // Create two separate connection pairs: A↔B and C↔D
        // (Each pair uses separate signaling clients, avoiding ID conflicts)
        let (tx_a, mut rx_a) = mpsc::channel::<Message>(100);
        let (tx_b, _rx_b) = mpsc::channel::<Message>(100);
        let (tx_c, mut rx_c) = mpsc::channel::<Message>(100);
        let (tx_d, _rx_d) = mpsc::channel::<Message>(100);
        let (self_tx_a, _self_rx_a) = mpsc::unbounded_channel::<Message>();
        let (self_tx_b, _self_rx_b) = mpsc::unbounded_channel::<Message>();
        let (self_tx_c, _self_rx_c) = mpsc::unbounded_channel::<Message>();
        let (self_tx_d, _self_rx_d) = mpsc::unbounded_channel::<Message>();

        let key_cert_a = create_test_key_cert("host_a");
        let key_cert_b = create_test_key_cert("host_b");
        let key_cert_c = create_test_key_cert("host_c");
        let key_cert_d = create_test_key_cert("host_d");
        let id_a = create_test_id("host_a", &key_cert_a);
        let id_b = create_test_id("host_b", &key_cert_b);
        let id_c = create_test_id("host_c", &key_cert_c);
        let id_d = create_test_id("host_d", &key_cert_d);

        let channel_a = WebChannel::new(id_a.clone(), tx_a, self_tx_a);
        let channel_b = WebChannel::new(id_b.clone(), tx_b, self_tx_b);
        let channel_c = WebChannel::new(id_c.clone(), tx_c, self_tx_c);
        let channel_d = WebChannel::new(id_d.clone(), tx_d, self_tx_d);

        // Establish A ↔ B connection
        tracing::info!("Establishing A ↔ B connection");
        establish_connection_pair(
            &channel_a,
            id_a.clone(),
            key_cert_a.clone(),
            &channel_b,
            id_b.clone(),
            key_cert_b.clone(),
            env.signaling_url(),
        )
        .await
        .unwrap();

        // Establish C ↔ D connection (completely separate pair)
        tracing::info!("Establishing C ↔ D connection");
        establish_connection_pair(
            &channel_c,
            id_c.clone(),
            key_cert_c.clone(),
            &channel_d,
            id_d.clone(),
            key_cert_d.clone(),
            env.signaling_url(),
        )
        .await
        .unwrap();

        // Consume automatic messages
        consume_automatic_messages(&mut rx_a).await;
        consume_automatic_messages(&mut rx_c).await;

        // Test B → A message
        tracing::info!("Sending B → A");
        channel_b
            .send(&id_a, None, Msg::FileShared(100))
            .await
            .unwrap();

        // Test D → C message
        tracing::info!("Sending D → C");
        channel_d
            .send(&id_c, None, Msg::FileShared(200))
            .await
            .unwrap();

        // Verify both connections work independently
        let received_a = timeout(Duration::from_secs(5), rx_a.recv()).await;
        assert!(received_a.is_ok(), "Should receive message at A");
        let msg_a = received_a.unwrap().unwrap();
        assert_eq!(msg_a.host, id_b);
        match msg_a.body {
            Msg::FileShared(doc_id) => assert_eq!(doc_id, 100),
            _ => panic!("Unexpected message type"),
        }

        let received_c = timeout(Duration::from_secs(5), rx_c.recv()).await;
        assert!(received_c.is_ok(), "Should receive message at C");
        let msg_c = received_c.unwrap().unwrap();
        assert_eq!(msg_c.host, id_d);
        match msg_c.body {
            Msg::FileShared(doc_id) => assert_eq!(doc_id, 200),
            _ => panic!("Unexpected message type"),
        }

        tracing::info!("test_multiple_concurrent_connections completed successfully");
    }

    #[ignore] // No recovery in webchannel.
    #[tokio::test]
    async fn test_connection_failure_recovery() {
        todo!("Test needs complete rewrite for new SignalingClient-based API")
    }

    #[tokio::test]
    async fn test_certificate_validation() {
        tracing::info!("Starting test_certificate_validation");
        let env = TestEnvironment::new().await.unwrap();

        let (tx1, _rx1) = mpsc::channel::<Message>(100);
        let (tx2, _rx2) = mpsc::channel::<Message>(100);
        let (self_tx1, _self_rx1) = mpsc::unbounded_channel::<Message>();
        let (self_tx2, _self_rx2) = mpsc::unbounded_channel::<Message>();

        let key_cert1 = create_test_key_cert("host1");
        let key_cert2 = create_test_key_cert("host2");
        let id1 = create_test_id("host1", &key_cert1);
        let id2 = create_test_id("host2", &key_cert2);

        let channel1 = WebChannel::new(id1.clone(), tx1, self_tx1);
        let channel2 = WebChannel::new(id2.clone(), tx2, self_tx2);

        // Get expected certificate hashes before connection
        let expected_cert1 = key_cert1.cert_der_hash();
        let expected_cert2 = key_cert2.cert_der_hash();

        // Verify certificate hashes are non-empty and properly formatted
        assert!(!expected_cert1.is_empty(), "Cert hash should not be empty");
        assert!(!expected_cert2.is_empty(), "Cert hash should not be empty");
        assert!(
            expected_cert1.contains(':'),
            "Cert hash should be colon-separated hex"
        );
        assert!(
            expected_cert2.contains(':'),
            "Cert hash should be colon-separated hex"
        );
        assert_ne!(expected_cert1, expected_cert2, "Certs should be different");

        // Establish connection (this validates certs during handshake)
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

        tracing::info!("test_certificate_validation completed successfully");
    }

    #[tokio::test]
    async fn test_high_throughput() {
        tracing::info!("Starting test_high_throughput");
        let env = TestEnvironment::new().await.unwrap();

        let (tx1, mut rx1) = mpsc::channel::<Message>(1000);
        let (tx2, _rx2) = mpsc::channel::<Message>(1000);
        let (self_tx1, _self_rx1) = mpsc::unbounded_channel::<Message>();
        let (self_tx2, _self_rx2) = mpsc::unbounded_channel::<Message>();

        let key_cert1 = create_test_key_cert("host1");
        let key_cert2 = create_test_key_cert("host2");
        let id1 = create_test_id("host1", &key_cert1);
        let id2 = create_test_id("host2", &key_cert2);

        let channel1 = WebChannel::new(id1.clone(), tx1, self_tx1);
        let channel2 = WebChannel::new(id2.clone(), tx2, self_tx2);

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

        consume_automatic_messages(&mut rx1).await;

        // Send 100 messages rapidly
        let num_messages = 100;
        tracing::info!("Sending {} messages", num_messages);

        for i in 0..num_messages {
            channel2.send(&id1, None, Msg::FileShared(i)).await.unwrap();
        }

        // Receive all messages
        let mut received_ids: Vec<u32> = Vec::new();
        for _ in 0..num_messages {
            let received = timeout(Duration::from_secs(10), rx1.recv()).await;
            assert!(
                received.is_ok(),
                "Should receive message {} within timeout",
                received_ids.len()
            );
            let msg = received.unwrap().unwrap();
            match msg.body {
                Msg::FileShared(doc_id) => {
                    received_ids.push(doc_id);
                }
                _ => panic!("Unexpected message type: {:?}", msg.body),
            }
        }

        assert_eq!(received_ids.len(), num_messages as usize);

        // Verify messages arrived in order
        for (i, &doc_id) in received_ids.iter().enumerate() {
            assert_eq!(
                doc_id, i as u32,
                "Message {} should have doc_id {}, got {}",
                i, i, doc_id
            );
        }

        tracing::info!("test_high_throughput completed successfully");
    }

    #[tokio::test]
    async fn test_ice_progress_messages() {
        tracing::info!("Starting test_ice_progress_messages");
        let env = TestEnvironment::new().await.unwrap();

        let (tx1, mut rx1) = mpsc::channel::<Message>(100);
        let (tx2, _rx2) = mpsc::channel::<Message>(100);
        let (self_tx1, _self_rx1) = mpsc::unbounded_channel::<Message>();
        let (self_tx2, _self_rx2) = mpsc::unbounded_channel::<Message>();

        let key_cert1 = create_test_key_cert("host1");
        let key_cert2 = create_test_key_cert("host2");
        let id1 = create_test_id("host1", &key_cert1);
        let id2 = create_test_id("host2", &key_cert2);

        let channel1 = WebChannel::new(id1.clone(), tx1, self_tx1);
        let channel2 = WebChannel::new(id2.clone(), tx2, self_tx2);

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

        // Collect all ICE progress messages
        let mut ice_states: Vec<String> = Vec::new();
        loop {
            match timeout(Duration::from_millis(500), rx1.recv()).await {
                Ok(Some(msg)) => {
                    if let Msg::IceProgress(_, state) = msg.body {
                        ice_states.push(state);
                    }
                }
                _ => break,
            }
        }

        // Verify we received expected ICE progress states
        tracing::info!("Received ICE states: {:?}", ice_states);
        assert!(
            !ice_states.is_empty(),
            "Should receive ICE progress messages"
        );
        assert!(
            ice_states.contains(&"Checking".to_string()),
            "Should have Checking state"
        );
        assert!(
            ice_states.contains(&"Connected".to_string()),
            "Should have Connected state"
        );

        tracing::info!("test_ice_progress_messages completed successfully");
    }
}
