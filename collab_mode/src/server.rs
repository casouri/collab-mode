use std::collections::HashMap;
use std::fmt::Display;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, atomic::AtomicU32};
use gapbuf::GapBuffer;
use tokio::sync::mpsc;
use crate::config_man::ConfigManager;
use crate::signaling::SignalingError;
use crate::webchannel::{self, Msg, Message as WMsg};
use crate::message::*;
use crate::types::*;
use crate::webchannel::WebChannel;

/// Stores relevant data for a document, used by the server.
#[derive(Debug)]
struct Doc {
    /// Human-readable name for the doc.
    name: String,
    /// Metadata for this doc.
    meta: JsonMap,
    /// The absolute filename of this doc on disk, if exists. This is
    /// used for automatically saving the file to disk. This is also
    /// used for checking whether a file is already opened as a doc.
    abs_filename: Option<PathBuf>,
    /// The server engine that transforms and stores ops for this doc.
    engine: ServerEngine,
    /// The document itself.
    buffer: GapBuffer<char>,
    /// The remote peers that subscribed to new ops of this doc, plus
    /// the last local seq received from them. (New ops they send must
    /// have local seq = 1 + the saved local seq.)
    subscribers: HashMap<SiteId, LocalSeq>,
}

/// Stores relevant for the server.
struct Server {
    /// Host id of this server.
    host_id: ServerId,
    /// SiteId given to ourselves.
    site_id: SiteId,
    /// SiteId given to the next connected client.
    next_site_id: AtomicU32,
    /// Docs hosted by this server.
    docs: HashMap<DocId, Doc>,
    /// Projects.
    projects: Vec<Project>,
    /// Map of recognized users.
    users: HashMap<Credential, SiteId>,
    /// Active remote peers.
    active_remotes: Vec<SiteId>,
    /// Maps host id to site id.
    site_id_map: HashMap<ServerId, SiteId>,
    /// When in attached mode (attached to an editor), list files and
    /// request file operation are delegated to the editor; otherwise
    /// read from disk directly.
    attached: bool,
    /// Whether the server is currently accepting new connections on a
    /// particular signaling server.
    accepting: HashMap<String, ()>,
    /// Configuration manager for the server.
    config: ConfigManager,
    /// Key and certificate for the server.
    key_cert: Arc<KeyCert>,
}

impl Server {
    pub fn new(host_id: ServerId, attached: bool, config: ConfigManager) -> anyhow::Result<Self> {
        let key_cert = config.get_key_and_cert(host_id.clone())?;
        let server = Server {
            host_id,
            site_id: 0,
            next_site_id: AtomicU32::new(1),
            docs: HashMap::new(),
            projects: Vec::new(),
            users: HashMap::new(),
            active_remotes: Vec::new(),
            site_id_map: HashMap::new(),
            attached,
            accepting: HashMap::new(),
            config,
            key_cert: Arc::new(key_cert),
        };
        Ok(server)
    }

    pub async fn run(&mut self, editor_tx: mpsc::Sender<lsp_server::Message>, mut editor_rx: mpsc::Receiver<lsp_server::Message>) -> anyhow::Result<()> {
        let (msg_tx, mut msg_rx) = mpsc::channel::<webchannel::Message>(1);
        let webchanel = WebChannel::new(self.host_id.clone(), msg_tx);

        loop {
            tokio::select! {
                // Handle messages from the editor.
                Some(msg) = editor_rx.recv() => {
                    tracing::info!("Received message from editor: {:?}", msg);
                    self.handle_editor_message(msg, &editor_tx, &webchanel).await;
                },
                // Handle messages from remote peers.
                Some(web_msg) = msg_rx.recv() => {
                    tracing::info!("Received message from remote: {:?}", web_msg);
                    self.handle_remote_message(web_msg, &editor_tx, &webchanel).await;
                },
            }
        }
    }

    async fn handle_editor_message(
        &mut self,
        msg: lsp_server::Message,
        editor_tx: &mpsc::Sender<lsp_server::Message>,
        webchannel: &WebChannel,
    ) -> anyhow::Result<()> {
        match msg {
            lsp_server::Message::Request(req) => {
                let req_id = req.id.clone();
                if let Err(err) = self.handle_editor_request(req, editor_tx, webchannel).await {
                    tracing::error!("Failed to handle editor request: {}", err);
                    let response = lsp_server::Response {
                        id: req_id,
                        result: None,
                        error: Some(lsp_server::ResponseError {
                            code: 30000, // FIXME: Use appropriate error code.
                            message: err.to_string(),
                            data: None,
                        }),
                    };
                    if let Err(err) = editor_tx.send(lsp_server::Message::Response(response)).await {
                        tracing::error!("Failed to send response to editor: {}", err);
                    }
                }
                Ok(())
            },
            lsp_server::Message::Response(resp) => {
                todo!();
            },
            lsp_server::Message::Notification(notif) => {
                if let Err(err) = self.handle_editor_notification(notif, editor_tx, webchannel).await {
                    tracing::error!("Failed to handle editor notification: {}", err);
                    let notif = lsp_server::Notification {
                        method: NotificationCode::Error.to_string(),
                        params: serde_json::json!({
                            "message": err.to_string(),
                        }),
                    };
                    if let Err(err) = editor_tx.send(lsp_server::Message::Notification(notif)).await {
                        tracing::error!("Failed to send notification to editor: {}", err);
                    }
                }
                Ok(())
            },
        }
    }

    async fn handle_editor_request(
        &self,
        req: lsp_server::Request,
        editor_tx: &mpsc::Sender<lsp_server::Message>,
        webchannel: &WebChannel,
    ) -> anyhow::Result<()> {
        match req.method.as_str() {
            "ListFiles" => {
                let params: ListFilesParams = serde_json::from_value(req.params)?;
                // Handle the request...
                Ok(())
            },
            "OpenFile" => {
                let params: OpenFileParams = serde_json::from_value(req.params)?;
                // Handle the request...
                Ok(())
            },
            _ => {
                tracing::warn!("Unknown request method: {}", req.method);
                // TODO: send back an error response.
                Ok(())
            }
        }
    }

    async fn handle_editor_notification(
        &mut self,
        notif: lsp_server::Notification,
        editor_tx: &mpsc::Sender<lsp_server::Message>,
        webchannel: &WebChannel,
    ) -> anyhow::Result<()> {
        match notif.method.as_str() {
            "AcceptConnection" => {
                let params: AcceptConnectionParams = serde_json::from_value(notif.params)?;
                if self.accepting.get(&params.signaling_addr).is_some() {
                    return Ok(());
                }
                self.accepting.insert(params.signaling_addr.clone(), ());
                send_notification(editor_tx, NotificationCode::AcceptingConnection, serde_json::json!({
                    "signaling_addr": params.signaling_addr,
                })).await;
                let key_cert = self.key_cert.clone();
                let mut webchannel_1 = webchannel.clone();
                let editor_tx_1 = editor_tx.clone();

                tokio::spawn(async move {
                    let res = webchannel_1.accept(key_cert, &params.signaling_addr).await;
                    if let Err(err) = res {
                        tracing::warn!("Stopped accepting connection: {}", err);
                        let msg = match err.downcast_ref() {
                            Some(SignalingError::TimesUp(time)) => format!("Allocated signaling time is up ({}s)", time),
                            Some(SignalingError::Closed) => format!("Signaling server closed the connection"),
                            Some(SignalingError::IdTaken(id)) => format!("Host id {} is already taken", id),
                            _ => err.to_string(),
                        };
                        send_notification(&editor_tx_1, NotificationCode::AcceptStopped, serde_json::json!({
                                "reason": msg,
                            })).await;

                    }
                });
            },
            "Connect" => {
                let params: ConnectParams = serde_json::from_value(notif.params)?;
                send_notification(editor_tx, NotificationCode::Connecting, serde_json::json!({
                    "hostId": params.host_id,
                })).await;
                let key_cert = self.key_cert.clone();
                let webchannel_1 = webchannel.clone();
                let editor_tx_1 = editor_tx.clone();

                tokio::spawn(async move {
                    if let Err(err) = webchannel_1.connect(params.host_id.clone(), key_cert, &params.signaling_addr).await {
                        tracing::error!("Failed to connect to {}: {}", params.host_id, err);
                        send_notification(&editor_tx_1, NotificationCode::ConnectionBroke, serde_json::json!({
                            "hostId": params.host_id,
                            "reason": err.to_string(),
                        })).await;
                    }
                });
            },
            _ => {
                tracing::warn!("Unknown notification method: {}", notif.method);
                // TODO: send back an error response.
            }
        }
        Ok(())
    }

    async fn handle_remote_message(
        &self,
        msg: webchannel::Message,
        editor_tx: &mpsc::Sender<lsp_server::Message>,
        webchannel: &WebChannel,
    ) -> anyhow::Result<()> {
        match msg.body {
            Msg::IceProgress(progress) => {
                send_notification(editor_tx, NotificationCode::ConnectionProgress, serde_json::json!({
                    "message": progress,
                })).await;
                Ok(())
            },
            Msg::ConnectionBroke(host_id) => {
                send_notification(editor_tx, NotificationCode::ConnectionBroke, serde_json::json!({
                    "hostId": host_id,
                    "reason": "",
                })).await;
                Ok(())
            },
            Msg::Hey(message) => {
                send_notification(editor_tx, NotificationCode::Hey, serde_json::json!({
                    "hostId": msg.host,
                    "message": message,
                })).await;
                Ok(())
            }
            _ => todo!()
        }
    }
}



// *** Helper functions

async fn send_notification<T: Display>(
    editor_tx: &mpsc::Sender<lsp_server::Message>,
    method: T,
    params: serde_json::Value,
) {
    let notif = lsp_server::Notification {
        method: method.to_string(),
        params,
    };
    if let Err(err) = editor_tx.send(lsp_server::Message::Notification(notif)).await {
        tracing::error!("Failed to send notification to editor: {}", err);
    }
}

async fn send_response(
    editor_tx: &mpsc::Sender<lsp_server::Message>,
    id: lsp_server::RequestId,
    result: Option<serde_json::Value>,
    error: Option<lsp_server::ResponseError>,
) {
    let response = lsp_server::Response {
        id,
        result,
        error,
    };
    if let Err(err) = editor_tx.send(lsp_server::Message::Response(response)).await {
        tracing::error!("Failed to send response to editor: {}", err);
    }
}

async fn send_connection_broke(
    editor_tx: &mpsc::Sender<lsp_server::Message>,
    host_id: ServerId,
    reason: String,
) {
    let notif = lsp_server::Notification {
        method: NotificationCode::ConnectionBroke.to_string(),
        params: serde_json::json!({
            "hostId": host_id,
            "reason": reason,
        }),
    };
    if let Err(err) = editor_tx.send(lsp_server::Message::Notification(notif)).await {
        tracing::error!("Failed to send connection broke notification to editor: {}", err);
    }
}
