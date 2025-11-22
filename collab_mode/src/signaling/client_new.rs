//! New signaling client implementation that integrates directly with Server.
//!
//! This module provides SignalingClient which manages a connection to a
//! signaling server and routes messages to the Server. Multiple peer
//! connections can be handled through a single SignalingClient.

use crate::config_man::ArcKeyCert;
use crate::signaling::{CertDerHash, EndpointId, ICECandidate, SignalingMsg, SDP};
use anyhow::anyhow;
use anyhow::Context;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite as tung;
use tracing::Instrument;

/// Manages multiple signaling clients, one per signaling server address.
pub struct SignalingChannel {
    /// Map of signaling clients by signaling server address
    clients: Arc<Mutex<HashMap<String, SignalingClient>>>,
    /// Channel to send signaling messages to Server
    signaling_msg_tx: mpsc::Sender<(String, SignalingMsg)>,
    /// Channel to send internal messages to Server
    msg_tx: mpsc::Sender<crate::webchannel::Message>,
}

impl SignalingChannel {
    /// Create a new SignalingChannel.
    pub fn new(
        signaling_msg_tx: mpsc::Sender<(String, SignalingMsg)>,
        msg_tx: mpsc::Sender<crate::webchannel::Message>,
    ) -> Self {
        SignalingChannel {
            clients: Arc::new(Mutex::new(HashMap::new())),
            signaling_msg_tx,
            msg_tx,
        }
    }

    /// Bind to a signaling server address.
    ///
    /// Creates a new SignalingClient for the given address if one
    /// doesn't exist. Doesn't block
    pub async fn bind(
        &mut self,
        addr: String,
        id: EndpointId,
        key_cert: ArcKeyCert,
    ) -> anyhow::Result<()> {
        if self.clients.lock().unwrap().contains_key(&addr) {
            return Ok(());
        }

        // Create cleanup closure that removes client from the map
        let clients_clone = self.clients.clone();
        let addr_clone = addr.clone();
        let cleanup = Box::new(move || {
            clients_clone.lock().unwrap().remove(&addr_clone);
        });

        let client = SignalingClient::bind(
            addr.clone(),
            id,
            key_cert,
            self.signaling_msg_tx.clone(),
            self.msg_tx.clone(),
            Some(cleanup),
        )
        .await?;

        self.clients.lock().unwrap().insert(addr, client);
        Ok(())
    }

    /// Send a message to a signaling server.
    pub async fn send(&self, signaling_addr: &str, msg: SignalingMsg) -> anyhow::Result<()> {
        let client = {
            let clients = self.clients.lock().unwrap();
            clients
                .get(signaling_addr)
                .ok_or_else(|| anyhow!("No signaling client for address {}", signaling_addr))?
                .clone()
        };
        client.send(msg).await
    }

    /// Create a Sock for communicating with a peer through a signaling server.
    pub async fn create_sock(
        &self,
        signaling_addr: &str,
        peer_id: EndpointId,
        peer_cert: CertDerHash,
    ) -> anyhow::Result<Sock> {
        let client = {
            let clients = self.clients.lock().unwrap();
            clients
                .get(signaling_addr)
                .ok_or_else(|| anyhow!("No signaling client for address {}", signaling_addr))?
                .clone()
        };
        Ok(client.create_sock(peer_id, peer_cert).await)
    }

    /// Remove a signaling client.
    pub fn remove(&mut self, signaling_addr: &str) {
        self.clients.lock().unwrap().remove(signaling_addr);
    }
}

/// Main signaling client that manages connection to signaling server
#[derive(Clone)]
pub struct SignalingClient {
    /// Signaling server address
    _addr: String,
    /// My endpoint ID
    id: EndpointId,
    /// My key/cert for authentication
    _my_key_cert: ArcKeyCert,
    /// Channel to send outgoing messages to signaling server
    msg_out_tx: mpsc::Sender<SignalingMsg>,
    /// Whether we're bound to signaling server
    bound: Arc<Mutex<bool>>,
    /// Map of active peer connections, when we receive SDP or
    /// candidate for a peer from the main ws stream, use these tx to
    /// send to the appropriate Sock.
    sock_tx_map: Arc<Mutex<HashMap<EndpointId, SockTx>>>,
    /// Background task handle (wrapped in Arc for Clone)
    _task_handle: Arc<JoinHandle<()>>,
    /// Shutdown signal sender - when dropped, signals task to exit
    _shutdown_tx: mpsc::Sender<()>,
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
    peer_cert: CertDerHash,
    sdp_rx: mpsc::Receiver<SDP>,
    msg_out_tx: mpsc::Sender<SignalingMsg>,
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
        signaling_msg_tx: mpsc::Sender<(String, SignalingMsg)>,
        msg_tx: mpsc::Sender<crate::webchannel::Message>,
        cleanup: Option<Box<dyn FnOnce() + Send + 'static>>,
    ) -> anyhow::Result<Self> {
        // Create channels for outgoing messages.
        let (msg_out_tx, msg_out_rx) = mpsc::channel(16);

        // Create shutdown channel.
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);

        // Shared state.
        let bound = Arc::new(Mutex::new(false));
        let socks = Arc::new(Mutex::new(HashMap::new()));

        // Spawn background task.
        let addr_clone = addr.clone();
        let id_clone = id.clone();
        let my_key_cert_clone = my_key_cert.clone();
        let signaling_msg_tx_clone = signaling_msg_tx.clone();
        let bound_clone = bound.clone();
        let socks_clone = socks.clone();

        let span = tracing::info_span!(
            "SignalingClient send_receive_stream",
            endpoint = %id,
            signaling_addr = %addr
        );

        let msg_tx_clone = msg_tx.clone();
        let task_handle = tokio::spawn(
            send_receive_stream(
                addr_clone,
                id_clone,
                my_key_cert_clone,
                signaling_msg_tx_clone,
                msg_tx_clone,
                msg_out_rx,
                bound_clone,
                socks_clone,
                shutdown_rx,
                cleanup,
            )
            .instrument(span),
        );

        let cert_hash = my_key_cert.cert_der_hash();
        let bind_msg = SignalingMsg::Bind(id.clone(), cert_hash);
        msg_out_tx
            .send(bind_msg)
            .await
            .context("Failed to send Bind message")?;

        Ok(SignalingClient {
            _addr: addr,
            id,
            _my_key_cert: my_key_cert,
            msg_out_tx,
            bound,
            sock_tx_map: socks,
            _task_handle: Arc::new(task_handle),
            _shutdown_tx: shutdown_tx,
        })
    }

    /// Send a message to the signaling server.
    pub async fn send(&self, msg: SignalingMsg) -> anyhow::Result<()> {
        self.msg_out_tx
            .send(msg)
            .await
            .map_err(|_| anyhow!("Signaling client channel closed"))
    }

    /// Create a Sock for communicating with a peer.
    ///
    /// The Sock allows exchanging SDP and ICE candidates with the peer
    /// through the signaling server.
    pub async fn create_sock(&self, peer_id: EndpointId, peer_cert: CertDerHash) -> Sock {
        let (sdp_tx, sdp_rx) = mpsc::channel(1);
        let (candidate_tx, candidate_rx) = mpsc::unbounded_channel();

        // Add to socks map.
        {
            let mut socks = self.sock_tx_map.lock().unwrap();
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
            peer_cert,
            sdp_rx,
            msg_out_tx: self.msg_out_tx.clone(),
            candidate_rx,
        }
    }

    /// Check if bound on signaling server.
    pub async fn is_bound(&self) -> bool {
        self.bound.lock().unwrap().clone()
    }

    /// Remove a Sock from the map.
    pub async fn remove_sock(&self, peer_id: &str) {
        self.sock_tx_map.lock().unwrap().remove(peer_id);
    }
}

impl Sock {
    /// Send SDP to peer through signaling server.
    pub async fn send_sdp(&self, sdp: SDP) -> anyhow::Result<()> {
        let msg = SignalingMsg::SDP(self.my_id.clone(), self.peer_id.clone(), sdp);
        self.msg_out_tx
            .send(msg)
            .await
            .map_err(|_| anyhow!("Signaling channel closed"))
    }

    /// Receive SDP from peer.
    pub async fn recv_sdp(&mut self) -> anyhow::Result<SDP> {
        self.sdp_rx
            .recv()
            .await
            .ok_or_else(|| anyhow!("SDP channel closed"))
    }

    /// Send ICE candidate to peer.
    pub async fn send_candidate(&self, candidate: ICECandidate) -> anyhow::Result<()> {
        let msg = SignalingMsg::Candidate(self.my_id.clone(), self.peer_id.clone(), candidate);
        self.msg_out_tx
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
            candidate_tx: self.msg_out_tx.clone(),
        }
    }
}

/// Helper for sending candidates without holding a reference to Sock.
#[derive(Clone)]
pub struct CandidateSender {
    my_id: EndpointId,
    peer_id: EndpointId,
    candidate_tx: mpsc::Sender<SignalingMsg>,
}

impl CandidateSender {
    /// Send ICE candidate to peer.
    pub async fn send_candidate(&mut self, candidate: ICECandidate) -> anyhow::Result<()> {
        let msg = SignalingMsg::Candidate(self.my_id.clone(), self.peer_id.clone(), candidate);
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
    signaling_msg_tx: mpsc::Sender<(String, SignalingMsg)>,
    msg_tx: mpsc::Sender<crate::webchannel::Message>,
    mut msg_out_rx: mpsc::Receiver<SignalingMsg>,
    bound: Arc<Mutex<bool>>,
    socks: Arc<Mutex<HashMap<EndpointId, SockTx>>>,
    mut shutdown_rx: mpsc::Receiver<()>,
    cleanup: Option<Box<dyn FnOnce() + Send + 'static>>,
) {
    // Connect to signaling server
    let stream_result = tung::connect_async(&addr).await;
    if let Err(e) = stream_result {
        tracing::error!("Failed to connect to signaling server {}: {}", addr, e);
        // Send AcceptStopped message
        let web_msg = crate::webchannel::Message {
            host: id.clone(),
            body: crate::message::Msg::AcceptStopped {
                signaling_addr: addr.clone(),
                reason: format!("Failed to connect: {}", e),
            },
            req_id: None,
        };
        let _ = msg_tx.send(web_msg).await;
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
                            match serde_json::from_str::<SignalingMsg>(&text) {
                                Ok(signaling_msg) => {
                                    if let Err(e) = handle_incoming_message(
                                        signaling_msg,
                                        &addr,
                                        &id,
                                        &signaling_msg_tx,
                                        &bound,
                                        &socks,
                                    ).await {
                                        tracing::warn!("Error handling incoming message: {}", e);
                                    }
                                }
                                Err(e) => {
                                    tracing::error!("Failed to parse signaling message: {}", e);
                                    let web_msg = crate::webchannel::Message {
                                        host: id.clone(),
                                        body: crate::message::Msg::AcceptStopped {
                                            signaling_addr: addr.clone(),
                                            reason: format!("Parse error: {}", e),
                                        },
                                        req_id: None,
                                    };
                                    let _ = msg_tx.send(web_msg).await;
                                }
                            }
                        }
                        Ok(tung::tungstenite::Message::Close(_)) => {
                            tracing::info!("Signaling server closed connection");
                            let web_msg = crate::webchannel::Message {
                                host: id.clone(),
                                body: crate::message::Msg::AcceptStopped {
                                    signaling_addr: addr.clone(),
                                    reason: "Connection closed".to_string(),
                                },
                                req_id: None,
                            };
                            let _ = msg_tx.send(web_msg).await;
                            break;
                        }
                        Err(e) => {
                            tracing::error!("Websocket error: {}", e);
                            let web_msg = crate::webchannel::Message {
                                host: id.clone(),
                                body: crate::message::Msg::AcceptStopped {
                                    signaling_addr: addr.clone(),
                                    reason: format!("Websocket error: {}", e),
                                },
                                req_id: None,
                            };
                            let _ = msg_tx.send(web_msg).await;
                            break;
                        }
                        _ => {}
                    }
                } else {
                    tracing::info!("Websocket stream closed");
                    let web_msg = crate::webchannel::Message {
                        host: id.clone(),
                        body: crate::message::Msg::AcceptStopped {
                            signaling_addr: addr.clone(),
                            reason: "Websocket stream closed".to_string(),
                        },
                        req_id: None,
                    };
                    let _ = msg_tx.send(web_msg).await;
                    break;
                }
            }

            // Send to websocket
            msg = msg_out_rx.recv() => {
                if let Some(signaling_msg) = msg {
                    let text = serde_json::to_string(&signaling_msg).unwrap();
                    if let Err(e) = ws_tx.send(tung::tungstenite::Message::Text(text)).await {
                        tracing::error!("Failed to send to websocket: {}", e);
                        let web_msg = crate::webchannel::Message {
                            host: id.clone(),
                            body: crate::message::Msg::AcceptStopped {
                                signaling_addr: addr.clone(),
                                reason: format!("Failed to send: {}", e),
                            },
                            req_id: None,
                        };
                        let _ = msg_tx.send(web_msg).await;
                        break;
                    }
                } else {
                    tracing::info!("Outgoing message channel closed, shutting down");
                    break;
                }
            }

            // Shutdown signal
            _ = shutdown_rx.recv() => {
                tracing::info!("Shutdown signal received, exiting cleanly");
                break;
            }
        }
    }

    // Cleanup: clear all socks to close peer connections
    socks.lock().unwrap().clear();
    tracing::info!("send_receive_stream for {} exiting", addr);

    // Call cleanup function if provided
    if let Some(cleanup) = cleanup {
        cleanup();
    }
}

/// Handle an incoming signaling message and route it appropriately.
async fn handle_incoming_message(
    msg: SignalingMsg,
    addr: &str,
    _my_id: &EndpointId,
    signaling_msg_tx: &mpsc::Sender<(String, SignalingMsg)>,
    bound: &Arc<Mutex<bool>>,
    socks: &Arc<Mutex<HashMap<EndpointId, SockTx>>>,
) -> anyhow::Result<()> {
    match &msg {
        SignalingMsg::Bound(_id) => {
            // Set bound flag
            {
                *bound.lock().unwrap() = true;
            }
            // Forward to Server
            let _ = signaling_msg_tx.send((addr.to_string(), msg)).await;
        }

        SignalingMsg::Connect(..) => {
            let _ = signaling_msg_tx.send((addr.to_string(), msg)).await;
        }

        SignalingMsg::SDP(sender_id, _my_id, sdp) => {
            // Route SDP to the appropriate sock
            let sock_tx = {
                let socks_map = socks.lock().unwrap();
                socks_map.get(sender_id).map(|tx| tx.sdp_tx.clone())
            };

            if let Some(sdp_tx) = sock_tx {
                if sdp_tx.send(sdp.clone()).await.is_err() {
                    tracing::debug!("Failed to send SDP to peer {}", sender_id);
                }
            } else {
                tracing::debug!("Received SDP from unknown peer {}, dropping", sender_id);
            }
        }

        SignalingMsg::Candidate(sender_id, _my_id, candidate) => {
            // Route candidate to the appropriate sock
            let sock_tx = {
                let socks_map = socks.lock().unwrap();
                socks_map.get(sender_id).map(|tx| tx.candidate_tx.clone())
            };

            if let Some(candidate_tx) = sock_tx {
                if candidate_tx.send(candidate.clone()).is_err() {
                    tracing::debug!("Failed to send candidate to peer {}", sender_id);
                }
            } else {
                tracing::debug!(
                    "Received candidate from unknown peer {}, dropping",
                    sender_id
                );
            }
        }

        SignalingMsg::IdTaken(_endpoint_id, _message)
        | SignalingMsg::IdNotFound(_endpoint_id, _message)
        | SignalingMsg::TimeUp(_endpoint_id, _message) => {
            // Forward error to Server
            let _ = signaling_msg_tx.send((addr.to_string(), msg)).await;
        }

        _ => {
            tracing::debug!("Received unexpected signaling message: {:?}", msg);
        }
    }

    Ok(())
}
