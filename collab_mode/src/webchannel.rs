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
use std::sync::Arc;
use tokio::sync::mpsc;
use webrtc_dtls::conn::DTLSConn;
use webrtc_ice::state::ConnectionState;
use webrtc_sctp::association;
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

pub struct WebChannel {
    my_hostid: ServerId,
    msg_tx: mpsc::Sender<Message>,
    assoc_tx: HashMap<ServerId, mpsc::Sender<Message>>,
}

impl WebChannel {
    pub fn new(my_hostid: ServerId, msg_tx: mpsc::Sender<Message>) -> Self {
        Self {
            my_hostid,
            msg_tx,
            assoc_tx: HashMap::new(),
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
        let sock = listener.accept().await?;
        let their_cert = sock.cert_hash();
        let conn = ice_accept(sock, None).await?;
        let dtls_connection =
            create_dtls_server(conn, listener.my_key_cert.clone(), their_cert).await?;
        let sctp_assoc = create_sctp_server(dtls_connection).await?;

        // TODO: Create a mpsc channel and thread, add the tx
        // into self's assoc_tx map, in the thread, receive messages
        // from the rx and open a new channel on sctp_assoc to send
        // the message to remote.

        // TODO: Create another thread that listens on sctp_assoc's
        // accept_stream, for each stream, spawn a new thread that
        // reads messages from the stream and sends them to the msg_tx
        // channel. If anything goes wrong, send a ConnectionBroke
        // message.

        todo!()
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
            remote_hostid,
            my_key_cert.clone(),
            signaling_addr,
            Some(progress_tx),
        )
        .await?;
        let dtls_connection = create_dtls_client(conn, my_key_cert, their_cert).await?;
        let sctp_assoc = create_sctp_client(dtls_connection).await?;

        // TODO: Create a mpsc channel and thread, add the tx
        // into self's assoc_tx map, in the thread, receive messages
        // from the rx and open a new channel on sctp_assoc to send
        // the message to remote.

        // TODO: Create another thread that listens on sctp_assoc's
        // accept_stream, for each stream, spawn a new thread that
        // reads messages from the stream and sends them to the msg_tx
        // channel. If anything goes wrong, send a ConnectionBroke
        // message.

        todo!()
    }

    pub fn send(&self, recipiant: ServerId, msg: Msg) -> anyhow::Result<()> {
        let message = Message {
            host: self.my_hostid.clone(),
            body: msg,
        };
        todo!();

        // TODO: Find the tx for the recipiant by searching in
        // self.assoc_tx, if not found, return an error, if found,
        // send the message.
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
