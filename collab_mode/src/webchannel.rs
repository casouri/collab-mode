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

use crate::message::Msg;
use crate::{
    config_man::{hash_der, ConfigProject},
    signaling::CertDerHash,
    types::*,
};
use anyhow::anyhow;
use lsp_server::RequestId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::DropGuard;
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
fn stream_id_init(my_id: &str, peer_id: &str) -> u16 {
    if my_id < peer_id {
        STREAM_ID_CLIENT_INIT // 1 (odd)
    } else {
        STREAM_ID_SERVER_INIT // 2 (even)
    }
}

/// Carries the transport-specific data needed to establish a connection
/// to a peer. [`WebChannel::connect`] dispatches on the variant.
pub enum Transport {
    /// Connect via WebRTC (SCTP/DTLS/ICE) using a [`Sock`] from the
    /// signaling client.
    ///
    /// [`Sock`]: crate::signaling::client::Sock
    Sock(crate::signaling::client::Sock),
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
    Io(ReaderWriter),
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
/// a separate bounded channel for self-messages. If we don’t use a
/// separate loopback channel, sending a message to ourselves will
/// block the main loop in server.rs. The loopback channel is bounded
/// (capacity 1000) and the sender uses `try_send`, so sending to self
/// never blocks; overflow should never happen, if it does happen, we
/// just shutdown.
pub struct DualReceiver {
    remote_rx: mpsc::Receiver<Message>,
    loopback_rx: mpsc::Receiver<Message>,
}

impl DualReceiver {
    /// Create a new dual receiver. Returns the dual receiver, and the
    /// remote and self-message tx.
    pub fn new() -> (Self, mpsc::Sender<Message>, mpsc::Sender<Message>) {
        let (remote_tx, remote_rx) = mpsc::channel::<Message>(1);
        let (loopback_tx, loopback_rx) = mpsc::channel::<Message>(1000);
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

/// Technically there can be other commands send to actors, but right
/// now there’s only message.
pub enum Command {
    Msg(Message),
}

/// Inner body of [`WebChannel::start_connect`]: await the future to
/// get the transport, run `connect` with it, then send all the
/// post-connect messages to the actor to finish up the connection
/// setup. On error, send a `Msg::FailedToConnect` to the server.
async fn start_connect_1<F>(
    chan: WebChannel,
    peer_id: ServerId,
    transport_fut: F,
    my_key_cert: ArcKeyCert,
    connection_id: u128,
    post_connect_msgs: Vec<Msg>,
    peer_cancel: crate::cancel::CancelManager,
) where
    F: std::future::Future<Output = anyhow::Result<Transport>> + Send + 'static,
{
    let result: anyhow::Result<()> = async {
        let transport = transport_fut.await?;
        chan.connect(
            peer_id.clone(),
            transport,
            my_key_cert,
            connection_id,
            peer_cancel,
        )
        .await?;
        for msg in post_connect_msgs {
            chan.send(&peer_id, None, msg).await?;
        }
        Ok(())
    }
    .await;

    if let Err(err) = result {
        tracing::warn!("start_connect failed for {}: {}", id_short(&peer_id), err);
        let _ = chan
            .remote_msg_tx
            .send(Message {
                host: chan.my_hostid.clone(),
                body: Msg::FailedToConnect {
                    peer: peer_id,
                    reason: err.to_string(),
                    connection_id,
                },
                req_id: None,
            })
            .await;
    }
}

/// Insert a new `RemoteHandle` for `peer` in `map`, wrapping a
/// `DropGuard` over the per-peer cancel token. If a live handle
/// already exists, returns [`WebChannelError::ConnectionExists`]
/// without changing the map. If a dead handle exists, it is evicted
/// (which cancels its orphan token via its `DropGuard`) and replaced.
/// Returns the receiver matching the inserted handle's `msg_tx`.
fn insert_handle(
    map: &Arc<Mutex<HashMap<ServerId, RemoteHandle>>>,
    peer: &ServerId,
    peer_cancel: &crate::cancel::CancelManager,
) -> anyhow::Result<mpsc::Receiver<Command>> {
    let mut map = map.lock().unwrap();
    if let Some(existing) = map.get(peer) {
        if !existing.is_dead() {
            return Err(anyhow!(WebChannelError::ConnectionExists(peer.clone())));
        }
        // Is dead, drop and run its cleanup.
        map.remove(peer);
    }
    let (tx, rx) = mpsc::channel::<Command>(16);
    let handle = RemoteHandle {
        msg_tx: tx,
        _cancel_guard: peer_cancel.token().drop_guard(),
    };
    map.insert(peer.clone(), handle);
    Ok(rx)
}

/// Handle stored in [`WebChannel::remote_map`]. Owns the per-peer
/// cancel guard: dropping the handle (via `remove`/`clear`/replace)
/// cancels the underlying [`CancellationToken`] and tears down both
/// the connect wrapper and the per-peer actor.
pub struct RemoteHandle {
    msg_tx: mpsc::Sender<Command>,
    /// On drop, cancels the per-peer `CancellationToken` shared with
    /// every `spawn_with_cancel` for this peer.
    _cancel_guard: DropGuard,
}

impl RemoteHandle {
    /// A handle is “dead” once its actor has dropped the receiver
    /// (connection broke or shutdown).
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
    /// Side channel for self-addressed messages so sending messages
    /// so sending self-messages never blocks.
    loopback_tx: mpsc::Sender<Message>,
    /// Server-wide root cancel manager. Each actor gets a child of
    /// this. The loopback- channel-full fallback fires
    /// `shutdown.cancel()` to abort the whole server.
    shutdown: crate::cancel::CancelManager,
    /// One entry per peer. The handle stores access to the actor that
    /// does the work, plu a cancel manager drop guard, which on drop,
    /// cleans up all the tasks spawned for that peer.
    remote_map: Arc<Mutex<HashMap<ServerId, RemoteHandle>>>,
    /// Populated only when this WebChannel is wired into a test
    /// factory. Shared by all WebChannels under the same factory so
    /// they can route to each other in-process. None in production.
    test_factory_state: Option<Arc<Mutex<FactoryState>>>,
    /// Simulated network delay for the test transport. Ignored when
    /// `test_factory_state` is None.
    test_travel_time: TravelTime,
}

impl WebChannel {
    pub fn new(
        my_hostid: ServerId,
        remote_msg_tx: mpsc::Sender<Message>,
        loopback_tx: mpsc::Sender<Message>,
        shutdown: crate::cancel::CancelManager,
    ) -> Self {
        Self {
            my_hostid,
            remote_msg_tx,
            loopback_tx,
            shutdown,
            remote_map: Arc::new(Mutex::new(HashMap::new())),
            test_factory_state: None,
            test_travel_time: TravelTime::Instant,
        }
    }

    /// Test-only constructor. All `WebChannel`s built under the same
    /// `factory_state` route messages to each other in-process via
    /// `TestRemote` actors instead of touching the network.
    pub fn new_for_test(
        my_hostid: ServerId,
        remote_msg_tx: mpsc::Sender<Message>,
        loopback_tx: mpsc::Sender<Message>,
        shutdown: crate::cancel::CancelManager,
        factory_state: Arc<Mutex<FactoryState>>,
        travel_time: TravelTime,
    ) -> Self {
        // Register so other test WebChannels can find our mailbox.
        factory_state.lock().unwrap().hosts.insert(
            my_hostid.clone(),
            HostState {
                inbox: Vec::new(),
                msg_tx: remote_msg_tx.clone(),
            },
        );
        Self {
            my_hostid,
            remote_msg_tx,
            loopback_tx,
            shutdown,
            remote_map: Arc::new(Mutex::new(HashMap::new())),
            test_factory_state: Some(factory_state),
            test_travel_time: travel_time,
        }
    }

    pub fn hostid(&self) -> ServerId {
        self.my_hostid.clone()
    }

    /// Starts the connection process for `peer_id`, which includes
    /// awaiting the future to get the transport, connecting with the
    /// peer with the transport, and sending some handshake messages
    /// (`post_connect_msgs`).
    pub fn start_connect<F>(
        &self,
        peer_id: ServerId,
        transport_fut: F,
        my_key_cert: ArcKeyCert,
        connection_id: u128,
        post_connect_msgs: Vec<Msg>,
    ) where
        F: std::future::Future<Output = anyhow::Result<Transport>> + Send + 'static,
    {
        let peer_cancel = self.shutdown.child();
        let chan = self.clone();
        peer_cancel.clone().spawn(start_connect_1(
            chan,
            peer_id,
            transport_fut,
            my_key_cert,
            connection_id,
            post_connect_msgs,
            peer_cancel,
        ));
    }

    /// Connects to `remote_id` via the `transport`.
    pub async fn connect(
        &self,
        remote_id: ServerId,
        transport: Transport,
        my_key_cert: ArcKeyCert,
        connection_id: u128,
        cancel: crate::cancel::CancelManager,
    ) -> anyhow::Result<()> {
        if self.test_factory_state.is_some() {
            return self.connect_test(remote_id, connection_id, cancel).await;
        }
        match transport {
            Transport::Sock(sock) => {
                self.connect_with_sock(sock, my_key_cert, connection_id, cancel)
                    .await
            }
            Transport::Ssh { ssh_host, command } => {
                self.connect_ssh(remote_id, ssh_host, command, connection_id, cancel)
                    .await
            }
            Transport::Io(rw) => self.connect_io(remote_id, rw, connection_id, cancel).await,
        }
    }

    /// Spawn an [`SshRemote`] for a peer reachable via ssh.
    async fn connect_ssh(
        &self,
        peer_id: ServerId,
        ssh_host: String,
        command: Vec<String>,
        connection_id: u128,
        cancel: crate::cancel::CancelManager,
    ) -> anyhow::Result<()> {
        let rx = insert_handle(&self.remote_map, &peer_id, &cancel)?;
        let remote = SshRemote::new(
            peer_id,
            ssh_host,
            command,
            self.remote_msg_tx.clone(),
            rx,
            connection_id,
            cancel.clone(),
        );
        cancel.spawn(remote.run());
        Ok(())
    }

    /// Spawn an [`IoRemote`] for an already-established byte stream
    /// (envoy stdio, ssh stdio).
    async fn connect_io(
        &self,
        peer_id: ServerId,
        rw: ReaderWriter,
        connection_id: u128,
        cancel: crate::cancel::CancelManager,
    ) -> anyhow::Result<()> {
        let rx = insert_handle(&self.remote_map, &peer_id, &cancel)?;
        let remote = IoRemote::new(peer_id, rw, self.remote_msg_tx.clone(), rx, connection_id);
        cancel.spawn(remote.run());
        Ok(())
    }

    /// In-process routing via `TestRemote`.
    pub(crate) async fn connect_test(
        &self,
        peer_id: ServerId,
        connection_id: u128,
        cancel: crate::cancel::CancelManager,
    ) -> anyhow::Result<()> {
        let factory_state = self
            .test_factory_state
            .as_ref()
            .expect("connect_test called without a test factory")
            .clone();

        let rx = insert_handle(&self.remote_map, &peer_id, &cancel)?;
        // Publish the live connection_id so tests can read it back
        // and construct synthetic failure messages that pass the
        // handler's stale-check (see [`FactoryState::peer_connection_id`]).
        factory_state
            .lock()
            .unwrap()
            .connection_ids
            .insert((self.my_hostid.clone(), peer_id.clone()), connection_id);
        let remote = TestRemote {
            peer_id,
            factory_state,
            travel_time: self.test_travel_time.clone(),
            rx,
        };
        cancel.spawn(remote.run());
        Ok(())
    }

    /// Connect using an existing Sock from SignalingClient.
    pub async fn connect_with_sock(
        &self,
        sock: crate::signaling::client::Sock,
        my_key_cert: ArcKeyCert,
        connection_id: u128,
        cancel: crate::cancel::CancelManager,
    ) -> anyhow::Result<()> {
        let peer_id = sock.id().to_string();

        let stream_id = stream_id_init(&self.my_hostid, &peer_id);
        let as_server = stream_id == STREAM_ID_SERVER_INIT;

        tracing::info!(
            "Connecting with sock: my_id={}, peer_id={}, init_stream_id={}",
            self.my_hostid,
            peer_id,
            stream_id
        );

        let rx = insert_handle(&self.remote_map, &peer_id, &cancel)?;

        // Establish connection. If this errors, cleanup is done by
        // the handle in the map, so no need to worry about it here.
        let (sctp_assoc, ice_agent) = self
            .establish_sctp(sock, my_key_cert, as_server, &peer_id)
            .await?;

        let remote = SctpRemote::new(
            peer_id,
            sctp_assoc,
            ice_agent,
            stream_id,
            self.remote_msg_tx.clone(),
            rx,
            connection_id,
            cancel.clone(),
        );
        cancel.spawn(remote.run());
        Ok(())
    }

    /// ICE/DTLS/SCTP establishment. The DTLS conn is kept alive by the
    /// SCTP association’s `net_conn`, so we don’t need to return it.
    async fn establish_sctp(
        &self,
        sock: crate::signaling::client::Sock,
        my_key_cert: ArcKeyCert,
        as_server: bool,
        peer_id: &ServerId,
    ) -> anyhow::Result<(Arc<association::Association>, Arc<webrtc_ice::agent::Agent>)> {
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
                        body: Msg::IceProgress {
                            peer: peer_for_progress.clone(),
                            message: progress.to_string(),
                        },
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

        Ok((sctp_assoc, ice_agent))
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
            // Send to self through the loopback channel. The loopback
            // channel should never be full, if it is, panic and
            // shutdown.
            if let Err(err) = self.loopback_tx.try_send(message) {
                tracing::error!("Fatal: loopback try_send failed: {:?}; shutting down", err);
                self.shutdown.cancel();
                return Err(anyhow!(
                    "Loopback channel is full which shouldn’t happen; shutting down"
                ));
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
                errs.push(peer);
            }
        }
        if !errs.is_empty() {
            return Err(anyhow!("broadcast failed for {} peer(s)", errs.len()));
        }
        Ok(())
    }

    pub fn disconnect(&self, peer: &ServerId) {
        // Removing the handle drops its `DropGuard`, which cancels
        // all the tasks spawned for this peer and runs cleanup.
        self.remote_map.lock().unwrap().remove(peer);
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        // Like disconnect but for every peer.
        self.remote_map.lock().unwrap().clear();
        Ok(())
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
    /// Tags the `ConnectionBroke` we emit on exit so a stale exit
    /// doesn’t kill a newer attempt. See
    /// `RemoteState::connection_id`.
    connection_id: u128,
    /// Used by [`SctpRemote::drop`] to run cleanup code async.
    cancel: crate::cancel::CancelManager,
}

/// Why the actor’s main loop exited.
enum Exit {
    OutgoingErr,
    IncomingClosed,
    SenderDropped,
    PingTimeout,
}

impl SctpRemote {
    fn new(
        peer_id: ServerId,
        sctp_assoc: Arc<association::Association>,
        ice_agent: Arc<webrtc_ice::agent::Agent>,
        stream_id_init: u16,
        remote_msg_tx: mpsc::Sender<Message>,
        rx: mpsc::Receiver<Command>,
        connection_id: u128,
        cancel: crate::cancel::CancelManager,
    ) -> Self {
        let now = now_secs();
        Self {
            peer_id,
            sctp_assoc,
            ice_agent,
            next_stream_id: stream_id_init,
            stream_id_parity: stream_id_init,
            remote_msg_tx,
            rx,
            // saturating_sub so the timer’s immediate first tick fires
            // a ping right away.
            last_ping_sent: now.saturating_sub(PING_INTERVAL_SECS),
            last_ping_recv: Arc::new(AtomicU64::new(now)),
            connection_id,
            cancel,
        }
    }

    /// Run select! in a loop, once the loop exits, send
    /// `ConnectionBroke`. Async teardown of SCTP/DTLS/ICE happens in
    /// Drop handler, so cleanup runs always runs, on normal exit,
    /// error, and future cancellation.
    async fn run(mut self) {
        // Open ping stream symmetrically.
        let ping_stream = match self
            .sctp_assoc
            .open_stream(PING_STREAM_ID, PayloadProtocolIdentifier::Binary)
            .await
        {
            Ok(stream) => stream,
            Err(e) => {
                tracing::warn!(
                    "Failed to open ping stream for {}: {e}",
                    id_short(&self.peer_id)
                );
                let _ = self
                    .remote_msg_tx
                    .send(Message {
                        host: self.peer_id.clone(),
                        body: Msg::ConnectionBroke {
                            peer: self.peer_id.clone(),
                            connection_id: self.connection_id,
                        },
                        req_id: None,
                    })
                    .await;
                return;
            }
        };

        // Real work happens here.
        let _exit = self.run_inner(ping_stream).await;

        // All Exit variants represent an error/broken state; shutdown
        // will not reach here. Send a ConnectionBroke notification
        // before Drop runs cleanup.
        let _ = self
            .remote_msg_tx
            .send(Message {
                host: self.peer_id.clone(),
                body: Msg::ConnectionBroke {
                    peer: self.peer_id.clone(),
                    connection_id: self.connection_id,
                },
                req_id: None,
            })
            .await;
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
                            tracing::warn!("Ping stream closed by {}", id_short(&self.peer_id));
                            return Exit::PingTimeout;
                        }
                        Ok(_) => {
                            self.last_ping_recv.store(now_secs(), Ordering::Relaxed);
                        }
                        Err(err) => {
                            tracing::warn!(
                                "Ping stream read error for {}: {:?}",
                                id_short(&self.peer_id),
                                err
                            );
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
                id_short(&self.peer_id),
                now.saturating_sub(last_recv)
            );
            return Some(Exit::PingTimeout);
        }

        if now.saturating_sub(self.last_ping_sent) < PING_INTERVAL_SECS {
            return None;
        }

        if let Err(e) = ping_stream.write(&bytes::Bytes::from_static(&[0u8])).await {
            tracing::warn!(
                "Failed to write ping byte to {}: {e}",
                id_short(&self.peer_id)
            );
            return Some(Exit::OutgoingErr);
        }
        self.last_ping_sent = now;
        None
    }
}

/// Async teardown of SCTP/DTLS/ICE.
impl Drop for SctpRemote {
    fn drop(&mut self) {
        // `sctp_assoc.shutdown()` waits for ack from the other side
        // and can hang indefinitely; bound it. `sctp_assoc.close()`
        // stops background tasks and closes the transport (DTLS close
        // included). `ice_agent.close()` tears down ICE / mdns last.
        // Spawn-uncancelled so cancellation of the per-peer scope
        // doesn’t also kill the teardown; the task tracker still
        // awaits the tasks on server shutdown.
        let sctp = self.sctp_assoc.clone();
        let ice = self.ice_agent.clone();
        self.cancel.spawn_uncancelled(async move {
            let _ = tokio::time::timeout(Duration::from_millis(500), sctp.shutdown()).await;
            let _ = tokio::time::timeout(Duration::from_secs(2), sctp.close()).await;
            let _ = ice.close().await;
        });
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
                        body: Msg::SerializationErr {
                            reason: self.peer_id.clone(),
                        },
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

        tracing::info!(
            "New stream (#{}) from {}",
            stream_id,
            id_short(&self.peer_id)
        );
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
                        body: Msg::SerializationErr {
                            reason: format!(
                                "Failed to read full length (8 bytes) prefix: only got {} bytes",
                                n
                            ),
                        },
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
                body: Msg::SerializationErr { reason: format!("Failed to read from stream because buffer too short which should never happen").to_string() },
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
                            body: Msg::SerializationErr {
                                reason: format!(
                                    "EOF while reading content: expected {} bytes, got {}",
                                    content_length,
                                    full_buffer.len()
                                ),
                            },
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
                    body: Msg::SerializationErr { reason: format!("Failed to read from stream because buffer too short which should never happen").to_string() },
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
                    body: Msg::SerializationErr {
                        reason: format!("Failed to deserialize message: {}", e),
                    },
                    req_id: None,
                })
                .await;
        }
    }
}

// *** Test transport

/// Simulated network delay for the test transport.
#[derive(Debug, Clone)]
pub enum TravelTime {
    /// Random delay between min and max milliseconds.
    Random(u64, u64),
    /// Fixed delay in milliseconds.
    Ms(u64),
    /// Instant delivery with no delay.
    Instant,
}

/// State for a single host in the test factory
struct HostState {
    inbox: Vec<Message>,
    msg_tx: mpsc::Sender<Message>,
}

/// Shared routing table for the test transport. One per logical
/// network; multiple `WebChannel`s register themselves in it via
/// `WebChannel::new_for_test`.
pub struct FactoryState {
    hosts: HashMap<ServerId, HostState>,
    connection_ids: HashMap<(ServerId, ServerId), u128>,
}

impl FactoryState {
    pub fn new() -> Self {
        Self {
            hosts: HashMap::new(),
            connection_ids: HashMap::new(),
        }
    }

    /// Look up the live `connection_id` registered by the
    /// `TestRemote` on `my_host` for `peer`. Returns `None` if no
    /// actor is currently registered.
    pub fn peer_connection_id(&self, my_host: &ServerId, peer: &ServerId) -> Option<u128> {
        self.connection_ids
            .get(&(my_host.clone(), peer.clone()))
            .copied()
    }

    /// Push a message onto the recipient’s inbox.
    fn push(&mut self, hostid: &ServerId, msg: Message) -> anyhow::Result<()> {
        let host = self
            .hosts
            .get_mut(hostid)
            .ok_or_else(|| anyhow!("Host {} not registered", hostid))?;
        host.inbox.push(msg);
        Ok(())
    }

    /// Drain the recipient’s inbox and clone its msg_tx so the caller
    /// can `send` outside the lock.
    fn drain(
        &mut self,
        hostid: &ServerId,
    ) -> anyhow::Result<(Vec<Message>, mpsc::Sender<Message>)> {
        let host = self
            .hosts
            .get_mut(hostid)
            .ok_or_else(|| anyhow!("Host {} not registered", hostid))?;
        Ok((std::mem::take(&mut host.inbox), host.msg_tx.clone()))
    }
}

impl Default for FactoryState {
    fn default() -> Self {
        Self::new()
    }
}

/// Factory for creating test web channels with in-memory message passing
#[derive(Clone)]
pub struct TestFactory {
    state: Arc<Mutex<FactoryState>>,
    travel_time: TravelTime,
}

impl TestFactory {
    pub fn new(travel_time: TravelTime) -> Self {
        Self {
            state: Arc::new(Mutex::new(FactoryState::new())),
            travel_time,
        }
    }

    /// Build a `WebChannel` wired into this factory.
    pub fn build_channel(
        &self,
        my_hostid: ServerId,
        remote_msg_tx: mpsc::Sender<Message>,
        loopback_tx: mpsc::Sender<Message>,
        shutdown: crate::cancel::CancelManager,
    ) -> WebChannel {
        WebChannel::new_for_test(
            my_hostid,
            remote_msg_tx,
            loopback_tx,
            shutdown,
            self.state.clone(),
            self.travel_time.clone(),
        )
    }

    /// Inject a message directly into `host`’s inbox and deliver it,
    /// bypassing any `TestRemote`. Test-only escape hatch for
    /// synthesizing transport-layer events (e.g. `ConnectionBroke`)
    /// that the in-process test transport doesn’t generate on its own.
    pub async fn inject_message(&self, host: &ServerId, msg: Message) -> anyhow::Result<()> {
        self.state.lock().unwrap().push(host, msg)?;
        TestRemote::deliver(&self.state, host).await
    }

    /// Look up the live `connection_id` registered when
    /// `WebChannel::connect_test` spawned `host`'s actor for
    /// `peer`. Tests need this to construct synthetic failure
    /// messages whose id matches the live state.
    pub fn peer_connection_id(&self, host: &ServerId, peer: &ServerId) -> Option<u128> {
        self.state.lock().unwrap().peer_connection_id(host, peer)
    }
}

/// Per-peer actor for the in-process test transport. Mirrors the
/// `SctpRemote` shape: spawned by `WebChannel::connect_test`, owns its
/// `Command` receiver, exits on `Shutdown` or when all senders drop.
struct TestRemote {
    peer_id: ServerId,
    factory_state: Arc<Mutex<FactoryState>>,
    travel_time: TravelTime,
    rx: mpsc::Receiver<Command>,
}

impl TestRemote {
    async fn run(mut self) {
        loop {
            tokio::select! {
                cmd = self.rx.recv() => match cmd {
                    Some(Command::Msg(msg)) => {
                        if let Err(e) = self.route(msg).await {
                            tracing::warn!(
                                "TestRemote routing to {} failed: {e}",
                                self.peer_id
                            );
                        }
                    }
                    None => return,
                }
            }
        }
    }

    /// Deliver messages from a host's inbox to its msg_tx. Returns
    /// error if inbox is not marked as deliverable. If travel_time is
    /// 0, delivers synchronously. Otherwise spawns a task that waits
    /// for the travel time and then delivers all pending messages.
    async fn route(&self, msg: Message) -> anyhow::Result<()> {
        self.factory_state
            .lock()
            .unwrap()
            .push(&self.peer_id, msg)?;

        let delay_ms = match &self.travel_time {
            TravelTime::Instant => 0,
            TravelTime::Ms(ms) => *ms,
            TravelTime::Random(min, max) => rand::random::<u64>() % (max - min + 1) + min,
        };

        if delay_ms == 0 {
            return Self::deliver(&self.factory_state, &self.peer_id).await;
        }

        let state = self.factory_state.clone();
        let peer = self.peer_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            if let Err(e) = Self::deliver(&state, &peer).await {
                tracing::warn!("Delayed deliver to {peer} failed: {e}");
            }
        });
        Ok(())
    }

    async fn deliver(state: &Arc<Mutex<FactoryState>>, recipient: &ServerId) -> anyhow::Result<()> {
        let (messages, msg_tx) = state.lock().unwrap().drain(recipient)?;
        for msg in messages {
            msg_tx
                .send(msg)
                .await
                .map_err(|_| anyhow!("recipient channel broke for {recipient}"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_id(name: &str) -> ServerId {
        format!("test-{}", name)
    }

    /// Build two test WebChannels bound to a shared `TestFactory`,
    /// drain their dual receivers for the caller.
    fn pair(travel: TravelTime) -> (WebChannel, DualReceiver, WebChannel, DualReceiver) {
        let factory = TestFactory::new(travel);
        let id_a = create_test_id("a");
        let id_b = create_test_id("b");
        let (dual_a, tx_a, self_tx_a) = DualReceiver::new();
        let (dual_b, tx_b, self_tx_b) = DualReceiver::new();
        let chan_a =
            factory.build_channel(id_a, tx_a, self_tx_a, crate::cancel::CancelManager::new());
        let chan_b =
            factory.build_channel(id_b, tx_b, self_tx_b, crate::cancel::CancelManager::new());
        (chan_a, dual_a, chan_b, dual_b)
    }

    #[tokio::test]
    async fn test_sync_send_instant() {
        let (chan_a, _dual_a, chan_b, mut dual_b) = pair(TravelTime::Instant);

        chan_a
            .connect_test(chan_b.hostid(), 1, crate::cancel::CancelManager::new())
            .await
            .unwrap();

        chan_a
            .send(&chan_b.hostid(), None, Msg::FileShared { doc: 42 })
            .await
            .unwrap();

        let msg = dual_b.recv().await.unwrap();
        assert_eq!(msg.host, chan_a.hostid());
        assert!(matches!(msg.body, Msg::FileShared { doc: 42 }));
    }

    #[tokio::test]
    async fn test_sync_send_ms_zero() {
        let (chan_a, _dual_a, chan_b, mut dual_b) = pair(TravelTime::Ms(0));

        chan_a
            .connect_test(chan_b.hostid(), 1, crate::cancel::CancelManager::new())
            .await
            .unwrap();

        chan_a
            .send(&chan_b.hostid(), None, Msg::FileShared { doc: 99 })
            .await
            .unwrap();

        let msg = dual_b.recv().await.unwrap();
        assert_eq!(msg.host, chan_a.hostid());
        assert!(matches!(msg.body, Msg::FileShared { doc: 99 }));
    }

    /// In the actor model there is no `deliverable` flag — a send to
    /// a peer that was never `connect`ed fails at `remote_map` lookup.
    #[tokio::test]
    async fn test_send_without_connect_fails() {
        let (chan_a, _dual_a, chan_b, _dual_b) = pair(TravelTime::Instant);

        // No connect call.
        let result = chan_a
            .send(&chan_b.hostid(), None, Msg::FileShared { doc: 1 })
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Not connected"));
    }

    #[tokio::test]
    async fn test_sync_broadcast() {
        let factory = TestFactory::new(TravelTime::Instant);

        let id_sender = create_test_id("sender");
        let id_recv1 = create_test_id("receiver1");
        let id_recv2 = create_test_id("receiver2");

        let (_dual_sender, tx_sender, self_tx_sender) = DualReceiver::new();
        let (mut dual_recv1, tx_recv1, self_tx_recv1) = DualReceiver::new();
        let (mut dual_recv2, tx_recv2, self_tx_recv2) = DualReceiver::new();

        let chan_sender = factory.build_channel(
            id_sender.clone(),
            tx_sender,
            self_tx_sender,
            crate::cancel::CancelManager::new(),
        );
        let _chan_recv1 = factory.build_channel(
            id_recv1.clone(),
            tx_recv1,
            self_tx_recv1,
            crate::cancel::CancelManager::new(),
        );
        let _chan_recv2 = factory.build_channel(
            id_recv2.clone(),
            tx_recv2,
            self_tx_recv2,
            crate::cancel::CancelManager::new(),
        );

        chan_sender
            .connect_test(id_recv1.clone(), 1, crate::cancel::CancelManager::new())
            .await
            .unwrap();
        chan_sender
            .connect_test(id_recv2.clone(), 1, crate::cancel::CancelManager::new())
            .await
            .unwrap();

        chan_sender
            .broadcast(None, Msg::FileShared { doc: 123 })
            .await
            .unwrap();

        let msg1 = dual_recv1.recv().await.unwrap();
        assert_eq!(msg1.host, id_sender);
        assert!(matches!(msg1.body, Msg::FileShared { doc: 123 }));

        let msg2 = dual_recv2.recv().await.unwrap();
        assert_eq!(msg2.host, id_sender);
        assert!(matches!(msg2.body, Msg::FileShared { doc: 123 }));
    }
}

mod byte_stream;
mod io_remote;
mod ssh_remote;
pub use byte_stream::{frame_read, frame_write, ReaderWriter};
use io_remote::IoRemote;
use ssh_remote::SshRemote;

#[cfg(test)]
mod e2e_tests;
