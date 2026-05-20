//! New signaling client implementation that integrates directly with Server.
//!
//! This module provides SignalingClient which manages a connection to a
//! signaling server and routes messages to the Server. Multiple peer
//! connections can be handled through a single SignalingClient.

use crate::config_man::ArcKeyCert;
use crate::signaling::auth::sign_identity;
use crate::signaling::{
    encode_trusted_header, EndpointId, ICECandidate, SignalingMsg, BIND_HEADER_ID,
    BIND_HEADER_IDENTITY, BIND_HEADER_SIGNATURE, BIND_HEADER_TRUSTED, SDP,
};
use crate::types::id_hash;
use anyhow::anyhow;
use anyhow::Context;
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite as tung;
use tracing::Instrument;

/// Build the WebSocket upgrade request with the four `X-Collab-*`
/// bind headers attached. The server's `accept_hdr_async` callback
/// validates them; if anything is missing or wrong, the upgrade is
/// rejected with HTTP 401.
fn build_bind_request(
    addr: &str,
    id: &str,
    identity_str: &str,
    signature_str: &str,
    trusted_header: &str,
) -> anyhow::Result<tung::tungstenite::http::Request<()>> {
    use tung::tungstenite::handshake::client::generate_key;
    use tung::tungstenite::http::{Request, Uri};
    let uri: Uri = addr.parse().context("invalid signaling addr")?;
    let host = uri
        .authority()
        .ok_or_else(|| anyhow!("signaling addr missing host"))?
        .as_str()
        .to_string();
    let req = Request::builder()
        .method("GET")
        .uri(addr)
        .header("Host", host)
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", generate_key())
        .header(BIND_HEADER_ID, id)
        .header(BIND_HEADER_IDENTITY, identity_str)
        .header(BIND_HEADER_SIGNATURE, signature_str)
        .header(BIND_HEADER_TRUSTED, trusted_header)
        .body(())
        .context("building upgrade request")?;
    Ok(req)
}

/// Trait for signaling channel operations. Provides async methods for
/// binding, sending, and creating socks.
#[async_trait]
pub trait SignalingChannelTrait: Send + Sync {
    /// Bind to a signaling server address. `trusted` is the initial
    /// trust list installed on the signaling server.
    async fn bind(
        &mut self,
        addr: String,
        id: EndpointId,
        key_cert: ArcKeyCert,
        trusted: Vec<EndpointId>,
    ) -> anyhow::Result<()>;

    /// Send a message to a signaling server.
    async fn send(&self, signaling_addr: &str, msg: SignalingMsg) -> anyhow::Result<()>;

    /// Create a Sock for communicating with a peer through a signaling server.
    async fn create_sock(&self, signaling_addr: &str, peer_id: EndpointId) -> anyhow::Result<Sock>;

    /// Remove a signaling client.
    fn remove(&mut self, signaling_addr: &str);
}

/// Dummy signaling channel for envoy mode. In envoy mode we never use
/// the signaling feature. Throw errors if actually used.
/// safety net.
pub struct NoopSignalingChannel;

#[async_trait]
impl SignalingChannelTrait for NoopSignalingChannel {
    async fn bind(
        &mut self,
        _addr: String,
        _id: EndpointId,
        _key_cert: ArcKeyCert,
        _trusted: Vec<EndpointId>,
    ) -> anyhow::Result<()> {
        Err(anyhow!("Envoy mode shouldn’t use signaling"))
    }

    async fn send(&self, _signaling_addr: &str, _msg: SignalingMsg) -> anyhow::Result<()> {
        Err(anyhow!("envoy mode: signaling unused"))
    }

    async fn create_sock(
        &self,
        _signaling_addr: &str,
        _peer_id: EndpointId,
    ) -> anyhow::Result<Sock> {
        Err(anyhow!("Envoy mode shouldn’t use signaling"))
    }

    fn remove(&mut self, _signaling_addr: &str) {}
}

/// Manages multiple signaling clients, one per signaling server address.
pub struct SignalingChannel {
    /// Map of signaling clients by signaling server address
    clients: Arc<Mutex<HashMap<String, SignalingClient>>>,
    /// Channel to send signaling messages to Server
    signaling_msg_tx: mpsc::Sender<crate::signaling::SignalingMessage>,
}

impl SignalingChannel {
    /// Create a new SignalingChannel.
    pub fn new(signaling_msg_tx: mpsc::Sender<crate::signaling::SignalingMessage>) -> Self {
        SignalingChannel {
            clients: Arc::new(Mutex::new(HashMap::new())),
            signaling_msg_tx,
        }
    }

    /// Bind to a signaling server address.
    ///
    /// Creates a new SignalingClient for the given address if one
    /// doesn't exist. Doesn't block.
    pub async fn bind(
        &mut self,
        addr: String,
        id: EndpointId,
        key_cert: ArcKeyCert,
        trusted: Vec<EndpointId>,
    ) -> anyhow::Result<()> {
        if self.clients.lock().unwrap().contains_key(&addr) {
            return Ok(());
        }

        // Create cleanup closure that removes client from the map.
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
            Some(cleanup),
            trusted,
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
    ) -> anyhow::Result<Sock> {
        let client = {
            let clients = self.clients.lock().unwrap();
            clients
                .get(signaling_addr)
                .ok_or_else(|| anyhow!("No signaling client for address {}", signaling_addr))?
                .clone()
        };
        Ok(client.create_sock(peer_id).await)
    }

    /// Remove a signaling client.
    pub fn remove(&mut self, signaling_addr: &str) {
        self.clients.lock().unwrap().remove(signaling_addr);
    }
}

#[async_trait]
impl SignalingChannelTrait for SignalingChannel {
    async fn bind(
        &mut self,
        addr: String,
        id: EndpointId,
        key_cert: ArcKeyCert,
        trusted: Vec<EndpointId>,
    ) -> anyhow::Result<()> {
        self.bind(addr, id, key_cert, trusted).await
    }

    async fn send(&self, signaling_addr: &str, msg: SignalingMsg) -> anyhow::Result<()> {
        self.send(signaling_addr, msg).await
    }

    async fn create_sock(&self, signaling_addr: &str, peer_id: EndpointId) -> anyhow::Result<Sock> {
        self.create_sock(signaling_addr, peer_id).await
    }

    fn remove(&mut self, signaling_addr: &str) {
        self.remove(signaling_addr);
    }
}

/// Main signaling client that manages connection to signaling server.
/// It runs background tasks but will self-cleanup when dropped.
#[derive(Clone)]
pub struct SignalingClient {
    /// Signaling server address
    _addr: String,
    /// My endpoint ID
    id: EndpointId,
    /// Channel to send outgoing messages to signaling server
    msg_out_tx: mpsc::Sender<SignalingMsg>,
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

/// Socket for communicating with a peer through signaling server.
pub struct Sock {
    my_id: EndpointId,
    peer_id: EndpointId,
    sdp_rx: mpsc::Receiver<SDP>,
    msg_out_tx: mpsc::Sender<SignalingMsg>,
    candidate_rx: mpsc::UnboundedReceiver<ICECandidate>,
}

impl SignalingClient {
    /// Bind to a signaling server. The bind payload (id, identity,
    /// signature, trust list) goes into `X-Collab-*` headers of the
    /// WebSocket upgrade. If connection succeeds, create a
    /// `SignalingMsg::Bound` message and send to `signaling_msg_tx`.
    pub async fn bind(
        addr: String,
        id: EndpointId,
        my_key_cert: ArcKeyCert,
        signaling_msg_tx: mpsc::Sender<crate::signaling::SignalingMessage>,
        cleanup: Option<Box<dyn FnOnce() + Send + 'static>>,
        trusted: Vec<EndpointId>,
    ) -> anyhow::Result<Self> {
        // Cert-hash sanity check.
        let cert_hash = my_key_cert.cert_der_hash();
        if id_hash(&id) != cert_hash {
            return Err(anyhow!(
                "Endpoint id hash {} doesn't match our cert hash {}",
                id_hash(&id),
                cert_hash
            ));
        }

        // Build the bind headers.
        let (identity, signature) = sign_identity(&my_key_cert)?;
        let trusted_header = encode_trusted_header(&trusted)?;
        let req = build_bind_request(
            &addr,
            &id,
            &identity.to_string(),
            &signature.0,
            &trusted_header,
        )?;

        let (stream, _resp) = tung::connect_async(req)
            .await
            .context("WebSocket bind upgrade failed")?;

        let bound = crate::signaling::SignalingMessage {
            signaling_addr: addr.clone(),
            msg: Ok(SignalingMsg::Bound {
                id: id.clone(),
                trusted: trusted.clone(),
            }),
        };
        let _ = signaling_msg_tx.send(bound).await;

        // Wire up the message loop.
        let (msg_out_tx, msg_out_rx) = mpsc::channel(16);
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);
        let socks = Arc::new(Mutex::new(HashMap::new()));

        let span = tracing::info_span!(
            "SignalingClient send_receive_stream",
            endpoint = %id,
            signaling_addr = %addr
        );

        // When the client is dropped, `shutdown_tx` is dropped too;
        // the background task will run `cleanup` and terminate.
        let task_handle = tokio::spawn(
            send_receive_stream(
                stream,
                addr.clone(),
                signaling_msg_tx,
                msg_out_rx,
                socks.clone(),
                shutdown_rx,
                cleanup,
            )
            .instrument(span),
        );

        Ok(SignalingClient {
            _addr: addr,
            id,
            msg_out_tx,
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
    /// The Sock allows exchanging SDP and ICE candidates with the
    /// peer through the signaling server.
    pub async fn create_sock(&self, peer_id: EndpointId) -> Sock {
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
            sdp_rx,
            msg_out_tx: self.msg_out_tx.clone(),
            candidate_rx,
        }
    }

    /// Remove a Sock from the map.
    pub async fn remove_sock(&self, peer_id: &str) {
        self.sock_tx_map.lock().unwrap().remove(peer_id);
    }
}

impl Sock {
    /// Send SDP to peer through signaling server.
    pub async fn send_sdp(&self, sdp: SDP) -> anyhow::Result<()> {
        let msg = SignalingMsg::SDP {
            sender: self.my_id.clone(),
            receiver: self.peer_id.clone(),
            sdp,
        };
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
        let msg = SignalingMsg::Candidate {
            sender: self.my_id.clone(),
            receiver: self.peer_id.clone(),
            candidate,
        };
        self.msg_out_tx
            .send(msg)
            .await
            .map_err(|_| anyhow!("Signaling channel closed"))
    }

    /// Receive ICE candidate from peer.
    pub async fn recv_candidate(&mut self) -> Option<ICECandidate> {
        self.candidate_rx.recv().await
    }

    /// Get peer's ID.
    pub fn id(&self) -> &str {
        &self.peer_id
    }

    /// Get peer's cert hash, which is the hash portion of
    /// [`Sock::id`].
    pub fn cert_hash(&self) -> &str {
        id_hash(&self.peer_id)
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
        let msg = SignalingMsg::Candidate {
            sender: self.my_id.clone(),
            receiver: self.peer_id.clone(),
            candidate,
        };
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
    stream: tung::WebSocketStream<tung::MaybeTlsStream<tokio::net::TcpStream>>,
    addr: String,
    signaling_msg_tx: mpsc::Sender<crate::signaling::SignalingMessage>,
    mut msg_out_rx: mpsc::Receiver<SignalingMsg>,
    socks: Arc<Mutex<HashMap<EndpointId, SockTx>>>,
    mut shutdown_rx: mpsc::Receiver<()>,
    cleanup: Option<Box<dyn FnOnce() + Send + 'static>>,
) {
    let (mut ws_tx, mut ws_rx) = stream.split();

    // Ping interval for keepalive (9 min, before server's 10 min inactivity timeout).
    let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(540));
    ping_interval.tick().await; // Skip immediate first tick.

    let mut termination_reason = "".to_string();
    loop {
        tokio::select! {
            // Send ping to keep connection alive.
            _ = ping_interval.tick() => {
                let ping_msg = SignalingMsg::Ping;
                let text = serde_json::to_string(&ping_msg).unwrap();
                if let Err(e) = ws_tx.send(tung::tungstenite::Message::Text(text)).await {
                    termination_reason = format!("Failed to send ping: {}", e);
                    tracing::warn!("{}", termination_reason);
                    break;
                }
                tracing::debug!("Sent ping to signaling server");
            }

            // Receive from websocket.
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
                                        &signaling_msg_tx,
                                        &socks,
                                    ).await {
                                        tracing::warn!("Error handling incoming message: {}", e);
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!("Failed to parse signaling message: {}", e);
                                    termination_reason = format!("Parse error: {}", e);
                                    break;
                                }
                            }
                        }
                        Ok(tung::tungstenite::Message::Close(_)) => {
                            tracing::info!("Signaling server closed connection");
                            termination_reason = "Signaling server closed connection".to_string();
                            break;
                        }
                        Err(e) => {
                            tracing::warn!("Websocket error: {}", e);
                            termination_reason = format!("Websocket error: {}", e);
                            break;
                        }
                        _ => {}
                    }
                } else {
                    tracing::info!("Websocket stream closed");
                    termination_reason = "Websocket stream closed".to_string();
                    break;
                }
            }

            // Send to websocket
            msg = msg_out_rx.recv() => {
                if let Some(signaling_msg) = msg {
                    let text = serde_json::to_string(&signaling_msg).unwrap();
                    if let Err(e) = ws_tx.send(tung::tungstenite::Message::Text(text)).await {
                        tracing::warn!("Failed to send to websocket: {}", e);
                        termination_reason = format!("Send error: {}", e);
                        break;
                    }
                } else {
                    tracing::info!("Outgoing message channel closed, shutting down");
                    termination_reason = "Outgoing message channel closed".to_string();
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

    let signaling_message = crate::signaling::SignalingMessage {
        signaling_addr: addr.clone(),
        msg: Err(crate::signaling::AcceptStopped(termination_reason)),
    };
    let _ = signaling_msg_tx.send(signaling_message).await;
}

/// Handle an incoming signaling message and route it appropriately.
async fn handle_incoming_message(
    msg: SignalingMsg,
    addr: &str,
    signaling_msg_tx: &mpsc::Sender<crate::signaling::SignalingMessage>,
    socks: &Arc<Mutex<HashMap<EndpointId, SockTx>>>,
) -> anyhow::Result<()> {
    match &msg {
        SignalingMsg::Trusted { .. } => {
            let signaling_message = crate::signaling::SignalingMessage {
                signaling_addr: addr.to_string(),
                msg: Ok(msg),
            };
            let _ = signaling_msg_tx.send(signaling_message).await;
        }

        SignalingMsg::Connect { .. } => {
            let signaling_message = crate::signaling::SignalingMessage {
                signaling_addr: addr.to_string(),
                msg: Ok(msg),
            };
            let _ = signaling_msg_tx.send(signaling_message).await;
        }

        SignalingMsg::SDP { sender, sdp, .. } => {
            // Route SDP to the appropriate sock.
            let sock_tx = {
                let socks_map = socks.lock().unwrap();
                socks_map.get(sender).map(|tx| tx.sdp_tx.clone())
            };

            if let Some(sdp_tx) = sock_tx {
                if sdp_tx.send(sdp.clone()).await.is_err() {
                    tracing::debug!("Failed to send SDP to peer {}", sender);
                }
            } else {
                tracing::debug!("Received SDP from unknown peer {}, dropping", sender);
            }
        }

        SignalingMsg::Candidate {
            sender, candidate, ..
        } => {
            // Route candidate to the appropriate sock.
            let sock_tx = {
                let socks_map = socks.lock().unwrap();
                socks_map.get(sender).map(|tx| tx.candidate_tx.clone())
            };

            if let Some(candidate_tx) = sock_tx {
                if candidate_tx.send(candidate.clone()).is_err() {
                    tracing::debug!("Failed to send candidate to peer {}", sender);
                }
            } else {
                tracing::debug!("Received candidate from unknown peer {}, dropping", sender);
            }
        }

        SignalingMsg::IdNotFound { .. } | SignalingMsg::Rejected { .. } => {
            // Forward the error to Server.
            let signaling_message = crate::signaling::SignalingMessage {
                signaling_addr: addr.to_string(),
                msg: Ok(msg),
            };
            let _ = signaling_msg_tx.send(signaling_message).await;
        }

        // Heart-beat reply from the Cloudflare worker. Nothing to do.
        SignalingMsg::Pong => {}

        _ => {
            tracing::debug!("Received unexpected signaling message: {:?}", msg);
        }
    }

    Ok(())
}

// ============================================================================
// Test infrastructure
// ============================================================================

/// State for a single endpoint in the test factory.
struct TestEndpointState {
    /// Channel to send signaling messages to Server.
    signaling_msg_tx: mpsc::Sender<crate::signaling::SignalingMessage>,
    /// Endpoints this endpoint is willing to receive from. Mirrors
    /// the production trust enforcement; updated by `bind` and the
    /// Trust handler.
    trusted: std::collections::HashSet<EndpointId>,
    /// Map of active peer socks for routing SDP and candidates.
    sock_tx_map: HashMap<EndpointId, SockTx>,
}

/// Internal state for the test signaling factory
struct TestFactoryState {
    /// Map of (signaling_addr, endpoint_id) -> endpoint state
    endpoints: HashMap<(String, EndpointId), TestEndpointState>,
}

/// Factory for creating test signaling channels with in-memory message passing
pub struct TestSignalingChannelFactory {
    state: Arc<Mutex<TestFactoryState>>,
}

impl TestSignalingChannelFactory {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(TestFactoryState {
                endpoints: HashMap::new(),
            })),
        }
    }

    /// Create a new test signaling channel for an endpoint
    pub fn get_channel(
        self: &Arc<Self>,
        signaling_msg_tx: mpsc::Sender<crate::signaling::SignalingMessage>,
    ) -> TestSignalingChannel {
        TestSignalingChannel {
            signaling_msg_tx,
            factory: self.clone(),
            bound_endpoints: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Route a Connect/SDP/Candidate message from sender to receiver.
    /// Looks up the receiver, runs the trust check (sending `Rejected`
    /// back to the sender on failure), then dispatches the payload:
    ///   - `Connect`   -> receiver's `signaling_msg_tx`
    ///   - `SDP`       -> receiver's `sock_tx_map[sender].sdp_tx`
    ///   - `Candidate` -> receiver's `sock_tx_map[sender].candidate_tx`
    fn route(&self, signaling_addr: &str, msg: SignalingMsg) -> anyhow::Result<()> {
        // Pull sender/receiver out without consuming msg.
        let (sender, receiver) = match &msg {
            SignalingMsg::Connect {
                sender, receiver, ..
            }
            | SignalingMsg::SDP {
                sender, receiver, ..
            }
            | SignalingMsg::Candidate {
                sender, receiver, ..
            } => (sender.clone(), receiver.clone()),
            other => return Err(anyhow!("route: unexpected variant {:?}", other)),
        };

        let state = self.state.lock().unwrap();
        let key = (signaling_addr.to_string(), receiver.clone());
        let endpoint_state = state.endpoints.get(&key).ok_or_else(|| {
            anyhow!(
                "Receiver {} not bound to signaling addr {}",
                receiver,
                signaling_addr
            )
        })?;

        // Trust check. On failure, deliver Rejected back to the sender
        // (matches the real signaling server's behaviour for all three
        // message types).
        if !endpoint_state.trusted.contains(&sender) {
            if let Some(sender_state) = state
                .endpoints
                .get(&(signaling_addr.to_string(), sender.clone()))
            {
                let signaling_message = crate::signaling::SignalingMessage {
                    signaling_addr: signaling_addr.to_string(),
                    msg: Ok(SignalingMsg::Rejected {
                        id: sender.clone(),
                        reason: format!("{} does not trust sender", receiver),
                    }),
                };
                let tx = sender_state.signaling_msg_tx.clone();
                tokio::spawn(async move {
                    let _ = tx.send(signaling_message).await;
                });
            }
            return Ok(());
        }

        match msg {
            SignalingMsg::Connect { .. } => {
                let signaling_message = crate::signaling::SignalingMessage {
                    signaling_addr: signaling_addr.to_string(),
                    msg: Ok(msg),
                };
                let tx = endpoint_state.signaling_msg_tx.clone();
                tokio::spawn(async move {
                    let _ = tx.send(signaling_message).await;
                });
            }
            SignalingMsg::SDP { sdp, .. } => {
                let sock_tx = endpoint_state.sock_tx_map.get(&sender).ok_or_else(|| {
                    anyhow!(
                        "Sock for peer {} not found in receiver {}",
                        sender,
                        receiver
                    )
                })?;
                let tx = sock_tx.sdp_tx.clone();
                tokio::spawn(async move {
                    let _ = tx.send(sdp).await;
                });
            }
            SignalingMsg::Candidate { candidate, .. } => {
                let sock_tx = endpoint_state.sock_tx_map.get(&sender).ok_or_else(|| {
                    anyhow!(
                        "Sock for peer {} not found in receiver {}",
                        sender,
                        receiver
                    )
                })?;
                let _ = sock_tx.candidate_tx.send(candidate);
            }
            _ => unreachable!(),
        }
        Ok(())
    }
}

/// Test signaling channel that uses in-memory message passing.
pub struct TestSignalingChannel {
    signaling_msg_tx: mpsc::Sender<crate::signaling::SignalingMessage>,
    factory: Arc<TestSignalingChannelFactory>,
    /// Tracks bound endpoints: (signaling_addr, endpoint_id).
    bound_endpoints: Arc<Mutex<Vec<(String, EndpointId)>>>,
}

#[async_trait]
impl SignalingChannelTrait for TestSignalingChannel {
    async fn bind(
        &mut self,
        addr: String,
        id: EndpointId,
        _key_cert: ArcKeyCert,
        trusted: Vec<EndpointId>,
    ) -> anyhow::Result<()> {
        let trust_set: std::collections::HashSet<EndpointId> = trusted.iter().cloned().collect();
        // Register this endpoint with the factory.
        {
            let mut state = self.factory.state.lock().unwrap();
            state.endpoints.insert(
                (addr.clone(), id.clone()),
                TestEndpointState {
                    signaling_msg_tx: self.signaling_msg_tx.clone(),
                    trusted: trust_set,
                    sock_tx_map: HashMap::new(),
                },
            );
        }

        // Track this binding in our local state.
        {
            let mut endpoints = self.bound_endpoints.lock().unwrap();
            if !endpoints.iter().any(|(a, i)| a == &addr && i == &id) {
                endpoints.push((addr.clone(), id.clone()));
            }
        }

        // Send a Bound message.
        let msg = SignalingMsg::Bound {
            id: id.clone(),
            trusted,
        };
        let signaling_message = crate::signaling::SignalingMessage {
            signaling_addr: addr,
            msg: Ok(msg),
        };
        self.signaling_msg_tx
            .send(signaling_message)
            .await
            .context("Failed to send Bound message")?;

        Ok(())
    }

    async fn send(&self, signaling_addr: &str, msg: SignalingMsg) -> anyhow::Result<()> {
        match msg {
            SignalingMsg::Connect { .. }
            | SignalingMsg::SDP { .. }
            | SignalingMsg::Candidate { .. } => {
                self.factory.route(signaling_addr, msg)?;
            }
            SignalingMsg::Trust { sender, trusted } => {
                // Update the factory's stored trust list and echo
                // `Trusted` back through `signaling_msg_tx`, mirroring
                // what the real signaling server does.
                let trust_set: std::collections::HashSet<EndpointId> =
                    trusted.iter().cloned().collect();
                {
                    let mut state = self.factory.state.lock().unwrap();
                    if let Some(entry) = state
                        .endpoints
                        .get_mut(&(signaling_addr.to_string(), sender.clone()))
                    {
                        entry.trusted = trust_set;
                    }
                }
                let signaling_message = crate::signaling::SignalingMessage {
                    signaling_addr: signaling_addr.to_string(),
                    msg: Ok(SignalingMsg::Trusted {
                        id: sender,
                        trusted,
                    }),
                };
                self.signaling_msg_tx
                    .send(signaling_message)
                    .await
                    .context("Failed to send Trusted message")?;
            }
            _ => (),
        }
        Ok(())
    }

    async fn create_sock(&self, signaling_addr: &str, peer_id: EndpointId) -> anyhow::Result<Sock> {
        // Get my_id from our bound endpoints
        let my_id = {
            let endpoints = self.bound_endpoints.lock().unwrap();
            endpoints
                .iter()
                .find(|(addr, _id)| addr == signaling_addr)
                .map(|(_addr, id)| id.clone())
                .ok_or_else(|| anyhow!("Not bound to signaling addr {}", signaling_addr))?
        };

        // Create channels for SDP and candidates
        let (sdp_tx, sdp_rx) = mpsc::channel(1);
        let (candidate_tx, candidate_rx) = mpsc::unbounded_channel();

        // Create msg_out channel to route messages through factory
        let (msg_out_tx, mut msg_out_rx) = mpsc::channel::<SignalingMsg>(16);

        // Register the sock in factory state
        {
            let mut state = self.factory.state.lock().unwrap();
            let key = (signaling_addr.to_string(), my_id.clone());
            if let Some(endpoint_state) = state.endpoints.get_mut(&key) {
                endpoint_state.sock_tx_map.insert(
                    peer_id.clone(),
                    SockTx {
                        sdp_tx,
                        candidate_tx,
                    },
                );
            } else {
                return Err(anyhow!("Not bound to signaling addr {}", signaling_addr));
            }
        }

        // Spawn task to route outgoing messages from this sock through factory
        let factory = self.factory.clone();
        let signaling_addr_clone = signaling_addr.to_string();
        tokio::spawn(async move {
            while let Some(msg) = msg_out_rx.recv().await {
                match &msg {
                    SignalingMsg::SDP { .. } | SignalingMsg::Candidate { .. } => {
                        let _ = factory.route(&signaling_addr_clone, msg);
                    }
                    _ => {
                        tracing::warn!("Unexpected message from sock: {:?}", msg);
                    }
                }
            }
        });

        Ok(Sock {
            my_id,
            peer_id,
            sdp_rx,
            msg_out_tx,
            candidate_rx,
        })
    }

    fn remove(&mut self, _signaling_addr: &str) {
        // No-op. Not needed.
    }
}
