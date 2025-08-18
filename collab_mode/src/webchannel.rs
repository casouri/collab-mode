//! The next iteration after webrpc. We let go of the request-response
//! abstraction, and lump everything into a single channel. In fact, !
//! multiple remote connections are lumped into a single channel. We !
//! also don't do chunking anymore, instead we create a SCTP stream for
//! every request.
//!
//! We could’ve defined message to take a impl Serialize +
//! Deserialize, but there’s really no need for it. So we just make
//! it take a Msg, it’s convenient and simple.

use crate::ice::{ice_accept, ice_bind, ice_connect};
use crate::message::Msg;
use crate::{config_man::hash_der, signaling::CertDerHash, types::*};
use anyhow::anyhow;
use lsp_server::RequestId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use webrtc_dtls::conn::DTLSConn;
use webrtc_ice::state::ConnectionState;
use webrtc_sctp::chunk::chunk_payload_data::PayloadProtocolIdentifier;
use webrtc_sctp::{association, stream};
use webrtc_util::Conn;

const STREAM_ID_HALF: u16 = 2u16.pow(15);
const STREAM_ID_INIT: u16 = 1;

#[derive(fmt_derive::Debug, fmt_derive::Display)]
pub enum WebchannelError {
    AlreadyAccepting,
}

impl std::error::Error for WebchannelError {}

/// Types of transport. currently only SCTP, we might add webrtc
/// later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransportType {
    SCTP,
}

// We don't have to split into three categories, but this should make
// it easier to manage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub host: ServerId,
    pub body: Msg,
    pub req_id: Option<RequestId>,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeclareProjectEntry {
    /// Absolute path to the project root.
    pub filename: String,
    /// Name of the project.
    pub name: String,
    pub meta: JsonMap,
}

#[derive(Clone)]
pub struct WebChannel {
    my_hostid: ServerId,
    msg_tx: mpsc::Sender<Message>,
    assoc_tx: Arc<Mutex<HashMap<ServerId, mpsc::Sender<Message>>>>,
    next_stream_id: Arc<AtomicU16>,
    /// Wether there’s an accepting thread running already for a
    /// signaling server URL.
    accepting: Arc<Mutex<HashMap<String, bool>>>,
}

impl WebChannel {
    pub fn new(my_hostid: ServerId, msg_tx: mpsc::Sender<Message>) -> Self {
        Self {
            my_hostid,
            msg_tx,
            assoc_tx: Arc::new(Mutex::new(HashMap::new())),
            next_stream_id: Arc::new(AtomicU16::new(STREAM_ID_INIT)),
            accepting: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn hostid(&self) -> ServerId {
        self.my_hostid.clone()
    }

    pub fn active_remotes(&self) -> Vec<ServerId> {
        self.assoc_tx.lock().unwrap().keys().cloned().collect()
    }

    /// Bind on signaling server, and run in a loop and accept
    /// connections. For each connection, setup message handling
    /// threads running in the background.
    pub async fn accept(
        &mut self,
        my_key_cert: ArcKeyCert,
        signaling_addr: &str,
        _transport_type: TransportType,
    ) -> anyhow::Result<()> {
        let accepting = {
            let mut accepting_map = self.accepting.lock().unwrap();
            let accepting = accepting_map
                .get(signaling_addr)
                .map_or(false, |x| x.clone());
            accepting_map.insert(signaling_addr.to_string(), true);
            accepting
        };

        if accepting {
            Err(anyhow!(WebchannelError::AlreadyAccepting))
        } else {
            let res = self.accept_inner(my_key_cert, signaling_addr).await;
            self.accepting
                .lock()
                .unwrap()
                .insert(signaling_addr.to_string(), false);
            res
        }
    }

    pub async fn accept_inner(
        &self,
        my_key_cert: ArcKeyCert,
        signaling_addr: &str,
    ) -> anyhow::Result<()> {
        let mut listener = ice_bind(self.hostid(), my_key_cert, signaling_addr).await?;

        loop {
            let sock = listener.accept().await?;
            let their_cert = sock.cert_hash();
            let remote_hostid = sock.id();

            let self_clone = self.clone();
            let my_key_cert = listener.my_key_cert.clone();
            let msg_tx = self.msg_tx.clone();
            let remote_hostid_clone = remote_hostid.to_string();

            tokio::spawn(async move {
                let conn = ice_accept(sock, None).await;
                if conn.is_err() {
                    tracing::error!("Failed to accept ICE connection: {}", conn.err().unwrap());
                    let _ = msg_tx
                        .send(Message {
                            host: remote_hostid_clone.clone(),
                            body: Msg::ConnectionBroke(remote_hostid),
                            req_id: None,
                        })
                        .await;
                    return;
                }
                let conn = conn.unwrap();

                let dtls_connection = create_dtls_server(conn, my_key_cert, their_cert).await;
                if dtls_connection.is_err() {
                    tracing::error!(
                        "Failed to create DTLS server: {}",
                        dtls_connection.err().unwrap()
                    );
                    let _ = msg_tx
                        .send(Message {
                            host: remote_hostid_clone.clone(),
                            body: Msg::ConnectionBroke(remote_hostid),
                            req_id: None,
                        })
                        .await;
                    return;
                }
                let dtls_connection = dtls_connection.unwrap();

                let sctp_assoc = create_sctp_server(dtls_connection).await;
                if sctp_assoc.is_err() {
                    tracing::error!(
                        "Failed to create SCTP server: {}",
                        sctp_assoc.err().unwrap()
                    );
                    let _ = msg_tx
                        .send(Message {
                            host: remote_hostid_clone.clone(),
                            body: Msg::ConnectionBroke(remote_hostid),
                            req_id: None,
                        })
                        .await;
                    return;
                }
                let sctp_assoc = sctp_assoc.unwrap();

                let res = self_clone
                    .setup_message_handling(&remote_hostid, sctp_assoc, STREAM_ID_HALF)
                    .await;
                if let Err(e) = res {
                    tracing::error!("Failed to setup message handling: {}", e);
                    let _ = msg_tx
                        .send(Message {
                            host: remote_hostid_clone.clone(),
                            body: Msg::ConnectionBroke(remote_hostid),
                            req_id: None,
                        })
                        .await;
                }

                let _ = self_clone
                    .send(
                        &remote_hostid_clone,
                        None,
                        Msg::Hey("Welcom to my server".to_string()),
                    )
                    .await;
            });
        }
    }

    /// Connect to a remote host through signaling server. Setup
    /// message handling threads that runs in the background. Blocks
    /// until the connection is established.
    pub async fn connect(
        &self,
        remote_hostid: ServerId,
        my_hostid: ServerId,
        my_key_cert: ArcKeyCert,
        signaling_addr: &str,
        _transport_type: TransportType,
    ) -> anyhow::Result<()> {
        let (progress_tx, mut progress_rx) = mpsc::channel::<ConnectionState>(1);
        let msg_tx = self.msg_tx.clone();
        let hostid = self.hostid();

        // Spawn a thread that sends ICE progress to editor.
        tokio::spawn(async move {
            while let Some(progress) = progress_rx.recv().await {
                tracing::info!("ICE progress: {progress}");
                let _ = msg_tx
                    .send(Message {
                        host: hostid.clone(),
                        body: Msg::IceProgress(progress.to_string()),
                        req_id: None,
                    })
                    .await;
            }
        });

        let (conn, their_cert) = ice_connect(
            remote_hostid.clone(),
            my_key_cert.clone(),
            signaling_addr,
            Some(progress_tx),
            Some(my_hostid),
        )
        .await?;
        let dtls_connection = create_dtls_client(conn, my_key_cert, their_cert).await?;
        let sctp_assoc = create_sctp_client(dtls_connection).await?;

        self.setup_message_handling(&remote_hostid, sctp_assoc, 0)
            .await?;

        self.send(
            &remote_hostid,
            None,
            Msg::Hey("Nice to meet ya".to_string()),
        )
        .await?;

        Ok(())
    }

    /// Send a message to a remote host. Doesn’t block.
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

        if recipient == &self.my_hostid || recipient == &SERVER_ID_SELF {
            // If recipient is ourselves, send to our own message channel.
            return self
                .msg_tx
                .send(message)
                .await
                .map_err(|_| anyhow!("Channel broke"));
        }

        let tx = self
            .assoc_tx
            .lock()
            .unwrap()
            .get(recipient)
            .cloned()
            .ok_or_else(|| anyhow!("Not connected"))?;

        tx.send(message)
            .await
            .map_err(|_| anyhow!("Channel broke"))?;

        Ok(())
    }

    /// Broadcasts a message to all connected peers. Doesn’t block.
    pub async fn broadcast(&self, req_id: Option<RequestId>, msg: Msg) -> anyhow::Result<()> {
        let message = Message {
            host: self.my_hostid.clone(),
            body: msg,
            req_id,
        };

        for (remote_hostid, tx) in self.assoc_tx.lock().unwrap().iter() {
            tx.send(message.clone())
                .await
                .map_err(|_| anyhow!("Can’t send msg to {}", remote_hostid,))?;
        }

        Ok(())
    }

    /// Create a threads that accepts SCTP streams from remote and read
    /// the message and sends to message channel; create another thread
    /// that reads from outgoing message channel and sends to remote
    /// host.
    ///
    /// `stream_id_base`: If caller is on the accept side, create
    /// stream id from 2^15 + 1 (id_base=2^15); on connect side,
    /// create stream id from 1 (id_base=0).
    async fn setup_message_handling(
        &self,
        remote_hostid: &ServerId,
        sctp_assoc: Arc<association::Association>,
        stream_id_base: u16,
    ) -> anyhow::Result<()> {
        // Create channel for outgoing messages
        let (tx, rx) = mpsc::channel(16);

        let (err_tx, mut err_rx) = mpsc::channel::<()>(1);

        // Store the sender in the association map
        self.assoc_tx
            .lock()
            .unwrap()
            .insert(remote_hostid.clone(), tx);

        // Spawn task to handle outgoing messages
        let sctp_assoc_clone = sctp_assoc.clone();
        let next_stream_id = self.next_stream_id.clone();
        let remote_hostid_clone = remote_hostid.clone();
        let msg_tx_clone = self.msg_tx.clone();

        tokio::spawn(async move {
            handle_outgoing_messages(
                rx,
                sctp_assoc_clone,
                next_stream_id,
                remote_hostid_clone,
                msg_tx_clone,
                stream_id_base,
                err_tx,
            )
            .await;
        });

        // Spawn task to handle incoming streams
        let msg_tx_clone = self.msg_tx.clone();
        let remote_hostid_clone = remote_hostid.to_string();
        let assoc_tx_clone = self.assoc_tx.clone();

        tokio::spawn(async move {
            tokio::select! {
                _ = handle_incoming_messages(
                    sctp_assoc,
                    msg_tx_clone.clone(),
                    remote_hostid_clone.clone(),
                    stream_id_base,
                ) => (),
                _ = err_rx.recv() => (),
            }

            // Clean up when connection closes
            assoc_tx_clone.lock().unwrap().remove(&remote_hostid_clone);

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
// stream for every message to send out. When creating new steam id,
// add `stream_id_base` to it, so the two ends of the connection
// create non-overlapping stream ids.
async fn handle_outgoing_messages(
    mut rx: mpsc::Receiver<Message>,
    sctp_assoc: Arc<association::Association>,
    next_stream_id: Arc<AtomicU16>,
    remote_hostid: ServerId,
    msg_tx: mpsc::Sender<Message>,
    stream_id_base: u16,
    err_tx: mpsc::Sender<()>,
) {
    while let Some(message) = rx.recv().await {
        let stream_id = stream_id_base + next_stream_id.fetch_add(1, Ordering::SeqCst);

        // open_stream only errs when _ourselves_ accept_stream
        // blocked, which shouldn’t happen. So just drop and let
        // setup_message_handling clean things up and send connection
        // broke error.
        let res = sctp_assoc
            .open_stream(stream_id, PayloadProtocolIdentifier::Binary)
            .await;
        if res.is_err() {
            tracing::error!(
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
                tracing::error!("Failed to serialize message: {}", e);
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
            tracing::error!("Failed to write length prefix to stream: {}", e);
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
                tracing::error!("Failed to write data chunk to stream: {}", e);
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
// send to `msg_tx`. `stream_id_base` is the same as the one passed to
// `handle_outgoing_messages`, we use it to determine which streams we
// get are actually from ourselves.
async fn handle_incoming_messages(
    sctp_assoc: Arc<association::Association>,
    msg_tx: mpsc::Sender<Message>,
    remote_hostid: ServerId,
    stream_id_base: u16,
) {
    loop {
        match sctp_assoc.accept_stream().await {
            Some(stream) => {
                let stream_id = stream.stream_identifier();
                // If `stream_id_base` is 2^15, that means we’re the
                // accept side of the overall connection, which means
                // all the streams created by us for sending out
                // messages will have stream id greater than that.
                // Then, if we get a stream with id greater than 2^15,
                // it’s from ourself, so don’t read from it.
                let stream_is_from_us = stream_id_base == 0
                    && stream_id < STREAM_ID_HALF + STREAM_ID_INIT
                    || stream_id_base == STREAM_ID_HALF
                        && stream_id >= STREAM_ID_HALF + STREAM_ID_INIT;

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
            tracing::error!(
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
                tracing::error!(
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

    let _ = stream.shutdown(std::net::Shutdown::Read).await;

    // Deserialize the message
    // match bincode::deserialize::<Message>(&full_buffer) {
    match serde_json::from_slice::<Message>(&full_buffer) {
        Ok(message) => {
            if let Err(e) = msg_tx.send(message).await {
                tracing::error!("Failed to send message to channel: {}", e);
                // No need to send error, the incoming_message handler
                // will detect broken stream.
            }
        }
        Err(e) => {
            tracing::error!(
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

#[cfg(test)]
mod e2e_tests;
