use crate::config_man::{AcceptMode, ConfigManager, ConfigProject};
use crate::engine::{ClientEngine, ServerEngine};
use crate::message::{self, *};
use crate::signaling::SignalingError;
use crate::types::*;
use crate::webchannel::{self, WebChannel};
use anyhow::{anyhow, Context};
use fmt_derive;
use gapbuf::GapBuffer;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, fmt_derive::Display)]
enum PathId {
    // Unique name of a buffer.
    Buffer(String),
    // Unique abs path of a file.
    Path(PathBuf),
}

impl PathId {
    fn is_buffer(&self) -> bool {
        match self {
            PathId::Buffer(..) => true,
            _ => false,
        }
    }

    #[allow(dead_code)]
    fn is_file(&self) -> bool {
        match self {
            PathId::Buffer(..) => false,
            _ => true,
        }
    }
}

/// Stores relevant data for a document, used by the server.
#[derive(Debug)]
struct Doc {
    /// Human-readable name for the doc.
    name: String,
    /// Metadata for this doc.
    meta: JsonMap,
    /// The absolute filename of this doc on disk, if exists.
    abs_filename: PathId,
    /// File desc of this doc.
    #[allow(dead_code)]
    file_desc: FileDesc,
    /// File object for saving file.
    disk_file: Option<std::fs::File>,
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
    /// This is only used for returning to editor, not used for
    /// comparison.
    file_desc: FileDesc,
    /// Site seq number of next local op.
    next_site_seq: LocalSeq,
    /// Metadata for this doc.
    #[allow(dead_code)]
    meta: JsonMap,
    /// Arrived remote ops are saved in this buffer until processed.
    remote_op_buffer: Vec<FatOp>,
    /// The server engine that transforms and stores ops for this doc.
    engine: ClientEngine,
    /// The document buffer.
    buffer: GapBuffer<char>,
}

#[derive(PartialEq, Eq, Debug, Copy, Clone, Serialize, Deserialize)]
pub enum ConnectionState {
    // Connected and well.
    Connected,
    // Trying to (re)connect.
    Connecting,
    // Connection was established but then broke. We try to reconnect
    // in this case.
    Disconnected,
    // Connection was never established. We don't try to reconnect in
    // this case.
    FailedToConnect,
}

#[derive(PartialEq, Eq, Debug, Clone)]
struct RemoteState {
    /// Connections state.
    state: ConnectionState,
    /// Signaling address used to connect to this remote.
    signaling_addr: String,
    /// Transport type used to connect to this remote.
    transport_type: webchannel::TransportType,
    /// Next reconnect should be attempted aftert this duration. We
    /// use exponential backoff: 1, 2, 4, 8, 16, 32, 64, 128, 256. Max at 256.
    next_reconnect_stride: u64,
    /// Next reconnect attempt time.
    next_reconnect_time: std::time::Instant,
}

impl RemoteState {
    fn connecting(signaling_addr: String, transport_type: webchannel::TransportType) -> Self {
        RemoteState {
            state: ConnectionState::Connecting,
            next_reconnect_stride: 1,
            signaling_addr,
            transport_type,
            next_reconnect_time: std::time::Instant::now(),
        }
    }
}

/// Stores relevant for the server.
pub struct Server {
    /// Host id of this server. Once set, this cannot change, because
    /// we use our own host_id in remote_docs, etc too.
    host_id: ServerId,
    /// SiteId given to ourselves.
    site_id: SiteId,
    /// SiteId given to the next connected client.
    next_site_id: AtomicU32,
    /// Next doc id to be used for new docs.
    next_doc_id: AtomicU32,
    /// Docs hosted by this server.
    docs: HashMap<DocId, Doc>,
    /// Remote docs.
    remote_docs: RemoteDocs,
    /// Projects.
    projects: HashMap<ProjectId, Project>,
    /// Map of recognized users.
    #[allow(dead_code)]
    users: HashMap<Credential, SiteId>,
    /// Active remote peers.
    active_remotes: HashMap<ServerId, RemoteState>,
    /// Maps host id to site id.
    site_id_map: HashMap<ServerId, SiteId>,
    /// Whether the server is currently accepting new connections on a
    /// particular signaling server.
    accepting: HashMap<String, ()>,
    /// Configuration manager for the server.
    config: ConfigManager,
    /// Key and certificate for the server.
    key_cert: Arc<KeyCert>,
    /// Trusted hosts with their certificate hashes (shared with WebChannel)
    trusted_hosts: Arc<Mutex<HashMap<ServerId, String>>>,
    /// Accept mode for incoming connections (shared with WebChannel)
    accept_mode: Arc<Mutex<AcceptMode>>,
}

// *** Impl Doc

impl Doc {
    pub fn new(
        name: String,
        meta: JsonMap,
        abs_filename: PathId,
        file_desc: FileDesc,
        content: &str,
        engine: ServerEngine,
        disk_file: Option<File>,
    ) -> Self {
        let mut buffer = GapBuffer::new();
        buffer.insert_many(0, content.chars());

        Self {
            name,
            meta,
            abs_filename,
            file_desc,
            engine,
            buffer,
            subscribers: HashMap::new(),
            disk_file,
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

    /// Save buffer content to file.
    pub fn save_to_disk(&mut self) -> anyhow::Result<()> {
        if let Some(ref mut file) = self.disk_file {
            let content: String = self.buffer.iter().collect();
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            file.write_all(content.as_bytes())
                .with_context(|| format!("Failed to write to file: {:?}", self.abs_filename))?;
            file.sync_all()
                .with_context(|| format!("Failed to sync file: {:?}", self.abs_filename))?;
            tracing::info!("Saved file: {:?}", self.abs_filename);
        }
        Ok(())
    }

    /// Return `true` if this doc matches `path`. Owned docs match by
    /// PathId. Because, eg, editor could try to share a single file
    /// which actually is inside a project, we need to be able to tell
    /// it’s in a project and make it a project file instead. Remote
    /// docs match by FileDesc.
    fn matches(&self, path: &PathId) -> bool {
        self.abs_filename == *path
    }
}

// *** Impl RemoteDoc

impl RemoteDoc {
    /// Apply an EditInstruction to the buffer.
    pub fn apply_edit_instruction(&mut self, instruction: &EditInstruction) {
        match instruction {
            EditInstruction::Ins(edits) => {
                for (pos, str) in edits.iter().rev() {
                    self.buffer.insert_many(*pos as usize, str.chars());
                }
            }
            EditInstruction::Del(edits) => {
                for (pos, str) in edits.iter().rev() {
                    let end = *pos as usize + str.chars().count();
                    self.buffer.drain((*pos as usize)..end);
                }
            }
        }
    }
}

// *** RemoteDocs

/// Container for remote documents that maintains consistency between
/// the docs map and editor file descriptor mapping.
#[derive(Debug)]
struct RemoteDocs {
    /// Remote docs keyed by (host_id, doc_id).
    docs: HashMap<(ServerId, DocId), RemoteDoc>,
    /// Map from editor file descriptor to remote doc id.
    editor_map: HashMap<EditorFileDesc, DocId>,
}

impl RemoteDocs {
    fn new() -> Self {
        Self {
            docs: HashMap::new(),
            editor_map: HashMap::new(),
        }
    }

    /// Insert a remote doc.
    fn insert(
        &mut self,
        host_id: ServerId,
        doc_id: DocId,
        remote_doc: RemoteDoc,
        editor_file_desc: EditorFileDesc,
    ) {
        self.docs.insert((host_id, doc_id), remote_doc);
        self.editor_map.insert(editor_file_desc, doc_id);
    }

    /// Get a remote doc by (ServerId, DocId).
    fn get(&self, host_id: &ServerId, doc_id: DocId) -> Option<&RemoteDoc> {
        self.docs.get(&(host_id.clone(), doc_id))
    }

    /// Get a mutable remote doc by (ServerId, DocId).
    fn get_mut(&mut self, host_id: &ServerId, doc_id: DocId) -> Option<&mut RemoteDoc> {
        self.docs.get_mut(&(host_id.clone(), doc_id))
    }

    /// Get a remote doc by EditorFileDesc.
    fn get_by_editor_desc(&self, editor_file: &EditorFileDesc) -> Option<(DocId, &RemoteDoc)> {
        let host_id = editor_file.host_id();
        let doc_id = *self.editor_map.get(editor_file)?;
        self.docs
            .get(&(host_id.clone(), doc_id))
            .map(|doc| (doc_id, doc))
    }

    /// Get a mutable remote doc by EditorFileDesc.
    fn get_mut_by_editor_desc(
        &mut self,
        editor_file: &EditorFileDesc,
    ) -> Option<(DocId, &mut RemoteDoc)> {
        let host_id = editor_file.host_id().clone();
        let doc_id = *self.editor_map.get(editor_file)?;
        self.docs
            .get_mut(&(host_id, doc_id))
            .map(|doc| (doc_id, doc))
    }

    /// Remove a remote doc by (ServerId, DocId).
    fn remove(&mut self, host_id: &ServerId, doc_id: DocId) -> Option<RemoteDoc> {
        let removed_doc = self.docs.remove(&(host_id.clone(), doc_id))?;
        // Clean up editor mapping
        let editor_file_desc = EditorFileDesc::new(removed_doc.file_desc.clone(), host_id.clone());
        self.editor_map.remove(&editor_file_desc);
        Some(removed_doc)
    }

    /// Remove a remote doc by EditorFileDesc.
    fn remove_by_editor_desc(&mut self, editor_file: &EditorFileDesc) -> Option<RemoteDoc> {
        let host_id = editor_file.host_id().clone();
        let doc_id = self.editor_map.remove(editor_file)?;
        self.docs.remove(&(host_id, doc_id))
    }

    /// Iterate over all remote docs.
    fn iter(&self) -> impl Iterator<Item = (&(ServerId, DocId), &RemoteDoc)> {
        self.docs.iter()
    }
}

// *** Impl Server

impl Server {
    pub fn new(host_id: ServerId, config: ConfigManager) -> anyhow::Result<Self> {
        let key_cert = config.get_key_and_cert(host_id.clone())?;

        // Get config values for trusted_hosts and accept_mode
        let current_config = config.config();
        let trusted_hosts = Arc::new(Mutex::new(current_config.trusted_hosts));
        let accept_mode = Arc::new(Mutex::new(current_config.accept_mode));

        let server = Server {
            host_id,
            site_id: 0,
            next_site_id: AtomicU32::new(1),
            next_doc_id: AtomicU32::new(1),
            docs: HashMap::new(),
            remote_docs: RemoteDocs::new(),
            projects: HashMap::new(),
            users: HashMap::new(),
            active_remotes: HashMap::new(),
            site_id_map: HashMap::new(),
            accepting: HashMap::new(),
            config,
            key_cert: Arc::new(key_cert),
            trusted_hosts,
            accept_mode,
        };
        Ok(server)
    }

    pub async fn run(
        &mut self,
        editor_tx: mpsc::Sender<lsp_server::Message>,
        mut editor_rx: mpsc::Receiver<lsp_server::Message>,
    ) -> anyhow::Result<()> {
        let (msg_tx, mut msg_rx) = mpsc::channel::<webchannel::Message>(1);

        // Use the Server's shared trusted_hosts and accept_mode
        let webchannel = WebChannel::new(
            self.host_id.clone(),
            msg_tx,
            self.trusted_hosts.clone(),
            self.accept_mode.clone(),
        );

        // Add initial projects
        let mut projects = self.config.config().projects;

        // Expand project paths to absolute paths
        expand_project_paths(&mut projects)?;

        for project in projects {
            let proj = Project {
                name: project.name.clone(),
                root: project.path,
                meta: serde_json::Map::new(),
            };
            self.projects.insert(project.name, proj);
        }

        loop {
            tokio::select! {
                // Handle messages from the editor.
                Some(msg) = editor_rx.recv() => {
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
                    let res = self.handle_remote_message(web_msg, &editor_tx, &webchannel).await;
                    // Normally the handler shouldn’t return an error.
                    // Most errors ar handled by sending error
                    // response to editor/remote.
                    if let Err(err) = res {
                        tracing::error!("Failed to handle remote message: {}", err);
                    }
                },
                _ = tokio::time::sleep(Duration::from_secs(2)) => {
                    let mut need_connect: Vec<(String, RemoteState)> = Vec::new();
                    for (host, remote) in self.active_remotes.iter_mut() {
                        if remote.state != ConnectionState::Disconnected {
                            continue;
                        }
                        if Instant::now() < remote.next_reconnect_time {
                            continue;
                        }
                        tracing::info!("Reconnecting to remote {}", host);
                        send_notification(&editor_tx, NotificationCode::Connecting, ConnectingNote {
                            host_id: host.clone(),

                        }).await;
                        need_connect.push((host.clone(), remote.clone()));
                        let stride = Duration::from_secs(remote.next_reconnect_stride);
                        remote.next_reconnect_stride = std::cmp::min(
                                remote.next_reconnect_stride * 2,
                                256,
                        );
                        remote.next_reconnect_time = Instant::now() + stride;
                    }

                    let next = Next::new(&editor_tx, None, &webchannel);
                    for (host, remote) in need_connect {
                        self.connect(&next, host.clone(), remote.signaling_addr, remote.transport_type).await;
                    }

                }
            }
        }
    }

    /// Return all locally hosted docs whose absolute paths are not
    /// under any declared project. These correspond to the _files
    /// virtual project.
    fn local_docs_not_in_any_project(&self) -> Vec<(DocId, &Doc)> {
        let project_roots: Vec<std::path::PathBuf> = self
            .projects
            .values()
            .map(|p| std::path::PathBuf::from(&p.root))
            .collect();
        let mut out: Vec<(DocId, &Doc)> = Vec::new();
        for (doc_id, doc) in &self.docs {
            if let PathId::Path(path) = &doc.abs_filename {
                if !project_roots.iter().any(|root| path.starts_with(root)) {
                    out.push((*doc_id, doc));
                }
            }
        }
        out
    }

    #[allow(dead_code)]
    fn path_id_from_file_desc(&self, file_desc: &FileDesc) -> anyhow::Result<PathId> {
        match file_desc {
            FileDesc::Project { id: project } => {
                if let Some(proj) = self.projects.get(project) {
                    let full_path = std::path::PathBuf::from(&proj.root);
                    Ok(PathId::Path(full_path))
                } else {
                    Err(anyhow!("Project {} not found", project))
                }
            }
            FileDesc::ProjectFile { project, file } => {
                if project == RESERVED_BUFFERS_PROJECT {
                    return Ok(PathId::Buffer(file.clone()));
                }
                if project == RESERVED_FILES_PROJECT {
                    return Ok(PathId::Path(PathBuf::from(file)));
                }
                if let Some(proj) = self.projects.get(project) {
                    let full_path = std::path::PathBuf::from(&proj.root).join(file);
                    Ok(PathId::Path(full_path))
                } else {
                    Err(anyhow!("Project {} not found", project))
                }
            }
        }
    }

    #[tracing::instrument(skip_all, fields(my_id = self.host_id))]
    async fn handle_editor_message(
        &mut self,
        msg: lsp_server::Message,
        editor_tx: &mpsc::Sender<lsp_server::Message>,
        webchannel: &WebChannel,
    ) -> anyhow::Result<()> {
        tracing::info!("From editor: {}", message_to_string(&msg));
        match msg {
            lsp_server::Message::Request(req) => {
                let req_id = req.id.clone();
                let next = Next::new(editor_tx, Some(req_id), webchannel);
                if let Err(err) = self.handle_editor_request(req, &next).await {
                    tracing::error!("Uncaught error when handling editor request: {}", err);
                    // Errors that reach this level are internal error.
                    let err = lsp_server::ResponseError {
                        code: ErrorCode::InternalError as i32,
                        message: err.to_string(),
                        data: None,
                    };
                    next.send_resp((), Some(err)).await;
                }
                Ok(())
            }
            lsp_server::Message::Response(_) => Ok(()),
            lsp_server::Message::Notification(notif) => {
                let next = Next::new(editor_tx, None, webchannel);
                if let Err(err) = self.handle_editor_notification(&next, notif).await {
                    tracing::error!("Failed to handle editor notification: {}", err);
                    let params = serde_json::json!({
                        "message": err.to_string(),
                    });
                    next.send_notif(NotificationCode::InternalError, params)
                        .await;
                }
                Ok(())
            }
        }
    }

    async fn handle_editor_request<'a>(
        &mut self,
        req: lsp_server::Request,
        next: &Next<'a>,
    ) -> anyhow::Result<()> {
        match req.method.as_str() {
            "Initialize" => {
                // Return InitResp with host_id for Initialize request
                let resp = message::InitResp {
                    host_id: self.host_id.clone(),
                };
                next.send_resp(resp, None).await;
                Ok(())
            }
            "ConnectionState" => {
                // Return connection states for all active remotes
                let mut connections = Vec::new();
                for (host_id, remote) in &self.active_remotes {
                    connections.push(message::ConnectionStateEntry {
                        host_id: host_id.clone(),
                        state: remote.state,
                    });
                }
                let resp = message::ConnectionStateResp { connections };
                next.send_resp(resp, None).await;
                Ok(())
            }
            "ListProjects" => {
                let params: message::ListProjectsParams = serde_json::from_value(req.params)?;
                let host_id = params.host_id;
                if !self.check_connection(next, &host_id).await {
                    return Ok(());
                }
                self.list_files_from_editor(next, host_id, None).await?;
                Ok(())
            }
            "ListFiles" => {
                let params: message::ListFilesParams = serde_json::from_value(req.params)?;
                let host_id = params.dir.host_id().clone();
                if !self.check_connection(next, &host_id).await {
                    return Ok(());
                }
                let dir = params.dir.into();
                self.list_files_from_editor(next, host_id, Some(dir))
                    .await?;
                Ok(())
            }
            "OpenFile" => {
                let params: OpenFileParams = serde_json::from_value(req.params)?;
                let host_id = params.file.host_id().clone();
                if !self.check_connection(next, &host_id).await {
                    return Ok(());
                }
                self.open_file_from_editor(next, params.file, params.mode)
                    .await?;
                Ok(())
            }
            "SendOps" => {
                let params: SendOpsParams = serde_json::from_value(req.params)?;
                self.handle_send_ops_from_editor(next, params).await?;
                Ok(())
            }
            "Undo" => {
                let params: UndoParams = serde_json::from_value(req.params)?;
                let resp = self.handle_undo_from_editor(params)?;
                next.send_resp(resp, None).await;
                Ok(())
            }
            "ShareFile" => {
                let params: ShareFileParams = serde_json::from_value(req.params)?;
                let resp = self.handle_share_file_from_editor(params)?;
                next.send_resp(resp, None).await;
                Ok(())
            }
            "UpdateConfig" => {
                let params: UpdateConfigParams = serde_json::from_value(req.params)?;
                self.handle_update_config_from_editor(params)?;
                next.send_resp((), None).await;
                Ok(())
            }
            "DeclareProjects" => {
                let mut params: DeclareProjectsParams = serde_json::from_value(req.params)?;

                // Check for reserved project name.
                for project in &params.projects {
                    if project.name == RESERVED_BUFFERS_PROJECT
                        || project.name == RESERVED_FILES_PROJECT
                    {
                        let error = lsp_server::ResponseError {
                            code: ErrorCode::BadRequest as i32,
                            message: format!(
                                "Project name {} is reserved and cannot be used",
                                project.name
                            ),
                            data: None,
                        };
                        next.send_resp((), Some(error)).await;
                        return Ok(());
                    }
                }

                let res = expand_project_paths(&mut params.projects);
                if let Err(err) = res {
                    let error = lsp_server::ResponseError {
                        code: ErrorCode::IoError as i32,
                        message: err.to_string(),
                        data: None,
                    };
                    next.send_resp((), Some(error)).await;
                    return Ok(());
                }

                for project_entry in params.projects {
                    let project = Project {
                        name: project_entry.name.clone(),
                        root: project_entry.path,
                        meta: JsonMap::new(),
                    };
                    self.projects.insert(project.name.clone(), project);
                }

                next.send_resp((), None).await;
                Ok(())
            }
            "MoveFile" => {
                let params: MoveFileParams = serde_json::from_value(req.params)?;
                if !self.check_connection(next, &params.host_id).await {
                    return Ok(());
                }
                self.handle_move_file_from_editor(next, params).await?;
                Ok(())
            }
            "SaveFile" => {
                let params: SaveFileParams = serde_json::from_value(req.params)?;
                if !self
                    .check_connection(next, &params.file.host_id().clone())
                    .await
                {
                    return Ok(());
                }
                self.handle_save_file_from_editor(next, params).await?;
                Ok(())
            }
            "DisconnectFromFile" => {
                let params: DisconnectFileParams = serde_json::from_value(req.params)?;
                if !self
                    .check_connection(next, &params.file.host_id().clone())
                    .await
                {
                    return Ok(());
                }
                self.handle_disconnect_from_file(next, params).await?;
                Ok(())
            }
            "DeleteFile" => {
                let params: DeleteFileParams = serde_json::from_value(req.params)?;
                let host_id = params.file.host_id().clone();
                if !self.check_connection(next, &host_id).await {
                    return Ok(());
                }
                self.handle_delete_file_from_editor(next, params).await?;
                Ok(())
            }
            _ => {
                tracing::warn!("Unknown request method: {}", req.method);
                // TODO: send back an error response.
                Ok(())
            }
        }
    }

    async fn handle_editor_notification<'a>(
        &mut self,
        next: &Next<'a>,
        notif: lsp_server::Notification,
    ) -> anyhow::Result<()> {
        match notif.method.as_str() {
            "AcceptConnection" => {
                let params: AcceptConnectionParams = serde_json::from_value(notif.params)?;
                if self.accepting.get(&params.signaling_addr).is_some() {
                    return Ok(());
                }
                self.accepting.insert(params.signaling_addr.clone(), ());
                next.send_notif(
                    NotificationCode::AcceptingConnection,
                    serde_json::json!({
                        "signaling_addr": params.signaling_addr,
                    }),
                )
                .await;
                let key_cert = self.key_cert.clone();
                let mut webchannel_1 = next.webchannel.clone();
                let editor_tx_1 = next.editor_tx.clone();

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
                if !self.remote_connected(&params.host_id) {
                    self.connect(
                        next,
                        params.host_id,
                        params.signaling_addr,
                        params.transport_type,
                    )
                    .await;
                } else {
                    next.send_notif(
                        NotificationCode::Connected,
                        HostAndMessageNote {
                            host_id: params.host_id,
                            message: "Already connected".to_string(),
                        },
                    )
                    .await;
                }
            }
            "SendInfo" => {
                let params: SendInfoParams = serde_json::from_value(notif.params)?;
                self.handle_send_info_from_editor(next, params).await?;
            }

            _ => {
                tracing::warn!("Unknown notification method: {}", notif.method);
                // TODO: send back an error response.
            }
        }
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(my_id = self.host_id))]
    async fn handle_remote_message(
        &mut self,
        msg: webchannel::Message,
        editor_tx: &mpsc::Sender<lsp_server::Message>,
        webchannel: &WebChannel,
    ) -> anyhow::Result<()> {
        tracing::info!("From remote: {}", &msg);
        let next = Next::new(editor_tx, msg.req_id.clone(), webchannel);
        match msg.body {
            Msg::IceProgress(remote_hostid, progress) => {
                next.send_notif(
                    NotificationCode::ConnectionProgress,
                    HostAndMessageNote {
                        host_id: remote_hostid,
                        message: progress,
                    },
                )
                .await;
                Ok(())
            }
            Msg::FailedToConnect(host_id, reason) => {
                if let Some(remote) = self.active_remotes.get_mut(&host_id) {
                    remote.state = ConnectionState::FailedToConnect;
                    remote.next_reconnect_stride = 1;
                    remote.next_reconnect_time = Instant::now();
                }
                next.send_notif(
                    NotificationCode::ConnectionBroke,
                    ConnectionBrokeNote { host_id, reason },
                )
                .await;
                Ok(())
            }
            Msg::ConnectionBroke(host_id) => {
                if let Some(remote) = self.active_remotes.get_mut(&host_id) {
                    remote.state = ConnectionState::Disconnected;
                    remote.next_reconnect_stride = 1;
                    remote.next_reconnect_time = Instant::now();
                }
                next.send_notif(
                    NotificationCode::ConnectionBroke,
                    ConnectionBrokeNote {
                        host_id,
                        reason: "".to_string(),
                    },
                )
                .await;
                Ok(())
            }
            Msg::Hey(hey_msg) => {
                if let Some(remote) = self.active_remotes.get_mut(&msg.host) {
                    remote.state = ConnectionState::Connected;
                }
                next.send_notif(
                    NotificationCode::Connected,
                    HostAndMessageNote {
                        host_id: msg.host,
                        message: hey_msg.message,
                    },
                )
                .await;

                // We might get new trusted host from accepting
                // connection, try to save it.
                let mut config = self.config.config();
                config.trusted_hosts = self.trusted_hosts.lock().unwrap().clone();
                let _ = self.config.replace_and_save(config);
                Ok(())
            }
            Msg::ListFiles { dir } => {
                if msg.req_id.is_some() {
                    self.list_files_from_remote(&next, dir, msg.host.clone())
                        .await?;
                } else {
                    tracing::warn!("Received ListFiles without req_id, ignoring.");
                    send_to_remote(
                        next.webchannel,
                        &msg.host,
                        None,
                        Msg::BadRequest("Missing req_id".to_string()),
                    )
                    .await;
                }
                Ok(())
            }
            Msg::FileList(files) => {
                if msg.req_id.is_none() {
                    tracing::warn!(
                        "Received FileList from remote {} without req_id, ignoring",
                        msg.host
                    );
                    let msg = HostAndMessageNote {
                        host_id: msg.host.clone(),
                        message: format!(
                            "Received file list from remote {}, but without req_id",
                            msg.host
                        ),
                    };
                    next.send_notif(NotificationCode::UnimportantError, msg)
                        .await;
                    return Ok(());
                }
                next.send_resp(ListFilesResp { files }, None).await;
                Ok(())
            }
            Msg::ErrorResp(_, error_msg) => {
                if msg.req_id.is_none() {
                    tracing::warn!("Received ErrorResp without req_id, ignoring");
                    let msg = HostAndMessageNote {
                        host_id: msg.host.clone(),
                        message: format!(
                            "Received error response from remote {}, but without response id: {}",
                            msg.host, error_msg
                        ),
                    };
                    next.send_notif(NotificationCode::UnimportantError, msg)
                        .await;
                    return Ok(());
                }

                // Send error response back to the editor
                let err = lsp_server::ResponseError {
                    code: 500,
                    message: error_msg,
                    data: None,
                };
                next.send_resp((), Some(err)).await;

                Ok(())
            }
            Msg::RequestFile(file_desc, mode) => {
                if msg.req_id.is_some() {
                    self.handle_request_file(&next, file_desc, msg.host.clone(), mode)
                        .await?;
                } else {
                    tracing::warn!("Received RequestFile without req_id, ignoring");
                    next.send_to_remote(&msg.host, Msg::BadRequest("Missing req_id".to_string()))
                        .await;
                }
                Ok(())
            }
            Msg::Snapshot(snapshot) => {
                if msg.req_id.is_some() {
                    self.handle_snapshot(&next, snapshot, msg.host.clone())
                        .await?;
                } else {
                    tracing::warn!("Received Snapshot without req_id, ignoring");
                }
                Ok(())
            }
            Msg::OpFromClient(context_ops) => {
                self.handle_op_from_client(&next, context_ops, msg.host.clone())
                    .await?;
                Ok(())
            }
            Msg::OpFromServer { doc, ops } => {
                self.handle_op_from_server(&next, doc, ops, msg.host.clone())
                    .await?;
                Ok(())
            }
            Msg::RequestOps { doc, after } => {
                self.handle_request_ops(&next, doc, after, msg.host.clone())
                    .await?;
                Ok(())
            }
            Msg::MoveFile(project_id, old_path, new_path) => {
                self.handle_move_file(&next, project_id, old_path, new_path, msg.host.clone())
                    .await?;
                Ok(())
            }
            Msg::FileMoved(project, old_path, new_path) => {
                // Non-nil req_id, meaning this is a response to our
                // request.
                if msg.req_id.is_some() {
                    // Send MoveFileResp to editor.
                    let resp = MoveFileResp {
                        host_id: msg.host.clone(),
                        project: project.clone(),
                        old_path: old_path.clone(),
                        new_path: new_path.clone(),
                    };
                    next.send_resp(resp, None).await;
                } else {
                    // No req_id, meaning we’re receiving this message
                    // because a doc we’re subscribed to moved.
                    let msg = FileMovedNotif {
                        host_id: msg.host,
                        project,
                        old_path,
                        new_path,
                    };
                    next.send_notif(NotificationCode::FileMoved, msg).await;
                }
                Ok(())
            }
            Msg::SaveFile(doc_id) => {
                let res = self.save_file_to_disk(&doc_id).await;
                let resp = if let Err(err) = res {
                    Msg::ErrorResp(ErrorCode::IoError, err.to_string())
                } else {
                    Msg::FileSaved(doc_id)
                };
                next.send_to_remote(&msg.host, resp).await;
                Ok(())
            }
            Msg::FileSaved(doc_id) => {
                if msg.req_id.is_none() {
                    tracing::warn!("Received FileSaved without req_id, ignoring");
                    next.send_to_remote(&msg.host, Msg::BadRequest("Missing req_id".to_string()))
                        .await;
                    return Ok(());
                }

                if let Some(remote_doc) = self.remote_docs.get(&msg.host, doc_id) {
                    let file = EditorFileDesc::new(remote_doc.file_desc.clone(), msg.host.clone());
                    let resp = SaveFileResp { file };
                    next.send_resp(resp, None).await;
                } else {
                    tracing::warn!(
                        "Doc {} not found when handling FileSaved from {}",
                        doc_id,
                        msg.host
                    );
                };
                Ok(())
            }
            Msg::StopSendingOps(doc_id) => {
                // Remove the remote host from the doc's subscriber list.
                if let Some(doc) = self.docs.get_mut(&doc_id) {
                    if doc.subscribers.remove(&msg.host).is_some() {
                        tracing::info!("Removed {} from subscribers of doc {}", msg.host, doc_id);
                    } else {
                        tracing::warn!("Host {} was not subscribed to doc {}", msg.host, doc_id);
                    }
                } else {
                    tracing::warn!(
                        "Doc {} not found when handling StopSendingOps from {}",
                        doc_id,
                        msg.host
                    );
                }
                Ok(())
            }
            Msg::DeleteFile(file_desc) => {
                self.handle_delete_file_from_remote(next, file_desc, msg.host)
                    .await?;
                Ok(())
            }
            Msg::FileDeleted(file_desc) => {
                // Response from remote that file was deleted.
                let editor_file_desc = EditorFileDesc::new(file_desc, msg.host.clone());
                if msg.req_id.is_some() {
                    // This is a response to our request, send success to editor.
                    let resp = DeleteFileResp {
                        file: editor_file_desc,
                    };
                    next.send_resp(resp, None).await;
                } else {
                    // This is a notification, send to editor.
                    let msg = FileDeletedNotif {
                        file: editor_file_desc,
                    };
                    next.send_notif(NotificationCode::FileDeleted, msg).await;
                }
                Ok(())
            }
            Msg::InfoFromClient(info) => {
                // Received from a client, broadcast to all subscribers
                let res = self.broadcast_info(&next, info, None).await;
                if let Err(err) = res {
                    tracing::warn!("{} when handling InfoFromClient from {}", err, msg.host);
                }
                Ok(())
            }
            Msg::InfoFromServer(info) => {
                let remote_doc = self.remote_docs.get(&msg.host, info.doc_id);
                if let Some(remote_doc) = remote_doc {
                    let editor_file =
                        EditorFileDesc::new(remote_doc.file_desc.clone(), msg.host.clone());

                    let info_json = serde_json::to_value(&info.value)?;
                    let note = SendInfoNote {
                        file: editor_file,
                        info: info_json,
                    };
                    next.send_notif(NotificationCode::InfoReceived, note).await;
                } else {
                    next.send_to_remote(&msg.host, Msg::StopSendingOps(info.doc_id))
                        .await;
                }
                Ok(())
            }
            Msg::PermissionDenied(message) => {
                if msg.req_id.is_some() {
                    let err = lsp_server::ResponseError {
                        code: ErrorCode::PermissionDenied as i32,
                        message,
                        data: None,
                    };
                    next.send_resp((), Some(err)).await;
                } else {
                    let msg = HostAndMessageNote {
                        host_id: msg.host,
                        message,
                    };
                    next.send_notif(NotificationCode::InternalError, msg).await;
                }
                Ok(())
            }
            _ => {
                tracing::error!(
                    "Unrecognized message type from {}: {:?}",
                    msg.host,
                    msg.body
                );
                Ok(())
            }
        }
    }

    // **** Handler functions

    async fn connect<'a>(
        &mut self,
        next: &Next<'a>,
        host_id: ServerId,
        signaling_addr: String,
        transport_type: webchannel::TransportType,
    ) {
        if host_id == self.host_id {
            return;
        }
        next.send_notif(
            NotificationCode::Connecting,
            ConnectingNote {
                host_id: host_id.clone(),
            },
        )
        .await;

        // If there’s an existing remote, keep the reconnect time/stride.
        if let Some(remote) = self.active_remotes.get_mut(&host_id) {
            remote.state = ConnectionState::Connecting;
        } else {
            self.active_remotes.insert(
                host_id.clone(),
                RemoteState::connecting(signaling_addr.clone(), transport_type.clone()),
            );
        }

        let key_cert = self.key_cert.clone();
        let webchannel_1 = next.webchannel.clone();
        let my_host_id = self.host_id.clone();

        tokio::spawn(async move {
            if let Err(err) = webchannel_1
                .connect(
                    host_id.clone(),
                    my_host_id.clone(),
                    key_cert,
                    &signaling_addr,
                    transport_type,
                )
                .await
            {
                let msg = format!("Failed to connect to {}: {}", host_id, err);
                tracing::error!(msg);
                send_to_remote(
                    &webchannel_1,
                    &my_host_id,
                    None,
                    Msg::FailedToConnect(host_id, err.to_string()),
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
    async fn list_files_from_editor<'a>(
        &mut self,
        next: &Next<'a>,
        host_id: ServerId,
        dir: Option<FileDesc>,
    ) -> anyhow::Result<()> {
        if self.host_id != host_id {
            next.send_to_remote(&host_id, Msg::ListFiles { dir }).await;
            return Ok(());
        }

        let files = self.list_files_from_disk(dir).await;

        match files {
            Ok(files) => {
                next.send_resp(ListFilesResp { files }, None).await;
            }
            Err(err) => {
                let err = lsp_server::ResponseError {
                    code: ErrorCode::IoError as i32,
                    message: err.to_string(),
                    data: None,
                };
                next.send_resp((), Some(err)).await;
            }
        }
        Ok(())
    }

    // In attached mode, delegate to editor, otherwise read from disk
    // and response immediately.
    async fn list_files_from_remote<'a>(
        &mut self,
        next: &Next<'a>,
        dir: Option<FileDesc>,
        remote_host_id: ServerId,
    ) -> anyhow::Result<()> {
        match self.list_files_from_disk(dir).await {
            Ok(files) => {
                let msg = Msg::FileList(files);
                next.send_to_remote(&remote_host_id, msg).await;
            }
            Err(err) => {
                let msg = Msg::ErrorResp(ErrorCode::IoError, err.to_string());
                next.send_to_remote(&remote_host_id, msg).await;
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
                // Special handling for buffers project.
                if project == RESERVED_BUFFERS_PROJECT {
                    return Err(anyhow!(
                        "{} doesn't have subdirectories",
                        RESERVED_BUFFERS_PROJECT
                    ));
                }

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
                        file: EditorFileDesc {
                            host_id: self.host_id.clone(),
                            project: project.clone(),
                            file: file_rel,
                        },
                        filename: file_name,
                        is_directory: stat.is_dir(),
                        meta: JsonMap::new(),
                    });
                }
                // Sort entries alphanumerically by filename
                result.sort_by(|a, b| a.filename.cmp(&b.filename));
                Ok(result)
            }
            Some(FileDesc::Project { id }) => {
                // Special handling for _buffers project: list only in-memory buffers.
                if id == RESERVED_BUFFERS_PROJECT {
                    let mut result = vec![];
                    for (_doc_id, doc) in &self.docs {
                        if doc.abs_filename.is_buffer() {
                            result.push(ListFileEntry {
                                file: EditorFileDesc {
                                    host_id: self.host_id.clone(),
                                    project: RESERVED_BUFFERS_PROJECT.to_string(),
                                    file: doc.name.clone(),
                                },
                                filename: doc.name.clone(),
                                is_directory: false,
                                meta: doc.meta.clone(),
                            });
                        }
                    }
                    result.sort_by(|a, b| a.filename.cmp(&b.filename));
                    return Ok(result);
                }

                // Special handling for _files project: list
                // standalone files not in any project.
                if id == RESERVED_FILES_PROJECT {
                    let mut entries = Vec::new();
                    for (_doc_id, doc) in self.local_docs_not_in_any_project() {
                        if let PathId::Path(path) = &doc.abs_filename {
                            entries.push(ListFileEntry {
                                file: EditorFileDesc {
                                    host_id: self.host_id.clone(),
                                    project: RESERVED_FILES_PROJECT.to_string(),
                                    file: path.display().to_string(),
                                },
                                filename: doc.name.clone(),
                                is_directory: false,
                                meta: doc.meta.clone(),
                            });
                        }
                    }
                    entries.sort_by(|a, b| a.filename.cmp(&b.filename));
                    return Ok(entries);
                }

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
                        file: EditorFileDesc {
                            host_id: self.host_id.clone(),
                            project: id.clone(),
                            file: file_name.clone(),
                        },
                        filename: file_name,
                        is_directory: stat.is_dir(),
                        meta: JsonMap::new(),
                    });
                }
                // Sort entries alphanumerically by filename
                result.sort_by(|a, b| a.filename.cmp(&b.filename));
                Ok(result)
            }
            None => {
                // List top-level projects and docs.
                let mut proj_result = vec![];
                // let mut buf_result = vec![];
                for (_, project) in &self.projects {
                    proj_result.push(ListFileEntry {
                        file: EditorFileDesc {
                            host_id: self.host_id.clone(),
                            project: project.name.clone(),
                            file: String::new(),
                        },
                        filename: project.name.clone(),
                        is_directory: true,
                        meta: project.meta.clone(),
                    });
                }
                for reserved in [RESERVED_BUFFERS_PROJECT, RESERVED_FILES_PROJECT] {
                    proj_result.push(ListFileEntry {
                        file: EditorFileDesc {
                            host_id: self.host_id.clone(),
                            project: reserved.to_string(),
                            file: String::new(),
                        },
                        filename: reserved.to_string(),
                        is_directory: true,
                        meta: JsonMap::new(),
                    });
                }
                // for (doc_id, doc) in &self.docs {
                //     if doc.abs_filename.is_none() {
                //         buf_result.push(ListFileEntry {
                //             file: EditorFileDesc::File {
                //                 host_id: self.host_id,
                //                 id: *doc_id
                //             },
                //             filename: doc.name.clone(),
                //             is_directory: false,
                //             meta: doc.meta.clone(),
                //         });
                //     }
                // }
                // Sort entries alphanumerically by filename
                proj_result.sort_by(|a, b| a.filename.cmp(&b.filename));
                // buf_result.sort_by(|a, b| a.filename.cmp(&b.filename));
                // proj_result.extend_from_slice(&buf_result[..]);
                Ok(proj_result)
            }
        }
    }

    /// Read file from disk and return the content as string.
    /// Return (content, file object, abs_path).
    async fn open_file_from_disk(
        &self,
        project_id: &ProjectId,
        rel_path: &str,
        mode: OpenMode,
    ) -> anyhow::Result<(String, File, PathBuf)> {
        if project_id == RESERVED_BUFFERS_PROJECT {
            return Err(anyhow!("No existing doc {}", rel_path));
        }

        // Construct full path.
        let full_path = if project_id == RESERVED_FILES_PROJECT {
            // For _files file, `rel_path` is an absolute path.
            std::path::PathBuf::from(rel_path)
        } else {
            // Normal project: join project root with the relative path.
            let project = self
                .projects
                .get(project_id)
                .ok_or_else(|| anyhow!("Project {} not found", project_id))?;
            std::path::PathBuf::from(&project.root).join(rel_path)
        };
        let mut open_options = std::fs::OpenOptions::new();
        open_options.read(true).write(true);
        if matches!(mode, OpenMode::Create) {
            open_options.create(true);
        }

        let mut file = open_options
            .open(&full_path)
            .with_context(|| format!("Failed to create/open file: {}", full_path.display()))?;

        // Check if file is binary.
        let mut buffer = vec![0; 8192]; // Read first 8KB
        let bytes_read = file
            .read(&mut buffer)
            .with_context(|| format!("Failed to read file: {}", full_path.display()))?;
        buffer.truncate(bytes_read);

        if content_inspector::inspect(&buffer).is_binary() {
            return Err(anyhow!("Cannot open binary file: {}", full_path.display()));
        }

        // Seek back to beginning and read as string
        file.seek(std::io::SeekFrom::Start(0))
            .with_context(|| format!("Failed to seek file: {}", full_path.display()))?;

        let mut content = String::new();
        file.read_to_string(&mut content)
            .with_context(|| format!("Failed to read file: {}", full_path.display()))?;

        Ok((content, file, full_path))
    }

    async fn open_file_from_editor<'a>(
        &mut self,
        next: &Next<'a>,
        file_desc: EditorFileDesc,
        mode: OpenMode,
    ) -> anyhow::Result<()> {
        let desc: FileDesc = file_desc.clone().into();

        if &self.host_id == file_desc.host_id() {
            // Local file.

            // Check if trying to open a project directory first.
            if file_desc.is_project() {
                let err = lsp_server::ResponseError {
                    code: ErrorCode::BadRequest as i32,
                    message: "Cannot open a project directory".to_string(),
                    data: None,
                };
                next.send_resp((), Some(err)).await;
                return Ok(());
            }

            let res = self.path_id_from_file_desc(&desc);
            if let Err(err) = res {
                let err = lsp_server::ResponseError {
                    code: ErrorCode::IoError as i32,
                    message: err.to_string(),
                    data: None,
                };
                next.send_resp((), Some(err)).await;
                return Ok(());
            }
            let path_id = res.unwrap();
            // Check if file is alreay opened.
            let mut opened_doc = None;
            for (doc_id, doc) in &mut self.docs {
                if doc.matches(&path_id) {
                    opened_doc = Some((doc_id.clone(), doc));
                }
            }

            let (doc_id, doc) = if let Some(doc) = opened_doc {
                doc
            } else {
                // Not opened, read from disk and create doc.
                let res = self
                    .open_file_from_disk(&file_desc.project, &file_desc.file, mode)
                    .await;
                if let Err(err) = res {
                    let err = lsp_server::ResponseError {
                        code: ErrorCode::IoError as i32,
                        message: err.to_string(),
                        data: None,
                    };
                    next.send_resp((), Some(err)).await;
                    return Ok(());
                }
                let (content, disk_file, full_path) = res.unwrap();
                // Create new doc
                let doc_id = self.next_doc_id.fetch_add(1, Ordering::SeqCst);
                let base_name = std::path::Path::new(&file_desc.file)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&file_desc.file)
                    .to_string();

                let doc = Doc::new(
                    base_name.clone(),
                    JsonMap::new(),
                    PathId::Path(full_path),
                    desc.clone(),
                    &content,
                    ServerEngine::new(content.len() as u64),
                    Some(disk_file),
                );
                self.docs.insert(doc_id, doc);
                (doc_id, self.docs.get_mut(&doc_id).unwrap())
            };

            // Add ourselves as a subscriber.
            doc.subscribers.insert(self.host_id.clone(), 0);

            // If the remote_doc doesn’t exists already, add it.
            let content: String = doc.buffer.iter().collect();
            if self.remote_docs.get_by_editor_desc(&file_desc).is_none() {
                let mut buffer = GapBuffer::new();
                buffer.insert_many(0, content.chars());
                let remote_doc = RemoteDoc {
                    name: doc.name.clone(),
                    file_desc: desc.clone(),
                    next_site_seq: 1,
                    meta: JsonMap::new(),
                    remote_op_buffer: Vec::new(),
                    // Use the current gseq from the doc’s engine.
                    engine: ClientEngine::new(
                        self.site_id,
                        doc.engine.current_seq(),
                        content.chars().count() as u64,
                    ),
                    buffer,
                };
                self.remote_docs.insert(
                    self.host_id.clone(),
                    doc_id,
                    remote_doc,
                    file_desc.clone(),
                );
            };

            // Send response
            let resp = OpenFileResp {
                content,
                site_id: self.site_id,
                filename: doc.name.clone(),
                file: file_desc.clone(),
            };
            next.send_resp(resp, None).await;
            return Ok(());
        }

        // Open remote file.

        // Check if we already have this file in remote_docs.
        let already_open = self.get_remote_doc(&file_desc);
        if let Some((_doc_id, remote_doc)) = already_open {
            let resp = OpenFileResp {
                content: remote_doc.buffer.iter().collect(),
                site_id: remote_doc.engine.site(),
                filename: remote_doc.name.clone(),
                file: file_desc,
            };
            next.send_resp(resp, None).await;
            return Ok(());
        }

        // Don’t have it opened, send request to remote.
        let msg = Msg::RequestFile(file_desc.clone().into(), mode);
        next.send_to_remote(&file_desc.host_id(), msg).await;
        Ok(())
    }

    async fn handle_request_file<'a>(
        &mut self,
        next: &Next<'a>,
        file_desc: FileDesc,
        remote_host_id: ServerId,
        mode: OpenMode,
    ) -> anyhow::Result<()> {
        // Check permission.
        if mode == OpenMode::Create && !self.config.create_allowed(&remote_host_id) {
            next.send_to_remote(
                &remote_host_id,
                Msg::PermissionDenied("Permission denied for creating file".to_string()),
            )
            .await;
            return Ok(());
        }

        let res = self.path_id_from_file_desc(&file_desc);
        if let Err(err) = res {
            next.send_to_remote(
                &remote_host_id,
                Msg::ErrorResp(ErrorCode::IoError, err.to_string()),
            )
            .await;
            return Ok(());
        }
        let path_id = res.unwrap();

        let site_id = self.site_id_of(&remote_host_id);
        match &file_desc {
            FileDesc::ProjectFile { project, file } => {
                // Check if file is already opened.
                for (doc_id, doc) in &mut self.docs {
                    if doc.matches(&path_id) {
                        // Don’t add remote as subscriber, wait until we
                        // receive RequestOps message.

                        let snapshot = NewSnapshot {
                            content: doc.buffer.iter().collect(),
                            name: doc.name.clone(),
                            seq: doc.engine.current_seq(),
                            site_id,
                            file_desc: file_desc.clone(),
                            doc_id: *doc_id,
                        };
                        next.send_to_remote(&remote_host_id, Msg::Snapshot(snapshot))
                            .await;
                        return Ok(());
                    }
                }

                // Not already open, read from disk.
                let res = self.open_file_from_disk(project, file, mode).await;
                if let Err(err) = res {
                    next.send_to_remote(
                        &remote_host_id,
                        Msg::ErrorResp(ErrorCode::IoError, format!("File not found: {}", err)),
                    )
                    .await;
                    return Ok(());
                }
                let (content, disk_file, full_path) = res.unwrap();

                // Create new doc
                let doc_id = self.next_doc_id.fetch_add(1, Ordering::SeqCst);
                let filename = std::path::Path::new(file)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(file)
                    .to_string();

                let doc = Doc::new(
                    filename.clone(),
                    JsonMap::new(),
                    PathId::Path(full_path),
                    file_desc.clone(),
                    &content,
                    ServerEngine::new(content.len() as u64),
                    Some(disk_file),
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
                next.send_to_remote(&remote_host_id, Msg::Snapshot(snapshot))
                    .await;
            }
            FileDesc::Project { .. } => {
                next.send_to_remote(
                    &remote_host_id,
                    Msg::BadRequest("Cannot open a project directory".to_string()),
                )
                .await;
            }
        }
        Ok(())
    }

    async fn handle_snapshot<'a>(
        &mut self,
        next: &Next<'a>,
        snapshot: NewSnapshot,
        remote_host_id: ServerId,
    ) -> anyhow::Result<()> {
        // First check if there's existing doc with the same file_desc
        // from the same host.
        for ((host, _), remote_doc) in self.remote_docs.iter() {
            if host == &remote_host_id && remote_doc.file_desc == snapshot.file_desc {
                let err = lsp_server::ResponseError {
                    code: ErrorCode::BadRequest as i32,
                    message: format!(
                        "File {} is already opened from {}",
                        remote_doc.name, remote_host_id
                    ),
                    data: None,
                };
                next.send_resp((), Some(err)).await;
                return Ok(());
            }
        }

        // Create RemoteDoc
        // Use the site_id assigned by the remote server (from snapshot)
        let mut buffer = GapBuffer::new();
        buffer.insert_many(0, snapshot.content.chars());
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
            buffer,
        };

        // Store with (host_id, remote_doc_id) as key.
        let editor_file_desc =
            EditorFileDesc::new(snapshot.file_desc.clone(), remote_host_id.clone());
        self.remote_docs.insert(
            remote_host_id.clone(),
            snapshot.doc_id,
            remote_doc,
            editor_file_desc,
        );

        // Send response to editor with the remote doc_id
        let resp = OpenFileResp {
            content: snapshot.content.clone(),
            site_id: snapshot.site_id,
            filename: snapshot.name,
            file: EditorFileDesc::new(snapshot.file_desc.clone(), remote_host_id.clone()),
        };
        next.send_resp(resp, None).await;

        // Send RequestOps to subscribe and get any ops after the snapshot
        let msg = Msg::RequestOps {
            doc: snapshot.doc_id,
            after: snapshot.seq,
        };
        next.send_to_remote(&remote_host_id, msg).await;
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

    /// Get a remote doc by `EditorFileDesc`.
    /// Returns the remote doc id and a mutable reference to the doc if found.
    fn get_remote_doc_mut(
        &mut self,
        editor_file: &EditorFileDesc,
    ) -> Option<(DocId, &mut RemoteDoc)> {
        self.remote_docs.get_mut_by_editor_desc(editor_file)
    }

    /// Get a remote doc by `EditorFileDesc`.
    /// Returns the remote doc id and an immutable reference to the doc if found.
    fn get_remote_doc(&self, editor_file: &EditorFileDesc) -> Option<(DocId, &RemoteDoc)> {
        self.remote_docs.get_by_editor_desc(editor_file)
    }

    async fn handle_op_from_client<'a>(
        &mut self,
        next: &Next<'a>,
        context_ops: ContextOps,
        remote_host_id: ServerId,
    ) -> anyhow::Result<()> {
        let doc_id = context_ops.doc();

        // Check write permission
        if !self.config.write_allowed(&remote_host_id) {
            next.send_to_remote(
                &remote_host_id,
                Msg::PermissionDenied("Write permission denied".to_string()),
            )
            .await;
            return Ok(());
        }

        // Find doc in docs (as the server/owner of the doc)
        let doc = self.docs.get_mut(&doc_id);
        if doc.is_none() {
            next.send_to_remote(
                &remote_host_id,
                Msg::BadRequest(format!("Doc {} not found", doc_id)),
            )
            .await;
            return Ok(());
        }
        let doc = doc.unwrap();

        let subscriber_data = doc.subscribers.get(&remote_host_id).copied();
        if subscriber_data.is_none() {
            let msg = Msg::BadRequest(format!("Doc {} ({}) not open", doc.name, doc_id));
            next.send_to_remote(&remote_host_id, msg).await;
            return Ok(());
        }
        let expected_seq = subscriber_data.unwrap() + 1;

        // Check if first op's local site seq matches subscriber's saved seq
        if let Some(first_op) = context_ops.ops.first() {
            if first_op.site_seq != expected_seq {
                // Send DocFatal message back
                let err = format!(
                    "Site seq mismatch: expected {}, got {}",
                    expected_seq, first_op.site_seq
                );
                let msg = Msg::DocFatal(doc_id, err);
                next.send_to_remote(&remote_host_id, msg).await;
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
            next.send_to_remote(
                subscriber_host_id,
                Msg::OpFromServer {
                    doc: doc_id,
                    ops: processed_ops.clone(),
                },
            )
            .await;
        }

        Ok(())
    }

    async fn handle_request_ops<'a>(
        &mut self,
        next: &Next<'a>,
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
            next.send_to_remote(
                &remote_host_id,
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
            next.send_to_remote(&remote_host_id, Msg::OpFromServer { doc: doc_id, ops })
                .await;
        }

        Ok(())
    }

    async fn handle_move_file<'a>(
        &mut self,
        next: &Next<'a>,
        project_id: ProjectId,
        old_path: String,
        new_path: String,
        remote_host_id: ServerId,
    ) -> anyhow::Result<()> {
        let res = self
            .move_file_on_disk(&project_id, &old_path, &new_path)
            .await;

        match res {
            Ok(subscribers) => {
                // Send FileMoved notification to all other
                // subscribers, including ourselves and
                // originall caller.
                for subscriber_id in subscribers {
                    let req_id = if remote_host_id == subscriber_id {
                        next.req_id.clone()
                    } else {
                        None
                    };
                    send_to_remote(
                        next.webchannel,
                        &subscriber_id,
                        req_id,
                        Msg::FileMoved(project_id.clone(), old_path.clone(), new_path.clone()),
                    )
                    .await;
                }
            }
            Err(err) => {
                next.send_to_remote(
                    &remote_host_id,
                    Msg::ErrorResp(ErrorCode::IoError, err.to_string()),
                )
                .await;
            }
        }
        Ok(())
    }

    async fn handle_op_from_server<'a>(
        &mut self,
        next: &Next<'a>,
        doc_id: DocId,
        ops: Vec<FatOp>,
        remote_host_id: ServerId,
    ) -> anyhow::Result<()> {
        if ops.is_empty() {
            return Ok(());
        }

        // Find doc in remote_docs with (host_id, doc_id) key
        let remote_doc = self.remote_docs.get_mut(&remote_host_id, doc_id);
        if remote_doc.is_none() {
            tracing::warn!(
                "Received OpFromServer for unknown doc {} from {}",
                doc_id,
                remote_host_id
            );
            next.send_to_remote(&remote_host_id, Msg::StopSendingOps(doc_id))
                .await;
            return Ok(());
        }
        let remote_doc = remote_doc.unwrap();
        tracing::info!(
            "Received OpFromServer for doc {} from {}",
            doc_id,
            remote_host_id
        );

        let our_site_id = remote_doc.engine.site();
        let op_is_not_ours = ops[0].site() != our_site_id;

        let old_buffer_len = remote_doc.remote_op_buffer.len();
        for remote_op in ops {
            let expected_gseq =
                remote_doc.engine.current_gseq() + remote_doc.remote_op_buffer.len() as u32 + 1;
            let gseq = remote_op.seq;
            // This shouldn’t ever happen.
            if gseq.is_none() {
                tracing::error!("Received remote op without global seq: {:?}", &remote_op);
                next.send_notif(
                    NotificationCode::InternalError,
                    HostAndMessageNote {
                        host_id: remote_host_id.clone(),
                        message: "Received remote op without global seq, terminating doc"
                            .to_string(),
                    },
                )
                .await;
                self.remote_docs.remove(&remote_host_id, doc_id);
                return Ok(());
            }
            let gseq = gseq.unwrap();

            if gseq < expected_gseq {
                // Outdated, ignore.
                tracing::warn!(
                    "Received remote op with smaller gseq ({}) than expected ({}), ignoring",
                    gseq,
                    expected_gseq,
                )
            } else if gseq > expected_gseq {
                // Missing ops in the middle, probably due to disconnection.
                tracing::warn!(
                    "Received remote op with larger gseq ({}) than expected ({}), skip, sent ops request to remote",
                    gseq,
                    expected_gseq,
                );
                let msg = Msg::RequestOps {
                    doc: doc_id,
                    after: expected_gseq - 1,
                };
                next.send_to_remote(&remote_host_id, msg).await;
            } else {
                // Save remote ops to buffer
                remote_doc.remote_op_buffer.push(remote_op);
            }
        }

        // Notify editor to get the new remote ops.
        let new_buffer_len = remote_doc.remote_op_buffer.len();
        if op_is_not_ours && new_buffer_len > old_buffer_len {
            let file_desc =
                EditorFileDesc::new(remote_doc.file_desc.clone(), remote_host_id.clone());
            next.send_notif(
                NotificationCode::RemoteOpsArrived,
                RemoteOpsArrivedNote {
                    host_id: remote_host_id,
                    doc_id,
                    file: file_desc,
                },
            )
            .await;
        }

        Ok(())
    }

    async fn handle_send_info_from_editor<'a>(
        &mut self,
        next: &Next<'a>,
        params: SendInfoParams,
    ) -> anyhow::Result<()> {
        let SendInfoParams { info, file } = params;

        // Remote doc.
        if let Some((doc_id, _remote_doc)) = self.get_remote_doc_mut(&file) {
            let info_value = serde_json::to_string(&info)?;
            let info = Info {
                doc_id,
                sender: file.host_id().clone(),
                value: info_value,
            };

            let remote_host_id = file.host_id().clone();
            next.send_to_remote(&remote_host_id, Msg::InfoFromClient(info))
                .await;
            return Ok(());
        }

        // Local doc. Check that we have a opened doc.
        let res = self.get_remote_doc(&file).map_or_else(
            || None,
            |(doc_id, _doc)| self.docs.get(&doc_id).map(|doc| (doc_id, doc)),
        );
        if res.is_none() {
            tracing::warn!("Received a info for non-exist file {}", file);
            return Ok(());
        }

        let (doc_id, _doc) = res.unwrap();

        let info_value = serde_json::to_string(&info)?;
        let info = Info {
            doc_id,
            sender: self.host_id.clone(),
            value: info_value,
        };

        let host_id = self.host_id.clone();
        // This call shouldn’t return any error since we already
        // verified that the doc exists.
        self.broadcast_info(next, info, Some(&host_id))
            .await
            .unwrap();
        Ok(())
    }

    async fn handle_send_ops_from_editor<'a>(
        &mut self,
        next: &Next<'a>,
        params: SendOpsParams,
    ) -> anyhow::Result<()> {
        let SendOpsParams { ops, file } = params;
        let (doc_id, remote_doc) = match self.get_remote_doc_mut(&file) {
            Some(v) => v,
            None => {
                let err = lsp_server::ResponseError {
                    code: ErrorCode::IoError as i32,
                    message: format!("File not found: {}", file),
                    data: None,
                };
                next.send_resp((), Some(err)).await;
                return Ok(());
            }
        };
        let host_id = file.host_id().clone();

        // Drain remote ops from buffer
        let remote_ops: Vec<FatOp> = remote_doc.remote_op_buffer.drain(..).collect();

        // Process local ops from editor first, because we want to
        // pretent the local ops arrives before the remote ops.
        for editor_op in ops {
            // Process the op with the engine
            let site_seq = remote_doc.next_site_seq;
            remote_doc.next_site_seq += 1;

            let instructions = remote_doc.engine.process_local_op(editor_op, site_seq);

            match instructions {
                Ok(instrs) => {
                    // Apply each processed op to the buffer
                    for instr in instrs {
                        remote_doc.apply_edit_instruction(&instr);
                    }
                }
                Err(err) => {
                    tracing::error!("Failed to process local op: {}", err);
                    let err = lsp_server::ResponseError {
                        code: ErrorCode::DocFatal as i32,
                        message: format!("Failed to process op: {}", err),
                        data: None,
                    };
                    next.send_resp((), Some(err)).await;
                    // Remove from remote_docs and mapping.
                    self.remote_docs.remove(&host_id, doc_id);
                    return Ok(());
                }
            }
        }

        // Process the drained remote ops.
        let mut lean_ops = Vec::new();
        for remote_op in remote_ops {
            if let Some((lean_op, _seq)) = remote_doc.engine.process_remote_op(remote_op)? {
                remote_doc.apply_edit_instruction(&lean_op.op);
                lean_ops.push(lean_op);
            }
        }

        // Package local ops if any
        if let Some(context_ops) = remote_doc.engine.maybe_package_local_ops(doc_id) {
            // Send to the owner (could be ourselves or a remote)
            // Don't attach req_id since this isn’t a direct response
            // (not that it matters much).
            next.send_to_remote_no_req_id(&host_id, Msg::OpFromClient(context_ops))
                .await;
        }

        // Send response with processed remote ops
        let resp = SendOpsResp {
            ops: lean_ops,
            last_seq: remote_doc.engine.current_gseq(),
        };
        next.send_resp(resp, None).await;
        Ok(())
    }

    fn handle_undo_from_editor(&mut self, params: UndoParams) -> anyhow::Result<UndoResp> {
        let (_doc_id, remote_doc) = self
            .get_remote_doc_mut(&params.file)
            .ok_or_else(|| anyhow::anyhow!("Remote doc not found: {}", params.file))?;

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
        let filename = params.filename.clone();
        let filename = match expanduser::expanduser(filename.clone()) {
            Ok(buf) => buf.to_string_lossy().to_string(),
            Err(_) => filename,
        };
        let file_path = std::path::Path::new(&filename);

        // Check if a file with this name/path already exists.
        let file_desc = if file_path.is_absolute() {
            FileDesc::ProjectFile {
                project: RESERVED_FILES_PROJECT.to_string(),
                file: filename.clone(),
            }
        } else {
            FileDesc::ProjectFile {
                project: RESERVED_BUFFERS_PROJECT.to_string(),
                file: params.filename.clone(),
            }
        };
        let path_id = self.path_id_from_file_desc(&file_desc)?;
        for doc in self.docs.values() {
            if doc.matches(&path_id) {
                return Err(anyhow!("File {} already exists", params.filename));
            }
        }

        // Generate a new doc_id
        let doc_id = self.next_doc_id.fetch_add(1, Ordering::SeqCst);

        // If we recognized an absolute filename, open it for
        // read/write so we can save.
        let disk_file: Option<File> = if file_path.exists() {
            let mut options = std::fs::OpenOptions::new();
            options.read(true).write(true);
            match options.open(&file_path) {
                Ok(file) => Some(file),
                Err(err) => {
                    tracing::warn!("Failed to open file {}: {}", filename, err);
                    None
                }
            }
        } else {
            None
        };

        let path_id = if file_path.exists() {
            PathId::Path(PathBuf::from(file_path))
        } else {
            PathId::Buffer(filename)
        };

        let mut doc = Doc::new(
            params.filename.clone(),
            params.meta.clone(),
            path_id,
            file_desc.clone(),
            &params.content,
            ServerEngine::new(params.content.len() as u64),
            disk_file,
        );

        // Add ourselves as a subscriber since we’re also a client
        doc.subscribers.insert(self.host_id.clone(), 0);

        // Store the doc in our owned docs
        self.docs.insert(doc_id, doc);

        // Also create a RemoteDoc for ourselves (we act as both server and client)
        let mut buffer = GapBuffer::new();
        buffer.insert_many(0, params.content.chars());
        let remote_doc = RemoteDoc {
            name: params.filename.clone(),
            file_desc: file_desc.clone(),
            next_site_seq: 1,
            meta: params.meta,
            remote_op_buffer: Vec::new(),
            engine: ClientEngine::new(self.site_id, 0, params.content.chars().count() as u64),
            buffer,
        };
        let editor_file_desc = EditorFileDesc::new(file_desc, self.host_id.clone());
        self.remote_docs.insert(
            self.host_id.clone(),
            doc_id,
            remote_doc,
            editor_file_desc.clone(),
        );

        // Return response with file and site_id
        let resp = ShareFileResp {
            file: editor_file_desc,
            site_id: self.site_id,
        };
        Ok(resp)
    }

    async fn handle_move_file_from_editor<'a>(
        &mut self,
        next: &Next<'a>,
        params: MoveFileParams,
    ) -> anyhow::Result<()> {
        let MoveFileParams {
            host_id,
            project,
            old_path,
            new_path,
        } = params;
        if self.host_id != host_id {
            // Handle remotely.
            next.send_to_remote(&host_id, Msg::MoveFile(project, old_path, new_path))
                .await;
        } else {
            // Handle locally.
            let res = self.move_file_on_disk(&project, &old_path, &new_path).await;
            match res {
                Err(err) => {
                    let err = lsp_server::ResponseError {
                        code: ErrorCode::IoError as i32,
                        message: err.to_string(),
                        data: None,
                    };
                    next.send_resp((), Some(err)).await;
                }
                Ok(subscribers) => {
                    // Send response to original caller
                    let resp = MoveFileParams {
                        host_id: self.host_id.clone(),
                        project: project.clone(),
                        old_path: old_path.clone(),
                        new_path: new_path.clone(),
                    };
                    next.send_resp(resp, None).await;

                    // Send FileMoved notification to all subscribers
                    // if doc was affected.
                    for subscriber_id in subscribers {
                        if subscriber_id != self.host_id {
                            send_to_remote(
                                next.webchannel,
                                &subscriber_id,
                                None,
                                Msg::FileMoved(project.clone(), old_path.clone(), new_path.clone()),
                            )
                            .await;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    async fn move_file_on_disk(
        &mut self,
        project_id: &ProjectId,
        old_path: &str,
        new_path: &str,
    ) -> anyhow::Result<Vec<ServerId>> {
        // Look up project
        let project = self
            .projects
            .get(project_id)
            .ok_or_else(|| anyhow!("Project {} not found", project_id))?;

        // Construct full paths.
        let old_full_path = std::path::PathBuf::from(&project.root).join(old_path);
        let new_full_path = std::path::PathBuf::from(&project.root).join(new_path);

        // Check if target path already exists on disk.
        if new_full_path.exists() {
            return Err(anyhow!(
                "Target path already exists on disk: {}",
                new_full_path.display()
            ));
        }

        // Create parent directory if it doesn’t exist
        if let Some(parent) = new_full_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create parent directory: {}", parent.display())
            })?;
        }

        // Move the file
        std::fs::rename(&old_full_path, &new_full_path).with_context(|| {
            format!(
                "Failed to move file from {} to {}",
                old_full_path.display(),
                new_full_path.display()
            )
        })?;

        // Update any open docs affected by the move and collect
        // subscribers to notify.
        let mut subscriber_set: HashSet<ServerId> = HashSet::new();

        for doc in self.docs.values_mut() {
            match doc.abs_filename.clone() {
                PathId::Buffer(_) => {
                    return Err(anyhow!("Cannot move buffer"));
                }
                PathId::Path(abs_filename) => {
                    if *abs_filename == old_full_path {
                        // Exact file moved.
                        doc.abs_filename = PathId::Path(new_full_path.clone());
                        for sub in doc.subscribers.keys() {
                            subscriber_set.insert(sub.clone());
                        }
                    } else if abs_filename.starts_with(&old_full_path) {
                        // File under a moved directory: rebase relative path.
                        if let Some(rel) = pathdiff::diff_paths(abs_filename, &old_full_path) {
                            let rebased = new_full_path.join(rel);
                            doc.abs_filename = PathId::Path(rebased.clone());
                            for sub in doc.subscribers.keys() {
                                subscriber_set.insert(sub.clone());
                            }
                        }
                    }
                }
            }
        }

        Ok(subscriber_set.into_iter().collect())
    }

    async fn handle_save_file_from_editor<'a>(
        &mut self,
        next: &Next<'a>,
        params: SaveFileParams,
    ) -> anyhow::Result<()> {
        let SaveFileParams { file } = params.clone();
        let host_id = file.host_id().clone();
        if host_id != self.host_id {
            // Remote: resolve doc id and forward.
            if let Some((doc_id, _)) = self.get_remote_doc(&file) {
                next.send_to_remote(&host_id, Msg::SaveFile(doc_id)).await;
                return Ok(());
            } else {
                let err = lsp_server::ResponseError {
                    code: ErrorCode::IoError as i32,
                    message: format!("Remote file not open: {}", file),
                    data: None,
                };
                next.send_resp((), Some(err)).await;
                return Ok(());
            }
        }

        // Local: find the doc by file and save.
        let maybe_doc_id = self.get_remote_doc(&file);

        if let Some((doc_id, _doc)) = maybe_doc_id {
            let res = self.save_file_to_disk(&doc_id).await;
            if let Err(err) = res {
                let err = lsp_server::ResponseError {
                    code: ErrorCode::IoError as i32,
                    message: err.to_string(),
                    data: None,
                };
                next.send_resp((), Some(err)).await;
            } else {
                next.send_resp(SaveFileParams { file }, None).await;
            }
        } else {
            let err = lsp_server::ResponseError {
                code: ErrorCode::IoError as i32,
                message: "File not open".to_string(),
                data: None,
            };
            next.send_resp((), Some(err)).await;
        }
        Ok(())
    }

    async fn handle_disconnect_from_file<'a>(
        &mut self,
        next: &Next<'a>,
        params: DisconnectFileParams,
    ) -> anyhow::Result<()> {
        let DisconnectFileParams { file } = params;

        if let Some((doc_id, _)) = self.get_remote_doc(&file) {
            let host_id = file.host_id().clone();
            self.remote_docs.remove_by_editor_desc(&file);
            tracing::info!(
                "Disconnected from remote doc {} on host {}",
                doc_id,
                host_id
            );
            next.send_to_remote(&host_id, Msg::StopSendingOps(doc_id))
                .await;
        } else {
            tracing::info!("Doc {} not found, nothing to disconnect", file);
        }

        // Send empty response to indicate success
        next.send_resp((), None).await;
        Ok(())
    }

    async fn handle_delete_file_from_editor<'a>(
        &mut self,
        next: &Next<'a>,
        params: DeleteFileParams,
    ) -> anyhow::Result<()> {
        let DeleteFileParams { file: editor_file } = params;
        let host_id = editor_file.host_id().clone();
        let file: FileDesc = editor_file.into();

        // Remote.
        if host_id != self.host_id {
            next.send_to_remote(&host_id, Msg::DeleteFile(file)).await;
            return Ok(());
        }

        // Local.
        match self.handle_delete_file_locally(&file).await {
            Ok(subscribers) => {
                // Send response to the editor.
                next.send_resp((), None).await;

                // Send FileDeleted notification to all subscribers.
                for subscriber_id in subscribers {
                    if subscriber_id != self.host_id {
                        send_to_remote(
                            next.webchannel,
                            &subscriber_id,
                            None,
                            Msg::FileDeleted(file.clone()),
                        )
                        .await;
                    }
                }
            }
            Err(err) => {
                let err = lsp_server::ResponseError {
                    code: ErrorCode::IoError as i32,
                    message: err.to_string(),
                    data: None,
                };
                next.send_resp((), Some(err)).await;
            }
        }
        Ok(())
    }

    async fn handle_delete_file_from_remote<'a>(
        &mut self,
        next: Next<'a>,
        file_desc: FileDesc,
        requester: ServerId,
    ) -> anyhow::Result<()> {
        // Check delete permission
        if !self.config.delete_allowed(&requester) {
            next.send_to_remote(
                &requester,
                Msg::PermissionDenied("Permission denied for deleting file".to_string()),
            )
            .await;
            return Ok(());
        }

        match self.handle_delete_file_locally(&file_desc).await {
            Ok(subscribers) => {
                let mut requester_notified = false;

                // Send FileDeleted message to all subscribers.
                for subscriber_id in &subscribers {
                    let msg = Msg::FileDeleted(file_desc.clone());
                    if subscriber_id == &requester {
                        next.send_to_remote(subscriber_id, msg).await;
                        requester_notified = true;
                    } else if subscriber_id != &self.host_id {
                        next.send_to_remote_no_req_id(subscriber_id, msg).await;
                    }
                }

                // If the requester is not a subscriber, still send them a response.
                let msg = Msg::FileDeleted(file_desc.clone());
                if !requester_notified {
                    next.send_to_remote(&requester, msg).await;
                }
            }
            Err(err) => {
                next.send_to_remote(
                    &requester,
                    Msg::ErrorResp(ErrorCode::IoError, err.to_string()),
                )
                .await;
            }
        }
        Ok(())
    }

    async fn handle_delete_file_locally(
        &mut self,
        file: &FileDesc,
    ) -> anyhow::Result<Vec<ServerId>> {
        let (docs_to_remove, path_to_delete, subscribers) =
            self.find_docs_and_path_to_delete(file)?;

        // Remove the docs, saving their content first.
        for doc_id in docs_to_remove {
            if let Some(mut doc) = self.docs.remove(&doc_id) {
                // Save and close the file if it has a file handle.
                if let Err(err) = doc.save_to_disk() {
                    // TODO: send this as an error to the original requester.
                    tracing::warn!("Failed to save doc {} before deletion: {}", doc_id, err);
                }
                tracing::info!("Removed doc {} due to file deletion", doc_id);
            }
        }

        // Delete the file or directory from disk if there's a path to delete.
        if let Some(path) = path_to_delete {
            if path.is_dir() {
                std::fs::remove_dir_all(&path)
                    .with_context(|| format!("Failed to delete directory: {}", path.display()))?;
                tracing::info!("Deleted directory: {}", path.display());
            } else if path.exists() {
                std::fs::remove_file(&path)
                    .with_context(|| format!("Failed to delete file: {}", path.display()))?;
                tracing::info!("Deleted file: {}", path.display());
            }
        }

        Ok(subscribers)
    }

    // We’re trying to delete `file`, find the files we need to delete
    // (if `file` is a directory), and find the subscribers we need to
    // notify of the deletion. If we’re deleting a whole directory,
    // hosts that subscribe to any of the to-be-deleted file in the
    // directory are included.
    fn find_docs_and_path_to_delete(
        &self,
        file: &FileDesc,
    ) -> anyhow::Result<(Vec<DocId>, Option<PathBuf>, Vec<ServerId>)> {
        let mut subscriber_set = HashSet::new();

        match file {
            FileDesc::Project { id } => {
                let project_data = self
                    .projects
                    .get(id)
                    .ok_or_else(|| anyhow!("Project {} not found", id))?;
                let project_root = std::path::PathBuf::from(&project_data.root);

                let mut docs_to_remove = Vec::new();
                for (doc_id, doc) in &self.docs {
                    if let PathId::Path(ref abs_filename) = doc.abs_filename {
                        if abs_filename.starts_with(&project_root) {
                            docs_to_remove.push(*doc_id);
                            // Collect subscribers into the set.
                            for subscriber_id in doc.subscribers.keys() {
                                subscriber_set.insert(subscriber_id.clone());
                            }
                        }
                    }
                }

                let subscribers: Vec<ServerId> = subscriber_set.into_iter().collect();
                Ok((docs_to_remove, Some(project_root), subscribers))
            }
            FileDesc::ProjectFile { project, .. }
                if project == RESERVED_BUFFERS_PROJECT || project == RESERVED_FILES_PROJECT =>
            {
                let path_id = self.path_id_from_file_desc(&file)?;
                for (doc_id, doc) in &self.docs {
                    if doc.matches(&path_id) {
                        return Ok((
                            vec![*doc_id],
                            match doc.abs_filename {
                                PathId::Path(ref path) => Some(path.clone()),
                                _ => None,
                            },
                            doc.subscribers.keys().cloned().collect(),
                        ));
                    }
                }
                return Ok((Vec::new(), None, vec![]));
            }
            FileDesc::ProjectFile {
                project,
                file: file_path,
            } => {
                let project_data = self
                    .projects
                    .get(project)
                    .ok_or_else(|| anyhow!("Project {} not found", project))?;
                let full_path = std::path::PathBuf::from(&project_data.root).join(file_path);

                let mut docs_to_remove = Vec::new();
                for (doc_id, doc) in &self.docs {
                    if let PathId::Path(ref abs_filename) = doc.abs_filename {
                        // Check if doc is under the path being deleted.
                        if abs_filename.starts_with(&full_path) || abs_filename == &full_path {
                            docs_to_remove.push(*doc_id);
                            // Collect subscribers into the set.
                            for subscriber_id in doc.subscribers.keys() {
                                subscriber_set.insert(subscriber_id.clone());
                            }
                        }
                    }
                }

                let subscribers: Vec<ServerId> = subscriber_set.into_iter().collect();
                Ok((docs_to_remove, Some(full_path), subscribers))
            }
        }
    }

    async fn broadcast_info<'a>(
        &mut self,
        next: &Next<'a>,
        info: Info,
        exclude_host: Option<&ServerId>,
    ) -> anyhow::Result<()> {
        let doc_id = info.doc_id;
        let doc = self.docs.get(&doc_id);
        if doc.is_none() {
            return Err(anyhow!("Doc {} not found", doc_id));
        }
        let doc = doc.unwrap();

        // Send InfoFromServer to all subscribers except the excluded host.
        for (subscriber_host_id, _) in &doc.subscribers {
            if Some(subscriber_host_id) != exclude_host {
                next.send_to_remote(subscriber_host_id, Msg::InfoFromServer(info.clone()))
                    .await;
            }
        }
        Ok(())
    }

    async fn save_file_to_disk(&mut self, doc_id: &DocId) -> anyhow::Result<()> {
        let doc = self.docs.get_mut(doc_id);
        if doc.is_none() {
            return Err(anyhow!("Doc {} not found", doc_id));
        }
        let doc = doc.unwrap();
        let file = &doc.disk_file;
        if file.is_none() {
            // TODO: Should we return an error to remote?
            tracing::warn!("Remote requeste to save a buffer to disk");
            return Ok(());
        }
        doc.save_to_disk()?;
        Ok(())
    }

    fn handle_update_config_from_editor(
        &mut self,
        params: UpdateConfigParams,
    ) -> anyhow::Result<()> {
        // Get current config
        let mut config = self.config.config();

        // Update accept_mode if provided
        if let Some(mode) = params.accept_mode {
            config.accept_mode = mode;
            // Update the shared accept_mode
            *self.accept_mode.lock().unwrap() = mode;
        }

        // Add trusted hosts if provided
        if let Some(hosts_to_add) = params.add_trusted_hosts {
            for (host_id, cert_hash) in hosts_to_add {
                config
                    .trusted_hosts
                    .insert(host_id.clone(), cert_hash.clone());
                // Update the shared trusted_hosts
                self.trusted_hosts
                    .lock()
                    .unwrap()
                    .insert(host_id, cert_hash);
            }
        }

        // Remove trusted hosts if provided
        if let Some(hosts_to_remove) = params.remove_trusted_hosts {
            for host_id in hosts_to_remove {
                config.trusted_hosts.remove(&host_id);
                // Update the shared trusted_hosts
                self.trusted_hosts.lock().unwrap().remove(&host_id);
            }
        }

        // Save the updated config to disk
        self.config.replace_and_save(config)?;

        Ok(())
    }

    fn remote_connected(&self, host_id: &ServerId) -> bool {
        dbg!(&self.active_remotes);
        if let Some(remote) = self.active_remotes.get(host_id) {
            remote.state == ConnectionState::Connected
        } else {
            false
        }
    }

    /// If connection to `host_id` isn’t active, send ConnectionBroke
    /// response and return false. Always return true of `host_id` is
    /// equal to our own host id.
    async fn check_connection<'a>(&self, next: &Next<'a>, host_id: &ServerId) -> bool {
        if *host_id == self.host_id {
            return true;
        }

        let connected = self.remote_connected(host_id);

        if !connected {
            next.send_resp(
                (),
                Some(lsp_server::ResponseError {
                    code: ErrorCode::NotConnected as i32,
                    message: format!("Not connected to remote {}", host_id),
                    data: None,
                }),
            )
            .await;
        }

        connected
    }
}

// *** Helper functions

struct Next<'a> {
    editor_tx: &'a mpsc::Sender<lsp_server::Message>,
    req_id: Option<lsp_server::RequestId>,
    webchannel: &'a WebChannel,
}

impl<'a> Next<'a> {
    fn new(
        editor_tx: &'a mpsc::Sender<lsp_server::Message>,
        req_id: Option<lsp_server::RequestId>,
        webchannel: &'a WebChannel,
    ) -> Self {
        Next {
            editor_tx,
            req_id,
            webchannel,
        }
    }

    async fn send_notif<T: Display>(&self, method: T, params: impl Serialize) {
        send_notification(self.editor_tx, method, params).await
    }

    async fn send_resp(&self, result: impl Serialize, error: Option<lsp_server::ResponseError>) {
        send_response(self.editor_tx, self.req_id.clone().unwrap(), result, error).await
    }

    async fn send_to_remote(&self, host_id: &ServerId, msg: Msg) {
        send_to_remote(self.webchannel, host_id, self.req_id.clone(), msg).await
    }

    async fn send_to_remote_no_req_id(&self, host_id: &ServerId, msg: Msg) {
        send_to_remote(self.webchannel, host_id, None, msg).await
    }
}

pub async fn send_notification<T: Display>(
    editor_tx: &mpsc::Sender<lsp_server::Message>,
    method: T,
    params: impl Serialize,
) {
    let notif = lsp_server::Notification {
        method: method.to_string(),
        params: serde_json::to_value(params).unwrap(),
    };
    tracing::info!("Send: {}", notification_to_string(&notif));
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
    tracing::info!("Send: {}", response_to_string(&response));
    if let Err(err) = editor_tx
        .send(lsp_server::Message::Response(response))
        .await
    {
        tracing::error!("Failed to send response to editor: {}", err);
    }
}

/// Send a message to a remote host via WebChannel with error logging.
async fn send_to_remote(
    webchannel: &WebChannel,
    host_id: &ServerId,
    req_id: Option<lsp_server::RequestId>,
    msg: Msg,
) {
    tracing::info!("Send to {}: {}", host_id, &msg);
    // We don’t let caller handle disconnect errors. Because
    // disconnect error is caught by the receiving end of each
    // connection, and it’ll send a ConnectionBroke message.
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
#[allow(dead_code)]
fn unwrap_req_id(req_id: &lsp_server::RequestId) -> anyhow::Result<i32> {
    req_id
        .to_string()
        .parse::<i32>()
        .map_err(|_| anyhow!("Can’t parse request id: {:?}", &req_id).into())
}

/// Helper function to expand project paths to absolute paths
fn expand_project_paths(projects: &mut Vec<ConfigProject>) -> anyhow::Result<()> {
    for project in projects {
        let expanded_path = expanduser::expanduser(&project.path)
            .map_err(|err| anyhow::anyhow!("Failed to expand path {}: {}", project.path, err))?;
        let expanded_str = expanded_path.to_string_lossy().to_string();

        // Check if the expanded path is absolute
        if !std::path::Path::new(&expanded_str).is_absolute() {
            return Err(anyhow::anyhow!(
                "Project path '{}' is not absolute. All project paths must be absolute.",
                expanded_str
            ));
        }

        project.path = expanded_str;
    }
    Ok(())
}

// *** Printing messages

/// Convert lsp_server::Request to a human-readable string
pub fn request_to_string(req: &lsp_server::Request) -> String {
    format!(
        "Req {} {} {}",
        &req.id,
        req.method,
        serde_json::to_string(&req.params).unwrap_or_else(|_| "?".to_string())
    )
}

/// Convert lsp_server::Response to a human-readable string
pub fn response_to_string(resp: &lsp_server::Response) -> String {
    let result_str = if let Some(ref result) = resp.result {
        serde_json::to_string(result).unwrap_or_else(|_| "?".to_string())
    } else {
        "null".to_string()
    };

    let error_str = if let Some(ref error) = resp.error {
        format!(" Error({}: {})", error.code, error.message)
    } else {
        "".to_string()
    };

    format!("Resp {} {}{}", &resp.id, result_str, error_str)
}

/// Convert lsp_server::Notification to a human-readable string
pub fn notification_to_string(notif: &lsp_server::Notification) -> String {
    format!(
        "Notif {} {}",
        notif.method,
        serde_json::to_string(&notif.params).unwrap_or_else(|_| "?".to_string())
    )
}

/// Convert lsp_server::Message to a human-readable string
pub fn message_to_string(msg: &lsp_server::Message) -> String {
    match msg {
        lsp_server::Message::Request(req) => request_to_string(req),
        lsp_server::Message::Response(resp) => response_to_string(resp),
        lsp_server::Message::Notification(notif) => notification_to_string(notif),
    }
}

// *** Tests

#[cfg(any(test, feature = "test-runner"))]
pub mod tests;

#[cfg(any(test, feature = "test-runner"))]
pub mod transcript_tests;
