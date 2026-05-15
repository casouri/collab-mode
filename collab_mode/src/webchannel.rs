//! The next iteration after webrpc. We forgo the request-response
//! abstraction, and lump everything into a single channel. In fact,
//! multiple remote connections are lumped into a single channel. We
//! also don’t do chunking anymore, instead we create a SCTP stream for
//! every request.
//!
//! We could’ve defined message to take a impl Serialize +
//! Deserialize, but there’s really no need for it. So we just make
//! it take a Msg, it’s convenient and simple.
//!
//! When connection breaks, webchannel cleans itself up and sends a
//! ConnectionBroke message to the main message channel.

use crate::message::{HeyMessage, Msg};
use crate::{
    config_man::{hash_der, ConfigProject},
    signaling::CertDerHash,
    types::*,
};
use anyhow::anyhow;
use async_trait::async_trait;
use lsp_server::RequestId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use webrtc_dtls::conn::DTLSConn;
use webrtc_ice::state::ConnectionState;
use webrtc_sctp::chunk::chunk_payload_data::PayloadProtocolIdentifier;
use webrtc_sctp::{association, stream};
use webrtc_util::Conn;

const STREAM_ID_CLIENT_INIT: u16 = 1; // Client uses odd stream IDs: 1, 3, 5, ...
const STREAM_ID_SERVER_INIT: u16 = 2; // Server uses even stream IDs: 2, 4, 6, ...

// Dedicated bidirectional SCTP stream for heartbeat pings. Both sides
// `open_stream(PING_STREAM_ID)` at the top of `SctpRemote::run`; SCTP
// resolves the duplicate open to a single bidirectional stream. Each
// side writes raw bytes (no framing, no serde); receipt of any byte is
// a liveness signal.
const PING_STREAM_ID: u16 = 0;
const PING_INTERVAL_SECS: u64 = 10;
const PING_TIMEOUT_SECS: u64 = 30;
const PING_CHECK_INTERVAL: Duration = Duration::from_secs(3);

/// Unix-epoch seconds. Timebase for the ping liveness counters so we
/// can stuff them into an `AtomicU64`.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// Only this condition needs special handling.
#[derive(Debug, Clone, Error, Serialize, Deserialize)]
pub enum WebChannelError {
    #[error("Connection to {0} already exists")]
    ConnectionExists(ServerId),
}

/// Determine which side uses odd stream IDs based on host ID ordering.
///
/// The host with lexicographically smaller ID uses odd stream IDs (1, 3, 5...)
/// The host with larger ID uses even stream IDs (2, 4, 6...)
///
/// This ensures both sides agree on stream ID parity without negotiation.
fn stream_id_init(my_id: &str, peer_id: &str) -> u16 {
    if my_id < peer_id {
        STREAM_ID_CLIENT_INIT // 1 (odd)
    } else {
        STREAM_ID_SERVER_INIT // 2 (even)
    }
}

/// Carries the transport-specific data needed to establish a connection
/// to a peer. Each [`MsgChannel`] implementation handles the variants it
/// supports and errors on the rest.
pub enum Transport {
    /// Connect via WebRTC (SCTP/DTLS/ICE) using a [`Sock`] from the
    /// signaling client.
    ///
    /// [`Sock`]: crate::signaling::client_new::Sock
    Sock(crate::signaling::client_new::Sock),
    /// Connect by spawning ssh to `ssh_host` and running `command` on
    /// the remote, using its stdio as the data channel. `comamnd`
    /// should end up running `collab-mode envoy`. Each segment is
    /// shell-escaped.
    Ssh {
        ssh_host: String,
        command: Vec<String>,
    },
    /// Connect over a pre-established byte stream. Used by the envoy
    /// (stdio) and as the underlying carrier for `Ssh`.
    Io(crate::io_channel::ReaderWriter),
    /// Used by in-memory tests; no real transport behind it.
    Test,
    /// Placeholder used by older tests where the channel implementation
    /// ignores the transport entirely (e.g. `TestWebChannel`).
    Dummy,
}

/// Trait for message channel operations. Provides async methods for
/// sending, connecting, and broadcasting messages.
#[async_trait]
pub trait MsgChannel: Send + Sync {
    /// Send a message to a remote host. Doesn't block.
    async fn send(
        &self,
        recipient: &ServerId,
        req_id: Option<RequestId>,
        msg: Msg,
    ) -> anyhow::Result<()>;

    /// Connect to a peer using the given [`Transport`]. Each implementation
    /// handles the variants it supports and returns an error for others.
    async fn connect(
        &self,
        remote_id: ServerId,
        transport: Transport,
        my_key_cert: ArcKeyCert,
    ) -> anyhow::Result<()>;

    #[allow(dead_code)]
    /// Broadcasts a message to all connected peers. Doesn't block.
    async fn broadcast(&self, req_id: Option<RequestId>, msg: Msg) -> anyhow::Result<()>;

    /// Disconnect from a peer, or give up establishing connections.
    /// If the peer doesn’t exist, no error is signaled.
    fn disconnect(&self, peer: &ServerId);

    /// Flush any pending outgoing messages to the wire and tear down
    /// all peer connections. This can potentially hang so caller must
    /// have a timeout around it.
    async fn shutdown(&self) -> anyhow::Result<()>;
}

/// Types of transport.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all_fields = "camelCase")]
pub enum TransportConfig {
    /// Connect via ICE + SCTP through the signaling server at
    /// `signaling_addr`.
    SCTP { signaling_addr: String },
    /// Connect by spawning ssh to `ssh_host` and running `command`.
    /// `command[0]` is the program, the rest are args. `projects` is
    /// the set of projects the host wants the envoy to expose; the
    /// envoy adopts these in `init_for_envoy`.
    SshStdio {
        ssh_host: String,
        command: Vec<String>,
        projects: Vec<ConfigProject>,
    },
}

impl TransportConfig {
    /// Returns the signaling address for SCTP transports, or `None` for
    /// SSH (which doesn't use a signaling server).
    pub fn signaling_addr(&self) -> Option<&str> {
        match self {
            TransportConfig::SCTP { signaling_addr } => Some(signaling_addr),
            TransportConfig::SshStdio { .. } => None,
        }
    }
}

// We don’t have to split into three categories, but this should make
// it easier to manage.
#[derive(Debug, Clone, Serialize, Deserialize, fmt_derive::Display)]
pub struct Message {
    pub host: ServerId,
    pub body: Msg,
    pub req_id: Option<RequestId>,
}

/// A receiver that combines a bounded channel for remote messages and
/// an unbounded channel for self-messages. If we don’t use a separate
/// loopback channel, sending a message to ourselves will block the
/// main loop in server.rs. I don’t want to give remote messages a
/// unbounded channel either. So two channels it is. Technically the
/// loopback channel doesn’t need to be unbounded if we try_send to
/// self, but in most cases unbounded channel should be fine.
pub struct DualReceiver {
    remote_rx: mpsc::Receiver<Message>,
    loopback_rx: mpsc::UnboundedReceiver<Message>,
}

impl DualReceiver {
    /// Create a new dual receiver. Returns the dual receiver, and the
    /// remote and self-message tx.
    pub fn new() -> (Self, mpsc::Sender<Message>, mpsc::UnboundedSender<Message>) {
        let (remote_tx, remote_rx) = mpsc::channel::<Message>(1);
        let (loopback_tx, loopback_rx) = mpsc::unbounded_channel::<Message>();
        (
            Self {
                remote_rx,
                loopback_rx,
            },
            remote_tx,
            loopback_tx,
        )
    }

    /// Receive from either channels.
    pub async fn recv(&mut self) -> Option<Message> {
        tokio::select! {
            msg = self.remote_rx.recv() => msg,
            msg = self.loopback_rx.recv() => msg,
        }
    }
}

/// In-band command sent over the per-peer mpsc.
///
/// `Msg` carries a regular outgoing message. `Shutdown` asks the actor
/// to tear down its SCTP/DTLS/ICE state and reply on the oneshot when
/// cleanup finishes (so callers can join the teardown).
pub enum Command {
    Msg(Message),
    Shutdown(oneshot::Sender<anyhow::Result<()>>),
}

/// Handle stored in [`WebChannel::remote_map`]. Cloneable.
#[derive(Clone)]
pub struct RemoteHandle {
    msg_tx: mpsc::Sender<Command>,
}

impl RemoteHandle {
    /// A handle is “dead” once its actor has dropped the receiver
    /// (connection broke or shutdown). The map keeps dead handles
    /// around until the next `send` (which surfaces a
    /// `ConnectionBroke`) or `connect` (which replaces). But no
    /// worries because normally whenever the connection is broken,
    /// main loop receives ConnectionBroke and will try to reconnect.
    fn is_dead(&self) -> bool {
        self.msg_tx.is_closed()
    }
}

#[derive(Clone)]
pub struct WebChannel {
    my_hostid: ServerId,
    /// Main mailbox the server reads from. Actors push deserialized
    /// peer messages, `ConnectionBroke`, `SerializationErr`, and
    /// `IceProgress` onto this.
    remote_msg_tx: mpsc::Sender<Message>,
    /// Side channel for self-addressed messages so they don’t fight the
    /// bounded `remote_msg_tx` for capacity.
    loopback_tx: mpsc::UnboundedSender<Message>,
    /// One entry per peer (live or recently-dead).
    remote_map: Arc<Mutex<HashMap<ServerId, RemoteHandle>>>,
}

impl WebChannel {
    pub fn new(
        my_hostid: ServerId,
        remote_msg_tx: mpsc::Sender<Message>,
        loopback_tx: mpsc::UnboundedSender<Message>,
    ) -> Self {
        Self {
            my_hostid,
            remote_msg_tx,
            loopback_tx,
            remote_map: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn hostid(&self) -> ServerId {
        self.my_hostid.clone()
    }

    /// Top-level dispatcher. Only `Sock` is wired up; the other
    /// transports will get their own `*Remote` actors later.
    pub async fn connect(
        &self,
        _remote_id: ServerId,
        transport: Transport,
        my_key_cert: ArcKeyCert,
    ) -> anyhow::Result<()> {
        match transport {
            Transport::Sock(sock) => self.connect_with_sock(sock, my_key_cert).await,
            Transport::Ssh { .. } => Err(anyhow!("WebChannel does not handle Ssh transports")),
            Transport::Io(_) => Err(anyhow!("WebChannel does not handle Io transports yet")),
            Transport::Test => Err(anyhow!("WebChannel does not handle Test transport yet")),
            Transport::Dummy => Err(anyhow!("WebChannel does not handle Dummy transport")),
        }
    }

    /// Connect using an existing Sock from SignalingClient.
    ///
    /// This is the unified connection method that works for both
    /// initiating and accepting connections. Stream ID parity is
    /// determined by host ID ordering rather than initiator/acceptor roles.
    pub async fn connect_with_sock(
        &self,
        sock: crate::signaling::client_new::Sock,
        my_key_cert: ArcKeyCert,
    ) -> anyhow::Result<()> {
        let peer_id = sock.id().to_string();

        // Stream-ID parity comes from host-ID ordering, not who dialed.
        let stream_id = stream_id_init(&self.my_hostid, &peer_id);
        let as_server = stream_id == STREAM_ID_SERVER_INIT;

        tracing::info!(
            "Connecting with sock: my_id={}, peer_id={}, init_stream_id={}",
            self.my_hostid,
            peer_id,
            stream_id
        );

        // Reserve the map slot up front. A live handle wins; a dead
        // one gets evicted. We never concurrently connect but good to
        // stay clean.
        let rx = {
            let mut map = self.remote_map.lock().unwrap();
            if let Some(h) = map.get(&peer_id) {
                if !h.is_dead() {
                    return Err(anyhow!(WebChannelError::ConnectionExists(peer_id)));
                }
                map.remove(&peer_id);
            }
            let (tx, rx) = mpsc::channel::<Command>(16);
            map.insert(peer_id.clone(), RemoteHandle { msg_tx: tx });
            rx
        };

        // Establish the wire. If anything below fails, evict our slot
        // and propagate.
        let setup = self
            .establish_sctp(sock, my_key_cert, as_server, &peer_id)
            .await;
        let (sctp_assoc, dtls_conn, ice_agent) = match setup {
            Ok(t) => t,
            Err(e) => {
                self.remote_map.lock().unwrap().remove(&peer_id);
                return Err(e);
            }
        };

        let remote = SctpRemote::new(
            peer_id,
            sctp_assoc,
            dtls_conn,
            ice_agent,
            stream_id,
            self.remote_msg_tx.clone(),
            rx,
        );
        tokio::spawn(remote.run());
        Ok(())
    }

    /// ICE/DTLS/SCTP establishment.
    async fn establish_sctp(
        &self,
        sock: crate::signaling::client_new::Sock,
        my_key_cert: ArcKeyCert,
        as_server: bool,
        peer_id: &ServerId,
    ) -> anyhow::Result<(
        Arc<association::Association>,
        Arc<DTLSConn>,
        Arc<webrtc_ice::agent::Agent>,
    )> {
        // Forward ICE progress to the server as IceProgress messages.
        // Exits when progress_tx drops (i.e. ICE finishes / fails).
        let (progress_tx, mut progress_rx) = mpsc::channel::<ConnectionState>(1);
        let progress_msg_tx = self.remote_msg_tx.clone();
        let my_id = self.my_hostid.clone();
        let peer_for_progress = peer_id.clone();
        tokio::spawn(async move {
            while let Some(progress) = progress_rx.recv().await {
                tracing::info!("ICE progress: {progress}");
                let _ = progress_msg_tx
                    .send(Message {
                        host: my_id.clone(),
                        body: Msg::IceProgress(peer_for_progress.clone(), progress.to_string()),
                        req_id: None,
                    })
                    .await;
            }
        });

        let (conn, their_cert, ice_agent) =
            crate::ice::ice_connect_with_sock(sock, Some(progress_tx), as_server).await?;
        let dtls_conn = if as_server {
            create_dtls_server(conn, my_key_cert, their_cert).await?
        } else {
            create_dtls_client(conn, my_key_cert, their_cert).await?
        };
        let sctp_assoc = if as_server {
            create_sctp_server(dtls_conn.clone()).await?
        } else {
            create_sctp_client(dtls_conn.clone()).await?
        };

        Ok((sctp_assoc, dtls_conn, ice_agent))
    }

    /// Send a message to a remote host. Doesn't block.
    pub async fn send(
        &self,
        recipient: &ServerId,
        req_id: Option<RequestId>,
        msg: Msg,
    ) -> anyhow::Result<()> {
        let message = Message {
            host: self.my_hostid.clone(),
            body: msg,
            req_id,
        };

        if recipient == &self.my_hostid {
            // Send to self via the unbounded channel.
            if let Err(err) = self.loopback_tx.send(message) {
                tracing::error!("Failed to send message to ourselves: {:?}", err);
            }
            return Ok(());
        }

        let tx = self
            .remote_map
            .lock()
            .unwrap()
            .get(recipient)
            .map(|h| h.msg_tx.clone())
            .ok_or_else(|| anyhow!("Not connected"))?;

        if tx.send(Command::Msg(message)).await.is_err() {
            // Leave the dead handle in the map, next connect() will
            // replace it. Don’t even send a ConnectionBroke message,
            // because the remote itself must have sent one already
            // when it broke.
            return Err(anyhow!("Channel broke"));
        }
        Ok(())
    }

    #[allow(dead_code)]
    /// Broadcasts a message to all connected peers. Doesn't block.
    pub async fn broadcast(&self, req_id: Option<RequestId>, msg: Msg) -> anyhow::Result<()> {
        let message = Message {
            host: self.my_hostid.clone(),
            body: msg,
            req_id,
        };

        // Clone the txs out so we don’t hold the lock across awaits.
        let peers: Vec<_> = self
            .remote_map
            .lock()
            .unwrap()
            .iter()
            .map(|(id, h)| (id.clone(), h.msg_tx.clone()))
            .collect();

        let mut errs = Vec::new();
        for (peer, tx) in peers {
            if tx.send(Command::Msg(message.clone())).await.is_err() {
                let _ = self
                    .remote_msg_tx
                    .send(Message {
                        host: peer.clone(),
                        body: Msg::ConnectionBroke(peer.clone()),
                        req_id: None,
                    })
                    .await;
                errs.push(peer);
            }
        }
        if !errs.is_empty() {
            return Err(anyhow!("broadcast failed for {} peer(s)", errs.len()));
        }
        Ok(())
    }

    pub fn disconnect(&self, peer: &ServerId) {
        let handle = self.remote_map.lock().unwrap().remove(peer);
        if let Some(handle) = handle {
            // Fire-and-forget: we don’t wait for the cleanup result
            // here. TODO: think about this more later, maybe this
            // should be try_send.
            tokio::spawn(async move {
                let (tx, _rx) = oneshot::channel();
                let _ = handle.msg_tx.send(Command::Shutdown(tx)).await;
            });
        }
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        let handles: Vec<RemoteHandle> = {
            let mut map = self.remote_map.lock().unwrap();
            map.drain().map(|(_, h)| h).collect()
        };
        let mut replies = Vec::with_capacity(handles.len());
        for handle in handles {
            let (tx, rx) = oneshot::channel();
            if handle.msg_tx.send(Command::Shutdown(tx)).await.is_ok() {
                replies.push(rx);
            }
        }
        for rx in replies {
            let _ = rx.await;
        }
        Ok(())
    }
}

// TODO: Remove this.
#[async_trait]
impl MsgChannel for WebChannel {
    async fn send(
        &self,
        recipient: &ServerId,
        req_id: Option<RequestId>,
        msg: Msg,
    ) -> anyhow::Result<()> {
        self.send(recipient, req_id, msg).await
    }

    async fn connect(
        &self,
        remote_id: ServerId,
        transport: Transport,
        my_key_cert: ArcKeyCert,
    ) -> anyhow::Result<()> {
        self.connect(remote_id, transport, my_key_cert).await
    }

    async fn broadcast(&self, req_id: Option<RequestId>, msg: Msg) -> anyhow::Result<()> {
        self.broadcast(req_id, msg).await
    }

    fn disconnect(&self, peer: &ServerId) {
        self.disconnect(peer)
    }

    async fn shutdown(&self) -> anyhow::Result<()> {
        self.shutdown().await
    }
}

// *** SctpRemote actor

/// Per-peer SCTP actor. Owned by a single tokio task; `WebChannel`
/// talks to it only via [`RemoteHandle`]. Probably could have just
/// passed everything into a run function (the run function consumes
/// the remote anyway), but using a struct looks a little tidier.
struct SctpRemote {
    peer_id: ServerId,
    sctp_assoc: Arc<association::Association>,
    dtls_conn: Arc<DTLSConn>,
    ice_agent: Arc<webrtc_ice::agent::Agent>,
    /// Next stream ID this actor will use for an outgoing message.
    /// Increments by 2 with wrapping; parity preserved.
    next_stream_id: u16,
    /// Parity of stream IDs we initiate. Used to filter out our own
    /// streams when SCTP echoes them back via `accept_stream`.
    stream_id_parity: u16,
    remote_msg_tx: mpsc::Sender<Message>,
    rx: mpsc::Receiver<Command>,
    /// Unix-epoch seconds of the last ping byte we wrote.
    last_ping_sent: u64,
    /// Unix-epoch seconds of the last byte received on any stream.
    /// Shared with the per-stream `read_from_stream` tasks.
    last_ping_recv: Arc<AtomicU64>,
}

/// Why the actor’s main loop exited. Determines the cleanup.
enum Exit {
    Shutdown(oneshot::Sender<anyhow::Result<()>>),
    OutgoingErr,
    IncomingClosed,
    SenderDropped,
    PingTimeout,
}

impl SctpRemote {
    fn new(
        peer_id: ServerId,
        sctp_assoc: Arc<association::Association>,
        dtls_conn: Arc<DTLSConn>,
        ice_agent: Arc<webrtc_ice::agent::Agent>,
        stream_id_init: u16,
        remote_msg_tx: mpsc::Sender<Message>,
        rx: mpsc::Receiver<Command>,
    ) -> Self {
        let now = now_secs();
        Self {
            peer_id,
            sctp_assoc,
            dtls_conn,
            ice_agent,
            next_stream_id: stream_id_init,
            stream_id_parity: stream_id_init,
            remote_msg_tx,
            rx,
            // saturating_sub so the timer’s immediate first tick fires
            // a ping right away.
            last_ping_sent: now.saturating_sub(PING_INTERVAL_SECS),
            last_ping_recv: Arc::new(AtomicU64::new(now)),
        }
    }

    /// Runs select! in a loop, once the loop exits, run cleanup. For
    /// graceful shutdowns the cleanup result is reported to the
    /// requester; for everything else we send `ConnectionBroke` to
    /// the main loop in server.rs to handle.
    async fn run(mut self) {
        // Open ping stream symmetrically.
        let ping_stream = match self
            .sctp_assoc
            .open_stream(PING_STREAM_ID, PayloadProtocolIdentifier::Binary)
            .await
        {
            Ok(stream) => stream,
            Err(e) => {
                tracing::warn!("Failed to open ping stream for {}: {e}", self.peer_id);
                let _ = self.cleanup().await;
                let _ = self
                    .remote_msg_tx
                    .send(Message {
                        host: self.peer_id.clone(),
                        body: Msg::ConnectionBroke(self.peer_id.clone()),
                        req_id: None,
                    })
                    .await;
                return;
            }
        };

        // Real work happens here.
        let exit = self.run_inner(ping_stream).await;
        let cleanup_res = self.cleanup().await;

        // Cleanup.
        match exit {
            Exit::Shutdown(tx) => {
                let _ = tx.send(cleanup_res);
            }
            Exit::OutgoingErr | Exit::IncomingClosed | Exit::SenderDropped | Exit::PingTimeout => {
                let _ = self
                    .remote_msg_tx
                    .send(Message {
                        host: self.peer_id.clone(),
                        body: Msg::ConnectionBroke(self.peer_id.clone()),
                        req_id: None,
                    })
                    .await;
            }
        }
    }

    /// Single select loop over the four event sources. Returns the
    /// reason we’re exiting.
    async fn run_inner(&mut self, ping_stream: Arc<stream::Stream>) -> Exit {
        // First tick fires immediately. At t=0 last_ping_recv = now
        // (not timed out) and last_ping_sent = now - PING_INTERVAL, so
        // both sides write their first ping byte right away.
        let mut ping_timer = tokio::time::interval(PING_CHECK_INTERVAL);
        let mut ping_buf = [0u8; 64];

        loop {
            tokio::select! {
                cmd = self.rx.recv() => match cmd {
                    Some(Command::Shutdown(tx)) => return Exit::Shutdown(tx),
                    Some(Command::Msg(msg)) => {
                        if self.handle_outgoing_message(msg).await.is_err() {
                            return Exit::OutgoingErr;
                        }
                    }
                    None => return Exit::SenderDropped,
                },
                stream = self.sctp_assoc.accept_stream() => match stream {
                    Some(stream) => self.handle_incoming_stream(stream),
                    None => return Exit::IncomingClosed,
                },
                _ = ping_timer.tick() => {
                    if let Some(exit) = self.handle_ping_tick(&ping_stream).await {
                        return exit;
                    }
                }
                res = ping_stream.read(&mut ping_buf) => {
                    match res {
                        Ok(0) => {
                            tracing::warn!("Ping stream closed by {}", self.peer_id);
                            return Exit::PingTimeout;
                        }
                        Ok(_) => {
                            self.last_ping_recv.store(now_secs(), Ordering::Relaxed);
                        }
                        Err(err) => {
                            tracing::warn!("Ping stream read error for {}: {:?}", self.peer_id, err);
                            return Exit::PingTimeout;
                        }
                    }
                }
            }
        }
    }

    /// This function does two things.
    /// 1. If too long has past since we receives a message or ping,
    /// consider the connection broken and abort. (webrtc_sctp doesn’t
    /// implement a heartbeat itself so we need this to detect
    /// connection breakage.)
    /// 2. Send out ping every n seconds.
    async fn handle_ping_tick(&mut self, ping_stream: &Arc<stream::Stream>) -> Option<Exit> {
        let now = now_secs();

        let last_recv = self.last_ping_recv.load(Ordering::Relaxed);
        if now.saturating_sub(last_recv) > PING_TIMEOUT_SECS {
            tracing::warn!(
                "Ping timeout for {}: last recv {}s ago",
                self.peer_id,
                now.saturating_sub(last_recv)
            );
            return Some(Exit::PingTimeout);
        }

        if now.saturating_sub(self.last_ping_sent) < PING_INTERVAL_SECS {
            return None;
        }

        if let Err(e) = ping_stream.write(&bytes::Bytes::from_static(&[0u8])).await {
            tracing::warn!("Failed to write ping byte to {}: {e}", self.peer_id);
            return Some(Exit::OutgoingErr);
        }
        self.last_ping_sent = now;
        None
    }

    /// Best-effort teardown of SCTP/DTLS/ICE, capped by a timeout so a
    /// wedged peer can’t block shutdown forever.
    async fn cleanup(&self) -> anyhow::Result<()> {
        let timeout = Duration::from_secs(5);
        tokio::time::timeout(timeout, async {
            let _ = self.sctp_assoc.shutdown().await;
            // These aren’t strictly necessary but they don’t hurt.
            let _ = self.sctp_assoc.close().await;
            let _ = self.dtls_conn.close().await;
            let _ = self.ice_agent.close().await;
        })
        .await
        .map_err(|_| anyhow!("cleanup timed out for {}", self.peer_id))?;
        Ok(())
    }
}

// *** DTLS and SCTP

const MAX_FRAME_SIZE: usize = 64 * 1024;
const RECEIVE_BUFFER_SIZE: u32 = 1024 * 1024;

/// Signal Error if `conn` doesn't provide the same certificate as `cert`.
async fn verify_dtls_cert(cert: CertDerHash, conn: &DTLSConn) -> anyhow::Result<()> {
    let certs = conn.connection_state().await.peer_certificates;
    if certs.is_empty() {
        return Err(anyhow!("Remote host provided empty certificate"));
    }
    let provided_cert_hash = hash_der(&certs[0]);
    // This is an error rather than failed permission check, so handle
    // it as any other error.
    if provided_cert_hash != cert {
        return Err(anyhow!("Certificate hash mismatch, cert from signaling server = {cert}, cert provided by DTLS = {provided_cert_hash}"));
    }
    Ok(())
}

async fn create_dtls_server(
    conn: Arc<dyn Conn + Send + Sync>,
    my_key_cert: ArcKeyCert,
    their_cert: CertDerHash,
) -> anyhow::Result<Arc<DTLSConn>> {
    let config = webrtc_dtls::config::Config {
        certificates: vec![my_key_cert.create_dtls_cert()],
        client_auth: webrtc_dtls::config::ClientAuthType::RequireAnyClientCert,
        // We accept any certificate, and then verifies the provided
        // certificate with the cert we got from signaling server.
        insecure_skip_verify: true,
        ..Default::default()
    };

    let conn = DTLSConn::new(conn, config, false, None).await?;
    verify_dtls_cert(their_cert, &conn).await?;
    Ok(Arc::new(conn))
}

async fn create_dtls_client(
    conn: Arc<dyn Conn + Send + Sync>,
    my_key_cert: ArcKeyCert,
    their_cert: CertDerHash,
) -> anyhow::Result<Arc<DTLSConn>> {
    let config = webrtc_dtls::config::Config {
        certificates: vec![my_key_cert.create_dtls_cert()],
        // We accept any certificate, and then verifies the provided
        // certificate with the cert we got from signaling server.
        insecure_skip_verify: true,
        ..Default::default()
    };

    let conn = DTLSConn::new(conn, config, true, None).await?;
    verify_dtls_cert(their_cert, &conn).await?;
    Ok(Arc::new(conn))
}

async fn create_sctp_server(
    conn: Arc<dyn Conn + Send + Sync>,
) -> anyhow::Result<Arc<association::Association>> {
    let assoc_config = association::Config {
        net_conn: conn,
        max_receive_buffer_size: RECEIVE_BUFFER_SIZE,
        max_message_size: MAX_FRAME_SIZE as u32,
        name: "data channel".to_string(),
    };
    let assoc = association::Association::server(assoc_config).await?;
    Ok(Arc::new(assoc))
}

async fn create_sctp_client(
    conn: Arc<dyn Conn + Send + Sync>,
) -> anyhow::Result<Arc<association::Association>> {
    let assoc_config = association::Config {
        net_conn: conn,
        max_receive_buffer_size: RECEIVE_BUFFER_SIZE,
        max_message_size: MAX_FRAME_SIZE as u32,
        name: "rpc data channel".to_string(),
    };
    let assoc = association::Association::client(assoc_config).await?;
    Ok(Arc::new(assoc))
}

// *** SctpRemote per-message handlers

impl SctpRemote {
    /// Frame and write one outgoing message on a fresh SCTP stream.
    /// Returns Err(()) on any fatal stream/serialization error; the
    /// caller turns that into `Exit::OutgoingErr` and we tear down.
    async fn handle_outgoing_message(&mut self, message: Message) -> Result<(), ()> {
        // Bump the stream-ID counter by 2 with wrapping. Parity is
        // preserved so the two ends of the connection never collide.
        let stream_id = self.next_stream_id;
        self.next_stream_id = self.next_stream_id.wrapping_add(2);

        // open_stream only errors when our own accept_stream is
        // blocked, which shouldn’t happen.
        let stream = match self
            .sctp_assoc
            .open_stream(stream_id, PayloadProtocolIdentifier::Binary)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("Failed to open stream #{}: {}", stream_id, e);
                return Err(());
            }
        };

        let data = match serde_json::to_string(&message) {
            Ok(s) => s.into_bytes(),
            Err(e) => {
                // I don’t expect this to happen.
                tracing::warn!("Failed to serialize message: {}", e);
                let _ = self
                    .remote_msg_tx
                    .send(Message {
                        host: self.peer_id.clone(),
                        body: Msg::SerializationErr(self.peer_id.clone()),
                        req_id: None,
                    })
                    .await;
                let _ = stream.shutdown(std::net::Shutdown::Both).await;
                return Err(());
            }
        };

        // Write 8-byte length prefix followed by the payload.
        let len_bytes = bytes::Bytes::from((data.len() as u64).to_be_bytes().to_vec());
        if let Err(e) = stream.write(&len_bytes).await {
            tracing::warn!("Failed to write length prefix to stream: {}", e);
            let _ = stream.shutdown(std::net::Shutdown::Both).await;
            return Err(());
        }

        // Send data in chunks of MAX_FRAME_SIZE.
        let mut offset = 0;
        while offset < data.len() {
            let chunk_end = std::cmp::min(offset + MAX_FRAME_SIZE, data.len());
            let chunk = bytes::Bytes::from(data[offset..chunk_end].to_vec());
            if let Err(e) = stream.write(&chunk).await {
                tracing::warn!("Failed to write data chunk to stream: {}", e);
                let _ = stream.shutdown(std::net::Shutdown::Both).await;
                return Err(());
            }
            offset = chunk_end;
        }
        let _ = stream.shutdown(std::net::Shutdown::Write).await;
        Ok(())
    }

    /// Handle one inbound SCTP stream. Streams with id matching our
    /// parity are streams we opened, ignore them and close the
    /// receiving half. Stream with `PING_STREAM_ID` is the ping
    /// stream, don’t close the receiving half, but also ignore it,
    /// because we alread have a copy. For everything else, spawn a
    /// task to read the message and forward it to `remote_msg_tx`.
    fn handle_incoming_stream(&self, stream: Arc<stream::Stream>) {
        let stream_id = stream.stream_identifier();

        // Ignore ping stream but don’t close the receiving half.
        if stream_id == PING_STREAM_ID {
            return;
        }

        // Client (init=1, odd) ignores odd streams; server (init=2,
        // even) ignores even streams.
        if (stream_id % 2) == (self.stream_id_parity % 2) {
            let s = stream.clone();
            tokio::spawn(async move {
                let _ = s.shutdown(std::net::Shutdown::Read).await;
            });
            return;
        }

        tracing::info!("New stream (#{}) from {}", stream_id, self.peer_id);
        let tx = self.remote_msg_tx.clone();
        let peer = self.peer_id.clone();
        let last_ping_recv = self.last_ping_recv.clone();
        tokio::spawn(async move {
            read_from_stream(stream, tx, peer, last_ping_recv).await;
        });
    }
}

async fn read_from_stream(
    stream: Arc<stream::Stream>,
    msg_tx: mpsc::Sender<Message>,
    remote_hostid: ServerId,
    last_ping_recv: Arc<AtomicU64>,
) {
    // First, read 8 bytes for the length
    let mut len_bytes = [0u8; 8];

    match stream.read(&mut len_bytes).await {
        Ok(n) => {
            if n < 8 {
                let _ = msg_tx
                    .send(Message {
                        host: remote_hostid.clone(),
                        body: Msg::SerializationErr(format!(
                            "Failed to read full length (8 bytes) prefix: only got {} bytes",
                            n
                        )),
                        req_id: None,
                    })
                    .await;
                let _ = stream.shutdown(std::net::Shutdown::Both).await;
                return;
            }
        }
        Err(err) => {
            // The only error stream.read might throw is
            // ErrShortBuffer. Which shouldn’t happen.
            tracing::warn!(
                "Failed to read from stream #{} from {}: {}",
                stream.stream_identifier(),
                remote_hostid,
                err
            );
            let _ = msg_tx.send(Message {
                host: remote_hostid.clone(),
                body: Msg::SerializationErr(format!("Failed to read from stream because buffer too short which should never happen").to_string()),
                req_id: None,
            }).await;
            let _ = stream.shutdown(std::net::Shutdown::Both).await;
            return;
        }
    }

    let content_length = u64::from_be_bytes(len_bytes) as usize;

    // Now read exactly content_length bytes using a chunk buffer
    let mut full_buffer = Vec::with_capacity(content_length);
    let mut chunk_buffer = vec![0u8; MAX_FRAME_SIZE];

    while full_buffer.len() < content_length {
        let remaining = content_length - full_buffer.len();
        let to_read = std::cmp::min(remaining, MAX_FRAME_SIZE);

        match stream.read(&mut chunk_buffer[..to_read]).await {
            Ok(n) => {
                if n == 0 {
                    // EOF before reading full content
                    let _ = msg_tx
                        .send(Message {
                            host: remote_hostid.clone(),
                            body: Msg::SerializationErr(format!(
                                "EOF while reading content: expected {} bytes, got {}",
                                content_length,
                                full_buffer.len()
                            )),
                            req_id: None,
                        })
                        .await;
                    let _ = stream.shutdown(std::net::Shutdown::Both).await;
                    return;
                }
                full_buffer.extend_from_slice(&chunk_buffer[..n]);
            }
            Err(e) => {
                // The only error stream.read might throw is
                // ErrShortBuffer. Which shouldn’t happen.
                tracing::warn!(
                    "Failed to read content from stream from {}: {}",
                    remote_hostid,
                    e
                );
                let _ = msg_tx.send(Message {
                    host: remote_hostid.clone(),
                    body: Msg::SerializationErr(format!("Failed to read from stream because buffer too short which should never happen").to_string()),
                    req_id: None,
                }).await;
                let _ = stream.shutdown(std::net::Shutdown::Both).await;
                return;
            }
        }
    }

    let _ = stream.shutdown(std::net::Shutdown::Both).await;

    // Deserialize the message
    // match bincode::deserialize::<Message>(&full_buffer) {
    match serde_json::from_slice::<Message>(&full_buffer) {
        Ok(message) => {
            last_ping_recv.store(now_secs(), Ordering::Relaxed);
            if let Err(e) = msg_tx.send(message).await {
                tracing::warn!("Failed to send message to channel: {}", e);
                // No need to send error, the incoming_message handler
                // will detect broken stream.
            }
        }
        Err(e) => {
            tracing::warn!(
                "Failed to deserialize message from {}: {}",
                remote_hostid,
                e
            );
            let _ = msg_tx
                .send(Message {
                    host: remote_hostid.clone(),
                    body: Msg::SerializationErr(format!("Failed to deserialize message: {}", e)),
                    req_id: None,
                })
                .await;
        }
    }
}

// *** Test utilities

/// Travel time configuration for message delivery
#[derive(Debug, Clone)]
pub enum TravelTime {
    /// Random delay between min and max milliseconds
    Random(u64, u64),
    /// Fixed delay in milliseconds
    Ms(u64),
    /// Instant delivery with no delay
    Instant,
}

/// State for a single host in the test factory
struct HostState {
    inbox: Vec<Message>,
    msg_tx: mpsc::Sender<Message>,
    self_tx: mpsc::UnboundedSender<Message>,
    deliverable: bool,
}

/// Shared state for the test factory
struct FactoryState {
    hosts: HashMap<ServerId, HostState>,
}

impl FactoryState {
    /// Synchronous part of deliver - checks deliverable flag, drains inbox.
    /// Returns messages and msg_tx without holding lock across await.
    fn deliver_sync(
        &mut self,
        hostid: &ServerId,
    ) -> anyhow::Result<(Vec<Message>, mpsc::Sender<Message>)> {
        let host_state = self
            .hosts
            .get_mut(hostid)
            .ok_or_else(|| anyhow!("Host {} not found", hostid))?;

        if !host_state.deliverable {
            return Err(anyhow!("Inbox for {} not deliverable", hostid));
        }

        let messages = std::mem::take(&mut host_state.inbox);
        let msg_tx = host_state.msg_tx.clone();
        Ok((messages, msg_tx))
    }
}

/// Factory for creating test web channels with in-memory message passing
pub struct TestWebChannelFactory {
    state: Arc<Mutex<FactoryState>>,
    travel_time: TravelTime,
}

impl TestWebChannelFactory {
    pub fn new(travel_time: TravelTime) -> Self {
        Self {
            state: Arc::new(Mutex::new(FactoryState {
                hosts: HashMap::new(),
            })),
            travel_time,
        }
    }

    /// Create a new test channel for a host
    pub fn get_channel(
        self: &Arc<Self>,
        my_hostid: ServerId,
        msg_tx: mpsc::Sender<Message>,
        self_tx: mpsc::UnboundedSender<Message>,
    ) -> TestWebChannel {
        let mut state = self.state.lock().unwrap();
        state.hosts.insert(
            my_hostid.clone(),
            HostState {
                inbox: Vec::new(),
                msg_tx,
                self_tx,
                deliverable: false,
            },
        );

        TestWebChannel {
            my_hostid,
            factory: self.clone(),
        }
    }

    /// Deliver messages from a host's inbox to its msg_tx.
    /// Returns error if inbox is not marked as deliverable.
    /// If travel_time is 0, delivers synchronously. Otherwise spawns a task
    /// that waits for the travel time and then delivers all pending messages.
    pub async fn deliver(&self, hostid: &ServerId) -> anyhow::Result<()> {
        // Calculate travel time in milliseconds
        let delay_ms = match &self.travel_time {
            TravelTime::Instant => 0,
            TravelTime::Ms(ms) => *ms,
            TravelTime::Random(min, max) => rand::random::<u64>() % (max - min + 1) + min,
        };

        if delay_ms == 0 {
            // Deliver synchronously
            let (messages, msg_tx) = { self.state.lock().unwrap().deliver_sync(hostid)? };

            for msg in messages {
                msg_tx
                    .send(msg)
                    .await
                    .map_err(|_| anyhow!("Channel broke for {}", hostid))?;
            }

            Ok(())
        } else {
            // Deliver asynchronously after delay
            let hostid = hostid.clone();
            let state = self.state.clone();

            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;

                let (messages, msg_tx) = match state.lock().unwrap().deliver_sync(&hostid) {
                    Ok(result) => result,
                    Err(e) => {
                        tracing::debug!("Failed to deliver to {}: {}", hostid, e);
                        return;
                    }
                };

                for msg in messages {
                    if let Err(e) = msg_tx.send(msg).await {
                        tracing::warn!("Channel broke for {}: {}", hostid, e);
                        break;
                    }
                }
            });

            Ok(())
        }
    }
}

/// Test web channel that uses in-memory message passing
#[derive(Clone)]
pub struct TestWebChannel {
    my_hostid: ServerId,
    factory: Arc<TestWebChannelFactory>,
}

impl TestWebChannel {
    pub fn hostid(&self) -> ServerId {
        self.my_hostid.clone()
    }
}

#[async_trait]
impl MsgChannel for TestWebChannel {
    async fn send(
        &self,
        recipient: &ServerId,
        req_id: Option<RequestId>,
        msg: Msg,
    ) -> anyhow::Result<()> {
        let message = Message {
            host: self.my_hostid.clone(),
            body: msg.clone(),
            req_id,
        };

        // Send to self via the unbounded channel.
        if recipient == &self.my_hostid {
            let self_tx = {
                let state = self.factory.state.lock().unwrap();
                let host_state = state
                    .hosts
                    .get(recipient)
                    .ok_or_else(|| anyhow!("Recipient {} not found", recipient))?;
                host_state.self_tx.clone()
            };

            return self_tx
                .send(message)
                .map_err(|_| anyhow!("Self channel closed for {}", recipient));
        }

        // Push to recipient's inbox
        {
            let mut state = self.factory.state.lock().unwrap();
            let host_state = state
                .hosts
                .get_mut(recipient)
                .ok_or_else(|| anyhow!("Recipient {} not found", recipient))?;

            host_state.inbox.push(message);
        }

        // Deliver immediately using factory's deliver method
        self.factory.deliver(recipient).await?;

        Ok(())
    }

    async fn connect(
        &self,
        remote_hostid: ServerId,
        _transport: Transport,
        _my_key_cert: ArcKeyCert,
    ) -> anyhow::Result<()> {
        // Mark both self and remote as deliverable
        {
            let mut state = self.factory.state.lock().unwrap();

            // Mark remote as deliverable
            let remote_host_state = state
                .hosts
                .get_mut(&remote_hostid)
                .ok_or_else(|| anyhow!("Remote host {} not found", remote_hostid))?;
            remote_host_state.deliverable = true;

            // Mark self as deliverable too
            let my_host_state = state
                .hosts
                .get_mut(&self.my_hostid)
                .ok_or_else(|| anyhow!("My host {} not found", self.my_hostid))?;
            my_host_state.deliverable = true;
        }

        // Send Hey messages bidirectionally to establish connection
        // From connector to remote
        self.send(
            &remote_hostid,
            None,
            Msg::Hey(HeyMessage {
                message: "Nice to meet ya".to_string(),
                credentials: "".to_string(),
                version: "v1.0.0".to_string(),
            }),
        )
        .await?;

        // From remote back to connector (simulate bidirectional handshake)
        {
            let mut state = self.factory.state.lock().unwrap();
            let my_host_state = state
                .hosts
                .get_mut(&self.my_hostid)
                .ok_or_else(|| anyhow!("My host {} not found", self.my_hostid))?;

            my_host_state.inbox.push(Message {
                host: remote_hostid.clone(),
                body: Msg::Hey(HeyMessage {
                    message: "Welcome to my server".to_string(),
                    credentials: "".to_string(),
                    version: "v1.0.0".to_string(),
                }),
                req_id: None,
            });
        }

        // Deliver the Hey from remote to self
        self.factory.deliver(&self.my_hostid).await?;

        Ok(())
    }

    async fn broadcast(&self, req_id: Option<RequestId>, msg: Msg) -> anyhow::Result<()> {
        let message = Message {
            host: self.my_hostid.clone(),
            body: msg,
            req_id,
        };

        // Collect all deliverable host IDs and push message to their inboxes
        let deliverable_hosts: Vec<_> = {
            let mut state = self.factory.state.lock().unwrap();
            state
                .hosts
                .iter_mut()
                .filter(|(_, host_state)| host_state.deliverable)
                .map(|(hostid, host_state)| {
                    host_state.inbox.push(message.clone());
                    hostid.clone()
                })
                .collect()
        };

        // Deliver to all hosts using factory's deliver method
        let mut errors = Vec::new();
        for hostid in deliverable_hosts {
            if let Err(e) = self.factory.deliver(&hostid).await {
                errors.push(e);
            }
        }

        if !errors.is_empty() {
            return Err(anyhow!(
                "Failed to broadcast to {} peer(s): {}",
                errors.len(),
                errors
                    .iter()
                    .map(|e| e.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }

        Ok(())
    }

    fn disconnect(&self, _peer: &ServerId) {
        // No-op for test channel
    }

    async fn shutdown(&self) -> anyhow::Result<()> {
        // Tests don't need flushing; messages are delivered synchronously.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    fn create_test_id(name: &str) -> ServerId {
        format!("test-{}", name)
    }

    #[tokio::test]
    async fn test_sync_send_instant() {
        let factory = Arc::new(TestWebChannelFactory::new(TravelTime::Instant));

        let (tx1, _rx1) = mpsc::channel::<Message>(10);
        let (tx2, mut rx2) = mpsc::channel::<Message>(10);
        let (self_tx1, _self_rx1) = mpsc::unbounded_channel::<Message>();
        let (self_tx2, _self_rx2) = mpsc::unbounded_channel::<Message>();

        let id1 = create_test_id("host1");
        let id2 = create_test_id("host2");

        let channel1 = factory.get_channel(id1.clone(), tx1, self_tx1);
        let channel2 = factory.get_channel(id2.clone(), tx2, self_tx2);

        // Mark both as deliverable by connecting to each other
        channel1
            .connect(
                id2.clone(),
                Transport::Dummy,
                Arc::new(crate::config_man::create_key_cert("test1")),
            )
            .await
            .unwrap();
        channel2
            .connect(
                id1.clone(),
                Transport::Dummy,
                Arc::new(crate::config_man::create_key_cert("test2")),
            )
            .await
            .unwrap();

        // Drain Hey messages from connection establishment
        let _ = rx2.recv().await; // Hey from channel1
        let _ = rx2.recv().await; // Hey from channel2 to itself

        // Send message from channel1 to channel2
        channel1
            .send(&id2, None, Msg::FileShared(42))
            .await
            .unwrap();

        // Should receive immediately with Instant travel time
        let msg = rx2.recv().await.unwrap();
        assert_eq!(msg.host, id1);
        assert!(matches!(msg.body, Msg::FileShared(42)));
    }

    #[tokio::test]
    async fn test_sync_send_ms_zero() {
        let factory = Arc::new(TestWebChannelFactory::new(TravelTime::Ms(0)));

        let (tx1, _rx1) = mpsc::channel::<Message>(10);
        let (tx2, mut rx2) = mpsc::channel::<Message>(10);
        let (self_tx1, _self_rx1) = mpsc::unbounded_channel::<Message>();
        let (self_tx2, _self_rx2) = mpsc::unbounded_channel::<Message>();

        let id1 = create_test_id("sender");
        let id2 = create_test_id("receiver");

        let channel1 = factory.get_channel(id1.clone(), tx1, self_tx1);
        let channel2 = factory.get_channel(id2.clone(), tx2, self_tx2);

        // Mark both as deliverable by connecting to each other
        channel1
            .connect(
                id2.clone(),
                Transport::Dummy,
                Arc::new(crate::config_man::create_key_cert("test1")),
            )
            .await
            .unwrap();
        channel2
            .connect(
                id1.clone(),
                Transport::Dummy,
                Arc::new(crate::config_man::create_key_cert("test2")),
            )
            .await
            .unwrap();

        // Drain Hey messages from connection establishment
        let _ = rx2.recv().await; // Hey from channel1
        let _ = rx2.recv().await; // Hey from channel2 to itself

        // Send message
        channel1
            .send(&id2, None, Msg::FileShared(99))
            .await
            .unwrap();

        // Should receive immediately with Ms(0)
        let msg = rx2.recv().await.unwrap();
        assert_eq!(msg.host, id1);
        assert!(matches!(msg.body, Msg::FileShared(99)));
    }

    #[tokio::test]
    async fn test_send_to_non_deliverable_fails() {
        let factory = Arc::new(TestWebChannelFactory::new(TravelTime::Instant));

        let (tx1, _rx1) = mpsc::channel::<Message>(10);
        let (tx2, _rx2) = mpsc::channel::<Message>(10);
        let (self_tx1, _self_rx1) = mpsc::unbounded_channel::<Message>();
        let (self_tx2, _self_rx2) = mpsc::unbounded_channel::<Message>();

        let id1 = create_test_id("sender");
        let id2 = create_test_id("receiver");

        let channel1 = factory.get_channel(id1.clone(), tx1, self_tx1);
        let _channel2 = factory.get_channel(id2.clone(), tx2, self_tx2);

        // Don't connect - both channels are not deliverable

        // Try to send to non-deliverable recipient - should fail
        let result = channel1.send(&id2, None, Msg::FileShared(1)).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not deliverable"));
    }

    #[tokio::test]
    async fn test_sync_broadcast() {
        let factory = Arc::new(TestWebChannelFactory::new(TravelTime::Instant));

        let (tx_sender, _rx_sender) = mpsc::channel::<Message>(10);
        let (tx_recv1, mut rx_recv1) = mpsc::channel::<Message>(10);
        let (tx_recv2, mut rx_recv2) = mpsc::channel::<Message>(10);
        let (self_tx_sender, _self_rx_sender) = mpsc::unbounded_channel::<Message>();
        let (self_tx_recv1, _self_rx_recv1) = mpsc::unbounded_channel::<Message>();
        let (self_tx_recv2, _self_rx_recv2) = mpsc::unbounded_channel::<Message>();

        let id_sender = create_test_id("sender");
        let id_recv1 = create_test_id("receiver1");
        let id_recv2 = create_test_id("receiver2");

        let channel_sender = factory.get_channel(id_sender.clone(), tx_sender, self_tx_sender);
        let channel_recv1 = factory.get_channel(id_recv1.clone(), tx_recv1, self_tx_recv1);
        let channel_recv2 = factory.get_channel(id_recv2.clone(), tx_recv2, self_tx_recv2);

        // Mark all as deliverable by connecting them
        channel_sender
            .connect(
                id_recv1.clone(),
                Transport::Dummy,
                Arc::new(crate::config_man::create_key_cert("sender")),
            )
            .await
            .unwrap();
        channel_recv1
            .connect(
                id_sender.clone(),
                Transport::Dummy,
                Arc::new(crate::config_man::create_key_cert("recv1")),
            )
            .await
            .unwrap();
        channel_sender
            .connect(
                id_recv2.clone(),
                Transport::Dummy,
                Arc::new(crate::config_man::create_key_cert("sender")),
            )
            .await
            .unwrap();
        channel_recv2
            .connect(
                id_sender.clone(),
                Transport::Dummy,
                Arc::new(crate::config_man::create_key_cert("recv2")),
            )
            .await
            .unwrap();

        // Drain Hey messages from connection establishment
        let _ = rx_recv1.recv().await; // Hey from sender
        let _ = rx_recv1.recv().await; // Hey from recv1 to itself
        let _ = rx_recv2.recv().await; // Hey from sender
        let _ = rx_recv2.recv().await; // Hey from recv2 to itself

        // Broadcast from sender
        channel_sender
            .broadcast(None, Msg::FileShared(123))
            .await
            .unwrap();

        // Both receivers should get the message
        let msg1 = rx_recv1.recv().await.unwrap();
        assert_eq!(msg1.host, id_sender);
        assert!(matches!(msg1.body, Msg::FileShared(123)));

        let msg2 = rx_recv2.recv().await.unwrap();
        assert_eq!(msg2.host, id_sender);
        assert!(matches!(msg2.body, Msg::FileShared(123)));
    }
}

#[cfg(test)]
mod e2e_tests;
