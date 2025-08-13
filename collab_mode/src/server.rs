use crate::config_man::ConfigManager;
use crate::engine::{ClientEngine, ServerEngine};
use crate::message::{self, *};
use crate::signaling::SignalingError;
use crate::types::*;
use crate::webchannel::WebChannel;
use crate::webchannel::{self, Msg};
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

/// Stores relevant data for a remote doc, used by the server.
#[derive(Debug)]
struct RemoteDoc {
    /// Human-readable name for the doc.
    name: String,
    /// The absolute filename of the file on remote peer’s disk, used
    /// for detecting already-opened docs in project.
    file_desc: FileDesc,
    /// Site seq number of next local op.
    next_site_seq: LocalSeq,
    /// Metadata for this doc.
    meta: JsonMap,
    /// Arrived remote ops are saved in this buffer until processed.
    remote_op_buffer: Vec<FatOp>,
    /// The server engine that transforms and stores ops for this doc.
    engine: ClientEngine,
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
    /// Next doc id to be used for new docs.
    next_doc_id: AtomicU32,
    /// Docs hosted by this server.
    docs: HashMap<DocId, Doc>,
    /// Docs hosted by remote peers.
    remote_docs: HashMap<DocId, RemoteDoc>,
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
            next_doc_id: AtomicU32::new(1),
            docs: HashMap::new(),
            remote_docs: HashMap::new(),
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
                    let res = self.handle_editor_message(msg, &editor_tx, &webchannel).await;
                    // Normally the handler shouldn’t return an error.
                    // Most errors ar handled by sending error
                    // response to editor/remote.
                    if let Err(err) = res {
                        tracing::error!("Failed to handle editor message: {}", err);
                    }
                },
                // Handle messages from remote peers.
                Some(web_msg) = msg_rx.recv() => {
                    tracing::info!("Received message from remote: {:?}", web_msg);
                    let res = self.handle_remote_message(web_msg, &editor_tx, &webchannel).await;
                    // Normally the handler shouldn’t return an error.
                    // Most errors ar handled by sending error
                    // response to editor/remote.
                    if let Err(err) = res {
                        tracing::error!("Failed to handle remote message: {}", err);
                    }
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
                    let err = lsp_server::ResponseError {
                        code: 30000, // FIXME: Use appropriate error code.
                        message: err.to_string(),
                        data: None,
                    };
                    send_response(editor_tx, req_id, (), Some(err)).await;
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

                // Check if the response has an error
                if let Some(error) = resp.error {
                    // Send ErrorResp to the remote
                    send_to_remote(
                        webchannel,
                        &orig_req.host_id,
                        Some(orig_req.req_id),
                        Msg::ErrorResp(format!("{}: {}", error.code, error.message)),
                    )
                    .await;
                    return Ok(());
                }

                // Only handle successful responses
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
                    let params = serde_json::json!({
                        "message": err.to_string(),
                    });
                    send_notification(editor_tx, NotificationCode::Error, params).await;
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
                self.open_file_from_editor(
                    editor_tx,
                    webchannel,
                    params.host_id,
                    params.file_desc,
                    req.id,
                )
                .await?;
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
        _editor_tx: &mpsc::Sender<lsp_server::Message>,
        webchannel: &WebChannel,
    ) -> anyhow::Result<()> {
        match orig_req.method.as_str() {
            "ListFiles" => {
                let resp: ListFilesResp = serde_json::from_value(result.unwrap_or_default())
                    .with_context(|| "Failed to parse ListFiles response")?;
                send_to_remote(
                    webchannel,
                    &orig_req.host_id,
                    Some(orig_req.req_id),
                    Msg::FileList(resp.files),
                )
                .await;
            }
            "RequestFile" => {
                // Parse the Snapshot response from editor
                let snapshot_val = result.unwrap_or_default();
                let buffer = snapshot_val["buffer"].as_str().unwrap_or("");
                let filename = snapshot_val["fileName"].as_str().unwrap_or("untitled");
                let seq = snapshot_val["seq"].as_u64().unwrap_or(0);
                
                let snapshot = NewSnapshot {
                    content: buffer.to_string(),
                    filename: filename.to_string(),
                    seq: seq as u32,
                };
                
                // Send snapshot back to remote
                send_to_remote(
                    webchannel,
                    &orig_req.host_id,
                    Some(orig_req.req_id),
                    Msg::Snapshot(snapshot),
                )
                .await;
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
                    send_to_remote(
                        webchannel,
                        &msg.host,
                        None,
                        Msg::BadRequest("Missing req_id".to_string()),
                    )
                    .await;
                }
                Ok(())
            }
            Msg::FileList(files) => {
                let resp_id = msg.req_id;
                if resp_id.is_none() {
                    tracing::warn!(
                        "Received FileList from remote {} without req_id, ignoring",
                        msg.host
                    );
                    send_notification(editor_tx, NotificationCode::Error, serde_json::json!({
                        "message": format!("Received file list from remote {}, but without req_id", msg.host),
                    })).await;
                    return Ok(());
                }
                let resp_id = resp_id.unwrap();
                send_response(editor_tx, resp_id, ListFilesResp { files }, None).await;
                Ok(())
            }
            Msg::ErrorResp(error_msg) => {
                if msg.req_id.is_none() {
                    tracing::warn!("Received ErrorResp without req_id, ignoring");
                    send_notification(editor_tx, NotificationCode::Error, serde_json::json!({
                        "message": format!("Received error response from remote {}, but without response id: {}", msg.host, error_msg),
                    })).await;
                    return Ok(());
                }
                let req_id = msg.req_id.unwrap();

                // Send error response back to the editor
                let err = lsp_server::ResponseError {
                    code: 500,
                    message: error_msg,
                    data: None,
                };
                send_response(editor_tx, req_id, (), Some(err)).await;

                Ok(())
            }
            Msg::RequestFile(file_desc) => {
                if let Some(req_id) = msg.req_id {
                    self.handle_request_file(
                        webchannel,
                        file_desc,
                        req_id,
                        msg.host.clone(),
                        editor_tx,
                    )
                    .await?;
                } else {
                    tracing::warn!("Received RequestFile without req_id, ignoring.");
                    send_to_remote(
                        webchannel,
                        &msg.host,
                        None,
                        Msg::BadRequest("Missing req_id".to_string()),
                    )
                    .await;
                }
                Ok(())
            }
            Msg::Snapshot(snapshot) => {
                if let Some(req_id) = msg.req_id {
                    self.handle_snapshot(
                        editor_tx,
                        snapshot,
                        req_id,
                        msg.host.clone(),
                    )
                    .await?;
                } else {
                    tracing::warn!("Received Snapshot without req_id, ignoring.");
                }
                Ok(())
            }
            _ => {
                panic!("Unhandled message type from {}: {:?}", msg.host, msg.body);
            }
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
        let my_host_id = self.host_id.clone();

        tokio::spawn(async move {
            if let Err(err) = webchannel_1
                .connect(
                    host_id.clone(),
                    my_host_id,
                    key_cert,
                    &signaling_addr,
                    transpot_type,
                )
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
            send_to_remote(webchannel, &host_id, Some(req_id), Msg::ListFiles { dir }).await;
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
            send_to_remote(
                webchannel,
                &remote_host_id,
                Some(req_id),
                Msg::FileList(files),
            )
            .await;
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
                    file: FileDesc::ProjectFile {
                        project: project_id.clone(),
                        file: file_rel.to_string_lossy().to_string(),
                    },
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
                    file: FileDesc::Project {
                        id: project.root.clone(),
                    },
                    filename: project.name.clone(),
                    is_directory: true,
                    meta: project.meta.clone(),
                });
            }
            for (docid, doc) in self.docs.iter() {
                result.push(ListFileEntry {
                    file: FileDesc::File { id: docid.clone() },
                    is_directory: false,
                    filename: doc.name.clone(),
                    meta: doc.meta.clone(),
                });
            }
            return Ok(result);
        }
    }

    async fn open_file_from_editor(
        &mut self,
        editor_tx: &mpsc::Sender<lsp_server::Message>,
        webchannel: &WebChannel,
        host_id: ServerId,
        file_desc: FileDesc,
        req_id: lsp_server::RequestId,
    ) -> anyhow::Result<()> {
        if self.host_id == host_id {
            // Opening our own file - not supported in attached mode
            if self.attached {
                let err = lsp_server::ResponseError {
                    code: 400,
                    message: "Cannot open local files in attached mode".to_string(),
                    data: None,
                };
                send_response(editor_tx, req_id, (), Some(err)).await;
                return Ok(());
            }
            // TODO: Handle opening local files in non-attached mode
            return Ok(());
        }

        // Check if we already have this file in remote_docs
        for (doc_id, remote_doc) in &self.remote_docs {
            if remote_doc.file_desc == file_desc {
                // Already have this doc, return it
                let resp = OpenFileResp {
                    content: self.get_remote_doc_content(*doc_id)?,
                    site_id: self.site_id,
                    filename: remote_doc.name.clone(),
                    doc_id: *doc_id,
                };
                send_response(editor_tx, req_id, resp, None).await;
                return Ok(());
            }
        }

        // Need to request file from remote
        let internal_req_id = self.next_req_id.fetch_add(1, Ordering::SeqCst);
        self.orig_req_map.insert(
            internal_req_id,
            OrigRequest {
                req_id: req_id.clone(),
                method: "OpenFile".to_string(),
                host_id: host_id.clone(),
            },
        );

        send_to_remote(
            webchannel,
            &host_id,
            Some(req_id),
            Msg::RequestFile(file_desc),
        )
        .await;
        Ok(())
    }

    async fn handle_request_file(
        &mut self,
        webchannel: &WebChannel,
        file_desc: FileDesc,
        req_id: lsp_server::RequestId,
        remote_host_id: ServerId,
        editor_tx: &mpsc::Sender<lsp_server::Message>,
    ) -> anyhow::Result<()> {
        // Store the remote request so we can handle the response later
        let editor_req_id = self.next_req_id.fetch_add(1, Ordering::SeqCst);
        let orig_req = OrigRequest {
            method: "RequestFile".to_string(),
            host_id: remote_host_id.clone(),
            req_id: req_id.clone(),
        };
        self.orig_req_map.insert(editor_req_id, orig_req);
        
        // Convert FileDesc to the format expected by the editor (using tagged representation)
        let params = match &file_desc {
            FileDesc::File { id } => {
                serde_json::json!({
                    "fileDesc": {
                        "type": "file",
                        "id": id
                    }
                })
            }
            FileDesc::Project { id } => {
                serde_json::json!({
                    "fileDesc": {
                        "type": "project",
                        "id": id
                    }
                })
            }
            FileDesc::ProjectFile { project, file } => {
                serde_json::json!({
                    "fileDesc": {
                        "type": "projectFile",
                        "project": project,
                        "file": file
                    }
                })
            }
        };
        
        // Send RequestFile request to our editor
        let request = lsp_server::Request {
            id: lsp_server::RequestId::from(editor_req_id),
            method: "RequestFile".to_string(),
            params,
        };
        
        editor_tx
            .send(lsp_server::Message::Request(request))
            .await?;
        
        Ok(())
    }

    async fn handle_snapshot(
        &mut self,
        editor_tx: &mpsc::Sender<lsp_server::Message>,
        snapshot: NewSnapshot,
        req_id: lsp_server::RequestId,
        remote_host_id: ServerId,
    ) -> anyhow::Result<()> {
        // Find doc_id for this snapshot
        let doc_id = self.next_doc_id.fetch_add(1, Ordering::SeqCst);
        
        // Create RemoteDoc
        let remote_doc = RemoteDoc {
            name: snapshot.filename.clone(),
            file_desc: FileDesc::File { id: doc_id }, // Will be updated based on context
            next_site_seq: 1,
            meta: serde_json::Map::new(),
            remote_op_buffer: Vec::new(),
            engine: ClientEngine::new(self.site_id, 0, 0),
        };
        
        self.remote_docs.insert(doc_id, remote_doc);

        // Send response to editor
        let resp = OpenFileResp {
            content: snapshot.content,
            site_id: self.site_id,
            filename: snapshot.filename,
            doc_id,
        };
        send_response(editor_tx, req_id, resp, None).await;
        Ok(())
    }

    fn file_desc_matches_doc(&self, file_desc: &FileDesc, doc: &Doc) -> bool {
        // Compare based on normalized filename
        match file_desc {
            FileDesc::File { .. } => false, // Can't match by ID since we don't know remote's ID
            FileDesc::Project { .. } => false,
            FileDesc::ProjectFile { file, .. } => {
                // Compare with doc's filename if available
                &doc.name == file || doc.name.ends_with(file)
            }
        }
    }

    fn create_doc_from_file_desc(&self, file_desc: &FileDesc) -> anyhow::Result<Doc> {
        let name = match file_desc {
            FileDesc::File { .. } => "untitled".to_string(),
            FileDesc::Project { id } => id.clone(),
            FileDesc::ProjectFile { file, .. } => file.clone(),
        };

        let abs_filename = match file_desc {
            FileDesc::ProjectFile { project, file } => {
                Some(PathBuf::from(project).join(file))
            }
            _ => None,
        };

        Ok(Doc {
            name,
            meta: serde_json::Map::new(),
            abs_filename,
            engine: ServerEngine::new(0),
            buffer: GapBuffer::new(),
            subscribers: HashMap::new(),
        })
    }

    fn create_snapshot(&self, doc_id: DocId) -> anyhow::Result<NewSnapshot> {
        let doc = self.docs.get(&doc_id)
            .ok_or_else(|| anyhow!("Doc {} not found", doc_id))?;
        
        let content: String = doc.buffer.iter().collect();
        
        Ok(NewSnapshot {
            content,
            filename: doc.name.clone(),
            seq: 0, // TODO: track sequence numbers
        })
    }

    fn get_remote_doc_content(&self, doc_id: DocId) -> anyhow::Result<String> {
        let remote_doc = self.remote_docs.get(&doc_id)
            .ok_or_else(|| anyhow!("Remote doc {} not found", doc_id))?;
        
        // TODO: Reconstruct content from engine
        Ok(String::new())
    }

    fn get_or_create_site_id(&mut self, host_id: &ServerId) -> SiteId {
        if let Some(&site_id) = self.site_id_map.get(host_id) {
            site_id
        } else {
            let site_id = self.next_site_id.fetch_add(1, Ordering::SeqCst);
            self.site_id_map.insert(host_id.clone(), site_id);
            site_id
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

/// Send a message to a remote host via WebChannel with error logging.
async fn send_to_remote(
    webchannel: &WebChannel,
    host_id: &ServerId,
    req_id: Option<lsp_server::RequestId>,
    msg: Msg,
) {
    if let Err(err) = webchannel.send(host_id, req_id, msg.clone()).await {
        tracing::error!(
            "Failed to send {:?} to remote host {}: {}",
            msg,
            host_id,
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
