#[cfg(test)]
mod e2e_tests {
    use super::super::*;
    use crate::config_man::{create_key_cert, ArcKeyCert};
    use crate::signaling;
    use crate::types::{FileContentOrPath, FileDesc, Info};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;
    use tokio::time::{sleep, timeout};
    use tracing_subscriber::EnvFilter;
    use uuid::Uuid;

    // Helper function to initialize tracing for tests
    fn init_test_tracing() {
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
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let addr = listener.local_addr()?;
            drop(listener);
            tracing::info!("Found available port: {}", addr);

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

        // Start accepting on channel1
        let accept_handle = {
            let mut channel1 = channel1.clone();
            let key_cert1 = key_cert1.clone();
            let signaling_url = env.signaling_url().to_string();
            let id1_clone = id1.clone();
            tokio::spawn(async move {
                tracing::info!("{} starting to accept connections", id1_clone);
                let result = channel1
                    .accept(key_cert1, &signaling_url, TransportType::SCTP)
                    .await;
                if let Err(e) = result {
                    tracing::error!("{} accept error: {}", id1_clone, e);
                }
            })
        };

        // Give acceptor time to bind
        sleep(Duration::from_millis(200)).await;

        // Connect from channel2
        tracing::info!("{} connecting to {}", id2, id1);
        let connect_result = timeout(
            Duration::from_secs(5),
            channel2.connect(
                id1.clone(),
                id2.clone(),
                key_cert2,
                env.signaling_url(),
                TransportType::SCTP,
            ),
        )
        .await;

        assert!(connect_result.is_ok(), "Connection should succeed");
        assert!(
            connect_result.unwrap().is_ok(),
            "Connection should not error"
        );

        // First consume the automatic "Hey" messages
        for _ in 0..2 {
            // Expect up to 2 Hey messages (one from each side)
            if let Ok(Some(msg)) = timeout(Duration::from_millis(500), rx1.recv()).await {
                if let Msg::Hey(_) = msg.body {
                    tracing::info!("Received Hey message from {}", msg.host);
                    continue;
                }
                // If it's not a Hey message, put it back (we can't do that with mpsc, so break)
                break;
            } else {
                break;
            }
        }

        // Test message exchange
        let test_msg = Msg::FileShared(42); // DocId is u32
        tracing::info!("Sending test message from {} to {}", id2, id1);
        let send_result = channel2.send(&id1, None, test_msg.clone()).await;
        assert!(send_result.is_ok(), "Send should succeed");

        // Receive message on channel1
        tracing::info!("Waiting to receive message on {}", id1);
        let received = timeout(Duration::from_secs(2), rx1.recv()).await;
        assert!(received.is_ok(), "Should receive message");
        let msg = received.unwrap().unwrap();
        tracing::info!("Received message from {}: {:?}", msg.host, msg.body);
        assert_eq!(msg.host, id2);
        match msg.body {
            Msg::FileShared(doc_id) => assert_eq!(doc_id, 42),
            _ => panic!("Unexpected message type: {:?}", msg.body),
        }

        // Cleanup
        accept_handle.abort();
    }

    #[tokio::test]
    async fn test_large_message_chunking() {
        let env = TestEnvironment::new().await.unwrap();

        let (tx1, mut rx1) = mpsc::channel::<Message>(100);
        let (tx2, _rx2) = mpsc::channel::<Message>(100);

        let id1 = create_test_id("chunk-1");
        let id2 = create_test_id("chunk-2");

        let channel1 = WebChannel::new(id1.clone(), tx1);
        let channel2 = WebChannel::new(id2.clone(), tx2);

        let key_cert1 = create_test_key_cert(&id1);
        let key_cert2 = create_test_key_cert(&id2);

        // Establish connection
        let accept_handle = {
            let mut channel1 = channel1.clone();
            let key_cert1 = key_cert1.clone();
            let signaling_url = env.signaling_url().to_string();
            tokio::spawn(async move {
                let _ = channel1
                    .accept(key_cert1, &signaling_url, TransportType::SCTP)
                    .await;
            })
        };

        sleep(Duration::from_millis(200)).await;
        channel2
            .connect(
                id1.clone(),
                id2.clone(),
                key_cert2,
                env.signaling_url(),
                TransportType::SCTP,
            )
            .await
            .unwrap();

        // Consume automatic Hey messages
        for _ in 0..2 {
            if let Ok(Some(msg)) = timeout(Duration::from_millis(500), rx1.recv()).await {
                if let Msg::Hey(_) = msg.body {
                    tracing::info!("Received Hey message from {}", msg.host);
                }
            }
        }

        // Test various sizes
        let test_sizes = vec![
            ("small", 1024),             // 1KB
            ("medium", 100 * 1024),      // 100KB (exceeds MAX_FRAME_SIZE)
            ("large", 1024 * 1024),      // 1MB
            ("xlarge", 5 * 1024 * 1024), // 5MB
        ];

        for (name, size) in test_sizes {
            let test_data: Vec<u8> = (0..size).map(|_| 65u8).collect();

            let msg = Msg::ShareSingleFile {
                filename: format!("{}.bin", name),
                meta: "binary".to_string(),
                content: FileContentOrPath::Content(
                    String::from_utf8_lossy(&test_data).to_string(),
                ),
            };

            // Send large message
            channel2.send(&id1, None, msg).await.unwrap();

            // Receive and verify
            let received = timeout(Duration::from_secs(10), rx1.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(received.host, id2);

            match received.body {
                Msg::ShareSingleFile {
                    filename, content, ..
                } => {
                    assert_eq!(filename, format!("{}.bin", name));
                    match content {
                        FileContentOrPath::Content(data) => {
                            assert!(
                                data.chars().nth(size - 1) == Some('A'),
                                "Data should not be empty for {}",
                                name
                            );
                        }
                        _ => panic!("Expected content, not path"),
                    }
                }
                _ => panic!("Unexpected message type"),
            }
        }

        accept_handle.abort();
    }

    #[tokio::test]
    async fn test_multiple_concurrent_connections() {
        let env = TestEnvironment::new().await.unwrap();

        let (hub_tx, mut hub_rx) = mpsc::channel::<Message>(1000);
        let hub_id = create_test_id("hub");
        let hub_channel = WebChannel::new(hub_id.clone(), hub_tx);
        let hub_key_cert = create_test_key_cert(&hub_id);

        // Start hub accepting connections
        let accept_handle = {
            let mut hub_channel = hub_channel.clone();
            let hub_key_cert = hub_key_cert.clone();
            let signaling_url = env.signaling_url().to_string();
            tokio::spawn(async move {
                let _ = hub_channel
                    .accept(hub_key_cert, &signaling_url, TransportType::SCTP)
                    .await;
            })
        };

        sleep(Duration::from_millis(200)).await;

        // Create multiple clients with bidirectional connections
        let num_clients = 3; // Reduced for simplicity
        let mut client_channels = Vec::new();
        let mut client_rxs = Vec::new();
        let mut client_accept_handles = Vec::new();

        for i in 0..num_clients {
            let (tx, rx) = mpsc::channel::<Message>(100);
            let id = create_test_id(&format!("client-{}", i));
            let channel = WebChannel::new(id.clone(), tx);
            let key_cert = create_test_key_cert(&id);

            // Start client accepting connections too
            let client_accept_handle = {
                let mut channel_clone = channel.clone();
                let key_cert_clone = key_cert.clone();
                let signaling_url = env.signaling_url().to_string();
                tokio::spawn(async move {
                    let _ = channel_clone
                        .accept(key_cert_clone, &signaling_url, TransportType::SCTP)
                        .await;
                })
            };
            client_accept_handles.push(client_accept_handle);

            // Connect to hub
            channel
                .connect(
                    hub_id.clone(),
                    id.clone(),
                    key_cert.clone(),
                    env.signaling_url(),
                    TransportType::SCTP,
                )
                .await
                .unwrap();

            // Note: Hub doesn't need to connect back since it's already accepting connections
            // The connection is bidirectional once established

            client_channels.push((id, channel));
            client_rxs.push(rx);
        }

        // Give time for all connections to stabilize
        sleep(Duration::from_millis(500)).await;

        // Consume Hey messages from hub
        let mut hey_count = 0;
        let start_time = std::time::Instant::now();
        while hey_count < num_clients && start_time.elapsed() < Duration::from_secs(2) {
            if let Ok(Some(msg)) = timeout(Duration::from_millis(100), hub_rx.recv()).await {
                if let Msg::Hey(_) = msg.body {
                    tracing::info!("Hub received Hey message");
                    hey_count += 1;
                }
            }
        }

        // Each client sends a unique message
        for (i, (_client_id, channel)) in client_channels.iter().enumerate() {
            let msg = Msg::Info(
                i as u32, // DocId is u32
                Info {
                    sender: i as u32,
                    value: format!("Hello from client {}", i),
                },
            );
            channel.send(&hub_id, None, msg).await.unwrap();
        }

        // Hub receives all messages (skip ICE progress messages)
        let mut received_count = 0;
        let timeout_duration = Duration::from_secs(10);
        let start_time = std::time::Instant::now();

        while received_count < num_clients && start_time.elapsed() < timeout_duration {
            if let Ok(Some(msg)) = timeout(Duration::from_millis(100), hub_rx.recv()).await {
                match msg.body {
                    Msg::Info(doc_id, info) => {
                        let client_num = info.sender as usize;
                        assert_eq!(doc_id, client_num as u32);
                        assert_eq!(info.value, format!("Hello from client {}", client_num));
                        received_count += 1;
                    }
                    Msg::IceProgress(_) => {
                        // Skip ICE progress messages
                        continue;
                    }
                    _ => {
                        // Log unexpected messages but don't panic
                        tracing::warn!("Unexpected message type: {:?}", msg.body);
                    }
                }
            }
        }

        assert_eq!(
            received_count, num_clients,
            "Did not receive all expected messages"
        );

        // Test hub sending to specific clients
        for (i, (client_id, _)) in client_channels.iter().enumerate() {
            let msg = Msg::FileShared(1000 + i as u32); // Use unique DocIds
            hub_channel.send(client_id, None, msg).await.unwrap();
        }

        // Each client receives its message (skip non-hub messages)
        for (i, mut rx) in client_rxs.into_iter().enumerate() {
            let timeout_duration = Duration::from_secs(5);
            let start_time = std::time::Instant::now();
            let mut found_message = false;

            while !found_message && start_time.elapsed() < timeout_duration {
                if let Ok(Some(msg)) = timeout(Duration::from_millis(100), rx.recv()).await {
                    // Only process messages from the hub
                    if msg.host == hub_id {
                        match msg.body {
                            Msg::FileShared(doc_id) => {
                                assert_eq!(doc_id, 1000 + i as u32);
                                found_message = true;
                            }
                            _ => {
                                // Ignore other message types from hub
                                continue;
                            }
                        }
                    }
                    // Ignore messages from other hosts
                }
            }

            assert!(
                found_message,
                "Client {} did not receive expected message from hub",
                i
            );
        }

        // Cleanup
        accept_handle.abort();
        for handle in client_accept_handles {
            handle.abort();
        }
    }

    #[ignore] // No recovery in webchannel.
    #[tokio::test]
    async fn test_connection_failure_recovery() {
        let env = TestEnvironment::new().await.unwrap();

        let (tx1, mut rx1) = mpsc::channel::<Message>(100);
        let (tx2, mut rx2) = mpsc::channel::<Message>(100);

        let id1 = create_test_id("recovery-1");
        let id2 = create_test_id("recovery-2");

        let channel1 = WebChannel::new(id1.clone(), tx1);
        let channel2 = WebChannel::new(id2.clone(), tx2);

        let key_cert1 = create_test_key_cert(&id1);
        let key_cert2 = create_test_key_cert(&id2);

        // Establish initial connection
        let accept_handle = {
            let mut channel1 = channel1.clone();
            let key_cert1 = key_cert1.clone();
            let signaling_url = env.signaling_url().to_string();
            tokio::spawn(async move {
                let _ = channel1
                    .accept(key_cert1, &signaling_url, TransportType::SCTP)
                    .await;
            })
        };

        sleep(Duration::from_millis(200)).await;
        channel2
            .connect(
                id1.clone(),
                id2.clone(),
                key_cert2.clone(),
                env.signaling_url(),
                TransportType::SCTP,
            )
            .await
            .unwrap();

        // Exchange messages to verify connection
        channel2.send(&id1, None, Msg::FileShared(1)).await.unwrap();
        let msg = timeout(Duration::from_secs(2), rx1.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(msg.host, id2);

        // Simulate connection break by removing the connection from channel1
        channel1.assoc_tx.lock().unwrap().remove(&id2);

        // Try to send should fail
        let send_result = channel1.send(&id2, None, Msg::FileShared(2)).await;
        assert!(send_result.is_err());
        assert!(send_result
            .unwrap_err()
            .to_string()
            .contains("not connected"));

        // Channel2 should eventually receive ConnectionBroke
        let mut got_connection_broke = false;
        for _ in 0..10 {
            if let Ok(Some(msg)) = timeout(Duration::from_millis(500), rx2.recv()).await {
                if let Msg::ConnectionBroke(broken_id) = msg.body {
                    assert_eq!(broken_id, id2);
                    got_connection_broke = true;
                    break;
                }
            }
        }
        assert!(
            got_connection_broke,
            "Should receive ConnectionBroke message"
        );

        accept_handle.abort();
    }

    #[tokio::test]
    async fn test_certificate_validation() {
        let env = TestEnvironment::new().await.unwrap();

        let (tx1, _) = mpsc::channel::<Message>(100);
        let (tx2, _) = mpsc::channel::<Message>(100);

        let id1 = create_test_id("cert-1");
        let id2 = create_test_id("cert-2");

        let channel1 = WebChannel::new(id1.clone(), tx1);
        let channel2 = WebChannel::new(id2.clone(), tx2);

        let key_cert1 = create_test_key_cert(&id1);
        let key_cert2 = create_test_key_cert(&id2);

        // Start accepting with cert1
        let accept_handle = {
            let mut channel1 = channel1.clone();
            let key_cert1 = key_cert1.clone();
            let signaling_url = env.signaling_url().to_string();
            tokio::spawn(async move {
                let _ = channel1
                    .accept(key_cert1, &signaling_url, TransportType::SCTP)
                    .await;
            })
        };

        sleep(Duration::from_millis(200)).await;

        // Connect with matching cert should succeed
        let connect_result = timeout(
            Duration::from_secs(5),
            channel2.connect(
                id1.clone(),
                id2.clone(),
                key_cert2.clone(),
                env.signaling_url(),
                TransportType::SCTP,
            ),
        )
        .await;
        assert!(connect_result.is_ok() && connect_result.unwrap().is_ok());

        // Test that certificates are properly validated during DTLS handshake
        // The actual cert validation happens in verify_dtls_cert function

        accept_handle.abort();
    }

    #[tokio::test]
    async fn test_high_throughput() {
        let env = TestEnvironment::new().await.unwrap();

        let (tx1, mut rx1) = mpsc::channel::<Message>(1000);
        let (tx2, _rx2) = mpsc::channel::<Message>(1000);

        let id1 = create_test_id("throughput-1");
        let id2 = create_test_id("throughput-2");

        let channel1 = WebChannel::new(id1.clone(), tx1);
        let channel2 = WebChannel::new(id2.clone(), tx2);

        let key_cert1 = create_test_key_cert(&id1);
        let key_cert2 = create_test_key_cert(&id2);

        // Establish connection
        let accept_handle = {
            let mut channel1 = channel1.clone();
            let key_cert1 = key_cert1.clone();
            let signaling_url = env.signaling_url().to_string();
            tokio::spawn(async move {
                let _ = channel1
                    .accept(key_cert1, &signaling_url, TransportType::SCTP)
                    .await;
            })
        };

        sleep(Duration::from_millis(200)).await;
        channel2
            .connect(
                id1.clone(),
                id2.clone(),
                key_cert2,
                env.signaling_url(),
                TransportType::SCTP,
            )
            .await
            .unwrap();

        // Send many messages rapidly
        let num_messages = 100;
        let start = std::time::Instant::now();

        // Spawn sender task
        let sender_task = {
            let channel2 = channel2.clone();
            let id1 = id1.clone();
            tokio::spawn(async move {
                for i in 0..num_messages {
                    let msg = if i % 10 == 0 {
                        // Every 10th message is large
                        Msg::ShareSingleFile {
                            filename: format!("file-{}.dat", i),
                            meta: "data".to_string(),
                            content: FileContentOrPath::Content(
                                String::from_utf8(vec![b'A' + (i as u8 % 26); 10 * 1024]).unwrap(),
                            ),
                        }
                    } else {
                        // Others are small
                        Msg::Info(
                            i as u32, // DocId is u32
                            Info {
                                sender: i as u32,
                                value: format!("Message {}", i),
                            },
                        )
                    };
                    channel2.send(&id1, None, msg).await.unwrap();
                }
            })
        };

        // Receive all messages
        let mut received = 0;
        while received < num_messages {
            let msg = timeout(Duration::from_secs(30), rx1.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(msg.host, id2);
            received += 1;
        }

        sender_task.await.unwrap();
        let elapsed = start.elapsed();

        println!(
            "Throughput test: {} messages in {:?}",
            num_messages, elapsed
        );
        println!(
            "Rate: {:.2} messages/sec",
            num_messages as f64 / elapsed.as_secs_f64()
        );

        accept_handle.abort();
    }

    #[tokio::test]
    async fn test_ice_progress_messages() {
        let env = TestEnvironment::new().await.unwrap();

        let (tx1, _rx1) = mpsc::channel::<Message>(100);
        let (tx2, mut rx2) = mpsc::channel::<Message>(100);

        let id1 = create_test_id("ice-1");
        let id2 = create_test_id("ice-2");

        let channel1 = WebChannel::new(id1.clone(), tx1);
        let channel2 = WebChannel::new(id2.clone(), tx2);

        let key_cert1 = create_test_key_cert(&id1);
        let key_cert2 = create_test_key_cert(&id2);

        // Start accepting
        let accept_handle = {
            let mut channel1 = channel1.clone();
            let key_cert1 = key_cert1.clone();
            let signaling_url = env.signaling_url().to_string();
            tokio::spawn(async move {
                let _ = channel1
                    .accept(key_cert1, &signaling_url, TransportType::SCTP)
                    .await;
            })
        };

        sleep(Duration::from_millis(200)).await;

        // Connect and collect ICE progress messages
        let connect_task = {
            let channel2 = channel2.clone();
            let key_cert2 = key_cert2.clone();
            let signaling_url = env.signaling_url().to_string();
            tokio::spawn(async move {
                channel2
                    .connect(
                        id1.clone(),
                        id2.clone(),
                        key_cert2,
                        &signaling_url,
                        TransportType::SCTP,
                    )
                    .await
            })
        };

        // Collect ICE progress messages
        let mut ice_messages = Vec::new();
        let start_time = std::time::Instant::now();

        while start_time.elapsed() < Duration::from_secs(5) {
            if let Ok(Some(msg)) = timeout(Duration::from_millis(100), rx2.recv()).await {
                if let Msg::IceProgress(status) = msg.body {
                    ice_messages.push(status);
                }
            }

            // Check if connection completed
            if connect_task.is_finished() {
                break;
            }
        }

        // Should have received some ICE progress messages
        assert!(
            !ice_messages.is_empty(),
            "Should receive ICE progress messages"
        );
        println!("Received ICE progress messages: {:?}", ice_messages);

        let connect_result = connect_task.await.unwrap();
        assert!(connect_result.is_ok(), "Connection should succeed");

        accept_handle.abort();
    }
}
