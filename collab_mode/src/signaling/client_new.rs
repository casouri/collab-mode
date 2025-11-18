//! New signaling client implementation that integrates directly with Server.
//!
//! This module provides SignalingClient which manages a connection to a
//! signaling server and routes messages to the Server. Multiple peer
//! connections can be handled through a single SignalingClient.

use crate::config_man::ArcKeyCert;
use crate::message::Msg;
use crate::signaling::{
    CertDerHash, EndpointId, ICECandidate, SignalingError, SignalingMessage, SDP,
};
use anyhow::anyhow;
use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite as tung;
use tracing::Instrument;

/// Main signaling client that manages connection to signaling server
#[derive(Clone)]
pub struct SignalingClient {
    /// Signaling server address
    addr: String,
    /// My endpoint ID
    id: EndpointId,
    /// My key/cert for authentication
    my_key_cert: ArcKeyCert,
    /// Channel to send messages to Server
    msg_tx: mpsc::Sender<Msg>,
    /// Channel to send outgoing messages to signaling server
    msg_out_tx: mpsc::Sender<SignalingMessage>,
    /// Whether we're bound to signaling server
    bound: Arc<Mutex<bool>>,
    /// Map of active peer connections (peer_id -> sock tx)
    socks: Arc<Mutex<HashMap<EndpointId, SockTx>>>,
    /// Background task handle (wrapped in Arc for Clone)
    _task_handle: Arc<JoinHandle<()>>,
}

/// Transmission end of a Sock
struct SockTx {
    sdp_tx: mpsc::Sender<SDP>,
    candidate_tx: mpsc::UnboundedSender<ICECandidate>,
}

/// Socket for communicating with a peer through signaling server
pub struct Sock {
    my_id: EndpointId,
    peer_id: EndpointId,
    my_cert: CertDerHash,
    peer_cert: CertDerHash,
    peer_sdp: Option<SDP>,
    sdp_rx: mpsc::Receiver<SDP>,
    candidate_tx: mpsc::Sender<SignalingMessage>,
    candidate_rx: mpsc::UnboundedReceiver<ICECandidate>,
}

impl SignalingClient {
    /// Create and bind a new signaling client.
    ///
    /// This connects to the signaling server, sends a Bind message,
    /// and spawns a background task to handle messages.
    pub async fn bind(
        addr: String,
        id: EndpointId,
        my_key_cert: ArcKeyCert,
        msg_tx: mpsc::Sender<Msg>,
    ) -> anyhow::Result<Self> {
        // Create channels for outgoing messages.
        let (msg_out_tx, msg_out_rx) = mpsc::channel(16);

        // Shared state.
        let bound = Arc::new(Mutex::new(false));
        let socks = Arc::new(Mutex::new(HashMap::new()));

        // Spawn background task.
        let addr_clone = addr.clone();
        let id_clone = id.clone();
        let my_key_cert_clone = my_key_cert.clone();
        let msg_tx_clone = msg_tx.clone();
        let bound_clone = bound.clone();
        let socks_clone = socks.clone();

        let span = tracing::info_span!(
            "SignalingClient send_receive_stream",
            endpoint = %id,
            signaling_addr = %addr
        );

        let task_handle = tokio::spawn(
            send_receive_stream(
                addr_clone,
                id_clone,
                my_key_cert_clone,
                msg_tx_clone,
                msg_out_rx,
                bound_clone,
                socks_clone,
            )
            .instrument(span),
        );

        let cert_hash = my_key_cert.cert_der_hash();
        let bind_msg = SignalingMessage::Bind(id.clone(), cert_hash);
        msg_out_tx
            .send(bind_msg)
            .await
            .context("Failed to send Bind message")?;

        Ok(SignalingClient {
            addr,
            id,
            my_key_cert,
            msg_tx,
            msg_out_tx,
            bound,
            socks,
            _task_handle: Arc::new(task_handle),
        })
    }

    /// Send a message to the signaling server.
    pub async fn send(&self, msg: SignalingMessage) -> anyhow::Result<()> {
        self.msg_out_tx
            .send(msg)
            .await
            .map_err(|_| anyhow!("Signaling client channel closed"))
    }

    /// Create a Sock for communicating with a peer.
    ///
    /// The Sock allows exchanging SDP and ICE candidates with the peer
    /// through the signaling server.
    pub async fn create_sock(
        &self,
        peer_id: EndpointId,
        peer_cert: CertDerHash,
        peer_sdp: Option<SDP>,
    ) -> Sock {
        let (sdp_tx, sdp_rx) = mpsc::channel(1);
        let (candidate_tx, candidate_rx) = mpsc::unbounded_channel();

        // Add to socks map.
        {
            let mut socks = self.socks.lock().unwrap();
            socks.insert(
                peer_id.clone(),
                SockTx {
                    sdp_tx,
                    candidate_tx,
                },
            );
        }

        Sock {
            my_id: self.id.clone(),
            peer_id,
            my_cert: self.my_key_cert.cert_der_hash(),
            peer_cert,
            peer_sdp,
            sdp_rx,
            candidate_tx: self.msg_out_tx.clone(),
            candidate_rx,
        }
    }

    /// Check if bound on signaling server.
    pub async fn is_bound(&self) -> bool {
        self.bound.lock().unwrap().clone()
    }

    /// Remove a Sock from the map.
    pub async fn remove_sock(&self, peer_id: &str) {
        self.socks.lock().unwrap().remove(peer_id);
    }
}

impl Sock {
    /// Send SDP to peer through signaling server.
    pub async fn send_sdp(&self, sdp: SDP, initiator: bool) -> anyhow::Result<()> {
        let msg = SignalingMessage::Connect(
            self.my_id.clone(),
            self.peer_id.clone(),
            sdp,
            self.my_cert.clone(),
            initiator,
        );
        self.candidate_tx
            .send(msg)
            .await
            .map_err(|_| anyhow!("Signaling channel closed"))
    }

    /// Receive SDP from peer.
    pub async fn recv_sdp(&mut self) -> anyhow::Result<SDP> {
        // If we already have peer_sdp, return it.
        if let Some(sdp) = self.peer_sdp.take() {
            return Ok(sdp);
        }

        // Otherwise wait for it.
        self.sdp_rx
            .recv()
            .await
            .ok_or_else(|| anyhow!("SDP channel closed"))
    }

    /// Send ICE candidate to peer.
    pub async fn send_candidate(&self, candidate: ICECandidate) -> anyhow::Result<()> {
        let msg = SignalingMessage::Candidate(self.my_id.clone(), self.peer_id.clone(), candidate);
        self.candidate_tx
            .send(msg)
            .await
            .map_err(|_| anyhow!("Signaling channel closed"))
    }

    /// Receive ICE candidate from peer.
    pub async fn recv_candidate(&mut self) -> Option<ICECandidate> {
        self.candidate_rx.recv().await
    }

    /// Get peer's certificate hash.
    pub fn cert_hash(&self) -> &str {
        &self.peer_cert
    }

    /// Get peer's ID.
    pub fn id(&self) -> &str {
        &self.peer_id
    }

    /// Get a candidate sender for this sock.
    ///
    /// This allows sending candidates without holding a reference to the sock.
    pub fn candidate_sender(&self) -> CandidateSender {
        CandidateSender {
            my_id: self.my_id.clone(),
            peer_id: self.peer_id.clone(),
            candidate_tx: self.candidate_tx.clone(),
        }
    }
}

/// Helper for sending candidates without holding a reference to Sock.
#[derive(Clone)]
pub struct CandidateSender {
    my_id: EndpointId,
    peer_id: EndpointId,
    candidate_tx: mpsc::Sender<SignalingMessage>,
}

impl CandidateSender {
    /// Send ICE candidate to peer.
    pub async fn send_candidate(&mut self, candidate: ICECandidate) -> anyhow::Result<()> {
        let msg = SignalingMessage::Candidate(self.my_id.clone(), self.peer_id.clone(), candidate);
        self.candidate_tx
            .send(msg)
            .await
            .map_err(|_| anyhow!("Signaling channel closed"))
    }
}

/// Background task that manages websocket connection to signaling server.
///
/// This task:
/// - Connects to the signaling server
/// - Routes incoming messages to Server or Socks
/// - Sends outgoing messages to signaling server
/// - Reports errors to Server
async fn send_receive_stream(
    addr: String,
    id: EndpointId,
    _my_key_cert: ArcKeyCert,
    msg_tx: mpsc::Sender<Msg>,
    mut msg_out_rx: mpsc::Receiver<SignalingMessage>,
    bound: Arc<Mutex<bool>>,
    socks: Arc<Mutex<HashMap<EndpointId, SockTx>>>,
) {
    // Connect to signaling server
    let stream_result = tung::connect_async(&addr).await;
    if let Err(e) = stream_result {
        tracing::error!("Failed to connect to signaling server {}: {}", addr, e);
        let _ = msg_tx
            .send(Msg::SignalingErr(
                addr.clone(),
                SignalingError::OtherError(format!("Failed to connect: {}", e)),
            ))
            .await;
        return;
    }

    let (stream, _) = stream_result.unwrap();
    let (mut ws_tx, mut ws_rx) = stream.split();

    loop {
        tokio::select! {
            // Receive from websocket
            msg = ws_rx.next() => {
                if let Some(msg_result) = msg {
                    match msg_result {
                        Ok(tung::tungstenite::Message::Text(text)) => {
                            // Parse and route message.
                            match serde_json::from_str::<SignalingMessage>(&text) {
                                Ok(signaling_msg) => {
                                    if let Err(e) = handle_incoming_message(
                                        signaling_msg,
                                        &addr,
                                        &id,
                                        &msg_tx,
                                        &bound,
                                        &socks,
                                    ).await {
                                        tracing::warn!("Error handling incoming message: {}", e);
                                    }
                                }
                                Err(e) => {
                                    tracing::error!("Failed to parse signaling message: {}", e);
                                    let _ = msg_tx.send(Msg::SignalingErr(
                                        addr.clone(),
                                        SignalingError::OtherError(format!("Parse error: {}", e)),
                                    )).await;
                                }
                            }
                        }
                        Ok(tung::tungstenite::Message::Close(_)) => {
                            tracing::info!("Signaling server closed connection");
                            let _ = msg_tx.send(Msg::SignalingErr(
                                addr.clone(),
                                SignalingError::ConnectionBroke,
                            )).await;
                            break;
                        }
                        Err(e) => {
                            tracing::error!("Websocket error: {}", e);
                            let _ = msg_tx.send(Msg::SignalingErr(
                                addr.clone(),
                                SignalingError::ConnectionBroke,
                            )).await;
                            break;
                        }
                        _ => {}
                    }
                } else {
                    tracing::info!("Websocket stream closed");
                    let _ = msg_tx.send(Msg::SignalingErr(
                        addr.clone(),
                        SignalingError::ConnectionBroke,
                    )).await;
                    break;
                }
            }

            // Send to websocket
            msg = msg_out_rx.recv() => {
                if let Some(signaling_msg) = msg {
                    let text = serde_json::to_string(&signaling_msg).unwrap();
                    if let Err(e) = ws_tx.send(tung::tungstenite::Message::Text(text)).await {
                        tracing::error!("Failed to send to websocket: {}", e);
                        let _ = msg_tx.send(Msg::SignalingErr(
                            addr.clone(),
                            SignalingError::ConnectionBroke,
                        )).await;
                        break;
                    }
                } else {
                    tracing::info!("Outgoing message channel closed, shutting down");
                    break;
                }
            }
        }
    }

    // Cleanup
    tracing::info!("send_receive_stream for {} exiting", addr);
}

/// Handle an incoming signaling message and route it appropriately.
async fn handle_incoming_message(
    msg: SignalingMessage,
    addr: &str,
    _my_id: &EndpointId,
    msg_tx: &mpsc::Sender<Msg>,
    bound: &Arc<Mutex<bool>>,
    socks: &Arc<Mutex<HashMap<EndpointId, SockTx>>>,
) -> anyhow::Result<()> {
    match &msg {
        SignalingMessage::Bound(_id) => {
            // Set bound flag
            {
                *bound.lock().unwrap() = true;
            }
            // Forward to Server
            let _ = msg_tx.send(Msg::SignalingMsg(addr.to_string(), msg)).await;
        }

        SignalingMessage::Connect(..) => {
            let _ = msg_tx.send(Msg::SignalingMsg(addr.to_string(), msg)).await;
        }

        SignalingMessage::Candidate(sender_id, _my_id, candidate) => {
            // Route candidate to the appropriate sock
            let sock_tx = {
                let socks_map = socks.lock().unwrap();
                socks_map.get(sender_id).map(|tx| tx.candidate_tx.clone())
            };

            if let Some(candidate_tx) = sock_tx {
                let _ = candidate_tx.send(candidate.clone());
            } else {
                tracing::debug!(
                    "Received candidate from unknown peer {}, dropping",
                    sender_id
                );
            }
        }

        SignalingMessage::Error(_endpoint_id, _message) => {
            // Forward error to Server
            let _ = msg_tx.send(Msg::SignalingMsg(addr.to_string(), msg)).await;
        }

        _ => {
            tracing::debug!("Received unexpected signaling message: {:?}", msg);
        }
    }

    Ok(())
}
