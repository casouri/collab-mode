//! The next iteration after webrpc. We let go of the request-response
//! abstraction, and lump everything into a single channel. In fact, !
//! multiple remote connections are lumped into a single channel. We !
//! also don't do chunking anymore, instead we create a SCTP stream for
//! every request.

use crate::ice::{ice_accept, ice_bind, ice_connect};
use crate::{config_man::hash_der, signaling::CertDerHash, types::*};
use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU16, Ordering};
use tokio::sync::mpsc;
use webrtc_dtls::conn::DTLSConn;
use webrtc_ice::state::ConnectionState;
use webrtc_sctp::{association, stream};
use webrtc_sctp::chunk::chunk_payload_data::PayloadProtocolIdentifier;
use webrtc_util::Conn;

// We don't have to split into three categories, but this should make
// it easier to manage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    host: ServerId,
    body: Msg,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Msg {
    ShareSingleFile {
        filename: String,
        meta: String,
        content: FileContentOrPath,
    },
    FileShared(DocId),
    ListFiles {
        project: Option<FilePath>,
    },
    FileList(Vec<ListFilesEntry>),
    DeclareProjects(Vec<DeclareProjectEntry>),
    OpFromClient(ContextOps),
    OpFromServer(Vec<FatOp>),
    RecvOpAndInfo {
        doc: DocId,
        after: GlobalSeq,
    },
    RequestFile(DocDesc),
    Snapshot(Snapshot),
    TakeDownFile(DocId),
    ResetFile(DocId),
    Login(Credential),
    LoggedYouIn(SiteId),
    Info(DocId, Info),

    // Misc
    IceProgress(String),

    // Errors
    ConnectionBroke(ServerId),
    StopSendingOps(DocId),
    DeserializationError(String),
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

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListFilesEntry {
    pub doc: FileDesc,
    pub filename: String,
    pub meta: String,
}

#[derive(Clone)]
pub struct WebChannel {
    my_hostid: ServerId,
    msg_tx: mpsc::Sender<Message>,
    assoc_tx: Arc<Mutex<HashMap<ServerId, mpsc::Sender<Message>>>>,
    next_stream_id: Arc<AtomicU16>,
}

impl WebChannel {
    pub fn new(my_hostid: ServerId, msg_tx: mpsc::Sender<Message>) -> Self {
        Self {
            my_hostid,
            msg_tx,
            assoc_tx: Arc::new(Mutex::new(HashMap::new())),
            next_stream_id: Arc::new(AtomicU16::new(1)),
        }
    }

    pub fn hostid(&self) -> ServerId {
        self.my_hostid.clone()
    }

    pub async fn accept(
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
            let listener_key_cert = listener.my_key_cert.clone();
            let msg_tx = self.msg_tx.clone();

            tokio::spawn(async move {
                let conn = ice_accept(sock, None).await;
                if conn.is_err() {
                    tracing::error!("Failed to accept ICE connection: {}", conn.err().unwrap());
                    let _ = msg_tx.send(Message {
                        host: remote_hostid.clone(),
                        body: Msg::ConnectionBroke(remote_hostid),
                    }).await;
                    return;
                }
                let conn = conn.unwrap();

                let dtls_connection = create_dtls_server(conn, listener_key_cert, their_cert).await;
                if dtls_connection.is_err() {
                    tracing::error!("Failed to create DTLS server: {}", dtls_connection.err().unwrap());
                    let _ = msg_tx.send(Message {
                        host: remote_hostid.clone(),
                        body: Msg::ConnectionBroke(remote_hostid),
                    }).await;
                    return;
                }
                let dtls_connection = dtls_connection.unwrap();

                let sctp_assoc = create_sctp_server(dtls_connection).await;
                if sctp_assoc.is_err() {
                    tracing::error!("Failed to create SCTP server: {}", sctp_assoc.err().unwrap());
                    let _ = msg_tx.send(Message {
                        host: remote_hostid.clone(),
                        body: Msg::ConnectionBroke(remote_hostid),
                    }).await;
                    return;
                }
                let sctp_assoc = sctp_assoc.unwrap();

                if let Err(e) = self_clone.setup_message_handling(remote_hostid.clone(), sctp_assoc, 2u16.pow(15)).await {
                    tracing::error!("Failed to setup message handling: {}", e);
                    let _ = msg_tx.send(Message {
                        host: remote_hostid.clone(),
                        body: Msg::ConnectionBroke(remote_hostid),
                    }).await;
                }
            });
        }
    }

    pub async fn connect(
        &self,
        remote_hostid: ServerId,
        my_key_cert: ArcKeyCert,
        signaling_addr: &str,
    ) -> anyhow::Result<()> {
        let (progress_tx, mut progress_rx) = mpsc::channel::<ConnectionState>(1);
        let msg_tx = self.msg_tx.clone();
        let hostid_clone = self.my_hostid.clone();

        tokio::spawn(async move {
            while let Some(progress) = progress_rx.recv().await {
                tracing::info!("ICE progress: {progress}");
                let _ = msg_tx
                    .send(Message {
                        host: hostid_clone.clone(),
                        body: Msg::IceProgress(progress.to_string()),
                    })
                    .await;
            }
        });

        let (conn, their_cert) = ice_connect(
            remote_hostid.clone(),
            my_key_cert.clone(),
            signaling_addr,
            Some(progress_tx),
        )
        .await?;
        let dtls_connection = create_dtls_client(conn, my_key_cert, their_cert).await?;
        let sctp_assoc = create_sctp_client(dtls_connection).await?;

        self.setup_message_handling(remote_hostid, sctp_assoc, 0).await?;

        Ok(())
    }

    pub async fn send(&self, recipient: ServerId, msg: Msg) -> anyhow::Result<()> {
        let message = Message {
            host: self.my_hostid.clone(),
            body: msg,
        };

        let tx = self.assoc_tx
            .lock()
            .unwrap()
            .get(&recipient)
            .cloned()
            .ok_or_else(|| anyhow!("Recipient {} not connected", recipient))?;

        tx.send(message).await
            .map_err(|_| anyhow!("Failed to send message to {}", recipient))?;

        Ok(())
    }

    // On accept side, create stream id from 2^15, on connect side,
    // create stream id from 0.
    async fn setup_message_handling(
        &self,
        remote_hostid: ServerId,
        sctp_assoc: Arc<association::Association>,
        stream_id_base: u16,
    ) -> anyhow::Result<()> {
        // Create channel for outgoing messages
        let (tx, rx) = mpsc::channel(16);

        // Store the sender in the association map
        self.assoc_tx.lock().unwrap().insert(remote_hostid.clone(), tx);

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
            ).await;
        });

        // Spawn task to handle incoming streams
        let msg_tx_clone = self.msg_tx.clone();
        let remote_hostid_clone = remote_hostid.clone();
        let assoc_tx_clone = self.assoc_tx.clone();

        tokio::spawn(async move {
            handle_incoming_streams(
                sctp_assoc,
                msg_tx_clone,
                remote_hostid_clone.clone(),
            ).await;

            // Clean up when connection closes
            assoc_tx_clone.lock().unwrap().remove(&remote_hostid_clone);
        });

        Ok(())
    }
}

// *** DTLS and SCTP

const MAX_FRAME_BODY_SIZE: usize = 32 * 1024;
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

async fn handle_outgoing_messages(
    mut rx: mpsc::Receiver<Message>,
    sctp_assoc: Arc<association::Association>,
    next_stream_id: Arc<AtomicU16>,
    remote_hostid: ServerId,
    msg_tx: mpsc::Sender<Message>,
    stream_id_base: u16,
) {
    while let Some(message) = rx.recv().await {
        let stream_id = stream_id_base + next_stream_id.fetch_add(2, Ordering::SeqCst);

        let res = sctp_assoc.open_stream(stream_id, PayloadProtocolIdentifier::Binary).await;
        if res.is_err() {
            tracing::error!("Failed to open stream: {}, breaking connection", res.err().unwrap());
            // Send ConnectionBroke message
            let _ = msg_tx.send(Message {
                host: remote_hostid.clone(),
                body: Msg::ConnectionBroke(remote_hostid),
            }).await;
            break; // Break and drop rx
        }
        let stream = res.unwrap();
        let data = match bincode::serialize(&message) {
            Ok(data) => data,
            Err(e) => {
                tracing::error!("Failed to serialize message: {}", e);
                let _ = msg_tx.send(Message {
                    host: remote_hostid.clone(),
                    body: Msg::ConnectionBroke(remote_hostid),
                }).await;
                break;
            }
        };

        // Write length prefix (8 bytes) followed by data
        let len_bytes = (data.len() as u64).to_be_bytes();
        let len_bytes = bytes::Bytes::from(len_bytes.to_vec());
        if let Err(e) = stream.write(&len_bytes).await {
            tracing::error!("Failed to write length prefix to stream: {}", e);
            continue;
        }

        // Send data in chunks of MAX_FRAME_SIZE
        let mut offset = 0;
        while offset < data.len() {
            let chunk_end = std::cmp::min(offset + MAX_FRAME_SIZE, data.len());
            let chunk = bytes::Bytes::from(data[offset..chunk_end].to_vec());

            if let Err(e) = stream.write(&chunk).await {
                tracing::error!("Failed to write data chunk to stream: {}", e);
                break;
            }

            offset = chunk_end;
        }
    }
}

async fn handle_incoming_streams(
    sctp_assoc: Arc<association::Association>,
    msg_tx: mpsc::Sender<Message>,
    remote_hostid: ServerId,
) {
    loop {
        match sctp_assoc.accept_stream().await {
            Some(stream) => {
                let msg_tx = msg_tx.clone();
                let remote_hostid = remote_hostid.clone();

                tokio::spawn(async move {
                    read_from_stream(stream, msg_tx, remote_hostid).await;
                });
            }
            None => {
                tracing::info!("No more streams from {}", remote_hostid);
                break;
            }
        }
    }

    // Connection closed, send ConnectionBroke message
    let _ = msg_tx.send(Message {
        host: remote_hostid.clone(),
        body: Msg::ConnectionBroke(remote_hostid),
    }).await;
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
                let _ = msg_tx.send(Message {
                    host: remote_hostid.clone(),
                    body: Msg::DeserializationError(
                        format!("Failed to read full length prefix: only got {} bytes", n)
                    ),
                }).await;
                return;
            }
        }
        Err(e) => {
            tracing::error!("Failed to read length from stream from {}: {}", remote_hostid, e);
            let _ = msg_tx.send(Message {
                host: remote_hostid.clone(),
                body: Msg::ConnectionBroke(remote_hostid),
            }).await;
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
                    let _ = msg_tx.send(Message {
                        host: remote_hostid.clone(),
                        body: Msg::DeserializationError(
                            format!("EOF while reading content: expected {} bytes, got {}", content_length, full_buffer.len())
                        ),
                    }).await;
                    return;
                }
                full_buffer.extend_from_slice(&chunk_buffer[..n]);
            }
            Err(e) => {
                tracing::error!("Failed to read content from stream from {}: {}", remote_hostid, e);
                let _ = msg_tx.send(Message {
                    host: remote_hostid.clone(),
                    body: Msg::ConnectionBroke(remote_hostid),
                }).await;
                return;
            }
        }
    }

    // Deserialize the message
    match bincode::deserialize::<Message>(&full_buffer) {
        Ok(message) => {
            if let Err(e) = msg_tx.send(message).await {
                tracing::error!("Failed to send message to channel: {}", e);
            }
        }
        Err(e) => {
            tracing::error!("Failed to deserialize message from {}: {}", remote_hostid, e);
            let _ = msg_tx.send(Message {
                host: remote_hostid.clone(),
                body: Msg::DeserializationError(format!("Failed to deserialize message: {}", e)),
            }).await;
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod e2e_tests;
