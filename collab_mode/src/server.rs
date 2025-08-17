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
    Arc,
};
use tokio::sync::mpsc;
use tracing::instrument;

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
    subscribers: HashMap<ServerId, LocalSeq>,
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
    /// Key is (host_id, doc_id) where doc_id is the remote server's doc_id.
    remote_docs: HashMap<(ServerId, DocId), RemoteDoc>,
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

// *** Impl Doc

impl Doc {
    pub fn new(
        name: String,
        meta: JsonMap,
        abs_filename: Option<PathBuf>,
        content: &str,
        engine: ServerEngine,
    ) -> Self {
        let mut buffer = GapBuffer::new();
        buffer.insert_many(0, content.chars());

        Self {
            name,
            meta,
            abs_filename,
            engine,
            buffer,
            subscribers: HashMap::new(),
        }
    }

    /// Apply `op` to the document.
    pub fn apply_op(&mut self, op: FatOp) -> anyhow::Result<()> {
        let instr = self.engine.convert_internal_op_and_apply(op)?;
        match instr {
            EditInstruction::Ins(edits) => {
                for (pos, str) in edits.into_iter().rev() {
                    self.buffer.insert_many(pos as usize, str.chars());
                }
            }
            EditInstruction::Del(edits) => {
                for (pos, str) in edits.into_iter().rev() {
                    self.buffer
                        .drain((pos as usize)..(pos as usize + str.chars().count()));
                }
            }
        }
        Ok(())
    }

    /// Return the current snapshot of the document.
    pub fn snapshot(&self, doc_id: DocId) -> Snapshot {
        Snapshot {
            buffer: self.buffer.iter().collect::<String>(),
            file_name: self.name.clone(),
            seq: self.engine.current_seq(),
            doc_id,
        }
    }
}

// *** Impl Server

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
            "SendOps" => {
                let params: SendOpsParams = serde_json::from_value(req.params)?;
                self.handle_send_ops_from_editor(
                    editor_tx,
                    webchannel,
                    params.doc_id,
                    params.host_id,
                    params.ops,
                    req.id,
                )
                .await?;
                Ok(())
            }
            "Undo" => {
                let params: UndoParams = serde_json::from_value(req.params)?;
                let resp = self.handle_undo_from_editor(params)?;
                send_response(editor_tx, req.id, resp, None).await;
                Ok(())
            }
            "ShareFile" => {
                let params: ShareFileParams = serde_json::from_value(req.params)?;
                let resp = self.handle_share_file_from_editor(params)?;
                send_response(editor_tx, req.id, resp, None).await;
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
            "DeclareProjects" => {
                let params: DeclareProjectsParams = serde_json::from_value(notif.params)?;
                for project_entry in params.projects {
                    let project = Project {
                        name: project_entry.name,
                        root: project_entry.filename,
                        meta: project_entry.meta,
                    };
                    self.projects.insert(project.root.clone(), project);
                }
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
                    self.handle_snapshot(editor_tx, webchannel, snapshot, req_id, msg.host.clone())
                        .await?;
                } else {
                    tracing::warn!("Received Snapshot without req_id, ignoring.");
                }
                Ok(())
            }
            Msg::OpFromClient(context_ops) => {
                self.handle_op_from_client(editor_tx, webchannel, context_ops, msg.host.clone())
                    .await?;
                Ok(())
            }
            Msg::OpFromServer(ops) => {
                self.handle_op_from_server(editor_tx, ops, msg.host.clone())
                    .await?;
                Ok(())
            }
            Msg::RequestOps { doc, after } => {
                self.handle_request_ops(webchannel, doc, after, msg.host.clone())
                    .await?;
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
        dir: Option<FileDesc>,
        req_id: lsp_server::RequestId,
    ) -> anyhow::Result<()> {
        if self.host_id != host_id {
            send_to_remote(webchannel, &host_id, Some(req_id), Msg::ListFiles { dir }).await;
            return Ok(());
        }

        let files = self
            .list_files_from_disk(dir)
            .await
            .with_context(|| "Failed to list files from disk")?;

        send_response(editor_tx, req_id, ListFilesResp { files }, None).await;

        Ok(())
    }

    // In attached mode, delegate to editor, otherwise read from disk
    // and response immediately.
    async fn list_files_from_remote(
        &mut self,
        _editor_tx: &mpsc::Sender<lsp_server::Message>,
        webchannel: &WebChannel,
        dir: Option<FileDesc>,
        req_id: lsp_server::RequestId,
        remote_host_id: ServerId,
    ) -> anyhow::Result<()> {
        match self.list_files_from_disk(dir).await {
            Ok(files) => {
                let msg = Msg::FileList(files);
                send_to_remote(webchannel, &remote_host_id, Some(req_id), msg).await;
            }
            Err(err) => {
                let msg = Msg::ErrorResp(err.to_string());
                send_to_remote(webchannel, &remote_host_id, Some(req_id), msg).await;
            }
        }
        Ok(())
    }

    async fn list_files_from_disk(
        &self,
        dir: Option<FileDesc>,
    ) -> anyhow::Result<Vec<ListFileEntry>> {
        match dir {
            Some(FileDesc::ProjectFile { project, file }) => {
                // List files in a project directory.
                let project_data = self
                    .projects
                    .get(&project)
                    .ok_or_else(|| anyhow!("Project {} not found", project))?;

                let filename = std::path::PathBuf::from(project_data.root.clone()).join(&file);
                let filename_str = filename.to_string_lossy().to_string();
                let stat = std::fs::metadata(&filename)
                    .with_context(|| format!("Can't access file {}", &filename_str))?;

                if !stat.is_dir() {
                    return Err(anyhow!(format!("Not a directory: {}", filename_str)));
                }

                let mut result = vec![];
                let files = std::fs::read_dir(&filename)
                    .with_context(|| format!("Can't access file {}", &filename_str))?;
                for entry in files {
                    let entry = entry
                        .with_context(|| format!("Can't access files in {}", &filename_str))?;
                    let file_name = entry.file_name().to_string_lossy().to_string();
                    let stat = entry
                        .metadata()
                        .with_context(|| format!("Can't access file {}", &file_name))?;

                    // Calculate relative path from project root using pathdiff
                    let file_path = entry.path();
                    let project_root = std::path::PathBuf::from(&project_data.root);
                    let file_rel = pathdiff::diff_paths(&file_path, &project_root)
                        .ok_or_else(|| anyhow!("Failed to calculate relative path"))?
                        .to_string_lossy()
                        .to_string();

                    result.push(ListFileEntry {
                        file: FileDesc::ProjectFile {
                            project: project.clone(),
                            file: file_rel,
                        },
                        filename: file_name,
                        is_directory: stat.is_dir(),
                        meta: JsonMap::new(),
                    });
                }
                Ok(result)
            }
            Some(FileDesc::Project { id }) => {
                // List files in project root directory.
                let project_data = self
                    .projects
                    .get(&id)
                    .ok_or_else(|| anyhow!("Project {} not found", id))?;

                let filename = std::path::PathBuf::from(project_data.root.clone());
                let filename_str = filename.to_string_lossy().to_string();

                let mut result = vec![];
                let files = std::fs::read_dir(&filename)
                    .with_context(|| format!("Can't access project root {}", &filename_str))?;
                for entry in files {
                    let entry = entry
                        .with_context(|| format!("Can't access files in {}", &filename_str))?;
                    let file_name = entry.file_name().to_string_lossy().to_string();
                    let stat = entry
                        .metadata()
                        .with_context(|| format!("Can't access file {}", &file_name))?;

                    // For project root, relative path is just the filename
                    result.push(ListFileEntry {
                        file: FileDesc::ProjectFile {
                            project: id.clone(),
                            file: file_name.clone(),
                        },
                        filename: file_name,
                        is_directory: stat.is_dir(),
                        meta: JsonMap::new(),
                    });
                }
                Ok(result)
            }
            Some(FileDesc::File { .. }) => {
                // Can't list a file, only directories
                Err(anyhow!("Cannot list files in a document, only directories"))
            }
            None => {
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
                Ok(result)
            }
        }
    }

    /// Read file from disk and return the content as string.
    async fn open_file_from_disk(
        &self,
        project_id: &ProjectId,
        rel_path: &str,
    ) -> anyhow::Result<String> {
        // Look up project
        let project = self
            .projects
            .get(project_id)
            .ok_or_else(|| anyhow!("Project {} not found", project_id))?;

        // Construct full path
        let full_path = std::path::PathBuf::from(&project.root).join(rel_path);

        // Read file content
        let content = std::fs::read_to_string(&full_path)
            .with_context(|| format!("Failed to read file: {}", full_path.display()))?;

        Ok(content)
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
            // Handle local file opening
            match &file_desc {
                FileDesc::ProjectFile { project, file } => {
                    // Check if already open
                    let full_path = std::path::PathBuf::from(&project).join(&file);
                    for (doc_id, doc) in &self.docs {
                        if let Some(ref abs_filename) = doc.abs_filename {
                            if abs_filename == &full_path {
                                // Already open, return existing doc
                                let resp = OpenFileResp {
                                    content: doc.buffer.iter().collect(),
                                    site_id: self.site_id,
                                    filename: doc.name.clone(),
                                    doc_id: *doc_id,
                                };
                                send_response(editor_tx, req_id, resp, None).await;
                                return Ok(());
                            }
                        }
                    }

                    // Read from disk
                    let content = self.open_file_from_disk(project, file).await;
                    if let Err(err) = content {
                        let err = lsp_server::ResponseError {
                            code: ErrorCode::FileNotFound as i32,
                            message: err.to_string(),
                            data: None,
                        };
                        send_response(editor_tx, req_id, (), Some(err)).await;
                        return Ok(());
                    }
                    let content = content.unwrap();

                    // Create new doc
                    let doc_id = self.next_doc_id.fetch_add(1, Ordering::SeqCst);
                    let filename = std::path::Path::new(file)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(file)
                        .to_string();

                    let mut doc = Doc::new(
                        filename.clone(),
                        JsonMap::new(),
                        Some(full_path),
                        &content,
                        ServerEngine::new(content.len() as u64),
                    );

                    // Add ourselves as a subscriber since we're also a client
                    doc.subscribers.insert(self.host_id.clone(), 0);

                    self.docs.insert(doc_id, doc);

                    // Also create a RemoteDoc for ourselves (we act as both server and client)
                    let remote_doc = RemoteDoc {
                        name: filename.clone(),
                        file_desc: file_desc.clone(),
                        next_site_seq: 1,
                        meta: JsonMap::new(),
                        remote_op_buffer: Vec::new(),
                        // Global seq starts from 1. So use 1 for the
                        // base_seq, meaning we expect next global seq
                        // to be 1.
                        engine: ClientEngine::new(self.site_id, 0, content.chars().count() as u64),
                    };
                    self.remote_docs
                        .insert((SERVER_ID_SELF.to_string(), doc_id), remote_doc);

                    // Send response
                    let resp = OpenFileResp {
                        content,
                        site_id: self.site_id,
                        filename,
                        doc_id,
                    };
                    send_response(editor_tx, req_id, resp, None).await;
                }
                FileDesc::File { id } => {
                    // Look up existing doc
                    if let Some(doc) = self.docs.get(id) {
                        let resp = OpenFileResp {
                            content: doc.buffer.iter().collect(),
                            site_id: self.site_id,
                            filename: doc.name.clone(),
                            doc_id: *id,
                        };
                        send_response(editor_tx, req_id, resp, None).await;
                    } else {
                        let err = lsp_server::ResponseError {
                            code: ErrorCode::FileNotFound as i32,
                            message: format!("Doc {} not found", id),
                            data: None,
                        };
                        send_response(editor_tx, req_id, (), Some(err)).await;
                    }
                }
                FileDesc::Project { .. } => {
                    let err = lsp_server::ResponseError {
                        code: ErrorCode::BadRequest as i32,
                        message: "Cannot open a project directory".to_string(),
                        data: None,
                    };
                    send_response(editor_tx, req_id, (), Some(err)).await;
                }
            }
            return Ok(());
        }

        // Open remote file.

        // Check if we already have this file in remote_docs
        for (doc_id, remote_doc) in &self.remote_docs {
            if remote_doc.file_desc == file_desc {
                let err = lsp_server::ResponseError {
                    code: ErrorCode::BadRequest as i32,
                    message: format!("File {} ({:?}) is already opened", remote_doc.name, doc_id),
                    data: None,
                };
                send_response(editor_tx, req_id, (), Some(err)).await;
                return Ok(());
            }
        }

        let msg = Msg::RequestFile(file_desc);
        send_to_remote(webchannel, &host_id, Some(req_id), msg).await;
        Ok(())
    }

    async fn handle_request_file(
        &mut self,
        webchannel: &WebChannel,
        file_desc: FileDesc,
        req_id: lsp_server::RequestId,
        remote_host_id: ServerId,
        _editor_tx: &mpsc::Sender<lsp_server::Message>,
    ) -> anyhow::Result<()> {
        let site_id = self.site_id_of(&remote_host_id);

        match &file_desc {
            FileDesc::ProjectFile { project, file } => {
                // Check if already open
                let full_path = std::path::PathBuf::from(&project).join(&file);

                // Look for existing doc with same abs_filename
                for (doc_id, doc) in &mut self.docs {
                    if let Some(ref abs_filename) = doc.abs_filename {
                        if abs_filename == &full_path {
                            // Already open, add remote as subscriber
                            doc.subscribers.insert(remote_host_id.clone(), 0);

                            // Send snapshot
                            let snapshot = NewSnapshot {
                                content: doc.buffer.iter().collect(),
                                name: doc.name.clone(),
                                seq: doc.engine.current_seq(),
                                site_id,
                                file_desc: file_desc.clone(),
                                doc_id: *doc_id,
                            };
                            send_to_remote(
                                webchannel,
                                &remote_host_id,
                                Some(req_id),
                                Msg::Snapshot(snapshot),
                            )
                            .await;
                            return Ok(());
                        }
                    }
                }

                // Not open, read from disk
                let content = self.open_file_from_disk(project, file).await;
                if let Err(err) = content {
                    send_to_remote(
                        webchannel,
                        &remote_host_id,
                        Some(req_id),
                        Msg::ErrorResp(format!("File not found: {}", err)),
                    )
                    .await;
                    return Ok(());
                }
                let content = content.unwrap();

                // Create new doc
                let doc_id = self.next_doc_id.fetch_add(1, Ordering::SeqCst);
                let filename = std::path::Path::new(file)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(file)
                    .to_string();

                let mut doc = Doc::new(
                    filename.clone(),
                    JsonMap::new(),
                    Some(full_path),
                    &content,
                    ServerEngine::new(content.len() as u64),
                );

                // Don’t add remote as subscriber, wait until we
                // receive RequestOps message.

                self.docs.insert(doc_id, doc);

                // Send snapshot
                let snapshot = NewSnapshot {
                    content,
                    name: filename,
                    seq: 0,
                    site_id,
                    file_desc,
                    doc_id,
                };
                send_to_remote(
                    webchannel,
                    &remote_host_id,
                    Some(req_id),
                    Msg::Snapshot(snapshot),
                )
                .await;
            }
            FileDesc::File { id } => {
                // Look up existing doc
                if let Some(doc) = self.docs.get_mut(id) {
                    // Add remote as subscriber
                    doc.subscribers.insert(remote_host_id.clone(), 0);

                    let snapshot = NewSnapshot {
                        content: doc.buffer.iter().collect(),
                        name: doc.name.clone(),
                        seq: doc.engine.current_seq(),
                        site_id,
                        file_desc: file_desc.clone(),
                        doc_id: *id,
                    };
                    send_to_remote(
                        webchannel,
                        &remote_host_id,
                        Some(req_id),
                        Msg::Snapshot(snapshot),
                    )
                    .await;
                } else {
                    send_to_remote(
                        webchannel,
                        &remote_host_id,
                        Some(req_id),
                        Msg::ErrorResp(format!("Doc {} not found", id)),
                    )
                    .await;
                }
            }
            FileDesc::Project { .. } => {
                send_to_remote(
                    webchannel,
                    &remote_host_id,
                    Some(req_id),
                    Msg::BadRequest("Cannot open a project directory".to_string()),
                )
                .await;
            }
        }
        Ok(())
    }

    async fn handle_snapshot(
        &mut self,
        editor_tx: &mpsc::Sender<lsp_server::Message>,
        webchannel: &WebChannel,
        snapshot: NewSnapshot,
        req_id: lsp_server::RequestId,
        remote_host_id: ServerId,
    ) -> anyhow::Result<()> {
        // First check if there's existing doc with the same file_desc from the same host.
        for ((host, _), remote_doc) in &self.remote_docs {
            if host == &remote_host_id && remote_doc.file_desc == snapshot.file_desc {
                let err = lsp_server::ResponseError {
                    code: ErrorCode::BadRequest as i32,
                    message: format!(
                        "File {} is already opened from {}",
                        remote_doc.name, remote_host_id
                    ),
                    data: None,
                };
                send_response(editor_tx, req_id, (), Some(err)).await;
                return Ok(());
            }
        }

        // Create RemoteDoc
        // Use the site_id assigned by the remote server (from snapshot)
        let remote_doc = RemoteDoc {
            name: snapshot.name.clone(),
            file_desc: snapshot.file_desc.clone(),
            next_site_seq: 1,
            meta: serde_json::Map::new(),
            remote_op_buffer: Vec::new(),
            engine: ClientEngine::new(
                snapshot.site_id,
                snapshot.seq,
                snapshot.content.chars().count() as u64,
            ),
        };

        // Store with (host_id, remote_doc_id) as key
        self.remote_docs
            .insert((remote_host_id.clone(), snapshot.doc_id), remote_doc);

        // Send response to editor with the remote doc_id
        let resp = OpenFileResp {
            content: snapshot.content.clone(),
            site_id: snapshot.site_id,
            filename: snapshot.name,
            doc_id: snapshot.doc_id,
        };
        send_response(editor_tx, req_id, resp, None).await;

        // Send RequestOps to subscribe and get any ops after the snapshot
        let msg = Msg::RequestOps {
            doc: snapshot.doc_id,
            after: snapshot.seq,
        };
        send_to_remote(webchannel, &remote_host_id, None, msg).await;

        Ok(())
    }

    /// Get the site id assigned to host. Create one if not assigned yet.
    fn site_id_of(&mut self, host_id: &ServerId) -> SiteId {
        if let Some(&site_id) = self.site_id_map.get(host_id) {
            site_id
        } else {
            let site_id = self.next_site_id.fetch_add(1, Ordering::SeqCst);
            self.site_id_map.insert(host_id.clone(), site_id);
            site_id
        }
    }

    async fn handle_op_from_client(
        &mut self,
        _editor_tx: &mpsc::Sender<lsp_server::Message>,
        webchannel: &WebChannel,
        context_ops: ContextOps,
        remote_host_id: ServerId,
    ) -> anyhow::Result<()> {
        let doc_id = context_ops.doc();

        // Find doc in docs (as the server/owner of the doc)
        let doc = self.docs.get_mut(&doc_id);
        if doc.is_none() {
            // Send DocFatal message back
            send_to_remote(
                webchannel,
                &remote_host_id,
                None,
                Msg::BadRequest(format!("Doc {} not found", doc_id)),
            )
            .await;
            return Ok(());
        }
        let doc = doc.unwrap();

        // Check if first op's local site seq matches subscriber's saved seq
        if let Some(first_op) = context_ops.ops.first() {
            let expected_seq = doc.subscribers.get(&remote_host_id).copied().unwrap_or(0) + 1;
            if first_op.site_seq != expected_seq {
                // Send DocFatal message back
                let err = format!(
                    "Site seq mismatch: expected {}, got {}",
                    expected_seq, first_op.site_seq
                );
                let msg = Msg::DocFatal(doc_id, err);
                send_to_remote(webchannel, &remote_host_id, None, msg).await;
                return Ok(());
            }
        }

        // Process ops using server engine
        let processed_ops = doc
            .engine
            .process_ops(context_ops.ops.clone(), context_ops.context)?;

        // Update subscriber's local seq to latest
        if let Some(last_op) = context_ops.ops.last() {
            doc.subscribers
                .insert(remote_host_id.clone(), last_op.site_seq);
        }

        // Apply ops to doc buffer
        for op in &processed_ops {
            doc.apply_op(op.clone())?;
        }

        // Send OpFromServer to all subscribers (including the sender)
        // The sender needs to receive the globally sequenced version of their op
        for (subscriber_host_id, _) in &doc.subscribers {
            send_to_remote(
                webchannel,
                subscriber_host_id,
                None,
                Msg::OpFromServer(processed_ops.clone()),
            )
            .await;
        }

        Ok(())
    }

    async fn handle_request_ops(
        &mut self,
        webchannel: &WebChannel,
        doc_id: DocId,
        after: GlobalSeq,
        remote_host_id: ServerId,
    ) -> anyhow::Result<()> {
        // Find the doc in self.docs
        let doc = self.docs.get_mut(&doc_id);
        if doc.is_none() {
            tracing::warn!(
                "Received RequestOps for unknown doc {} from {}",
                doc_id,
                remote_host_id
            );
            send_to_remote(
                webchannel,
                &remote_host_id,
                None,
                Msg::BadRequest(format!("Doc {} not found", doc_id)),
            )
            .await;
            return Ok(());
        }
        let doc = doc.unwrap();

        // Add remote as subscriber
        doc.subscribers.insert(remote_host_id.clone(), 0);

        // Get all ops after the specified sequence
        let ops = doc.engine.global_ops_after(after);

        // Send OpFromServer message with the ops
        if !ops.is_empty() {
            send_to_remote(webchannel, &remote_host_id, None, Msg::OpFromServer(ops)).await;
        }

        Ok(())
    }

    async fn handle_op_from_server(
        &mut self,
        editor_tx: &mpsc::Sender<lsp_server::Message>,
        ops: Vec<FatOp>,
        remote_host_id: ServerId,
    ) -> anyhow::Result<()> {
        if ops.is_empty() {
            return Ok(());
        }

        let doc_id = ops[0].doc;

        // Find doc in remote_docs with (host_id, doc_id) key
        let key = (remote_host_id.clone(), doc_id);
        let remote_doc = self.remote_docs.get_mut(&key);
        if remote_doc.is_none() {
            tracing::warn!(
                "Received OpFromServer for unknown doc {} from {}",
                doc_id,
                remote_host_id
            );
            return Ok(());
        }
        let remote_doc = remote_doc.unwrap();

        let our_site_id = remote_doc.engine.site();
        let op_is_not_ours = ops[0].site() != our_site_id;

        // Save remote ops to buffer
        remote_doc.remote_op_buffer.extend(ops);

        if op_is_not_ours {
            send_notification(
                editor_tx,
                NotificationCode::RemoteOpsArrived,
                serde_json::json!({
                    "hostId": remote_host_id,
                    "docId": doc_id,
                }),
            )
            .await;
        }

        Ok(())
    }

    async fn handle_send_ops_from_editor(
        &mut self,
        editor_tx: &mpsc::Sender<lsp_server::Message>,
        webchannel: &WebChannel,
        doc_id: DocId,
        host_id: ServerId,
        ops: Vec<EditorFatOp>,
        req_id: lsp_server::RequestId,
    ) -> anyhow::Result<()> {
        // Look in remote_docs with (host_id, doc_id) key
        let remote_doc = self.remote_docs.get_mut(&(host_id.clone(), doc_id));
        if remote_doc.is_none() {
            let err = lsp_server::ResponseError {
                code: ErrorCode::DocNotFound as i32,
                message: format!("Doc {} from host {} not found", doc_id, host_id),
                data: None,
            };
            send_response(editor_tx, req_id, (), Some(err)).await;
            return Ok(());
        }
        let remote_doc = remote_doc.unwrap();

        // Drain remote ops from buffer
        let remote_ops: Vec<FatOp> = remote_doc.remote_op_buffer.drain(..).collect();

        // Process local ops from editor first, because we want to
        // pretent the local ops arrives before the remote ops.
        for editor_op in ops {
            // Process the op with the engine
            let site_seq = remote_doc.next_site_seq;
            remote_doc.next_site_seq += 1;

            if let Err(err) = remote_doc
                .engine
                .process_local_op(editor_op, doc_id, site_seq)
            {
                tracing::error!("Failed to process local op: {}", err);
                let err = lsp_server::ResponseError {
                    code: ErrorCode::DocFatal as i32,
                    message: format!("Failed to process op: {}", err),
                    data: None,
                };
                send_response(editor_tx, req_id, (), Some(err)).await;
                return Ok(());
            }
        }

        // Process the drained remote ops.
        let mut lean_ops = Vec::new();
        for remote_op in remote_ops {
            if let Some((lean_op, _seq)) = remote_doc.engine.process_remote_op(remote_op)? {
                lean_ops.push(lean_op);
            }
        }

        // Package local ops if any
        if let Some(context_ops) = remote_doc.engine.maybe_package_local_ops() {
            // Send to the owner (could be ourselves or a remote)
            send_to_remote(webchannel, &host_id, None, Msg::OpFromClient(context_ops)).await;
        }

        // Send response with processed remote ops
        let resp = SendOpsResp {
            ops: lean_ops,
            last_seq: remote_doc.engine.current_gseq(),
        };
        send_response(editor_tx, req_id, resp, None).await;
        Ok(())
    }

    fn handle_undo_from_editor(&mut self, params: UndoParams) -> anyhow::Result<UndoResp> {
        // Check if it's a remote doc
        let key = (params.host_id.clone(), params.doc_id.clone());
        let remote_doc = self
            .remote_docs
            .get_mut(&key)
            .ok_or_else(|| anyhow::anyhow!("Remote doc not found: {:?}", key))?;

        let ops = match params.kind {
            UndoKind::Undo => remote_doc.engine.generate_undo_op(),
            UndoKind::Redo => remote_doc.engine.generate_redo_op(),
        };
        let resp = UndoResp { ops };
        Ok(resp)
    }

    fn handle_share_file_from_editor(
        &mut self,
        params: ShareFileParams,
    ) -> anyhow::Result<ShareFileResp> {
        // Generate a new doc_id
        let doc_id = self.next_doc_id.fetch_add(1, Ordering::SeqCst);

        // Create a new owned Doc
        let mut doc = Doc::new(
            params.filename.clone(),
            params.meta.clone(),
            None, // No abs_filename since this is content-based
            &params.content,
            ServerEngine::new(params.content.len() as u64),
        );

        // Add ourselves as a subscriber since we're also a client
        doc.subscribers.insert(self.host_id.clone(), 0);

        // Store the doc in our owned docs
        self.docs.insert(doc_id, doc);

        // Also create a RemoteDoc for ourselves (we act as both server and client)
        let remote_doc = RemoteDoc {
            name: params.filename.clone(),
            file_desc: FileDesc::File { id: doc_id },
            next_site_seq: 1,
            meta: params.meta,
            remote_op_buffer: Vec::new(),
            engine: ClientEngine::new(self.site_id, 0, params.content.chars().count() as u64),
        };
        self.remote_docs
            .insert((SERVER_ID_SELF.to_string(), doc_id), remote_doc);

        // Return response with doc_id and site_id
        let resp = ShareFileResp {
            doc_id,
            site_id: self.site_id,
        };
        Ok(resp)
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
