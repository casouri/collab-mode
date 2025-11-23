//! Tests for TestSignalingChannel routing functionality

use super::*;
use crate::signaling::client_new::{SignalingChannelTrait, TestSignalingChannelFactory};
use crate::signaling::{SignalingMessage, SignalingMsg, SDP};
use std::sync::Arc;
use tokio::sync::mpsc;

#[tokio::test]
async fn test_sdp_and_ice_candidate_routing() {
    // Create test signaling factory
    let factory = Arc::new(TestSignalingChannelFactory::new());

    // Create channels for receiving signaling messages
    let (signaling_tx_a, mut signaling_rx_a) = mpsc::channel::<SignalingMessage>(16);
    let (signaling_tx_b, mut signaling_rx_b) = mpsc::channel::<SignalingMessage>(16);

    // Create test signaling channels for two endpoints
    let mut channel_a = factory.get_channel(signaling_tx_a);
    let mut channel_b = factory.get_channel(signaling_tx_b);

    // Define endpoint IDs
    let id_a = "endpoint-a".to_string();
    let id_b = "endpoint-b".to_string();
    let signaling_addr = "test-signaling-addr".to_string();

    // Create dummy certificates
    let cert_a = "cert-hash-a".to_string();
    let cert_b = "cert-hash-b".to_string();

    // Create dummy key/cert (not actually used in test signaling)
    let temp_dir_a = tempfile::TempDir::new().unwrap();
    let config_a = ConfigManager::new(Some(temp_dir_a.path().to_path_buf()), None).unwrap();
    let key_cert_a = Arc::new(config_a.get_key_and_cert(id_a.clone()).unwrap());

    let temp_dir_b = tempfile::TempDir::new().unwrap();
    let config_b = ConfigManager::new(Some(temp_dir_b.path().to_path_buf()), None).unwrap();
    let key_cert_b = Arc::new(config_b.get_key_and_cert(id_b.clone()).unwrap());

    // Both endpoints bind to the signaling address
    channel_a
        .bind(signaling_addr.clone(), id_a.clone(), key_cert_a.clone())
        .await
        .unwrap();

    channel_b
        .bind(signaling_addr.clone(), id_b.clone(), key_cert_b.clone())
        .await
        .unwrap();

    // Drain the Bound messages
    let _ = signaling_rx_a.recv().await.unwrap();
    let _ = signaling_rx_b.recv().await.unwrap();

    // Endpoint A creates a Sock for communicating with endpoint B
    let mut sock_a = channel_a
        .create_sock(&signaling_addr, id_b.clone(), cert_b.clone())
        .await
        .unwrap();

    // Endpoint B creates a Sock for communicating with endpoint A
    let mut sock_b = channel_b
        .create_sock(&signaling_addr, id_a.clone(), cert_a.clone())
        .await
        .unwrap();

    // Test SDP routing: A -> B
    let sdp_from_a = "v=0\r\no=- 123 0 IN IP4 192.168.1.1\r\ns=test-offer\r\n".to_string();

    sock_a.send_sdp(sdp_from_a.clone()).await.unwrap();

    // Endpoint B should receive the SDP
    let received_sdp = sock_b.recv_sdp().await.unwrap();
    assert_eq!(received_sdp, sdp_from_a);

    // Test SDP routing: B -> A
    let sdp_from_b = "v=0\r\no=- 456 0 IN IP4 192.168.1.2\r\ns=test-answer\r\n".to_string();

    sock_b.send_sdp(sdp_from_b.clone()).await.unwrap();

    // Endpoint A should receive the SDP
    let received_sdp = sock_a.recv_sdp().await.unwrap();
    assert_eq!(received_sdp, sdp_from_b);

    // Test ICE candidate routing: A -> B
    let candidate_from_a = "candidate:1 1 UDP 2130706431 192.168.1.1 54321 typ host".to_string();
    sock_a
        .send_candidate(candidate_from_a.clone())
        .await
        .unwrap();

    // Endpoint B should receive the candidate
    let received_candidate = sock_b.recv_candidate().await.unwrap();
    assert_eq!(received_candidate, candidate_from_a);

    // Test ICE candidate routing: B -> A
    let candidate_from_b = "candidate:2 1 UDP 2130706431 192.168.1.2 54322 typ host".to_string();
    sock_b
        .send_candidate(candidate_from_b.clone())
        .await
        .unwrap();

    // Endpoint A should receive the candidate
    let received_candidate = sock_a.recv_candidate().await.unwrap();
    assert_eq!(received_candidate, candidate_from_b);
}

#[tokio::test]
async fn test_connect_message_routing() {
    // Create test signaling factory
    let factory = Arc::new(TestSignalingChannelFactory::new());

    // Create channels for receiving signaling messages
    let (signaling_tx_a, mut signaling_rx_a) = mpsc::channel::<SignalingMessage>(16);
    let (signaling_tx_b, mut signaling_rx_b) = mpsc::channel::<SignalingMessage>(16);

    // Create test signaling channels for two endpoints
    let mut channel_a = factory.get_channel(signaling_tx_a);
    let mut channel_b = factory.get_channel(signaling_tx_b);

    // Define endpoint IDs
    let id_a = "endpoint-a".to_string();
    let id_b = "endpoint-b".to_string();
    let signaling_addr = "test-signaling-addr".to_string();

    // Create dummy certificates
    let cert_a = "cert-hash-a".to_string();

    // Create dummy key/cert (not actually used in test signaling)
    let temp_dir_a = tempfile::TempDir::new().unwrap();
    let config_a = ConfigManager::new(Some(temp_dir_a.path().to_path_buf()), None).unwrap();
    let key_cert_a = Arc::new(config_a.get_key_and_cert(id_a.clone()).unwrap());

    let temp_dir_b = tempfile::TempDir::new().unwrap();
    let config_b = ConfigManager::new(Some(temp_dir_b.path().to_path_buf()), None).unwrap();
    let key_cert_b = Arc::new(config_b.get_key_and_cert(id_b.clone()).unwrap());

    // Both endpoints bind to the signaling address
    channel_a
        .bind(signaling_addr.clone(), id_a.clone(), key_cert_a.clone())
        .await
        .unwrap();

    channel_b
        .bind(signaling_addr.clone(), id_b.clone(), key_cert_b.clone())
        .await
        .unwrap();

    // Drain the Bound messages
    let _ = signaling_rx_a.recv().await.unwrap();
    let _ = signaling_rx_b.recv().await.unwrap();

    // Endpoint A sends a Connect message to endpoint B
    let connect_msg = SignalingMsg::Connect(
        id_a.clone(),   // sender
        id_b.clone(),   // receiver
        cert_a.clone(), // sender cert
        true,           // initiator
    );

    channel_a
        .send(&signaling_addr, connect_msg.clone())
        .await
        .unwrap();

    // Endpoint B should receive the Connect message
    let received_message = signaling_rx_b.recv().await.unwrap();
    assert_eq!(received_message.signaling_addr, signaling_addr);

    match received_message.msg {
        Ok(SignalingMsg::Connect(sender_id, receiver_id, sender_cert, initiator)) => {
            assert_eq!(sender_id, id_a);
            assert_eq!(receiver_id, id_b);
            assert_eq!(sender_cert, cert_a);
            assert_eq!(initiator, true);
        }
        _ => panic!("Expected Connect message, got {:?}", received_message.msg),
    }

    // Test reverse direction: B sends Connect to A
    let connect_msg_b = SignalingMsg::Connect(
        id_b.clone(),              // sender
        id_a.clone(),              // receiver
        "cert-hash-b".to_string(), // sender cert
        false,                     // not initiator (responding)
    );

    channel_b
        .send(&signaling_addr, connect_msg_b.clone())
        .await
        .unwrap();

    // Endpoint A should receive the Connect message
    let received_message = signaling_rx_a.recv().await.unwrap();
    assert_eq!(received_message.signaling_addr, signaling_addr);

    match received_message.msg {
        Ok(SignalingMsg::Connect(sender_id, receiver_id, sender_cert, initiator)) => {
            assert_eq!(sender_id, id_b);
            assert_eq!(receiver_id, id_a);
            assert_eq!(sender_cert, "cert-hash-b");
            assert_eq!(initiator, false);
        }
        _ => panic!("Expected Connect message, got {:?}", received_message.msg),
    }
}
