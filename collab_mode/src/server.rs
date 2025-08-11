use crate::config_man::ConfigManager;
use crate::message::{self, *};
use crate::signaling::SignalingError;
use crate::types::*;
use crate::webchannel::WebChannel;
use crate::webchannel::{self, Message as WMsg, Msg};
use anyhow::{anyhow, Context};
use gapbuf::GapBuffer;
use serde::Serialize;
use std::collections::HashMap;
use std::fmt::Display;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicI32, AtomicU32, Ordering},
    Arc, Mutex,
};
use tokio::sync::mpsc;

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

/// When remote sends a request and we delegate it to the editor, we
/// store the original request so we can route the response back to
/// the remote.
struct OrigRequest {
    req_id: lsp_server::RequestId,
    method: String,
    host_id: ServerId,
}

/// Stores relevant for the server.
pub struct Server {
    /// Host id of this server.
    host_id: ServerId,
    /// SiteId given to ourselves.
    site_id: SiteId,
    /// SiteId given to the next connected client.
    next_site_id: AtomicU32,
    /// Next request id to be used for requests to editor.
    next_req_id: AtomicI32,
    /// Docs hosted by this server.
    docs: HashMap<DocId, Doc>,
    /// Projects.
    projects: HashMap<ProjectId, Project>,
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
    /// If we get a remote request with a request id and we send a
    /// request to editor to handle, the request id of the request to
    /// editor maps to the request id of the received request in this
    /// map. Also record the method name of the request sent to the
    /// editor, and the remote host.
    orig_req_map: HashMap<i32, OrigRequest>,
}

impl Server {
    pub fn new(host_id: ServerId, attached: bool, config: ConfigManager) -> anyhow::Result<Self> {
        let key_cert = config.get_key_and_cert(host_id.clone())?;
        let server = Server {
            host_id,
            site_id: 0,
            next_site_id: AtomicU32::new(1),
            next_req_id: AtomicI32::new(1),
            docs: HashMap::new(),
            projects: HashMap::new(),
            users: HashMap::new(),
            active_remotes: Vec::new(),
            site_id_map: HashMap::new(),
            attached,
            accepting: HashMap::new(),
            config,
            key_cert: Arc::new(key_cert),
            orig_req_map: HashMap::new(),
        };
        Ok(server)
    }

    pub async fn run(
        &mut self,
        editor_tx: mpsc::Sender<lsp_server::Message>,
        mut editor_rx: mpsc::Receiver<lsp_server::Message>,
    ) -> anyhow::Result<()> {
        let (msg_tx, mut msg_rx) = mpsc::channel::<webchannel::Message>(1);
        let webchannel = WebChannel::new(self.host_id.clone(), msg_tx);

        loop {
            tokio::select! {
                // Handle messages from the editor.
                Some(msg) = editor_rx.recv() => {
                    tracing::info!("Received message from editor: {:?}", msg);
                    self.handle_editor_message(msg, &editor_tx, &webchannel).await;
                },
                // Handle messages from remote peers.
                Some(web_msg) = msg_rx.recv() => {
                    tracing::info!("Received message from remote: {:?}", web_msg);
                    self.handle_remote_message(web_msg, &editor_tx, &webchannel).await;
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
                    if let Err(err) = editor_tx
                        .send(lsp_server::Message::Response(response))
                        .await
                    {
                        tracing::error!("Failed to send response to editor: {}", err);
                    }
                }
                Ok(())
            }
            lsp_server::Message::Response(resp) => {
                let resp_id = unwrap_req_id(&resp.id);
                if resp_id.is_err() {
                    tracing::error!("Failed to parse request id: {:?}", &resp_id);
                    send_notification(
                        editor_tx,
                        NotificationCode::Error,
                        serde_json::json!({
                            "message": format!("Failed to parse request id: {}", resp_id.unwrap_err()),
                        }),
                    )
                        .await;
                    return Ok(());
                }
                let resp_id = resp_id.unwrap();
                let orig_req = self.orig_req_map.remove(&resp_id);
                if orig_req.is_none() {
                    tracing::warn!(
                        "Received ListFiles response without a matching request id: {}, ignoring",
                        resp_id
                    );
                    return Ok(());
                }
                let orig_req = orig_req.unwrap();
                if let Err(err) = self
                    .handle_editor_response(orig_req, resp.result, editor_tx, webchannel)
                    .await
                {
                    match err.downcast_ref::<serde_json::Error>() {
                        Some(err) => {
                            send_notification(
                                editor_tx,
                                NotificationCode::Error,
                                serde_json::json!({
                                    "message": format!("Failed to parse response: {}", err),
                                }),
                            )
                            .await;
                        }
                        None => (),
                    }
                    tracing::error!("Failed to handle editor response: {}", err);
                    // If it’s not a parsing error, it’s some error
                    // sending the response back to remote, don’t need
                    // to bother our editor in that case.
                }
                Ok(())
            }
            lsp_server::Message::Notification(notif) => {
                if let Err(err) = self
                    .handle_editor_notification(notif, editor_tx, webchannel)
                    .await
                {
                    tracing::error!("Failed to handle editor notification: {}", err);
                    let notif = lsp_server::Notification {
                        method: NotificationCode::Error.to_string(),
                        params: serde_json::json!({
                            "message": err.to_string(),
                        }),
                    };
                    if let Err(err) = editor_tx
                        .send(lsp_server::Message::Notification(notif))
                        .await
                    {
                        tracing::error!("Failed to send notification to editor: {}", err);
                    }
                }
                Ok(())
            }
        }
    }

    async fn handle_editor_request(
        &mut self,
        req: lsp_server::Request,
        editor_tx: &mpsc::Sender<lsp_server::Message>,
        webchannel: &WebChannel,
    ) -> anyhow::Result<()> {
        match req.method.as_str() {
            "ListFiles" => {
                let params: message::ListFilesParams = serde_json::from_value(req.params)?;
                // Either sends a request to remote or sends a
                // response to editor.
                self.list_files_from_editor(
                    editor_tx,
                    webchannel,
                    params.host_id,
                    params.dir,
                    req.id,
                )
                .await?;
                Ok(())
            }
            "OpenFile" => {
                let params: OpenFileParams = serde_json::from_value(req.params)?;
                // Handle the request...
                Ok(())
            }
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
                send_notification(
                    editor_tx,
                    NotificationCode::AcceptingConnection,
                    serde_json::json!({
                        "signaling_addr": params.signaling_addr,
                    }),
                )
                .await;
                let key_cert = self.key_cert.clone();
                let mut webchannel_1 = webchannel.clone();
                let editor_tx_1 = editor_tx.clone();

                tokio::spawn(async move {
                    let res = webchannel_1
                        .accept(key_cert, &params.signaling_addr, params.transport_type)
                        .await;
                    if let Err(err) = res {
                        tracing::warn!("Stopped accepting connection: {}", err);
                        let msg = match err.downcast_ref() {
                            Some(SignalingError::TimesUp(time)) => {
                                format!("Allocated signaling time is up ({}s)", time)
                            }
                            Some(SignalingError::Closed) => {
                                format!("Signaling server closed the connection")
                            }
                            Some(SignalingError::IdTaken(id)) => {
                                format!("Host id {} is already taken", id)
                            }
                            _ => err.to_string(),
                        };
                        send_notification(
                            &editor_tx_1,
                            NotificationCode::AcceptStopped,
                            serde_json::json!({
                                "reason": msg,
                            }),
                        )
                        .await;
                    }
                });
            }
            "Connect" => {
                let params: ConnectParams = serde_json::from_value(notif.params)?;
                self.connect(
                    editor_tx,
                    webchannel,
                    params.host_id,
                    params.signaling_addr,
                    params.transport_type,
                )
                .await;
            }
            _ => {
                tracing::warn!("Unknown notification method: {}", notif.method);
                // TODO: send back an error response.
            }
        }
        Ok(())
    }

    async fn handle_editor_response(
        &mut self,
        orig_req: OrigRequest,
        result: Option<serde_json::Value>,
        editor_tx: &mpsc::Sender<lsp_server::Message>,
        webchannel: &WebChannel,
    ) -> anyhow::Result<()> {
        match orig_req.method.as_str() {
            "ListFiles" => {
                let resp: ListFilesResp = serde_json::from_value(result.unwrap_or_default())
                    .with_context(|| "Failed to parse ListFiles response")?;
                webchannel
                    .send(
                        &orig_req.host_id,
                        Some(orig_req.req_id),
                        Msg::FileList(resp.files),
                    )
                    .await?;
            }
            _ => {
                tracing::warn!(
                    "Unknown method read from editor response: {}",
                    orig_req.method
                );
            }
        }
        Ok(())
    }
    async fn handle_remote_message(
        &mut self,
        msg: webchannel::Message,
        editor_tx: &mpsc::Sender<lsp_server::Message>,
        webchannel: &WebChannel,
    ) -> anyhow::Result<()> {
        match msg.body {
            Msg::IceProgress(progress) => {
                send_notification(
                    editor_tx,
                    NotificationCode::ConnectionProgress,
                    serde_json::json!({
                        "message": progress,
                    }),
                )
                .await;
                Ok(())
            }
            Msg::ConnectionBroke(host_id) => {
                send_notification(
                    editor_tx,
                    NotificationCode::ConnectionBroke,
                    serde_json::json!({
                        "hostId": host_id,
                        "reason": "",
                    }),
                )
                .await;
                Ok(())
            }
            Msg::Hey(message) => {
                send_notification(
                    editor_tx,
                    NotificationCode::Hey,
                    serde_json::json!({
                        "hostId": msg.host,
                        "message": message,
                    }),
                )
                .await;
                Ok(())
            }
            Msg::ListFiles { dir } => {
                if let Some(req_id) = msg.req_id {
                    self.list_files_from_remote(
                        editor_tx,
                        webchannel,
                        dir,
                        req_id,
                        msg.host.clone(),
                    )
                    .await?;
                } else {
                    tracing::warn!("Received ListFiles without req_id, ignoring.");
                    webchannel
                        .send(
                            &msg.host,
                            None,
                            Msg::BadRequest("Missing req_id".to_string()),
                        )
                        .await?;
                }
                Ok(())
            }
            _ => todo!(),
        }
    }

    // **** Handler functions

    async fn connect(
        &mut self,
        editor_tx: &mpsc::Sender<lsp_server::Message>,
        webchannel: &WebChannel,
        host_id: ServerId,
        signaling_addr: String,
        transpot_type: webchannel::TransportType,
    ) {
        if host_id == self.host_id {
            return;
        }
        send_notification(
            editor_tx,
            NotificationCode::Connecting,
            serde_json::json!({
                "hostId": host_id,
            }),
        )
        .await;
        let key_cert = self.key_cert.clone();
        let webchannel_1 = webchannel.clone();
        let editor_tx_1 = editor_tx.clone();

        tokio::spawn(async move {
            if let Err(err) = webchannel_1
                .connect(host_id.clone(), key_cert, &signaling_addr, transpot_type)
                .await
            {
                tracing::error!("Failed to connect to {}: {}", host_id, err);
                send_notification(
                    &editor_tx_1,
                    NotificationCode::ConnectionBroke,
                    serde_json::json!({
                        "hostId": host_id,
                        "reason": err.to_string(),
                    }),
                )
                .await;
            }
        });
    }

    // If host_id is not us, delegate to remote, if it’s us, handle
    // ourselves: if in attached mode, respond with empty because
    // editor should be the source of truth so why ur asking me man??
    // In non-attached mode read files from disk. If dir is None, list
    // top-level projects and docs, if dir non-nil, list files in that
    // dir.
    async fn list_files_from_editor(
        &mut self,
        editor_tx: &mpsc::Sender<lsp_server::Message>,
        webchannel: &WebChannel,
        host_id: ServerId,
        dir: Option<ProjectFile>,
        req_id: lsp_server::RequestId,
    ) -> anyhow::Result<()> {
        if self.host_id != host_id {
            webchannel
                .send(&host_id, Some(req_id), Msg::ListFiles { dir })
                .await?;
            return Ok(());
        }
        // Host id is ourselves:
        // It doesn’t make sense to list files for ourselves in attached mode.
        if self.attached {
            send_response(editor_tx, req_id, ListFilesResp { files: vec![] }, None).await;
            return Ok(());
        }

        let files = self
            .list_files_from_dist(dir)
            .await
            .with_context(|| "Failed to list files from disk")?;

        send_response(editor_tx, req_id, ListFilesResp { files }, None).await;

        Ok(())
    }

    // In attached mode, delegate to editor, otherwise read from disk
    // and response immediately.
    async fn list_files_from_remote(
        &mut self,
        editor_tx: &mpsc::Sender<lsp_server::Message>,
        webchannel: &WebChannel,
        dir: Option<ProjectFile>,
        req_id: lsp_server::RequestId,
        remote_host_id: ServerId,
    ) -> anyhow::Result<()> {
        let id = self.next_req_id.fetch_add(1, Ordering::SeqCst);
        if self.attached {
            self.orig_req_map.insert(
                id,
                OrigRequest {
                    req_id,
                    method: "ListFiles".to_string(),
                    host_id: remote_host_id.clone(),
                },
            );
            send_request(
                editor_tx,
                id,
                "ListFiles",
                ListFilesParams {
                    dir,
                    host_id: "".to_string(),
                    signaling_addr: "".to_string(),
                    credential: "".to_string(),
                },
            )
            .await;
        } else {
            // Not in attached mode, read files from disk.
            let files = self
                .list_files_from_dist(dir)
                .await
                .with_context(|| "Failed to list files from disk")?;
            webchannel
                .send(&remote_host_id, Some(req_id), Msg::FileList(files))
                .await?;
        }
        Ok(())
    }

    async fn list_files_from_dist(
        &self,
        dir: Option<ProjectFile>,
    ) -> anyhow::Result<Vec<ListFileEntry>> {
        // Not in attached mode, read files from disk.
        if let Some((project_id, rel_filename)) = dir {
            // List files in a project directory.
            let project = self
                .projects
                .get(&project_id)
                .ok_or_else(|| anyhow!("Project {} not found", project_id))?;

            let filename = std::path::PathBuf::from(project.root.clone()).join(rel_filename);
            let filename_str = filename.to_string_lossy().to_string();
            let stat = std::fs::metadata(&filename)
                .with_context(|| format!("Can't access file {}", &filename_str))?;

            if !stat.is_dir() {
                return Err(anyhow!(format!("Not a directory: {}", filename_str)));
            }

            let mut result = vec![];
            let files = std::fs::read_dir(&filename)
                .with_context(|| format!("Can't access file {}", &filename_str))?;
            for file in files {
                let file =
                    file.with_context(|| format!("Can't access files in {}", &filename_str))?;
                let file_str = file.file_name().to_string_lossy().to_string();
                let stat = file
                    .metadata()
                    .with_context(|| format!("Can't access file {}", &file_str))?;
                let file_rel = pathdiff::diff_paths(&project.root, &file_str).unwrap();
                result.push(ListFileEntry {
                    file: FileDesc::ProjectFile((
                        project_id.clone(),
                        file_rel.to_string_lossy().to_string(),
                    )),
                    filename: file.file_name().to_string_lossy().to_string(),
                    is_directory: stat.is_dir(),
                    meta: JsonMap::new(),
                });
            }
            return Ok(result);
        } else {
            // List top-level projects and docs.
            let mut result = vec![];
            for (_, project) in self.projects.iter() {
                result.push(ListFileEntry {
                    file: FileDesc::Project(project.root.clone()),
                    filename: project.name.clone(),
                    is_directory: true,
                    meta: project.meta.clone(),
                });
            }
            for (docid, doc) in self.docs.iter() {
                result.push(ListFileEntry {
                    file: FileDesc::File(docid.clone()),
                    is_directory: false,
                    filename: doc.name.clone(),
                    meta: doc.meta.clone(),
                });
            }
            return Ok(result);
        }
    }
}

// *** Helper functions

pub async fn send_notification<T: Display>(
    editor_tx: &mpsc::Sender<lsp_server::Message>,
    method: T,
    params: serde_json::Value,
) {
    let notif = lsp_server::Notification {
        method: method.to_string(),
        params,
    };
    if let Err(err) = editor_tx
        .send(lsp_server::Message::Notification(notif))
        .await
    {
        tracing::error!("Failed to send notification to editor: {}", err);
    }
}

async fn send_response(
    editor_tx: &mpsc::Sender<lsp_server::Message>,
    id: lsp_server::RequestId,
    result: impl Serialize,
    error: Option<lsp_server::ResponseError>,
) {
    let response = lsp_server::Response {
        id,
        result: Some(serde_json::to_value(result).unwrap()),
        error,
    };
    if let Err(err) = editor_tx
        .send(lsp_server::Message::Response(response))
        .await
    {
        tracing::error!("Failed to send response to editor: {}", err);
    }
}

async fn send_request(
    editor_tx: &mpsc::Sender<lsp_server::Message>,
    req_id: i32,
    method: &str,
    params: impl Serialize,
) {
    let request = lsp_server::Request {
        id: lsp_server::RequestId::from(req_id),
        method: method.to_string(),
        params: serde_json::to_value(params).unwrap(),
    };
    if let Err(err) = editor_tx.send(lsp_server::Message::Request(request)).await {
        tracing::error!("Failed to send request to editor: {}", err);
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
    if let Err(err) = editor_tx
        .send(lsp_server::Message::Notification(notif))
        .await
    {
        tracing::error!(
            "Failed to send connection broke notification to editor: {}",
            err
        );
    }
}

/// Convert a JSONRPC request id to an i32.
fn unwrap_req_id(req_id: &lsp_server::RequestId) -> anyhow::Result<i32> {
    req_id
        .to_string()
        .parse::<i32>()
        .map_err(|_| anyhow!("Can’t parse request id: {:?}", &req_id).into())
}

#[cfg(test)]
mod tests;
