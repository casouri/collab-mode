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
use crate::{config_man::hash_der, signaling::CertDerHash, types::*};
use anyhow::anyhow;
use async_trait::async_trait;
use lsp_server::RequestId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use webrtc_dtls::conn::DTLSConn;
use webrtc_ice::state::ConnectionState;
use webrtc_sctp::chunk::chunk_payload_data::PayloadProtocolIdentifier;
use webrtc_sctp::{association, stream};
use webrtc_util::Conn;

const STREAM_ID_CLIENT_INIT: u16 = 1; // Client uses odd stream IDs: 1, 3, 5, ...
const STREAM_ID_SERVER_INIT: u16 = 2; // Server uses even stream IDs: 2, 4, 6, ...

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
    /// Connect by spawning ssh to `ssh_host` and using its stdio as the
    /// data channel.
    Ssh { ssh_host: String },
    /// Placeholder used by tests where the channel implementation ignores
    /// the transport entirely (e.g. `TestWebChannel`).
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
}

/// Types of transport. currently only SCTP, we might add webrtc
/// later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransportType {
    SCTP,
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

struct RemoteChannels {
    /// Caller uses this tx to send outgoing messages to remotes.
    msg_tx: mpsc::Sender<Message>,
    /// This rx is dropped when this struct is dropped, causing the tx
    /// to receive a message and stop the background task.
    _cancel_rx: mpsc::Receiver<()>,
}

#[derive(Clone)]
pub struct WebChannel {
    my_hostid: ServerId,
    remote_msg_tx: mpsc::Sender<Message>,
    loopback_tx: mpsc::UnboundedSender<Message>,
    assoc_tx: Arc<Mutex<HashMap<ServerId, RemoteChannels>>>,
    next_stream_id: Arc<AtomicU16>,
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
            assoc_tx: Arc::new(Mutex::new(HashMap::new())),
            // Will be set in setup_message_handling to 1 or 2.
            next_stream_id: Arc::new(AtomicU16::new(0)),
        }
    }

    pub fn hostid(&self) -> ServerId {
        self.my_hostid.clone()
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

        // Determine stream ID parity based on host ID ordering
        let stream_id = stream_id_init(&self.my_hostid, &peer_id);

        tracing::info!(
            "Connecting with sock: my_id={}, peer_id={}, init_stream_id={}",
            self.my_hostid,
            peer_id,
            stream_id
        );

        let (progress_tx, mut progress_rx) = mpsc::channel::<ConnectionState>(1);
        let remote_msg_tx = self.remote_msg_tx.clone();
        let hostid = self.hostid();
        let hostid_clone = hostid.clone();
        let peer_id_clone = peer_id.clone();
        let as_server = stream_id_init(&hostid_clone, &peer_id) == 1;

        // Spawn a thread that sends ICE progress to editor. If some
        // thing goes wrong in the following steps, progress_rx.recv
        // will error and this thread will exit.
        tokio::spawn(async move {
            while let Some(progress) = progress_rx.recv().await {
                tracing::info!("ICE progress: {progress}");
                let _ = remote_msg_tx
                    .send(Message {
                        host: hostid.clone(),
                        body: Msg::IceProgress(peer_id_clone.clone(), progress.to_string()),
                        req_id: None,
                    })
                    .await;
            }
        });

        // Establish connection with ICE.
        let ((conn, their_cert, ice_agent), conn_broke_rx) =
            crate::ice::ice_connect_with_sock(sock, Some(progress_tx), as_server).await?;

        let dtls_connection = if as_server {
            create_dtls_server(conn, my_key_cert, their_cert).await?
        } else {
            create_dtls_client(conn, my_key_cert, their_cert).await?
        };
        let sctp_assoc = if as_server {
            create_sctp_server(dtls_connection.clone()).await?
        } else {
            create_sctp_client(dtls_connection.clone()).await?
        };

        // Spawn background tasks to handle messages.
        self.setup_message_handling(
            &peer_id,
            sctp_assoc,
            stream_id,
            conn_broke_rx,
            dtls_connection,
            ice_agent,
        )
        .await?;

        Ok(())
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
            .assoc_tx
            .lock()
            .unwrap()
            .get(recipient)
            .map(|channels| channels.msg_tx.clone())
            .ok_or_else(|| anyhow!("Not connected"))?;

        tx.send(message)
            .await
            .map_err(|_| anyhow!("Channel broke"))?;

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

        // Clone the entries to avoid holding the lock across await.
        let peers: Vec<_> = self
            .assoc_tx
            .lock()
            .unwrap()
            .iter()
            .map(|(hostid, channels)| (hostid.clone(), channels.msg_tx.clone()))
            .collect();

        let mut results = Vec::new();
        for (remote_hostid, tx) in peers {
            let result = tx
                .send(message.clone())
                .await
                .map_err(|_| anyhow!("Can't send msg to {}", remote_hostid));
            results.push(result);
        }

        // Check if any sends failed
        let errors: Vec<_> = results.into_iter().filter_map(|r| r.err()).collect();
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

    /// Create threads that accept SCTP streams from remote and read
    /// the message and sends to message channel; create another thread
    /// that reads from outgoing message channel and sends to remote
    /// host.
    ///
    /// `stream_id_init`: Initial stream ID for this side. One side
    /// uses even IDs (2, 4, 6, ...), the other uses odd IDs (1, 3, 5,
    /// ...). Stream IDs increment by 2 and wrap around on overflow.
    async fn setup_message_handling(
        &self,
        remote_hostid: &ServerId,
        sctp_assoc: Arc<association::Association>,
        stream_id_init: u16,
        conn_broke_rx: oneshot::Receiver<()>,
        dtls_conn: Arc<DTLSConn>,
        ice_agent: Arc<webrtc_ice::agent::Agent>,
    ) -> anyhow::Result<()> {
        // Create channel for outgoing messages
        let (tx, rx) = mpsc::channel(16);
        let (cancel_tx, cancel_rx) = mpsc::channel::<()>(1);
        let (err_tx, mut err_rx) = mpsc::channel::<()>(1);

        // Store the sender in the association map.
        {
            let mut map = self.assoc_tx.lock().unwrap();
            if map.get(remote_hostid).is_some() {
                return Err(anyhow!(WebChannelError::ConnectionExists(
                    remote_hostid.clone()
                )));
            }
            let channels = RemoteChannels {
                msg_tx: tx,
                _cancel_rx: cancel_rx,
            };
            map.insert(remote_hostid.clone(), channels);
        }

        // Initialize the stream ID counter to the initial value for this side.
        // This will be 1 for client (odd IDs) or 2 for server (even IDs).
        self.next_stream_id.store(stream_id_init, Ordering::SeqCst);

        // Spawn task to handle outgoing messages.
        let sctp_assoc_clone = sctp_assoc.clone();
        let next_stream_id = self.next_stream_id.clone();
        let remote_hostid_clone = remote_hostid.clone();
        let msg_tx_clone = self.remote_msg_tx.clone();

        tokio::spawn(async move {
            handle_outgoing_messages(
                rx,
                sctp_assoc_clone,
                next_stream_id,
                remote_hostid_clone,
                msg_tx_clone,
                err_tx,
            )
            .await;
        });

        // Spawn task to handle incoming streams.
        let msg_tx_clone = self.remote_msg_tx.clone();
        let remote_hostid_clone = remote_hostid.to_string();
        let assoc_tx_clone = self.assoc_tx.clone();

        tokio::spawn(async move {
            tokio::select! {
                _ = handle_incoming_messages(
                    sctp_assoc.clone(),
                    msg_tx_clone.clone(),
                    remote_hostid_clone.clone(),
                    stream_id_init,
                ) => (),
                // When the cancel_rx in the map is dropped, this
                // completes and we clean up.
                _ = cancel_tx.closed() => {
                    tracing::info!("Background task for {} exiting because: cancelled", remote_hostid_clone);
                },
                // When there’s an async error, we terminate and clean up.
                _ = err_rx.recv() => {
                    tracing::info!("Background task for {} exiting because: receives error", remote_hostid_clone);
                },
                // Why do we need a channel to know that connection broke?
                // SCTP SHOULD have implemented heartbeat and return None
                // with connection breaks, but doesn't. Which means when
                // connection breaks, accept_stream just hangs
                // indefinitely. To take the matter into our own hands, I
                // added this channel.
                _ = conn_broke_rx => (),
            }

            // Clean up when connection closes
            assoc_tx_clone.lock().unwrap().remove(&remote_hostid_clone);

            let res = sctp_assoc.close().await;
            tracing::info!("SCTP association closed: {:?}", res);
            let res = dtls_conn.close().await;
            tracing::info!("DTLS connection closed: {:?}", res);
            let res = ice_agent.close().await;
            tracing::info!("ICE agent closed: {:?}", res);

            // Connection closed, send ConnectionBroke message
            let _ = msg_tx_clone
                .send(Message {
                    host: remote_hostid_clone.clone(),
                    body: Msg::ConnectionBroke(remote_hostid_clone),
                    req_id: None,
                })
                .await;
        });

        Ok(())
    }
}

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
        _remote_id: ServerId,
        transport: Transport,
        my_key_cert: ArcKeyCert,
    ) -> anyhow::Result<()> {
        match transport {
            Transport::Sock(sock) => self.connect_with_sock(sock, my_key_cert).await,
            Transport::Ssh { .. } => Err(anyhow!("WebChannel does not handle Ssh transports")),
            Transport::Dummy => Err(anyhow!("WebChannel does not handle Dummy transport")),
        }
    }

    async fn broadcast(&self, req_id: Option<RequestId>, msg: Msg) -> anyhow::Result<()> {
        self.broadcast(req_id, msg).await
    }

    fn disconnect(&self, peer: &ServerId) {
        let mut map = self.assoc_tx.lock().unwrap();
        map.remove(peer);
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

// *** Helper functions for WebChannel

// Read messages from `rx` and send them out to remote. Open a new
// stream for every message to send out. Stream IDs increment by 2
// with wrapping to maintain odd/even property, ensuring the two ends
// of the connection create non-overlapping stream IDs.
async fn handle_outgoing_messages(
    mut rx: mpsc::Receiver<Message>,
    sctp_assoc: Arc<association::Association>,
    next_stream_id: Arc<AtomicU16>,
    remote_hostid: ServerId,
    msg_tx: mpsc::Sender<Message>,
    err_tx: mpsc::Sender<()>,
) {
    while let Some(message) = rx.recv().await {
        // Fetch current stream ID and increment by 2 with wrapping.
        // This maintains odd/even property: odd stays odd, even stays even.
        let stream_id = next_stream_id
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                Some(current.wrapping_add(2))
            })
            .unwrap();

        // open_stream only errs when _ourselves_ accept_stream
        // blocked, which shouldn’t happen. So just drop and let
        // setup_message_handling clean things up and send connection
        // broke error.
        let res = sctp_assoc
            .open_stream(stream_id, PayloadProtocolIdentifier::Binary)
            .await;
        if res.is_err() {
            tracing::warn!(
                "Failed to open stream #{}: {}",
                stream_id,
                res.err().unwrap()
            );
            let _ = err_tx.send(()).await;
            break; // Break and drop rx
        }
        let stream = res.unwrap();
        // let data = match bincode::serialize(&message) {
        let data = match serde_json::to_string(&message) {
            Ok(data) => data.into_bytes(),
            Err(e) => {
                // I don’t expect this to happen.
                tracing::warn!("Failed to serialize message: {}", e);
                let _ = msg_tx
                    .send(Message {
                        host: remote_hostid.clone(),
                        body: Msg::SerializationErr(remote_hostid),
                        req_id: None,
                    })
                    .await;
                let _ = err_tx.send(()).await;
                let _ = stream.shutdown(std::net::Shutdown::Both).await;
                break;
            }
        };

        // Write length prefix (8 bytes) followed by data
        let len_bytes = (data.len() as u64).to_be_bytes();
        let len_bytes = bytes::Bytes::from(len_bytes.to_vec());
        if let Err(e) = stream.write(&len_bytes).await {
            tracing::warn!("Failed to write length prefix to stream: {}", e);
            // Let setup_message_handling send the connection broke message.
            let _ = err_tx.send(()).await;
            let _ = stream.shutdown(std::net::Shutdown::Both).await;
            break;
        }

        // Send data in chunks of MAX_FRAME_SIZE
        let mut offset = 0;
        while offset < data.len() {
            let chunk_end = std::cmp::min(offset + MAX_FRAME_SIZE, data.len());
            let chunk = bytes::Bytes::from(data[offset..chunk_end].to_vec());

            if let Err(e) = stream.write(&chunk).await {
                tracing::warn!("Failed to write data chunk to stream: {}", e);
                // Let setup_message_handling send the connection broke message.
                let _ = err_tx.send(()).await;
                let _ = stream.shutdown(std::net::Shutdown::Both).await;
                break;
            }

            offset = chunk_end;
        }
        let _ = stream.shutdown(std::net::Shutdown::Write).await;
    }
}

// Listen for new SCTP streams, each stream is a new message. For each
// new stream, spawn a new thread that reads a message from it and
// send to `msg_tx`. `stream_id_init` is the same as the one passed to
// `handle_outgoing_messages`, we use it to determine which streams we
// get are actually from ourselves by checking odd/even parity.
async fn handle_incoming_messages(
    sctp_assoc: Arc<association::Association>,
    msg_tx: mpsc::Sender<Message>,
    remote_hostid: ServerId,
    stream_id_init: u16,
) {
    loop {
        let stream_result = sctp_assoc.accept_stream().await;
        match stream_result {
            Some(stream) => {
                let stream_id = stream.stream_identifier();
                // Check if stream has same odd/even parity as our init value.
                // If so, it's from ourselves, so don't read from it.
                // Client (init=1, odd) ignores odd streams.
                // Server (init=2, even) ignores even streams.
                let stream_is_from_us = (stream_id % 2) == (stream_id_init % 2);

                if stream_is_from_us {
                    let _ = stream.shutdown(std::net::Shutdown::Read).await;
                    continue;
                }

                let msg_tx = msg_tx.clone();
                let remote_hostid = remote_hostid.clone();

                tracing::info!(
                    "New stream (#{}) from {}",
                    stream.stream_identifier(),
                    remote_hostid
                );
                tokio::spawn(async move {
                    read_from_stream(stream, msg_tx, remote_hostid).await;
                });
            }
            None => {
                // When this function returns, the caller cleans up
                // assoc_tx map and sends connection closed message.
                tracing::info!("No more streams from {}", remote_hostid);
                break;
            }
        }
    }

    // No need to use the shutdown method (graceful shutdown) since
    // something already went wrong.
    if let Err(e) = sctp_assoc.close().await {
        tracing::warn!(
            "Failed to close SCTP association for {}: {}",
            remote_hostid,
            e
        );
    } else {
        tracing::debug!("SCTP association closed successfully for {}", remote_hostid);
    }
}

async fn read_from_stream(
    stream: Arc<stream::Stream>,
    msg_tx: mpsc::Sender<Message>,
    remote_hostid: ServerId,
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
