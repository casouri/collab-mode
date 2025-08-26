use crate::config_man::{AcceptMode, ConfigManager};
use crate::engine::{ClientEngine, ServerEngine};
use crate::message::{self, *};
use crate::signaling::SignalingError;
use crate::types::*;
use crate::webchannel::{self, WebChannel};
use anyhow::{anyhow, Context};
use gapbuf::GapBuffer;
use lsp_server::ResponseError;
use serde::Serialize;
use std::collections::HashMap;
use std::fmt::Display;
use std::fs::File;
use std::io::{Read, Write};
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
    /// The document buffer.
    buffer: GapBuffer<char>,
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
        abs_filename: Option<PathBuf>,
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
            next_req_id: AtomicI32::new(1),
            next_doc_id: AtomicU32::new(1),
            docs: HashMap::new(),
            remote_docs: HashMap::new(),
            projects: HashMap::new(),
            users: HashMap::new(),
            active_remotes: Vec::new(),
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
        for project in self.config.config().projects {
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

    #[tracing::instrument(skip_all, fields(my_id = self.host_id))]
    async fn handle_editor_message(
        &mut self,
        msg: lsp_server::Message,
        editor_tx: &mpsc::Sender<lsp_server::Message>,
        webchannel: &WebChannel,
    ) -> anyhow::Result<()> {
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
            "ListFiles" => {
                let params: message::ListFilesParams = serde_json::from_value(req.params)?;
                // Either sends a request to remote or sends a
                // response to editor.
                self.list_files_from_editor(next, params.host_id, params.dir)
                    .await?;
                Ok(())
            }
            "OpenFile" => {
                let params: OpenFileParams = serde_json::from_value(req.params)?;
                self.open_file_from_editor(
                    next,
                    params.host_id,
                    params.file_desc,
                    req.id,
                    params.mode,
                )
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
            "MoveFile" => {
                let params: MoveFileParams = serde_json::from_value(req.params)?;
                self.handle_move_file_from_editor(next, params).await?;
                Ok(())
            }
            "SaveFile" => {
                let params: SaveFileParams = serde_json::from_value(req.params)?;
                self.handle_save_file_from_editor(next, params).await?;
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
                self.connect(
                    next,
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
                    self.projects.insert(project.name.clone(), project);
                }
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
        let next = Next::new(editor_tx, msg.req_id.clone(), webchannel);
        match msg.body {
            Msg::IceProgress(progress) => {
                next.send_notif(
                    NotificationCode::ConnectionProgress,
                    HostAndMessageNote {
                        host_id: msg.host,
                        message: progress,
                    },
                )
                .await;
                Ok(())
            }
            Msg::ConnectionBroke(host_id) => {
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
            Msg::Hey(message) => {
                next.send_notif(
                    NotificationCode::Hey,
                    HostAndMessageNote {
                        host_id: msg.host,
                        message,
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
            Msg::OpFromServer(ops) => {
                self.handle_op_from_server(&next, ops, msg.host.clone())
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
                    let msg = MoveFileResp {
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

                let resp = SaveFileResp {
                    host_id: msg.host,
                    doc_id,
                };
                next.send_resp(resp, None).await;
                Ok(())
            }
            _ => {
                panic!("Unhandled message type from {}: {:?}", msg.host, msg.body);
            }
        }
    }

    // **** Handler functions

    async fn connect<'a>(
        &mut self,
        next: &Next<'a>,
        host_id: ServerId,
        signaling_addr: String,
        transpot_type: webchannel::TransportType,
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
        let key_cert = self.key_cert.clone();
        let webchannel_1 = next.webchannel.clone();
        let editor_tx_1 = next.editor_tx.clone();
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
                    HostAndMessageNote {
                        host_id,
                        message: err.to_string(),
                    },
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
                            id: project.name.clone(),
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
        mode: OpenMode,
    ) -> anyhow::Result<(String, File)> {
        // Look up project
        let project = self
            .projects
            .get(project_id)
            .ok_or_else(|| anyhow!("Project {} not found", project_id))?;

        // Construct full path
        let full_path = std::path::PathBuf::from(&project.root).join(rel_path);
        let mut open_options = std::fs::OpenOptions::new();
        open_options.read(true).write(true);
        if matches!(mode, OpenMode::Create) {
            open_options.create(true);
        }

        let mut file = open_options
            .open(&full_path)
            .with_context(|| format!("Failed to create/open file: {}", full_path.display()))?;

        let mut content = String::new();
        file.read_to_string(&mut content)
            .with_context(|| format!("Failed to read file: {}", full_path.display()))?;

        Ok((content, file))
    }

    async fn open_file_from_editor<'a>(
        &mut self,
        next: &Next<'a>,
        host_id: ServerId,
        file_desc: FileDesc,
        req_id: lsp_server::RequestId,
        mode: OpenMode,
    ) -> anyhow::Result<()> {
        if self.host_id == host_id {
            // Handle local file opening
            match &file_desc {
                FileDesc::ProjectFile { project, file } => {
                    // Look up project by name
                    let project_data = self
                        .projects
                        .get(project)
                        .ok_or_else(|| anyhow!("Project {} not found", project))?;

                    // Check if already open
                    let full_path = std::path::PathBuf::from(&project_data.root).join(&file);
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
                                next.send_resp(resp, None).await;
                                return Ok(());
                            }
                        }
                    }

                    // Read from disk
                    let res = self.open_file_from_disk(project, file, mode).await;
                    if let Err(err) = res {
                        let err = lsp_server::ResponseError {
                            code: ErrorCode::IoError as i32,
                            message: err.to_string(),
                            data: None,
                        };
                        next.send_resp((), Some(err)).await;
                        return Ok(());
                    }
                    let (content, disk_file) = res.unwrap();

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
                        Some(disk_file),
                    );

                    // Add ourselves as a subscriber since we're also a client
                    doc.subscribers.insert(self.host_id.clone(), 0);

                    self.docs.insert(doc_id, doc);

                    // Also create a RemoteDoc for ourselves (we act as both server and client)
                    let mut buffer = GapBuffer::new();
                    buffer.insert_many(0, content.chars());
                    let remote_doc = RemoteDoc {
                        name: filename.clone(),
                        file_desc: file_desc.clone(),
                        next_site_seq: 1,
                        meta: JsonMap::new(),
                        remote_op_buffer: Vec::new(),
                        // Global seq starts from 1. So use 0 for the
                        // base_seq, meaning we expect next global seq
                        // to be 1.
                        engine: ClientEngine::new(self.site_id, 0, content.chars().count() as u64),
                        buffer,
                    };
                    self.remote_docs
                        .insert((self.host_id.clone(), doc_id), remote_doc);

                    // Send response
                    let resp = OpenFileResp {
                        content,
                        site_id: self.site_id,
                        filename,
                        doc_id,
                    };
                    next.send_resp(resp, None).await;
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
                        next.send_resp(resp, None).await;
                    } else {
                        let err = lsp_server::ResponseError {
                            code: ErrorCode::IoError as i32,
                            message: format!("Doc {} not found", id),
                            data: None,
                        };
                        next.send_resp((), Some(err)).await;
                    }
                }
                FileDesc::Project { .. } => {
                    let err = lsp_server::ResponseError {
                        code: ErrorCode::BadRequest as i32,
                        message: "Cannot open a project directory".to_string(),
                        data: None,
                    };
                    next.send_resp((), Some(err)).await;
                }
            }
            return Ok(());
        }

        // Open remote file.

        // Check if we already have this file in remote_docs
        for ((doc_host_id, doc_id), remote_doc) in &self.remote_docs {
            if doc_host_id == &host_id && remote_doc.file_desc == file_desc {
                // Return the current buffer content instead of error
                let resp = OpenFileResp {
                    content: remote_doc.buffer.iter().collect(),
                    site_id: remote_doc.engine.site(),
                    filename: remote_doc.name.clone(),
                    doc_id: *doc_id,
                };
                next.send_resp(resp, None).await;
                return Ok(());
            }
        }

        let msg = Msg::RequestFile(file_desc, mode);
        next.send_to_remote(&host_id, msg).await;
        Ok(())
    }

    async fn handle_request_file<'a>(
        &mut self,
        next: &Next<'a>,
        file_desc: FileDesc,
        remote_host_id: ServerId,
        mode: OpenMode,
    ) -> anyhow::Result<()> {
        let site_id = self.site_id_of(&remote_host_id);

        match &file_desc {
            FileDesc::ProjectFile { project, file } => {
                // Look up project by name
                let project_data = self
                    .projects
                    .get(project)
                    .ok_or_else(|| anyhow!("Project {} not found", project))?;

                // Check if already open
                let full_path = std::path::PathBuf::from(&project_data.root).join(&file);

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
                            next.send_to_remote(&remote_host_id, Msg::Snapshot(snapshot))
                                .await;
                            return Ok(());
                        }
                    }
                }

                // Not open, read from disk
                let res = self.open_file_from_disk(project, file, mode).await;
                if let Err(err) = res {
                    next.send_to_remote(
                        &remote_host_id,
                        Msg::ErrorResp(ErrorCode::IoError, format!("File not found: {}", err)),
                    )
                    .await;
                    return Ok(());
                }
                let (content, disk_file) = res.unwrap();

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
                    Some(full_path),
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
                    next.send_to_remote(&remote_host_id, Msg::Snapshot(snapshot))
                        .await;
                } else {
                    next.send_to_remote(
                        &remote_host_id,
                        Msg::ErrorResp(ErrorCode::IoError, format!("Doc {} not found", id)),
                    )
                    .await;
                }
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

    async fn handle_op_from_client<'a>(
        &mut self,
        next: &Next<'a>,
        context_ops: ContextOps,
        remote_host_id: ServerId,
    ) -> anyhow::Result<()> {
        let doc_id = context_ops.doc();

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
            next.send_to_remote(subscriber_host_id, Msg::OpFromServer(processed_ops.clone()))
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
            next.send_to_remote(&remote_host_id, Msg::OpFromServer(ops))
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
                        host_id: remote_host_id,
                        message: "Received remote op without global seq, terminating doc"
                            .to_string(),
                    },
                )
                .await;
                self.remote_docs.remove(&key);
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
                    "Received remote op with larger gseq ({}) than expected ({}), ignoring",
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
            next.send_notif(
                NotificationCode::RemoteOpsArrived,
                RemoteOpsArrivedNote {
                    host_id: remote_host_id,
                    doc_id,
                },
            )
            .await;
        }

        Ok(())
    }

    async fn handle_send_ops_from_editor<'a>(
        &mut self,
        next: &Next<'a>,
        params: SendOpsParams,
    ) -> anyhow::Result<()> {
        let SendOpsParams {
            ops,
            doc_id,
            host_id,
        } = params;
        // Look in remote_docs with (host_id, doc_id) key
        let remote_doc = self.remote_docs.get_mut(&(host_id.clone(), doc_id));
        if remote_doc.is_none() {
            let err = lsp_server::ResponseError {
                code: ErrorCode::IoError as i32,
                message: format!("Doc {} from host {} not found", doc_id, host_id),
                data: None,
            };
            next.send_resp((), Some(err)).await;
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

            let instructions = remote_doc
                .engine
                .process_local_op(editor_op, doc_id, site_seq);

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
                    self.remote_docs.remove(&(host_id, doc_id));
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
        if let Some(context_ops) = remote_doc.engine.maybe_package_local_ops() {
            // Send to the owner (could be ourselves or a remote)
            next.send_to_remote(&host_id, Msg::OpFromClient(context_ops))
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
            None,
            &params.content,
            ServerEngine::new(params.content.len() as u64),
            None,
        );

        // Add ourselves as a subscriber since we're also a client
        doc.subscribers.insert(self.host_id.clone(), 0);

        // Store the doc in our owned docs
        self.docs.insert(doc_id, doc);

        // Also create a RemoteDoc for ourselves (we act as both server and client)
        let mut buffer = GapBuffer::new();
        buffer.insert_many(0, params.content.chars());
        let remote_doc = RemoteDoc {
            name: params.filename.clone(),
            file_desc: FileDesc::File { id: doc_id },
            next_site_seq: 1,
            meta: params.meta,
            remote_op_buffer: Vec::new(),
            engine: ClientEngine::new(self.site_id, 0, params.content.chars().count() as u64),
            buffer,
        };
        self.remote_docs
            .insert((self.host_id.clone(), doc_id), remote_doc);

        // Return response with doc_id and site_id
        let resp = ShareFileResp {
            doc_id,
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

        // Construct full paths
        let old_full_path = std::path::PathBuf::from(&project.root).join(old_path);
        let new_full_path = std::path::PathBuf::from(&project.root).join(new_path);

        // Create parent directory if it doesn't exist
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

        // Update doc's abs_filename if the file is currently open and
        // collect subscribers.
        for doc in self.docs.values_mut() {
            if let Some(ref mut abs_filename) = doc.abs_filename {
                if abs_filename == &old_full_path {
                    *abs_filename = new_full_path.clone();
                    let subscribers: Vec<ServerId> = doc.subscribers.keys().cloned().collect();
                    return Ok(subscribers);
                }
            }
        }

        Ok(Vec::new())
    }

    async fn handle_save_file_from_editor<'a>(
        &mut self,
        next: &Next<'a>,
        params: SaveFileParams,
    ) -> anyhow::Result<()> {
        let SaveFileParams { host_id, doc_id } = params.clone();
        if host_id != self.host_id {
            // Handle remotely.
            next.send_to_remote(&host_id, Msg::SaveFile(doc_id)).await;
            return Ok(());
        }

        // Handle locally.
        let res = self.save_file_to_disk(&doc_id).await;

        if let Err(err) = res {
            let err = lsp_server::ResponseError {
                code: ErrorCode::IoError as i32,
                message: err.to_string(),
                data: None,
            };
            next.send_resp((), Some(err)).await;
        } else {
            next.send_resp(params, None).await;
        }
        Ok(())
    }

    async fn save_file_to_disk(&mut self, doc_id: &DocId) -> anyhow::Result<()> {
        let doc = self.docs.get(doc_id);
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
        let file = file.as_ref().unwrap();
        let mut writer = std::io::BufWriter::new(file);
        let content: String = doc.buffer.iter().collect();
        writer
            .write_all(content.as_bytes())
            .with_context(|| anyhow!("Error writing to {:?}", doc.abs_filename))
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

#[cfg(any(test, feature = "test-runner"))]
pub mod tests;

#[cfg(any(test, feature = "test-runner"))]
pub mod transcript_tests;
