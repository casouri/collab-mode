//! New signaling client that tightly couples with the server.
//!
//! `SignalingChannel` is the entry point. It owns a map from address
//! to client actors. In production each bind opens a real WebSocket
//! connection actor ([`RealSignalingClient`]); in tests we route
//! in-process via a shared [`TestFactoryState`] using test actor
//! ([`TestSignalingClient`]).

use crate::config_man::ArcKeyCert;
use crate::signaling::auth::sign_identity;
use crate::signaling::{
    encode_trusted_header, EndpointId, ICECandidate, SignalingMessage, SignalingMsg,
    BIND_HEADER_ID, BIND_HEADER_IDENTITY, BIND_HEADER_SIGNATURE, BIND_HEADER_TRUSTED, SDP,
};
use crate::types::{id_hash, id_short};
use anyhow::{anyhow, Context};
use futures_util::{SinkExt, StreamExt};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite as tung;
use tracing::Instrument;

// *** Signaling channel

/// Manages multiple signaling clients, one per signaling server
/// address. Production uses [`SignalingChannel::new`]; tests use
/// [`SignalingChannel::new_for_test`] with a shared
/// [`TestFactoryState`].
#[derive(Clone)]
pub struct SignalingChannel {
    clients: Arc<Mutex<HashMap<String, ClientHandle>>>,
    signaling_msg_tx: mpsc::Sender<SignalingMessage>,
    test_factory_state: Option<Arc<Mutex<TestFactoryState>>>,
}

impl SignalingChannel {
    /// Create a signaling client that forwards messages from
    /// signaling servers to `signaling_msg_tx`.
    pub fn new(signaling_msg_tx: mpsc::Sender<SignalingMessage>) -> Self {
        Self {
            clients: Arc::new(Mutex::new(HashMap::new())),
            signaling_msg_tx,
            test_factory_state: None,
        }
    }

    /// Test-only constructor. All `SignalingChannel`s built under the
    /// same `factory_state` route messages to each other in-memory.
    pub fn new_for_test(
        signaling_msg_tx: mpsc::Sender<SignalingMessage>,
        factory_state: Arc<Mutex<TestFactoryState>>,
    ) -> Self {
        Self {
            clients: Arc::new(Mutex::new(HashMap::new())),
            signaling_msg_tx,
            test_factory_state: Some(factory_state),
        }
    }

    /// Bind to a signaling server. No-op if already bound to the same
    /// address.
    pub async fn bind(
        &self,
        addr: String,
        id: EndpointId,
        key_cert: ArcKeyCert,
        trusted: Vec<EndpointId>,
    ) -> anyhow::Result<()> {
        if self.clients.lock().unwrap().contains_key(&addr) {
            return Ok(());
        }
        if let Some(factory_state) = self.test_factory_state.clone() {
            self.bind_test(addr, id, factory_state, trusted).await
        } else {
            self.bind_real(addr, id, key_cert, trusted).await
        }
    }

    /// Send a signaling message to `signaling_addr`.
    pub async fn send(&self, signaling_addr: &str, msg: SignalingMsg) -> anyhow::Result<()> {
        let tx = self.handle_tx(signaling_addr)?;
        tx.send(Command::Send(msg))
            .await
            .map_err(|_| anyhow!("Signaling client {} channel closed", signaling_addr))
    }

    /// Create a [`Sock`] for communicating with `peer_id` through the
    /// client bound to `signaling_addr`.
    pub async fn create_sock(
        &self,
        signaling_addr: &str,
        peer_id: EndpointId,
    ) -> anyhow::Result<Sock> {
        let tx = self.handle_tx(signaling_addr)?;
        let (reply_tx, reply_rx) = oneshot::channel();
        tx.send(Command::CreateSock {
            peer_id,
            reply: reply_tx,
        })
        .await
        .map_err(|_| anyhow!("Signaling client {} channel closed", signaling_addr))?;
        reply_rx
            .await
            .map_err(|_| anyhow!("Signaling client {} dropped before reply", signaling_addr))
    }

    /// Remove a signaling client.
    pub fn remove(&self, signaling_addr: &str) {
        let handle = self.clients.lock().unwrap().remove(signaling_addr);
        if let Some(handle) = handle {
            tokio::spawn(async move {
                let (reply_tx, _reply_rx) = oneshot::channel();
                let _ = handle.msg_tx.send(Command::Shutdown(reply_tx)).await;
            });
        }
    }

    fn handle_tx(&self, signaling_addr: &str) -> anyhow::Result<mpsc::Sender<Command>> {
        self.clients
            .lock()
            .unwrap()
            .get(signaling_addr)
            .map(|h| h.msg_tx.clone())
            .ok_or_else(|| anyhow!("No signaling client for address {}", signaling_addr))
    }

    async fn bind_real(
        &self,
        addr: String,
        id: EndpointId,
        my_key_cert: ArcKeyCert,
        trusted: Vec<EndpointId>,
    ) -> anyhow::Result<()> {
        // Cert-hash sanity check.
        let cert_hash = my_key_cert.cert_der_hash();
        if id_hash(&id) != cert_hash {
            return Err(anyhow!(
                "Endpoint id hash {} doesn't match our cert hash {}",
                id_hash(&id),
                cert_hash
            ));
        }

        // Build the bind headers and upgrade.
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

        let _ = self
            .signaling_msg_tx
            .send(SignalingMessage {
                signaling_addr: addr.clone(),
                msg: Ok(SignalingMsg::Bound {
                    id: id.clone(),
                    trusted: trusted.clone(),
                }),
            })
            .await;

        // Create the actor that manages the connection.
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(16);
        let clients_for_cleanup = self.clients.clone();
        let addr_for_cleanup = addr.clone();
        let cleanup = Box::new(move || {
            clients_for_cleanup
                .lock()
                .unwrap()
                .remove(&addr_for_cleanup);
        });

        let actor = RealSignalingClient {
            id: id.clone(),
            addr: addr.clone(),
            stream: Some(stream),
            signaling_msg_tx: self.signaling_msg_tx.clone(),
            cmd_rx,
            cmd_tx: cmd_tx.clone(),
            socks: HashMap::new(),
            cleanup: Some(cleanup),
        };

        let span = tracing::info_span!(
            "RealSignalingClient",
            endpoint = %id,
            signaling_addr = %addr
        );
        tokio::spawn(actor.run().instrument(span));

        self.clients
            .lock()
            .unwrap()
            .insert(addr, ClientHandle { msg_tx: cmd_tx });
        Ok(())
    }

    async fn bind_test(
        &self,
        addr: String,
        id: EndpointId,
        factory_state: Arc<Mutex<TestFactoryState>>,
        trusted: Vec<EndpointId>,
    ) -> anyhow::Result<()> {
        // Register endpoint in factory state.
        {
            let mut state = factory_state.lock().unwrap();
            state.endpoints.insert(
                (addr.clone(), id.clone()),
                TestEndpointState {
                    signaling_msg_tx: self.signaling_msg_tx.clone(),
                    trusted: trusted.iter().cloned().collect(),
                    sock_tx_map: HashMap::new(),
                },
            );
        }

        // Send Bound.
        self.signaling_msg_tx
            .send(SignalingMessage {
                signaling_addr: addr.clone(),
                msg: Ok(SignalingMsg::Bound {
                    id: id.clone(),
                    trusted: trusted.clone(),
                }),
            })
            .await
            .context("Failed to send Bound message")?;

        // Spawn the actor.
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(16);
        let actor = TestSignalingClient {
            id: id.clone(),
            addr: addr.clone(),
            signaling_msg_tx: self.signaling_msg_tx.clone(),
            factory_state,
            cmd_rx,
            cmd_tx_for_self: cmd_tx.clone(),
        };
        tokio::spawn(actor.run());

        self.clients
            .lock()
            .unwrap()
            .insert(addr, ClientHandle { msg_tx: cmd_tx });
        Ok(())
    }
}

// *** Sock

/// Socket for exchanging SDP and ICE candidates with a peer through
/// the signaling server.
pub struct Sock {
    my_id: EndpointId,
    peer_id: EndpointId,
    sdp_rx: mpsc::Receiver<SDP>,
    candidate_rx: mpsc::Receiver<ICECandidate>,
    // tx to the actor managing the connection.
    cmd_tx: mpsc::Sender<Command>,
}

impl Sock {
    /// Send SDP to peer.
    pub async fn send_sdp(&self, sdp: SDP) -> anyhow::Result<()> {
        let msg = SignalingMsg::SDP {
            sender: self.my_id.clone(),
            receiver: self.peer_id.clone(),
            sdp,
        };
        self.cmd_tx
            .send(Command::Send(msg))
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
        self.cmd_tx
            .send(Command::Send(msg))
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
            cmd_tx: self.cmd_tx.clone(),
        }
    }
}

/// Helper for sending candidates without holding a reference to Sock.
#[derive(Clone)]
pub struct CandidateSender {
    my_id: EndpointId,
    peer_id: EndpointId,
    cmd_tx: mpsc::Sender<Command>,
}

impl CandidateSender {
    /// Send ICE candidate to peer.
    pub async fn send_candidate(&mut self, candidate: ICECandidate) -> anyhow::Result<()> {
        let msg = SignalingMsg::Candidate {
            sender: self.my_id.clone(),
            receiver: self.peer_id.clone(),
            candidate,
        };
        self.cmd_tx
            .send(Command::Send(msg))
            .await
            .map_err(|_| anyhow!("Signaling channel closed"))
    }
}

// *** Actor

/// Stored in `SignalingChannel::clients`, one per bound address.
struct ClientHandle {
    msg_tx: mpsc::Sender<Command>,
}

/// Commands a client actor accepts.
enum Command {
    /// Send a signaling message.
    Send(SignalingMsg),
    /// Create a `Sock`, the sock is sent to the `reply` tx.
    CreateSock {
        peer_id: EndpointId,
        reply: oneshot::Sender<Sock>,
    },
    /// Shutdown the connection. Send () to the tx once cleanup is done.
    Shutdown(oneshot::Sender<()>),
}

/// State for each sock in an actor. (Actor maintains a map that maps
/// peer id to this state.)
struct SockTx {
    sdp_tx: mpsc::Sender<SDP>,
    candidate_tx: mpsc::Sender<ICECandidate>,
}

struct RealSignalingClient {
    id: EndpointId,
    /// Address of the signaling server we’re connecting to.
    addr: String,
    /// Taken inside `run`; wrap in Option so the actor can be moved
    /// without re-borrowing.
    stream: Option<tung::WebSocketStream<tung::MaybeTlsStream<tokio::net::TcpStream>>>,
    /// Messages from signaling server are forwarded to this tx.
    signaling_msg_tx: mpsc::Sender<SignalingMessage>,
    /// Receives commands from this rx.
    cmd_rx: mpsc::Receiver<Command>,
    /// Cloned for each Sock created.
    cmd_tx: mpsc::Sender<Command>,
    /// Maintains tx for socks for forwarding candidate/SDP.
    socks: HashMap<EndpointId, SockTx>,
    /// Removes this client from the parent map when the actor exits.
    cleanup: Option<Box<dyn FnOnce() + Send + 'static>>,
}

impl RealSignalingClient {
    /// Run the actor in a loop and handle messages.
    async fn run(mut self) {
        let stream = self.stream.take().expect("run called twice");
        let (mut ws_tx, mut ws_rx) = stream.split();

        // Ping interval for keepalive (9 min, before server's 10 min
        // inactivity timeout).
        let mut ping_interval = tokio::time::interval(Duration::from_secs(540));
        ping_interval.tick().await; // Skip first tick.

        let mut termination = String::new();
        let mut shutdown_reply: Option<oneshot::Sender<()>> = None;

        loop {
            tokio::select! {
                _ = ping_interval.tick() => {
                    let ping_msg = SignalingMsg::Ping;
                    let text = serde_json::to_string(&ping_msg).unwrap();
                    if let Err(e) = ws_tx.send(tung::tungstenite::Message::Text(text)).await {
                        termination = format!("Failed to send ping: {}", e);
                        tracing::warn!("{}", termination);
                        break;
                    }
                    tracing::debug!("Sent ping to signaling server");
                }

                msg = ws_rx.next() => {
                    let Some(msg_result) = msg else {
                        tracing::info!("Websocket stream closed");
                        termination = "Websocket stream closed".to_string();
                        break;
                    };
                    match msg_result {
                        Ok(tung::tungstenite::Message::Text(text)) => {
                            match serde_json::from_str::<SignalingMsg>(&text) {
                                Ok(signaling_msg) => {
                                    self.handle_incoming(signaling_msg).await;
                                }
                                Err(e) => {
                                    tracing::warn!("Failed to parse signaling message: {}", e);
                                    termination = format!("Parse error: {}", e);
                                    break;
                                }
                            }
                        }
                        Ok(tung::tungstenite::Message::Close(_)) => {
                            tracing::info!("Signaling server closed connection");
                            termination = "Signaling server closed connection".to_string();
                            break;
                        }
                        Err(e) => {
                            tracing::warn!("Websocket error: {}", e);
                            termination = format!("Websocket error: {}", e);
                            break;
                        }
                        _ => {}
                    }
                }

                cmd = self.cmd_rx.recv() => {
                    let Some(cmd) = cmd else {
                        termination = "Command channel closed".to_string();
                        break;
                    };
                    match cmd {
                        Command::Send(msg) => {
                            let text = serde_json::to_string(&msg).unwrap();
                            if let Err(e) = ws_tx.send(tung::tungstenite::Message::Text(text)).await {
                                tracing::warn!("Failed to send to websocket: {}", e);
                                termination = format!("Send error: {}", e);
                                break;
                            }
                        }
                        Command::CreateSock { peer_id, reply } => {
                            let (sdp_tx, sdp_rx) = mpsc::channel(1);
                            let (candidate_tx, candidate_rx) = mpsc::channel(100);
                            self.socks.insert(
                                peer_id.clone(),
                                SockTx { sdp_tx, candidate_tx },
                            );
                            let sock = Sock {
                                my_id: self.id.clone(),
                                peer_id,
                                sdp_rx,
                                candidate_rx,
                                cmd_tx: self.cmd_tx.clone(),
                            };
                            let _ = reply.send(sock);
                        }
                        Command::Shutdown(reply) => {
                            shutdown_reply = Some(reply);
                            break;
                        }
                    }
                }
            }
        }

        // Cleanup: drop sock channels, run the cleanup function,
        // notify the server.
        self.socks.clear();
        tracing::info!("RealSignalingClient for {} exiting", self.addr);
        if let Some(cleanup) = self.cleanup.take() {
            cleanup();
        }
        let _ = self
            .signaling_msg_tx
            .send(SignalingMessage {
                signaling_addr: self.addr.clone(),
                msg: Err(crate::signaling::AcceptStopped(termination)),
            })
            .await;
        if let Some(reply) = shutdown_reply {
            let _ = reply.send(());
        }
    }

    /// Handle incoming message from signaling server.
    async fn handle_incoming(&mut self, msg: SignalingMsg) {
        match &msg {
            SignalingMsg::Trusted { .. }
            | SignalingMsg::Connect { .. }
            | SignalingMsg::IdNotFound { .. }
            | SignalingMsg::Rejected { .. } => {
                let _ = self
                    .signaling_msg_tx
                    .send(SignalingMessage {
                        signaling_addr: self.addr.clone(),
                        msg: Ok(msg),
                    })
                    .await;
            }
            SignalingMsg::SDP { sender, sdp, .. } => {
                if let Some(sock_tx) = self.socks.get(sender) {
                    if sock_tx.sdp_tx.send(sdp.clone()).await.is_err() {
                        tracing::debug!("Failed to send SDP to peer {}", id_short(sender));
                    }
                } else {
                    tracing::debug!(
                        "Received SDP from unknown peer {}, dropping",
                        id_short(sender)
                    );
                }
            }
            SignalingMsg::Candidate {
                sender, candidate, ..
            } => {
                if let Some(sock_tx) = self.socks.get(sender) {
                    if sock_tx.candidate_tx.send(candidate.clone()).await.is_err() {
                        tracing::debug!("Failed to send candidate to peer {}", id_short(sender));
                    }
                } else {
                    tracing::debug!(
                        "Received candidate from unknown peer {}, dropping",
                        id_short(sender)
                    );
                }
            }
            // Heart-beat reply from the Cloudflare worker.
            SignalingMsg::Pong => {}
            _ => {
                tracing::debug!("Received unexpected signaling message: {:?}", msg);
            }
        }
    }
}

// *** Test

/// State for a single endpoint in the test factory.
struct TestEndpointState {
    signaling_msg_tx: mpsc::Sender<SignalingMessage>,
    /// Endpoints this endpoint is willing to receive from. Mirrors
    /// the production trust enforcement; updated by `bind` and the
    /// `Trust` handler.
    trusted: HashSet<EndpointId>,
    /// Map of active peer socks for routing SDP and candidates.
    sock_tx_map: HashMap<EndpointId, SockTx>,
}

/// Shared state for in-process signaling routing in tests. All
/// `SignalingChannel`s built under the same `TestFactoryState` route
/// to each other.
#[derive(Default)]
pub struct TestFactoryState {
    /// Map of (signaling_addr, endpoint_id) -> endpoint state.
    endpoints: HashMap<(String, EndpointId), TestEndpointState>,
}

impl TestFactoryState {
    /// Route a Connect/SDP/Candidate message from sender to receiver.
    /// Looks up the receiver, runs the trust check (sending
    /// `Rejected` back to the sender on failure), then dispatches:
    ///   - `Connect`   -> receiver's `signaling_msg_tx`
    ///   - `SDP`       -> receiver's `sock_tx_map[sender].sdp_tx`
    ///   - `Candidate` -> receiver's `sock_tx_map[sender].candidate_tx`
    fn route(
        state: &Arc<Mutex<TestFactoryState>>,
        signaling_addr: &str,
        msg: SignalingMsg,
    ) -> anyhow::Result<()> {
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

        let s = state.lock().unwrap();
        let key = (signaling_addr.to_string(), receiver.clone());
        let endpoint_state = s.endpoints.get(&key).ok_or_else(|| {
            anyhow!(
                "Receiver {} not bound to signaling addr {}",
                receiver,
                signaling_addr
            )
        })?;

        // Trust check. On failure, deliver Rejected back to the
        // sender (matches the real signaling server's behaviour for
        // all three message types).
        if !endpoint_state.trusted.contains(&sender) {
            if let Some(sender_state) = s
                .endpoints
                .get(&(signaling_addr.to_string(), sender.clone()))
            {
                let signaling_message = SignalingMessage {
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
                let signaling_message = SignalingMessage {
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
                let tx = sock_tx.candidate_tx.clone();
                tokio::spawn(async move {
                    let _ = tx.send(candidate).await;
                });
            }
            _ => unreachable!(),
        }
        Ok(())
    }
}

struct TestSignalingClient {
    id: EndpointId,
    addr: String,
    signaling_msg_tx: mpsc::Sender<SignalingMessage>,
    factory_state: Arc<Mutex<TestFactoryState>>,
    cmd_rx: mpsc::Receiver<Command>,
    cmd_tx_for_self: mpsc::Sender<Command>,
}

impl TestSignalingClient {
    async fn run(mut self) {
        let mut shutdown_reply: Option<oneshot::Sender<()>> = None;
        while let Some(cmd) = self.cmd_rx.recv().await {
            match cmd {
                Command::Send(msg) => self.handle_send(msg).await,
                Command::CreateSock { peer_id, reply } => {
                    let sock = self.create_sock(peer_id);
                    let _ = reply.send(sock);
                }
                Command::Shutdown(reply) => {
                    shutdown_reply = Some(reply);
                    break;
                }
            }
        }

        // Cleanup: remove our endpoint registration.
        self.factory_state
            .lock()
            .unwrap()
            .endpoints
            .remove(&(self.addr.clone(), self.id.clone()));

        if let Some(reply) = shutdown_reply {
            let _ = reply.send(());
        }
    }

    async fn handle_send(&self, msg: SignalingMsg) {
        match msg {
            SignalingMsg::Connect { .. }
            | SignalingMsg::SDP { .. }
            | SignalingMsg::Candidate { .. } => {
                if let Err(e) = TestFactoryState::route(&self.factory_state, &self.addr, msg) {
                    tracing::debug!("Test signaling route failed: {}", e);
                }
            }
            SignalingMsg::Trust { sender, trusted } => {
                // Update stored trust list and echo `Trusted` back,
                // mirroring the real signaling server.
                let trust_set: HashSet<EndpointId> = trusted.iter().cloned().collect();
                {
                    let mut state = self.factory_state.lock().unwrap();
                    if let Some(entry) = state
                        .endpoints
                        .get_mut(&(self.addr.clone(), sender.clone()))
                    {
                        entry.trusted = trust_set;
                    }
                }
                let signaling_message = SignalingMessage {
                    signaling_addr: self.addr.clone(),
                    msg: Ok(SignalingMsg::Trusted {
                        id: sender,
                        trusted,
                    }),
                };
                let _ = self.signaling_msg_tx.send(signaling_message).await;
            }
            _ => {}
        }
    }

    fn create_sock(&self, peer_id: EndpointId) -> Sock {
        let (sdp_tx, sdp_rx) = mpsc::channel(1);
        let (candidate_tx, candidate_rx) = mpsc::channel(100);

        // Register sock in factory state under our own endpoint key.
        {
            let mut state = self.factory_state.lock().unwrap();
            if let Some(endpoint_state) = state
                .endpoints
                .get_mut(&(self.addr.clone(), self.id.clone()))
            {
                endpoint_state.sock_tx_map.insert(
                    peer_id.clone(),
                    SockTx {
                        sdp_tx,
                        candidate_tx,
                    },
                );
            }
        }

        Sock {
            my_id: self.id.clone(),
            peer_id,
            sdp_rx,
            candidate_rx,
            cmd_tx: self.cmd_tx_for_self.clone(),
        }
    }
}

// *** Helper functions

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
