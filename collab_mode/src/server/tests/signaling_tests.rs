//! Tests for the in-memory test signaling routing.

use super::*;
use crate::signaling::client_new::{SignalingChannel, TestFactoryState};
use crate::signaling::{SignalingMessage, SignalingMsg};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

#[tokio::test]
async fn test_sdp_and_ice_candidate_routing() {
    let state = Arc::new(Mutex::new(TestFactoryState::default()));

    let (signaling_tx_a, mut signaling_rx_a) = mpsc::channel::<SignalingMessage>(16);
    let (signaling_tx_b, mut signaling_rx_b) = mpsc::channel::<SignalingMessage>(16);

    let channel_a = SignalingChannel::new_for_test(signaling_tx_a, state.clone());
    let channel_b = SignalingChannel::new_for_test(signaling_tx_b, state.clone());

    let signaling_addr = "test-signaling-addr".to_string();

    let temp_dir_a = tempfile::TempDir::new().unwrap();
    let config_a = ConfigManager::new(Some(temp_dir_a.path().to_path_buf()), None).unwrap();
    let key_cert_a = Arc::new(config_a.get_key_and_cert("endpoint-a").unwrap());
    let id_a = format!("endpoint-a::{}", key_cert_a.cert_der_hash());

    let temp_dir_b = tempfile::TempDir::new().unwrap();
    let config_b = ConfigManager::new(Some(temp_dir_b.path().to_path_buf()), None).unwrap();
    let key_cert_b = Arc::new(config_b.get_key_and_cert("endpoint-b").unwrap());
    let id_b = format!("endpoint-b::{}", key_cert_b.cert_der_hash());

    // Both endpoints bind to the signaling address, mutually trusting each other.
    channel_a
        .bind(
            signaling_addr.clone(),
            id_a.clone(),
            key_cert_a.clone(),
            vec![id_b.clone()],
        )
        .await
        .unwrap();

    channel_b
        .bind(
            signaling_addr.clone(),
            id_b.clone(),
            key_cert_b.clone(),
            vec![id_a.clone()],
        )
        .await
        .unwrap();

    // Drain the Bound messages
    let _ = signaling_rx_a.recv().await.unwrap();
    let _ = signaling_rx_b.recv().await.unwrap();

    let mut sock_a = channel_a
        .create_sock(&signaling_addr, id_b.clone())
        .await
        .unwrap();
    let mut sock_b = channel_b
        .create_sock(&signaling_addr, id_a.clone())
        .await
        .unwrap();

    // SDP A -> B
    let sdp_from_a = "v=0\r\no=- 123 0 IN IP4 192.168.1.1\r\ns=test-offer\r\n".to_string();
    sock_a.send_sdp(sdp_from_a.clone()).await.unwrap();
    assert_eq!(sock_b.recv_sdp().await.unwrap(), sdp_from_a);

    // SDP B -> A
    let sdp_from_b = "v=0\r\no=- 456 0 IN IP4 192.168.1.2\r\ns=test-answer\r\n".to_string();
    sock_b.send_sdp(sdp_from_b.clone()).await.unwrap();
    assert_eq!(sock_a.recv_sdp().await.unwrap(), sdp_from_b);

    // Candidate A -> B
    let candidate_from_a = "candidate:1 1 UDP 2130706431 192.168.1.1 54321 typ host".to_string();
    sock_a
        .send_candidate(candidate_from_a.clone())
        .await
        .unwrap();
    assert_eq!(sock_b.recv_candidate().await.unwrap(), candidate_from_a);

    // Candidate B -> A
    let candidate_from_b = "candidate:2 1 UDP 2130706431 192.168.1.2 54322 typ host".to_string();
    sock_b
        .send_candidate(candidate_from_b.clone())
        .await
        .unwrap();
    assert_eq!(sock_a.recv_candidate().await.unwrap(), candidate_from_b);
}

#[tokio::test]
async fn test_connect_message_routing() {
    // Create test signaling factory
    let state = Arc::new(Mutex::new(TestFactoryState::default()));

    let (signaling_tx_a, mut signaling_rx_a) = mpsc::channel::<SignalingMessage>(16);
    let (signaling_tx_b, mut signaling_rx_b) = mpsc::channel::<SignalingMessage>(16);

    let channel_a = SignalingChannel::new_for_test(signaling_tx_a, state.clone());
    let channel_b = SignalingChannel::new_for_test(signaling_tx_b, state.clone());

    let signaling_addr = "test-signaling-addr".to_string();

    let temp_dir_a = tempfile::TempDir::new().unwrap();
    let config_a = ConfigManager::new(Some(temp_dir_a.path().to_path_buf()), None).unwrap();
    let key_cert_a = Arc::new(config_a.get_key_and_cert("endpoint-a").unwrap());
    let id_a = format!("endpoint-a::{}", key_cert_a.cert_der_hash());

    let temp_dir_b = tempfile::TempDir::new().unwrap();
    let config_b = ConfigManager::new(Some(temp_dir_b.path().to_path_buf()), None).unwrap();
    let key_cert_b = Arc::new(config_b.get_key_and_cert("endpoint-b").unwrap());
    let id_b = format!("endpoint-b::{}", key_cert_b.cert_der_hash());

    // Both endpoints bind to the signaling address, mutually trusting each other.
    channel_a
        .bind(
            signaling_addr.clone(),
            id_a.clone(),
            key_cert_a.clone(),
            vec![id_b.clone()],
        )
        .await
        .unwrap();

    channel_b
        .bind(
            signaling_addr.clone(),
            id_b.clone(),
            key_cert_b.clone(),
            vec![id_a.clone()],
        )
        .await
        .unwrap();

    // Drain the Bound messages
    let _ = signaling_rx_a.recv().await.unwrap();
    let _ = signaling_rx_b.recv().await.unwrap();

    // A -> B
    channel_a
        .send(
            &signaling_addr,
            SignalingMsg::Connect {
                sender: id_a.clone(),
                receiver: id_b.clone(),
                initiator: true,
            },
        )
        .await
        .unwrap();

    // Endpoint B should receive the Connect message
    let received_message = signaling_rx_b.recv().await.unwrap();
    assert_eq!(received_message.signaling_addr, signaling_addr);

    match received_message.msg {
        Ok(SignalingMsg::Connect {
            sender,
            receiver,
            initiator,
        }) => {
            assert_eq!(sender, id_a);
            assert_eq!(receiver, id_b);
            assert_eq!(initiator, true);
        }
        _ => panic!("Expected Connect message, got {:?}", received_message.msg),
    }

    // B -> A
    channel_b
        .send(
            &signaling_addr,
            SignalingMsg::Connect {
                sender: id_b.clone(),
                receiver: id_a.clone(),
                initiator: false,
            },
        )
        .await
        .unwrap();

    // Endpoint A should receive the Connect message
    let received_message = signaling_rx_a.recv().await.unwrap();
    assert_eq!(received_message.signaling_addr, signaling_addr);

    match received_message.msg {
        Ok(SignalingMsg::Connect {
            sender,
            receiver,
            initiator,
        }) => {
            assert_eq!(sender, id_b);
            assert_eq!(receiver, id_a);
            assert_eq!(initiator, false);
        }
        _ => panic!("Expected Connect message, got {:?}", received_message.msg),
    }
}
