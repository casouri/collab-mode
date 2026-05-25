//! Main body of the collab server.
//!
//! Uses [`webchannel`] for communicating with remote servers, and
//! [`crate::signaling::client`] for communicating with the signaling server.
//!
//! The main entry point is [`Server::run`], which runs in a loop and calls:
//! - [`Server::handle_editor_message`] for messages from the editor
//! - [`Server::handle_remote_message`] for messages from remote peers
//! - [`Server::handle_signaling_msg`] for messages from the signaling server
//!
//! Each handler function dispatches to specific message handlers. The server
//! mostly runs in one synchronous command loop. Long async processes (like
//! establishing connections) are split into several messages and their
//! corresponding handlers.
//!
//! Everything in the main loop is sync and SHOULD NOT block. I tried
//! a more async setup where each doc has its own loop and messages
//! are dispatched to each doc, etc. That’s too messy and hard to
//! reason about what’s going on.
//!
//! The reconnect tick handler handles reconnect and timeout for
//! connections. But there’s no timeout handling for editor requests,
//! editor will handle timeout on their end anyway.

use crate::config_man::{ConfigManager, ConfigProject};
use crate::engine::{ClientEngine, InternalDoc, ServerEngine};
use crate::error::CollabError;
use crate::message::{self, *};
use crate::signaling::SignalingMsg;
use crate::types::*;
use crate::webchannel::{self, WebChannel, WebChannelError};
use anyhow::{anyhow, Context};
use fmt_derive;
use futures::future::join_all;
use gapbuf::GapBuffer;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicU32, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;

const SHUTDOWN_TIMEOUT: u64 = 10;

// This is the only condition that needs special handling.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("Permission denied: {0}")]
    PermissionDenied(String),
}

/// Closure for the webchannel used by `Server`. [`Server::run`]
/// passes tokio message channels (`msg_tx`, `self_tx`) and the
/// closure returns a constructed [`WebChannel`].
pub type WebChannelFactory = Box<
    dyn FnOnce(mpsc::Sender<webchannel::Message>, mpsc::Sender<webchannel::Message>) -> WebChannel
        + Send,
>;

/// Closure for the signaling channel used by the `Server`.
/// [`Server::run`] passes `signaling_msg_tx` and the closure returns
/// a constructed [`signaling::client::SignalingChannel`].
pub type SignalingChannelFactory = Box<
    dyn FnOnce(
            mpsc::Sender<crate::signaling::SignalingMessage>,
        ) -> crate::signaling::client::SignalingChannel
        + Send,
>;

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

/// Connection state for remote host.
#[derive(PartialEq, Eq, Debug, Copy, Clone, Serialize, Deserialize)]
pub enum ConnectionState {
    /// Connected and well.
    Connected,
    /// Trying to (re)connect. Sent Connect message to remote.
    /// (unix_epoch_seconds, is_new_connection). is_new_connection is
    /// true if this is a fresh connect, false if reconnecting.
    ConnectingStage1(u64, bool),
    /// Sent Connect message remote and received Connect message from remote.
    /// (unix_epoch_seconds, is_new_connection).
    ConnectingStage2(u64, bool),
    /// Waiting for signaling server to connect before we can attempt
    /// connection to this remote.
    Pending,
    /// Connection was established but then broke. We try to reconnect
    /// in this case.
    Disconnected,
    /// Connection was never established. We don't try to reconnect in
    /// this case.
    FailedToConnect,
}

/// Get current unix epoch in seconds.
fn unix_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

// *** WorldState types
//
// Declarative connection management. `Server` carries two
// `WorldState` values: `ideal_world` (the editor's declared intent)
// and `current_world` (the current state). `fix_world` reconciles the
// second toward the first.

/// Status of a connection to a signaling server.
#[derive(Debug, Clone, Default)]
pub enum SignalingStatus {
    /// Declared but not yet attempted. Equivalent to “no entry”.
    #[default]
    Pending,
    /// `bind()` in flight, awaiting `Bound`.
    Binding { since: Instant },
    /// Active.
    Bound,
    /// Transient failure. `fix_world` retries when now ≥ next_retry_at.
    Disconnected {
        next_retry_at: Instant,
        stride_secs: u64,
    },
    /// Hard failure. `fix_world` ignores; explicit editor request resets.
    Failed { reason: String },
}

/// State of a signaling server entry in `WorldState`. `trusted` is
/// the list the signaling server has confirmed via `Bound`/`Trusted`
/// (only used by current world).
#[derive(Debug, Clone, Default)]
pub struct SignalingState {
    pub status: SignalingStatus,
    pub trusted: HashSet<ServerId>,
    pub trust_sent_at: Option<u64>,
    /// Stride used for the next reconnect attempt, in seconds.
    pub retry_stride_secs: u64,
}

/// Status of a connection to a remote peer.
#[derive(Debug, Clone)]
pub enum RemoteStatus {
    /// Declared but not yet attempted. Equivalent to “no entry”.
    Pending,
    /// We sent (or are about to send) `SignalingMsg::Connect`.
    ConnectingStage1 { since: Instant },
    /// SDP/ICE in progress, awaiting `Msg::Hey`.
    ConnectingStage2 { since: Instant },
    /// `Msg::Hey` received.
    Connected,
    /// Was connected at least once, then broke. `fix_world` retries.
    Disconnected {
        next_retry_at: Instant,
        stride_secs: u64,
    },
    /// Hard failure or first-attempt failure. No auto-retry.
    Failed { reason: String },
}

/// State of a remote peer entry in `WorldState`.
#[derive(Debug, Clone)]
pub struct RemoteState {
    pub transport: webchannel::TransportConfig,
    pub status: RemoteStatus,
    /// True once we have reached `Connected` for this peer at least once.
    /// Used on timeout to decide between `Failed` and `Disconnected`.
    pub ever_connected: bool,
    /// Stride used for the next reconnect attempt, in seconds.
    pub retry_stride_secs: u64,
}

/// Declarative connection state. In `Server.ideal_world` this records
/// what the editor has declared; the `status` fields are written as
/// `Connected` / `Bound` and never read. In `Server.current_world` the
/// `status` fields track the live state machine.
#[derive(Debug, Clone, Default)]
pub struct WorldState {
    pub signaling: HashMap<String, SignalingState>,
    pub remotes: HashMap<ServerId, RemoteState>,
    /// Global trusted-host set. Only used by `ideal_world`
    pub trusted: HashSet<ServerId>,
}

/// Initial reconnect stride and the cap, both in seconds. Stride
/// doubles on each consecutive failure: 1, 2, 4, …, 60.
const INITIAL_RETRY_STRIDE_SECS: u64 = 1;
const MAX_RETRY_STRIDE_SECS: u64 = 60;

fn next_stride(cur: u64) -> u64 {
    cur.saturating_mul(2).min(MAX_RETRY_STRIDE_SECS)
}

impl WorldState {
    pub fn set_remote_connecting1(
        &mut self,
        peer: &ServerId,
        transport: webchannel::TransportConfig,
    ) {
        let (ever, stride) = self
            .remotes
            .get(peer)
            .map(|r| (r.ever_connected, r.retry_stride_secs))
            .unwrap_or((false, INITIAL_RETRY_STRIDE_SECS));
        self.remotes.insert(
            peer.clone(),
            RemoteState {
                transport,
                status: RemoteStatus::ConnectingStage1 {
                    since: Instant::now(),
                },
                ever_connected: ever,
                retry_stride_secs: stride,
            },
        );
    }

    pub fn set_remote_connecting2(
        &mut self,
        peer: &ServerId,
        transport: webchannel::TransportConfig,
    ) {
        let (ever, stride) = self
            .remotes
            .get(peer)
            .map(|r| (r.ever_connected, r.retry_stride_secs))
            .unwrap_or((false, INITIAL_RETRY_STRIDE_SECS));
        self.remotes.insert(
            peer.clone(),
            RemoteState {
                transport,
                status: RemoteStatus::ConnectingStage2 {
                    since: Instant::now(),
                },
                ever_connected: ever,
                retry_stride_secs: stride,
            },
        );
    }

    pub fn set_remote_pending(&mut self, peer: &ServerId, transport: webchannel::TransportConfig) {
        let (ever, stride) = self
            .remotes
            .get(peer)
            .map(|r| (r.ever_connected, r.retry_stride_secs))
            .unwrap_or((false, INITIAL_RETRY_STRIDE_SECS));
        self.remotes.insert(
            peer.clone(),
            RemoteState {
                transport,
                status: RemoteStatus::Pending,
                ever_connected: ever,
                retry_stride_secs: stride,
            },
        );
    }

    pub fn mark_remote_connected(&mut self, peer: &ServerId) {
        if let Some(state) = self.remotes.get_mut(peer) {
            state.status = RemoteStatus::Connected;
            state.ever_connected = true;
            state.retry_stride_secs = INITIAL_RETRY_STRIDE_SECS;
        }
    }

    pub fn mark_remote_disconnected(&mut self, peer: &ServerId) {
        if let Some(state) = self.remotes.get_mut(peer) {
            let stride = state.retry_stride_secs;
            state.status = RemoteStatus::Disconnected {
                next_retry_at: Instant::now() + Duration::from_secs(stride),
                stride_secs: stride,
            };
            state.retry_stride_secs = next_stride(stride);
        }
    }

    pub fn mark_remote_failed(&mut self, peer: &ServerId, reason: String) {
        if let Some(state) = self.remotes.get_mut(peer) {
            state.status = RemoteStatus::Failed { reason };
        }
    }

    pub fn remove_remote(&mut self, peer: &ServerId) {
        self.remotes.remove(peer);
    }

    pub fn set_signaling_binding(&mut self, addr: &str) {
        // Preserve any existing trust/trust_sent_at on transition.
        self.signaling
            .entry(addr.to_string())
            .and_modify(|s| {
                s.status = SignalingStatus::Binding {
                    since: Instant::now(),
                }
            })
            .or_insert_with(|| SignalingState {
                status: SignalingStatus::Binding {
                    since: Instant::now(),
                },
                ..Default::default()
            });
    }

    pub fn mark_signaling_bound(&mut self, addr: &str) {
        self.signaling
            .entry(addr.to_string())
            .and_modify(|s| {
                s.status = SignalingStatus::Bound;
                s.retry_stride_secs = INITIAL_RETRY_STRIDE_SECS;
            })
            .or_insert_with(|| SignalingState {
                status: SignalingStatus::Bound,
                ..Default::default()
            });
    }

    pub fn mark_signaling_disconnected(&mut self, addr: &str) {
        if let Some(state) = self.signaling.get_mut(addr) {
            let stride = if state.retry_stride_secs == 0 {
                INITIAL_RETRY_STRIDE_SECS
            } else {
                state.retry_stride_secs
            };
            state.status = SignalingStatus::Disconnected {
                next_retry_at: Instant::now() + Duration::from_secs(stride),
                stride_secs: stride,
            };
            state.retry_stride_secs = next_stride(stride);
        }
    }

    pub fn mark_signaling_failed(&mut self, addr: &str, reason: String) {
        self.signaling
            .entry(addr.to_string())
            .and_modify(|s| {
                s.status = SignalingStatus::Failed {
                    reason: reason.clone(),
                }
            })
            .or_insert_with(|| SignalingState {
                status: SignalingStatus::Failed { reason },
                ..Default::default()
            });
    }

    pub fn remove_signaling(&mut self, addr: &str) {
        self.signaling.remove(addr);
    }
}

/// Stores relevant for the server.
pub struct Server {
    /// Host id of this server. Once set, this cannot change, because
    /// we use our own host_id in remote_docs, etc too. Host id should
    /// be in the form of `<name>::<cert hash>`.
    host_id: ServerId,
    /// Name part of the `host_id`.
    host_name: String,
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
    /// Maps host id to site id.
    site_id_map: HashMap<ServerId, SiteId>,
    /// Configuration manager for the server.
    config: ConfigManager,
    /// Key and certificate for the server.
    key_cert: Arc<KeyCert>,
    /// Declared world state: what the editor wants us to be doing.
    /// Only editor handlers write here. `fix_world` reads only.
    pub ideal_world: WorldState,
    /// Observed world state: connections in flight or live, with
    /// backoff bookkeeping. Message handlers and `fix_world` write here.
    pub current_world: WorldState,
    /// Cert hashes trusted only for the lifetime of this process,
    /// eg. the temp envoy cert generated during outgoing SSH connect.
    /// Not user-declared, so does not belong in `WorldState.trusted`.
    pub local_trusted_hosts: HashSet<ServerId>,
    /// Signaled when we need to run fix_world.
    fix_world_notify: Arc<Notify>,
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
            EditInstruction::Ins { edits } => {
                for edit in edits.into_iter().rev() {
                    let pos = edit.pos;
                    let str = edit.content;
                    let gapbuf_len = self.buffer.len();
                    tracing::debug!(
                        pos,
                        gapbuf_len,
                        text_len = str.chars().count(),
                        "LocalDoc applying Ins to gapbuf"
                    );
                    if pos as usize > gapbuf_len {
                        let msg = format!("Insertion pos > gapbuf_len, {pos} > {gapbuf_len}");
                        tracing::warn!("{}", msg);
                        return Err(anyhow!(CollabError::DocFatal(msg)));
                    }
                    self.buffer.insert_many(pos as usize, str.chars());
                }
            }
            EditInstruction::Del { edits } => {
                for edit in edits.into_iter().rev() {
                    let pos = edit.pos;
                    let str = edit.content;
                    self.buffer
                        .drain((pos as usize)..(pos as usize + str.chars().count()));
                }
            }
        }

        // Verify that engine's editor_len matches the actual buffer
        // length. FIXME: Remove this once we’re certain that the
        // implementation is bugless, since getting the internal doc
        // len is O(n).
        let engine_len = self.engine.editor_len();
        let buffer_len = self.buffer.len() as u64;
        if engine_len != buffer_len {
            return Err(CollabError::DocFatal(format!(
                "Engine editor_len {} doesn't match buffer len {}",
                engine_len, buffer_len
            ))
            .into());
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
    /// Apply an EditInstruction to the buffer without checking engine/buffer sync.
    /// Use `verify_buffer_engine_sync` after applying all instructions from a
    /// single editor operation.
    fn apply_edit_instruction_unchecked(
        &mut self,
        instruction: &EditInstruction,
    ) -> anyhow::Result<()> {
        match instruction {
            EditInstruction::Ins { edits } => {
                for edit in edits.iter().rev() {
                    let pos = edit.pos;
                    let str = &edit.content;
                    let gapbuf_len = self.buffer.len();
                    tracing::debug!(
                        pos,
                        gapbuf_len,
                        text_len = str.chars().count(),
                        "RemoteDoc applying Ins to gapbuf"
                    );
                    if pos as usize > gapbuf_len {
                        let msg = format!("Insertion pos > gapbuf_len, {pos} > {gapbuf_len}");
                        tracing::warn!("{}", msg);
                        return Err(anyhow!(CollabError::DocFatal(msg)));
                    }
                    self.buffer.insert_many(pos as usize, str.chars());
                }
            }
            EditInstruction::Del { edits } => {
                for edit in edits.iter().rev() {
                    let pos = edit.pos;
                    let str = &edit.content;
                    let end = pos as usize + str.chars().count();
                    self.buffer.drain((pos as usize)..end);
                }
            }
        }
        Ok(())
    }

    /// Verify that engine's editor_len matches the actual buffer length.
    fn verify_buffer_engine_sync(&self) -> anyhow::Result<()> {
        let engine_len = self.engine.editor_len();
        let buffer_len = self.buffer.len() as u64;
        if engine_len != buffer_len {
            return Err(CollabError::DocFatal(format!(
                "Engine editor_len {} doesn't match buffer len {}",
                engine_len, buffer_len
            ))
            .into());
        }
        Ok(())
    }

    /// Apply an EditInstruction to the buffer and verify sync.
    pub fn apply_edit_instruction(&mut self, instruction: &EditInstruction) -> anyhow::Result<()> {
        self.apply_edit_instruction_unchecked(instruction)?;
        self.verify_buffer_engine_sync()
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
        let name = id_name(&host_id)
            .ok_or(anyhow!(
                "host_id has no name; expects `<name>::<cert-hash>`"
            ))?
            .to_string();
        let key_cert = config.get_key_and_cert(&name)?;
        Self::new_with_keycert(host_id, name, config, Arc::new(key_cert))
    }

    /// Like [`Server::new`] but uses an externally-supplied
    /// [`ArcKeyCert`] instead of loading from disk. The envoy uses
    /// this to adopt the identity the remote host gave it in
    /// `Msg::EnvoyInit`.
    pub fn new_with_keycert(
        host_id: ServerId,
        host_name: String,
        config: ConfigManager,
        key_cert: ArcKeyCert,
    ) -> anyhow::Result<Self> {
        let current_config = config.config();
        let ideal_world = WorldState {
            trusted: current_config.trusted_hosts,
            ..Default::default()
        };

        let server = Server {
            host_id,
            host_name,
            site_id: 0,
            next_site_id: AtomicU32::new(1),
            next_doc_id: AtomicU32::new(1),
            docs: HashMap::new(),
            remote_docs: RemoteDocs::new(),
            projects: HashMap::new(),
            site_id_map: HashMap::new(),
            config,
            key_cert,
            ideal_world,
            current_world: WorldState::default(),
            local_trusted_hosts: HashSet::new(),
            fix_world_notify: Arc::new(Notify::new()),
        };
        Ok(server)
    }

    /// Bootstrap server state for envoy mode from a parsed
    /// `Msg::EnvoyInit` payload.
    ///
    /// - Registers the projects to share.
    /// - Trusts the host.
    /// - Pre-populates `active_remotes[host_id]` so the existing
    ///   `Hey` handler can flip it to `Connected` when the host's `Hey`
    ///   arrives.
    pub fn init_for_envoy(
        &mut self,
        host_id: ServerId,
        mut projects: Vec<ConfigProject>,
    ) -> anyhow::Result<()> {
        // Reject reserved project names, same as in
        // handle_declare_projects.
        for proj in &projects {
            if proj.name == RESERVED_BUFFERS_PROJECT || proj.name == RESERVED_FILES_PROJECT {
                return Err(anyhow!("Project name {} is reserved", proj.name));
            }
        }

        expand_project_paths(&mut projects)?;
        for proj in projects {
            self.projects.insert(
                proj.name.clone(),
                Project {
                    name: proj.name,
                    root: proj.path,
                    meta: serde_json::Map::new(),
                },
            );
        }

        self.ideal_world.trusted.insert(host_id.clone());

        // Trust the host that initiated us.
        self.config.add_permission(
            host_id.clone(),
            crate::config_man::Permission {
                write: true,
                create: true,
                delete: true,
            },
        );

        // Populate current_world.remotes in ConnectingStage2 so the
        // Hey handler can mark it Connected. Also declare it in
        // ideal_world so fix_world doesn't tear it down.
        let envoy_transport = webchannel::TransportConfig::SshStdio {
            ssh_host: "N/A".into(),
            command: Vec::new(),
            projects: Vec::new(),
        };
        self.ideal_world.remotes.insert(
            host_id.clone(),
            RemoteState {
                transport: envoy_transport.clone(),
                status: RemoteStatus::Connected,
                ever_connected: false,
                retry_stride_secs: INITIAL_RETRY_STRIDE_SECS,
            },
        );
        self.current_world
            .set_remote_connecting2(&host_id, envoy_transport);

        Ok(())
    }

    pub async fn run(
        &mut self,
        editor_tx: mpsc::Sender<lsp_server::Message>,
        mut editor_rx: mpsc::Receiver<lsp_server::Message>,
        webchannel_factory: WebChannelFactory,
        signaling_channel_factory: SignalingChannelFactory,
        shutdown: std::sync::Arc<tokio::sync::Notify>,
    ) -> anyhow::Result<()> {
        // webrtc-dtls uses the ring feature of rustls, so there's an
        // ambiguity of which provider to use. Here we just set the
        // default provider to ring explicitly.
        // REF: https://github.com/rustls/rustls/issues/1938
        let _ = rustls::crypto::ring::default_provider().install_default();

        let (mut dual_rx, msg_tx, self_tx) = webchannel::DualReceiver::new();
        let (signaling_msg_tx, mut signaling_msg_rx) =
            mpsc::channel::<crate::signaling::SignalingMessage>(16);

        let signaling_channel = signaling_channel_factory(signaling_msg_tx.clone());
        let webchannel = webchannel_factory(msg_tx.clone(), self_tx.clone());

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

        let mut fix_world_ticker = tokio::time::interval(Duration::from_secs(1));
        // Skip the immediate first tick; we want intervals, not edge-trigger on start.
        fix_world_ticker.tick().await;

        let fix_world_notify = self.fix_world_notify.clone();

        loop {
            tokio::select! {
                // Shutdown signal, can be editor disconnect, SIGINT,
                // or SIGTERM.
                _ = shutdown.notified() => break,
                // Handle messages from the editor.
                msg = editor_rx.recv() => {
                    let Some(msg) = msg else { break; };
                    let res = self.handle_editor_message(msg, &editor_tx, &webchannel, &msg_tx, &signaling_channel).await;
                    // Normally the handler shouldn't return an error.
                    // Most errors ar handled by sending error
                    // response to editor/remote.
                    if let Err(err) = res {
                        tracing::warn!("Failed to handle editor message: {}", err);
                    }
                },
                // Handle messages from remote peers and self.
                Some(web_msg) = dual_rx.recv() => {
                    let res = self.handle_remote_message(web_msg, &editor_tx, &webchannel).await;
                    // Normally the handler shouldn't return an error.
                    // Most errors ar handled by sending error
                    // response to editor/remote.
                    if let Err(err) = res {
                        tracing::warn!("Failed to handle remote message: {}", err);
                    }
                },
                // Handle messages from signaling clients.
                Some(signaling_message) = signaling_msg_rx.recv() => {
                    let next = Next::new(&editor_tx, None, &webchannel);
                    match signaling_message.msg {
                        Ok(signaling_msg) => {
                            let res = self.handle_signaling_msg(signaling_message.signaling_addr, signaling_msg, &next, &msg_tx, &signaling_channel).await;
                            if let Err(err) = res {
                                tracing::warn!("Failed to handle signaling message: {}", err);
                            }
                        }
                        Err(accept_stopped) => {
                            let signaling_addr = signaling_message.signaling_addr;
                            let reason = accept_stopped.0;

                            self.current_world.mark_signaling_disconnected(&signaling_addr);

                            next.send_notif(
                                NotificationCode::AcceptStopped,
                                serde_json::json!({
                                    "signalingAddr": signaling_addr,
                                    "reason": reason,
                                }),
                            )
                            .await;
                            self.fix_world_next();
                        }
                    }
                },
                // Run fix_world to reconnect, etc.
                _ = fix_world_notify.notified() => {
                    self.fix_world(&editor_tx, &msg_tx, &webchannel, &signaling_channel).await;
                }
                // Trigger fix_world every 1s.
                _ = fix_world_ticker.tick() => {
                    self.fix_world(&editor_tx, &msg_tx, &webchannel, &signaling_channel).await;
                }
            }
        }

        // Try to shutdown gracefully: close connections grafully,
        // save all unsaved files.
        let total_timeout = Duration::from_secs(SHUTDOWN_TIMEOUT);
        if tokio::time::timeout(total_timeout, self.shutdown(webchannel))
            .await
            .is_err()
        {
            tracing::warn!("Timeout exceeded ({}s), force quit", SHUTDOWN_TIMEOUT);
        }
        Ok(())
    }

    /// Save owned docs to disk, send `Bye` to connected peers, then
    /// flush the webchannel. Each step is best-effort: errors are
    /// logged and never propagated, so a single failure does not
    /// prevent the rest of shutdown from running. Be sure to bound
    /// this with timeout.
    async fn shutdown(&mut self, webchannel: WebChannel) {
        tracing::info!("Shuting down");

        let mut handles: Vec<JoinHandle<()>> = Vec::new();

        // Send Bye to every peer.
        let handle = tokio::spawn(async move {
            if let Err(err) = webchannel.broadcast(None, Msg::Bye).await {
                tracing::warn!("Failed to send Bye to remote hosts: {}", err);
            }
            if let Err(err) = webchannel.shutdown().await {
                tracing::warn!("Error shutting down webchannel: {}", err);
            }
        });
        handles.push(handle);

        // Save all docs to disk in parallel.
        for (_id, mut doc) in self.docs.drain() {
            let handle = tokio::spawn(async move {
                let file_desc = doc.file_desc.clone();
                if let Err(err) = doc.save_to_disk() {
                    tracing::warn!("Failed to save doc {} during shutdown: {}", file_desc, err);
                }
            });
            handles.push(handle);
        }

        join_all(handles).await;
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

    /// Check whether `cert_hash` is trusted.
    pub fn is_cert_trusted(&self, cert_hash: &str) -> bool {
        self.ideal_world
            .trusted
            .iter()
            .any(|id| id_hash(id) == cert_hash)
            || self
                .local_trusted_hosts
                .iter()
                .any(|id| id_hash(id) == cert_hash)
    }

    /// Ask the main loop to run `fix_world` on its next iteration.
    fn fix_world_next(&self) {
        self.fix_world_notify.notify_one();
    }

    /// Reconcile `current_world` toward `ideal_world`. Idempotent.
    /// Runs every 1s and on `fix_world_notify` pokes.
    async fn fix_world(
        &mut self,
        editor_tx: &mpsc::Sender<lsp_server::Message>,
        msg_tx: &mpsc::Sender<webchannel::Message>,
        webchannel: &WebChannel,
        signaling_channel: &crate::signaling::client::SignalingChannel,
    ) {
        self.fix_world_signaling(editor_tx, signaling_channel).await;
        self.fix_world_remotes(editor_tx, msg_tx, webchannel, signaling_channel)
            .await;
        self.fix_world_timeouts(editor_tx, webchannel).await;
    }

    /// Remote pass: drive `current_world.remotes` toward `ideal_world.remotes`.
    async fn fix_world_remotes(
        &mut self,
        editor_tx: &mpsc::Sender<lsp_server::Message>,
        msg_tx: &mpsc::Sender<webchannel::Message>,
        webchannel: &WebChannel,
        signaling_channel: &crate::signaling::client::SignalingChannel,
    ) {
        let now = Instant::now();

        // 1. Close entries that are no longer in ideal world.
        let to_remove: Vec<ServerId> = self
            .current_world
            .remotes
            .keys()
            .filter(|peer| !self.ideal_world.remotes.contains_key(*peer))
            .cloned()
            .collect();
        for peer in &to_remove {
            webchannel.disconnect(peer);
            self.current_world.remove_remote(peer);
        }

        // 2. For each ideal entry, start a connection if missing,
        //    Pending, or Disconnected past its retry_at.
        let to_start: Vec<(ServerId, webchannel::TransportConfig)> = self
            .ideal_world
            .remotes
            .iter()
            .filter_map(|(peer, ideal)| {
                let go = match self.current_world.remotes.get(peer) {
                    None => true,
                    Some(cur) => match &cur.status {
                        RemoteStatus::Pending => true,
                        RemoteStatus::Disconnected { next_retry_at, .. } => now >= *next_retry_at,
                        _ => false,
                    },
                };
                if go {
                    Some((peer.clone(), ideal.transport.clone()))
                } else {
                    None
                }
            })
            .collect();

        let next = Next::new(editor_tx, None, webchannel);
        for (peer, transport) in to_start {
            // SCTP requires the corresponding signaling server to be Bound.
            if let webchannel::TransportConfig::SCTP { signaling_addr } = &transport {
                let ready = matches!(
                    self.current_world
                        .signaling
                        .get(signaling_addr)
                        .map(|s| &s.status),
                    Some(SignalingStatus::Bound)
                );
                if !ready {
                    // Add to the pending list, connection proces will
                    // be picked up once the signaling server is
                    // connected.
                    self.current_world
                        .set_remote_pending(&peer, transport.clone());
                    continue;
                }
            }
            self.try_connect_remote(&next, peer, transport, msg_tx, signaling_channel)
                .await;
        }
    }

    /// Timeout pass: Stage1 > 10s and Stage2 > 30s flip to Disconnected
    /// (if `ever_connected`) or Failed (if never connected).
    async fn fix_world_timeouts(
        &mut self,
        editor_tx: &mpsc::Sender<lsp_server::Message>,
        webchannel: &WebChannel,
    ) {
        let now = Instant::now();
        let timeouts: Vec<(ServerId, &'static str)> = self
            .current_world
            .remotes
            .iter()
            .filter_map(|(peer, state)| match state.status {
                RemoteStatus::ConnectingStage1 { since }
                    if now.duration_since(since) > Duration::from_secs(10) =>
                {
                    Some((peer.clone(), "stage1 timeout"))
                }
                RemoteStatus::ConnectingStage2 { since }
                    if now.duration_since(since) > Duration::from_secs(30) =>
                {
                    Some((peer.clone(), "stage2 timeout"))
                }
                _ => None,
            })
            .collect();
        for (peer, reason) in timeouts {
            let ever = self
                .current_world
                .remotes
                .get(&peer)
                .map(|r| r.ever_connected)
                .unwrap_or(false);
            if ever {
                self.current_world.mark_remote_disconnected(&peer);
            } else {
                self.current_world
                    .mark_remote_failed(&peer, reason.to_string());
            }
            webchannel.disconnect(&peer);
            send_notification(
                editor_tx,
                NotificationCode::FailedToConnect,
                ConnectionBrokeNote {
                    host_id: peer,
                    reason: reason.to_string(),
                },
            )
            .await;
        }
    }

    /// Signaling pass: Connect to signaling servers. Also handles
    /// bind timeouts.
    async fn fix_world_signaling(
        &mut self,
        editor_tx: &mpsc::Sender<lsp_server::Message>,
        signaling_channel: &crate::signaling::client::SignalingChannel,
    ) {
        let now = Instant::now();

        // 1. Timeout: Binding for more than 10s becomes Failed and the
        //    client is dropped.
        let timed_out: Vec<String> = self
            .current_world
            .signaling
            .iter()
            .filter_map(|(addr, state)| match state.status {
                SignalingStatus::Binding { since }
                    if now.duration_since(since) > Duration::from_secs(10) =>
                {
                    Some(addr.clone())
                }
                _ => None,
            })
            .collect();
        for addr in &timed_out {
            self.current_world
                .mark_signaling_failed(addr, "bind timeout".to_string());
            signaling_channel.remove(addr);
        }

        // 2. Tear down current entries that are no longer in ideal.
        let to_remove: Vec<String> = self
            .current_world
            .signaling
            .keys()
            .filter(|addr| !self.ideal_world.signaling.contains_key(*addr))
            .cloned()
            .collect();
        for addr in &to_remove {
            signaling_channel.remove(addr);
            self.current_world.remove_signaling(addr);
        }

        // 3. For each ideal entry, start binding when missing/pending,
        //    or when a Disconnected entry has elapsed its retry time.
        //    Collect addresses first to avoid holding a borrow across
        //    the async bind call.
        let mut to_bind: Vec<String> = Vec::new();
        for (addr, _) in self.ideal_world.signaling.iter() {
            match self.current_world.signaling.get(addr) {
                None => to_bind.push(addr.clone()),
                Some(state) => match &state.status {
                    SignalingStatus::Pending => to_bind.push(addr.clone()),
                    SignalingStatus::Disconnected { next_retry_at, .. }
                        if now >= *next_retry_at =>
                    {
                        to_bind.push(addr.clone());
                    }
                    _ => {}
                },
            }
        }

        for addr in to_bind {
            self.current_world.set_signaling_binding(&addr);
            send_notification(
                editor_tx,
                NotificationCode::ConnectionProgress,
                serde_json::json!({
                    "message": format!("Binding to signaling server {}", addr),
                }),
            )
            .await;
            let trusted_vec: Vec<ServerId> = self
                .ideal_world
                .signaling
                .get(&addr)
                .map(|s| s.trusted.iter().cloned().collect())
                .unwrap_or_default();
            let res = signaling_channel
                .bind(
                    addr.clone(),
                    self.host_id.clone(),
                    self.key_cert.clone(),
                    trusted_vec,
                )
                .await;
            if let Err(err) = res {
                tracing::warn!("Failed to bind to signaling server {}: {}", addr, err);
                self.current_world
                    .mark_signaling_failed(&addr, err.to_string());
                send_notification(
                    editor_tx,
                    NotificationCode::AcceptStopped,
                    AcceptingStoppedNote { addr },
                )
                .await;
            }
        }

        // 4. Sync trust lists for Bound entries whose confirmed
        //    `current` list doesn't match the global trust list in
        //    `self.ideal_world.trusted`.
        let now_sec = unix_epoch_secs();
        let ideal_trusted = &self.ideal_world.trusted;
        let to_sync: Vec<(String, Vec<ServerId>)> = self
            .current_world
            .signaling
            .iter()
            .filter_map(|(addr, cur)| {
                if !matches!(cur.status, SignalingStatus::Bound) {
                    return None;
                }
                if cur.trusted == *ideal_trusted {
                    return None;
                }
                if let Some(t) = cur.trust_sent_at {
                    if now_sec.saturating_sub(t) < 5 {
                        return None;
                    }
                }
                Some((addr.clone(), ideal_trusted.iter().cloned().collect()))
            })
            .collect();
        for (addr, trusted) in to_sync {
            if let Some(s) = self.current_world.signaling.get_mut(&addr) {
                s.trust_sent_at = Some(now_sec);
            }
            let msg = crate::signaling::SignalingMsg::Trust {
                sender: self.host_id.clone(),
                trusted,
            };
            if let Err(err) = signaling_channel.send(&addr, msg).await {
                tracing::warn!("Failed to send Trust for {}: {}", addr, err);
            }
        }
    }

    #[tracing::instrument(skip_all, fields(my_id = id_short(&self.host_id)))]
    async fn handle_editor_message(
        &mut self,
        msg: lsp_server::Message,
        editor_tx: &mpsc::Sender<lsp_server::Message>,
        webchannel: &WebChannel,
        msg_tx: &mpsc::Sender<webchannel::Message>,
        signaling_channel: &crate::signaling::client::SignalingChannel,
    ) -> anyhow::Result<()> {
        tracing::info!("From editor: {}", message_to_string(&msg));
        match msg {
            lsp_server::Message::Request(req) => {
                let req_id = req.id.clone();
                let next = Next::new(editor_tx, Some(req_id), &webchannel);
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
                let next = Next::new(editor_tx, None, &webchannel);
                if let Err(err) = self
                    .handle_editor_notification(&next, notif, msg_tx, signaling_channel)
                    .await
                {
                    tracing::warn!("Failed to handle editor notification: {}", err);
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
                self.handle_connection_state_from_editor(next).await;
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
            "MakeDirectory" => {
                let params: MakeDirectoryParams = serde_json::from_value(req.params)?;
                let host_id = params.file.host_id().clone();
                if !self.check_connection(next, &host_id).await {
                    return Ok(());
                }
                self.make_directory_from_editor(next, params.file).await?;
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
            "CloseFile" => {
                let params: DeleteFileParams = serde_json::from_value(req.params)?;
                self.handle_close_file_from_editor(next, params, false)
                    .await?;
                Ok(())
            }
            "DeleteFile" => {
                let params: DeleteFileParams = serde_json::from_value(req.params)?;
                let host_id = params.file.host_id().clone();
                if !self.check_connection(next, &host_id).await {
                    return Ok(());
                }
                self.handle_close_file_from_editor(next, params, true)
                    .await?;
                Ok(())
            }
            "PrintHistory" => {
                let params: PrintHistoryParams = serde_json::from_value(req.params)?;
                self.handle_print_history_from_editor(next, params).await;
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
        _msg_tx: &mpsc::Sender<webchannel::Message>,
        signaling_channel: &crate::signaling::client::SignalingChannel,
    ) -> anyhow::Result<()> {
        match notif.method.as_str() {
            "AcceptConnection" => {
                let mut params: AcceptConnectionParams = serde_json::from_value(notif.params)?;
                params.addr = normalize_signaling_addr(&params.addr);
                self.handle_accept_connection(next, params, signaling_channel)
                    .await;
            }
            "Connect" => {
                let mut params: ConnectParams = serde_json::from_value(notif.params)?;
                if let webchannel::TransportConfig::SCTP { signaling_addr } =
                    &mut params.transport_config
                {
                    *signaling_addr = normalize_signaling_addr(signaling_addr);
                }
                // Declare intent in ideal_world.
                self.ideal_world.remotes.insert(
                    params.host_id.clone(),
                    RemoteState {
                        transport: params.transport_config.clone(),
                        status: RemoteStatus::Connected,
                        ever_connected: false,
                        retry_stride_secs: INITIAL_RETRY_STRIDE_SECS,
                    },
                );
                // Reset a Failed entry to Pending so fix_world retries.
                if let Some(state) = self.current_world.remotes.get_mut(&params.host_id) {
                    if matches!(state.status, RemoteStatus::Failed { .. }) {
                        state.status = RemoteStatus::Pending;
                    }
                }
                if self.remote_connected(&params.host_id) {
                    next.send_notif(
                        NotificationCode::Connected,
                        HostAndMessageNote {
                            host_id: params.host_id,
                            message: "Already connected".to_string(),
                        },
                    )
                    .await;
                }
                self.fix_world_next();
            }
            "SendInfo" => {
                let params: SendInfoParams = serde_json::from_value(notif.params)?;
                self.handle_send_info_from_editor(next, params).await?;
            }
            "StopAccepting" => {
                let mut params: StopAcceptingParams = serde_json::from_value(notif.params)?;
                params.addr = normalize_signaling_addr(&params.addr);

                // Withdraw intent from ideal_world. fix_world will tear down current_world.
                self.ideal_world.signaling.remove(&params.addr);

                // Send AcceptStopped notification
                next.send_notif(
                    NotificationCode::AcceptStopped,
                    serde_json::json!({
                        "signalingAddr": params.addr,
                        "reason": "Stopped accepting by user request",
                    }),
                )
                .await;
                self.fix_world_next();
            }
            _ => {
                tracing::warn!("Unknown notification method: {}", notif.method);
                // TODO: send back an error response.
            }
        }
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(my_id = id_short(&self.host_id)))]
    async fn handle_remote_message(
        &mut self,
        msg: webchannel::Message,
        editor_tx: &mpsc::Sender<lsp_server::Message>,
        webchannel: &WebChannel,
    ) -> anyhow::Result<()> {
        tracing::info!("From remote: {}", remote_message_to_string(&msg));
        let next = Next::new(editor_tx, msg.req_id.clone(), webchannel);
        match msg.body {
            Msg::IceProgress {
                peer: remote_hostid,
                message: progress,
            } => {
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
            Msg::FailedToConnect {
                peer: host_id,
                reason,
            } => {
                self.current_world
                    .mark_remote_failed(&host_id, reason.clone());
                next.send_notif(
                    NotificationCode::ConnectionBroke,
                    ConnectionBrokeNote { host_id, reason },
                )
                .await;
                Ok(())
            }
            Msg::ConnectionBroke { peer: host_id } => {
                // Mark as failed if connection never established.
                let ever = self
                    .current_world
                    .remotes
                    .get(&host_id)
                    .map(|r| r.ever_connected)
                    .unwrap_or(false);
                if ever {
                    self.current_world.mark_remote_disconnected(&host_id);
                    self.fix_world_next();
                } else {
                    self.current_world
                        .mark_remote_failed(&host_id, "broke before reaching Connected".into());
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
            Msg::Hey { hey: hey_msg } => {
                let host = msg.host;
                self.current_world.mark_remote_connected(&host);
                next.send_notif(
                    NotificationCode::Connected,
                    HostAndMessageNote {
                        host_id: host.clone(),
                        message: hey_msg.message,
                    },
                )
                .await;

                // We might get new trusted host from accepting
                // connection, try to save it.
                let mut config = self.config.config();
                config.trusted_hosts = self.ideal_world.trusted.clone();
                let res = self.config.replace_and_save(config);

                if let Err(err) = res {
                    let msg = HostAndMessageNote {
                        host_id: host.clone(),
                        message: format!("Failed to save new trusted host to config file, {}", err),
                    };
                    next.send_notif(NotificationCode::UnimportantError, msg)
                        .await;
                }
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
                        Msg::BadRequest {
                            reason: "Missing req_id".to_string(),
                        },
                    )
                    .await;
                }
                Ok(())
            }
            Msg::FileList { files } => {
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
            Msg::ErrorResp {
                code,
                file,
                message,
            } => {
                // If this is a response to a request, send error response to editor
                if msg.req_id.is_some() {
                    let err = lsp_server::ResponseError {
                        code: code as i32,
                        message: message.clone(),
                        data: None,
                    };
                    next.send_resp((), Some(err)).await;
                } else {
                    // Otherwise send notification
                    next.send_notif(
                        NotificationCode::ErrorResponse,
                        ErrorResponseNote {
                            code,
                            file,
                            message,
                        },
                    )
                    .await;
                }
                Ok(())
            }
            Msg::RequestFile {
                file: file_desc,
                mode,
            } => {
                if msg.req_id.is_some() {
                    self.handle_request_file(&next, file_desc, msg.host.clone(), mode)
                        .await?;
                } else {
                    tracing::warn!("Received RequestFile without req_id, ignoring");
                    next.send_to_remote(
                        &msg.host,
                        Msg::BadRequest {
                            reason: "Missing req_id".to_string(),
                        },
                    )
                    .await;
                }
                Ok(())
            }
            Msg::Snapshot { snapshot } => {
                if msg.req_id.is_some() {
                    self.handle_snapshot(&next, snapshot, msg.host.clone())
                        .await?;
                } else {
                    tracing::warn!("Received Snapshot without req_id, ignoring");
                }
                Ok(())
            }
            Msg::MakeDirectory { file: file_desc } => {
                if !self.config.write_allowed(&msg.host) {
                    next.send_to_remote(
                        &msg.host,
                        Msg::PermissionDenied {
                            reason: "Write permission denied".to_string(),
                        },
                    )
                    .await;
                    return Ok(());
                }
                let (project, file) = match &file_desc {
                    FileDesc::ProjectFile { project, file } => (project.clone(), file.clone()),
                    FileDesc::Project { id } => {
                        let reply = Msg::ErrorResp {
                            code: ErrorCode::BadRequest,
                            file: Some(file_desc.clone()),
                            message: format!("Cannot create project root as directory: {}", id),
                        };
                        next.send_to_remote(&msg.host, reply).await;
                        return Ok(());
                    }
                };
                let res = self.make_directory_on_disk(&project, &file).await;
                let reply = if let Err(err) = res {
                    Msg::ErrorResp {
                        code: ErrorCode::IoError,
                        file: Some(file_desc),
                        message: err.to_string(),
                    }
                } else {
                    Msg::DirectoryMade { file: file_desc }
                };
                next.send_to_remote(&msg.host, reply).await;
                Ok(())
            }
            Msg::DirectoryMade { file: file_desc } => {
                if msg.req_id.is_none() {
                    tracing::warn!("Received DirectoryMade without req_id, ignoring");
                    return Ok(());
                }
                let resp = MakeDirectoryResp {
                    file: EditorFileDesc::new(file_desc, msg.host.clone()),
                };
                next.send_resp(resp, None).await;
                Ok(())
            }
            Msg::OpFromClient { ops: context_ops } => {
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
            Msg::MoveFile {
                project,
                source,
                dest,
            } => {
                // Check write permission.
                if !self.config.write_allowed(&msg.host) {
                    next.send_to_remote(
                        &msg.host,
                        Msg::PermissionDenied {
                            reason: "Write permission denied".to_string(),
                        },
                    )
                    .await;
                    return Ok(());
                }

                self.handle_move_file(&next, project, source, dest, msg.host.clone())
                    .await?;
                Ok(())
            }
            Msg::FileMoved {
                project,
                source: old_path,
                dest: new_path,
            } => {
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
                    // No req_id, meaning we're receiving this message
                    // because a doc we're subscribed to moved.
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
            Msg::SaveFile { doc: doc_id } => {
                // Check write permission.
                if !self.config.write_allowed(&msg.host) {
                    next.send_to_remote(
                        &msg.host,
                        Msg::PermissionDenied {
                            reason: "Write permission denied".to_string(),
                        },
                    )
                    .await;
                    return Ok(());
                }

                let res = self.save_file_to_disk(&doc_id).await;
                let resp = if let Err(err) = res {
                    Msg::ErrorResp {
                        code: ErrorCode::IoError,
                        file: self.docs.get(&doc_id).map(|doc| doc.file_desc.clone()),
                        message: err.to_string(),
                    }
                } else {
                    Msg::FileSaved { doc: doc_id }
                };
                next.send_to_remote(&msg.host, resp).await;
                Ok(())
            }
            Msg::FileSaved { doc: doc_id } => {
                if msg.req_id.is_none() {
                    tracing::warn!("Received FileSaved without req_id, ignoring");
                    next.send_to_remote(
                        &msg.host,
                        Msg::BadRequest {
                            reason: "Missing req_id".to_string(),
                        },
                    )
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
            Msg::StopSendingOps { doc: doc_id } => {
                // Remove the remote host from the doc's subscriber list.
                if let Some(doc) = self.docs.get_mut(&doc_id) {
                    if doc.subscribers.remove(&msg.host).is_some() {
                        tracing::info!(
                            "Removed {} from subscribers of doc {}",
                            id_short(&msg.host),
                            doc_id
                        );
                    } else {
                        tracing::warn!(
                            "Host {} was not subscribed to doc {}",
                            id_short(&msg.host),
                            doc_id
                        );
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
            Msg::CloseFile { file, delete } => {
                self.handle_close_file_from_remote(next, file, msg.host, delete)
                    .await?;
                Ok(())
            }
            Msg::FileDeleted { file: file_desc } => {
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
            Msg::FileClosed { file: file_desc } => {
                // Notification from remote that file was closed.
                let editor_file_desc = EditorFileDesc::new(file_desc, msg.host.clone());
                let note = FileClosedNote {
                    file: editor_file_desc,
                };
                next.send_notif(NotificationCode::FileClosed, note).await;
                Ok(())
            }
            Msg::InfoFromClient { info } => {
                // Received from a client, broadcast to all subscribers
                let res = self.broadcast_info(&next, info).await;
                if let Err(err) = res {
                    tracing::warn!(
                        "{} when handling InfoFromClient from {}",
                        err,
                        id_short(&msg.host)
                    );
                }
                Ok(())
            }
            Msg::InfoFromServer { info } => {
                let remote_doc = self.remote_docs.get(&msg.host, info.doc_id);
                if let Some(remote_doc) = remote_doc {
                    let editor_file =
                        EditorFileDesc::new(remote_doc.file_desc.clone(), msg.host.clone());

                    let info_json = serde_json::from_str(&info.value)?;
                    let note = SendInfoNote {
                        file: editor_file,
                        info: info_json,
                        sender: info.sender,
                    };
                    next.send_notif(NotificationCode::InfoReceived, note).await;
                } else {
                    next.send_to_remote(&msg.host, Msg::StopSendingOps { doc: info.doc_id })
                        .await;
                }
                Ok(())
            }
            Msg::PermissionDenied { reason: message } => {
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
            Msg::DocFatal {
                doc: doc_id,
                reason,
            } => {
                let remote_doc = self.remote_docs.remove(&msg.host, doc_id);
                let msg = ErrorResponseNote {
                    code: ErrorCode::DocFatal,
                    file: remote_doc.map(|doc| doc.file_desc),
                    message: reason,
                };
                next.send_notif(NotificationCode::ErrorResponse, msg).await;
                Ok(())
            }
            Msg::Bye => {
                // Peer is shutting down. Drop our entry for them so
                // fix_world doesn’t try to reach them, and tell the
                // editor to clean up the host without scheduling a retry.
                let host = msg.host;
                self.current_world.remove_remote(&host);
                next.webchannel.disconnect(&host);
                self.fix_world_next();
                next.send_notif(
                    NotificationCode::RemoteLeft,
                    HostAndMessageNote {
                        host_id: host,
                        message: "Peer said goodbye".to_string(),
                    },
                )
                .await;
                Ok(())
            }
            _ => {
                let message = format!(
                    "Unrecognized message type from {}: {:?}",
                    msg.host, msg.body
                );
                tracing::warn!(message);
                let msg = HostAndMessageNote {
                    host_id: msg.host,
                    message,
                };
                next.send_notif(NotificationCode::InternalError, msg).await;
                Ok(())
            }
        }
    }

    // **** Handler functions

    async fn try_connect_remote<'a>(
        &mut self,
        next: &Next<'a>,
        host_id: ServerId,
        transport_config: webchannel::TransportConfig,
        msg_tx: &mpsc::Sender<webchannel::Message>,
        signaling_channel: &crate::signaling::client::SignalingChannel,
    ) {
        if host_id == self.host_id {
            return;
        }

        // Dispatch to ssh transport.
        if let webchannel::TransportConfig::SshStdio {
            ssh_host,
            command,
            projects,
        } = &transport_config
        {
            let ssh_host = ssh_host.clone();
            let command = command.clone();
            let projects = projects.clone();
            self.try_connect_remote_ssh(next, host_id, ssh_host, command, projects, msg_tx)
                .await;
            return;
        }

        // Dispatch to normal SCTP transport.
        return self
            .try_connect_remote_sctp(next, host_id, transport_config, msg_tx, signaling_channel)
            .await;
    }

    async fn try_connect_remote_sctp<'a>(
        &mut self,
        next: &Next<'a>,
        host_id: ServerId,
        transport_config: webchannel::TransportConfig,
        _msg_tx: &mpsc::Sender<webchannel::Message>,
        signaling_channel: &crate::signaling::client::SignalingChannel,
    ) {
        let signaling_addr = match &transport_config {
            webchannel::TransportConfig::SCTP { signaling_addr } => signaling_addr.clone(),
            webchannel::TransportConfig::SshStdio { .. } => unreachable!(),
        };

        next.send_notif(
            NotificationCode::Connecting,
            ConnectingNote {
                host_id: host_id.clone(),
            },
        )
        .await;

        self.current_world
            .set_remote_connecting1(&host_id, transport_config);

        // Send Connect message to signaling server.
        // SDP exchange happens later via ice_connect_with_sock.
        self.send_connect_message(next, host_id, &signaling_addr, signaling_channel, true)
            .await;

        // The connection continues in handle_signaling_msg when we
        // receive the peer's Connect response message.
    }

    /// Connect to a remote envoy via ssh. Spawn `ssh ssh_host --
    /// collab-mode --envoy`, send `Msg::EnvoyInit`, then `Msg::Hey`.
    async fn try_connect_remote_ssh<'a>(
        &mut self,
        next: &Next<'a>,
        host_id: ServerId,
        ssh_host: String,
        command: Vec<String>,
        projects: Vec<crate::config_man::ConfigProject>,
        msg_tx: &mpsc::Sender<webchannel::Message>,
    ) {
        next.send_notif(
            NotificationCode::Connecting,
            ConnectingNote {
                host_id: host_id.clone(),
            },
        )
        .await;

        let transport_config = webchannel::TransportConfig::SshStdio {
            ssh_host: ssh_host.clone(),
            command: command.clone(),
            projects: projects.clone(),
        };
        self.current_world
            .set_remote_connecting1(&host_id, transport_config);

        // Generate key+cert for the envoy to use, and trust it.
        let envoy_name = format!("{}-envoy", self.host_name);
        let (envoy_key_pem, envoy_cert_pem, envoy_cert_hash) =
            match crate::config_man::generate_temp_key_cert(&envoy_name) {
                Ok(t) => t,
                Err(err) => {
                    tracing::warn!("Failed to generate cert for {}: {}", &envoy_name, &err);
                    self.current_world
                        .mark_remote_failed(&host_id, format!("Failed to generate cert: {}", err));
                    let msg = webchannel::Message {
                        host: envoy_name.clone(),
                        body: Msg::FailedToConnect {
                            peer: envoy_name,
                            reason: format!("Failed to generate cert: {}", err),
                        },
                        req_id: None,
                    };
                    let _ = msg_tx.send(msg).await;
                    return;
                }
            };
        let envoy_id = format!("{}::{}", envoy_name, envoy_cert_hash);
        // The envoy connects back as a peer using this temporary cert.
        // Only valid for this process's lifetime; lives outside the
        // user-declared `ideal_world.trusted`.
        self.local_trusted_hosts.insert(envoy_id.clone());

        let envoy_init = Msg::EnvoyInit {
            host_id: self.host_id.clone(),
            envoy_id: envoy_id.clone(),
            envoy_key_pem,
            envoy_cert_pem,
            projects,
        };

        let webchannel = next.webchannel.clone();
        let key_cert = self.key_cert.clone();
        let my_host_id = self.host_id.clone();
        let msg_tx = msg_tx.clone();

        tokio::spawn(async move {
            let res = async {
                webchannel
                    .connect(
                        envoy_id.clone(),
                        webchannel::Transport::Ssh {
                            ssh_host: ssh_host.clone(),
                            command: command.clone(),
                        },
                        key_cert,
                    )
                    .await?;
                webchannel.send(&envoy_id, None, envoy_init).await?;
                webchannel
                    .send(
                        &envoy_id,
                        None,
                        Msg::Hey {
                            hey: message::HeyMessage {
                                message: "Nice to meet ya".to_string(),
                                credentials: "".to_string(),
                                version: "v1.0.0".to_string(),
                            },
                        },
                    )
                    .await?;
                anyhow::Ok(())
            }
            .await;

            if let Err(err) = res {
                tracing::warn!("Failed to connect to {}: {}", id_short(&envoy_id), err);
                let msg = webchannel::Message {
                    host: my_host_id,
                    body: Msg::FailedToConnect {
                        peer: envoy_id,
                        reason: err.to_string(),
                    },
                    req_id: None,
                };
                let _ = msg_tx.send(msg).await;
            }
        });
    }

    /// Helper function for sending connect message to signaling
    /// server plus cleaning up if failed to send (set remote state
    /// and send notification to editor).
    async fn send_connect_message<'a>(
        &mut self,
        next: &Next<'a>,
        peer_id: ServerId,
        signaling_addr: &str,
        signaling_channel: &crate::signaling::client::SignalingChannel,
        initiator: bool,
    ) {
        let msg = crate::signaling::SignalingMsg::Connect {
            sender: self.host_id.clone(),
            receiver: peer_id.clone(),
            initiator,
        };
        let res = signaling_channel.send(signaling_addr, msg).await;
        if let Err(err) = res {
            tracing::warn!("Failed to send Connect message: {}", err);

            // Update remote state to Failed.
            self.current_world
                .mark_remote_failed(&peer_id, format!("Failed to send Connect message: {}", err));

            // Notify editor
            next.send_notif(
                NotificationCode::ConnectionBroke,
                ConnectionBrokeNote {
                    host_id: peer_id.clone(),
                    reason: format!("Failed to send Connect message: {}", err),
                },
            )
            .await;
        }
    }

    // If host_id is not us, delegate to remote, if it’s us, read
    // files from disk. If dir is None, list top-level projects and
    // docs, if dir non-nil, list files in that dir.
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

    async fn list_files_from_remote<'a>(
        &mut self,
        next: &Next<'a>,
        dir: Option<FileDesc>,
        remote_host_id: ServerId,
    ) -> anyhow::Result<()> {
        match self.list_files_from_disk(dir.clone()).await {
            Ok(files) => {
                let msg = Msg::FileList { files: files };
                next.send_to_remote(&remote_host_id, msg).await;
            }
            Err(err) => {
                let msg = Msg::ErrorResp {
                    code: ErrorCode::IoError,
                    file: dir,
                    message: err.to_string(),
                };
                next.send_to_remote(&remote_host_id, msg).await;
            }
        }
        Ok(())
    }

    async fn list_files_from_disk(
        &self,
        dir: Option<FileDesc>,
    ) -> anyhow::Result<Vec<ListFilesEntry>> {
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

                    result.push(ListFilesEntry {
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
                            result.push(ListFilesEntry {
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
                            entries.push(ListFilesEntry {
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
                    result.push(ListFilesEntry {
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
                    proj_result.push(ListFilesEntry {
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
                    proj_result.push(ListFilesEntry {
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
        create_allowed: bool,
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

        // Check if create is allowed when file doesn't exist.
        if matches!(mode, OpenMode::Create) && !create_allowed && !full_path.exists() {
            return Err(ServerError::PermissionDenied(format!(
                "Creating file '{}' is not allowed",
                full_path.display()
            ))
            .into());
        }

        let mut open_options = std::fs::OpenOptions::new();
        open_options.read(true).write(true);
        if matches!(mode, OpenMode::Create) && create_allowed {
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
                // Not opened, read from disk and create doc. For open
                // request from ourselves, always allow create.
                let allow_create = true;
                let res = self
                    .open_file_from_disk(&file_desc.project, &file_desc.file, mode, allow_create)
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
                    // Use the current gseq and internal_doc from the doc's engine.
                    engine: ClientEngine::new(
                        self.site_id,
                        doc.engine.current_seq(),
                        doc.engine.internal_doc().clone(),
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
        let msg = Msg::RequestFile {
            file: file_desc.clone().into(),
            mode: mode,
        };
        next.send_to_remote(&file_desc.host_id(), msg).await;
        Ok(())
    }

    async fn make_directory_from_editor<'a>(
        &mut self,
        next: &Next<'a>,
        file_desc: EditorFileDesc,
    ) -> anyhow::Result<()> {
        if &self.host_id == file_desc.host_id() {
            // Local.
            let res = self
                .make_directory_on_disk(&file_desc.project, &file_desc.file)
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
            let resp = MakeDirectoryResp { file: file_desc };
            next.send_resp(resp, None).await;
            return Ok(());
        }

        // Remote.
        let msg = Msg::MakeDirectory {
            file: file_desc.clone().into(),
        };
        next.send_to_remote(&file_desc.host_id(), msg).await;
        Ok(())
    }

    async fn make_directory_on_disk(
        &self,
        project_id: &ProjectId,
        rel_path: &str,
    ) -> anyhow::Result<()> {
        if project_id == RESERVED_BUFFERS_PROJECT || project_id == RESERVED_FILES_PROJECT {
            return Err(anyhow!(
                "Cannot create directory in reserved project {}",
                project_id
            ));
        }
        let project = self
            .projects
            .get(project_id)
            .ok_or_else(|| anyhow!("Project {} not found", project_id))?;
        let full_path = std::path::PathBuf::from(&project.root).join(rel_path);

        std::fs::create_dir_all(&full_path)
            .with_context(|| format!("Failed to create directory: {}", full_path.display()))?;
        Ok(())
    }

    async fn handle_request_file<'a>(
        &mut self,
        next: &Next<'a>,
        file_desc: FileDesc,
        remote_host_id: ServerId,
        mode: OpenMode,
    ) -> anyhow::Result<()> {
        let res = self.path_id_from_file_desc(&file_desc);
        if let Err(err) = res {
            next.send_to_remote(
                &remote_host_id,
                Msg::ErrorResp {
                    code: ErrorCode::IoError,
                    file: Some(file_desc.clone()),
                    message: err.to_string(),
                },
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
                            internal_doc: doc.engine.internal_doc().clone(),
                            name: doc.name.clone(),
                            seq: doc.engine.current_seq(),
                            site_id,
                            file_desc: file_desc.clone(),
                            doc_id: *doc_id,
                        };
                        next.send_to_remote(&remote_host_id, Msg::Snapshot { snapshot: snapshot })
                            .await;
                        return Ok(());
                    }
                }

                // Not already open, read from disk.
                let create_allowed = self.config.create_allowed(&remote_host_id);
                let res = self
                    .open_file_from_disk(project, file, mode, create_allowed)
                    .await;
                if let Err(err) = res {
                    let code = match err.downcast_ref::<ServerError>() {
                        Some(ServerError::PermissionDenied(_)) => ErrorCode::PermissionDenied,
                        _ => ErrorCode::IoError,
                    };
                    next.send_to_remote(
                        &remote_host_id,
                        Msg::ErrorResp {
                            code,
                            file: Some(file_desc.clone()),
                            message: err.to_string(),
                        },
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
                let content_len = content.chars().count() as u64;
                let snapshot = NewSnapshot {
                    content,
                    internal_doc: InternalDoc::new(content_len),
                    name: filename,
                    seq: 0,
                    site_id,
                    file_desc,
                    doc_id,
                };
                next.send_to_remote(&remote_host_id, Msg::Snapshot { snapshot: snapshot })
                    .await;
            }
            FileDesc::Project { .. } => {
                next.send_to_remote(
                    &remote_host_id,
                    Msg::BadRequest {
                        reason: "Cannot open a project directory".to_string(),
                    },
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
            engine: ClientEngine::new(snapshot.site_id, snapshot.seq, snapshot.internal_doc),
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

        // Check permission.
        if !self.config.write_allowed(&remote_host_id) {
            next.send_to_remote(
                &remote_host_id,
                Msg::PermissionDenied {
                    reason: "Write permission denied".to_string(),
                },
            )
            .await;
            return Ok(());
        }

        // Get the doc.
        let doc = self.docs.get_mut(&doc_id);
        if doc.is_none() {
            next.send_to_remote(
                &remote_host_id,
                Msg::BadRequest {
                    reason: format!("Doc {} not found", doc_id),
                },
            )
            .await;
            return Ok(());
        }
        let doc = doc.unwrap();

        // Check site seq.
        let subscriber_data = doc.subscribers.get(&remote_host_id).copied();
        if subscriber_data.is_none() {
            let msg = Msg::BadRequest {
                reason: format!("Doc {} ({}) not open", doc.name, doc_id),
            };
            next.send_to_remote(&remote_host_id, msg).await;
            return Ok(());
        }
        let expected_seq = subscriber_data.unwrap() + 1;

        // Check if first op's local site seq matches what we expect.
        // Even with disconnect, we shouldn’t receive bogus ops from
        // client.
        if let Some(first_op) = context_ops.ops.first() {
            if first_op.site_seq != expected_seq {
                let err = format!(
                    "Site seq mismatch: expected {}, got {}",
                    expected_seq, first_op.site_seq
                );
                let msg = Msg::DocFatal {
                    doc: doc_id,
                    reason: err,
                };
                next.send_to_remote(&remote_host_id, msg).await;
                return Ok(());
            }
        }

        // Process ops.
        let processed_ops = doc
            .engine
            .process_ops(context_ops.ops.clone(), context_ops.context)?;

        // Record subscriber's latest received site seq.
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
                Msg::BadRequest {
                    reason: format!("Doc {} not found", doc_id),
                },
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
                        Msg::FileMoved {
                            project: project_id.clone(),
                            source: old_path.clone(),
                            dest: new_path.clone(),
                        },
                    )
                    .await;
                }
            }
            Err(err) => {
                next.send_to_remote(
                    &remote_host_id,
                    Msg::ErrorResp {
                        code: ErrorCode::IoError,
                        file: None,
                        message: err.to_string(),
                    },
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
            next.send_to_remote(&remote_host_id, Msg::StopSendingOps { doc: doc_id })
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
                tracing::warn!("Received remote op without global seq: {:?}", &remote_op);
                next.send_notif(
                    NotificationCode::ErrorResponse,
                    ErrorResponseNote {
                        code: ErrorCode::DocFatal,
                        file: Some(remote_doc.file_desc.clone()),
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
        let last_seq = remote_doc.remote_op_buffer.last().and_then(|op| op.seq);
        if op_is_not_ours && new_buffer_len > old_buffer_len && new_buffer_len > 0 {
            let file_desc =
                EditorFileDesc::new(remote_doc.file_desc.clone(), remote_host_id.clone());
            next.send_notif(
                NotificationCode::RemoteOpsArrived,
                RemoteOpsArrivedNote {
                    file: file_desc,
                    last_seq: last_seq.unwrap(),
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
                sender: self.host_id.clone(),
                value: info_value,
            };

            let remote_host_id = file.host_id().clone();
            next.send_to_remote(&remote_host_id, Msg::InfoFromClient { info: info })
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

        // This call shouldn’t return any error since we already
        // verified that the doc exists.
        self.broadcast_info(next, info).await.unwrap();
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

        let handle_error = async |msg: String, remote_docs: &mut RemoteDocs| -> () {
            tracing::warn!(msg);
            let err = lsp_server::ResponseError {
                code: ErrorCode::DocFatal as i32,
                message: msg,
                data: None,
            };
            next.send_resp((), Some(err)).await;
            // Remove from remote_docs and mapping.
            remote_docs.remove(&host_id, doc_id);
        };

        // Process local ops from editor first, because we want to
        // pretent the local ops arrives before the remote ops.
        let process_local_ops = || -> anyhow::Result<()> {
            for editor_op in ops {
                // Process the op with the engine
                let site_seq = remote_doc.next_site_seq;
                remote_doc.next_site_seq += 1;

                let instructions = remote_doc.engine.process_local_op(editor_op, site_seq)?;
                // Apply each processed instruction to the buffer without checking,
                // then verify sync once after all instructions from this editor op.
                // This is needed because Undo/Redo generate multiple instructions
                // that need to be applied as a batch.
                for instr in instructions {
                    remote_doc.apply_edit_instruction_unchecked(&instr)?;
                }
                remote_doc.verify_buffer_engine_sync()?;
            }
            Ok(())
        };
        if let Err(err) = process_local_ops() {
            handle_error(
                format!("Failed to process local ops: {}", err),
                &mut self.remote_docs,
            )
            .await;
            return Ok(());
        }

        // Process the drained remote ops.
        let mut lean_ops = Vec::new();
        let apply_remote_ops = || -> anyhow::Result<()> {
            for remote_op in remote_ops {
                if let Some((lean_op, _seq)) = remote_doc.engine.process_remote_op(remote_op)? {
                    remote_doc.apply_edit_instruction(&lean_op.op)?;
                    lean_ops.push(lean_op);
                }
            }
            Ok(())
        };
        if let Err(err) = apply_remote_ops() {
            handle_error(
                format!("Failed to apply remote ops: {}", err),
                &mut self.remote_docs,
            )
            .await;
            return Ok(());
        }

        // Package local ops if any
        if let Some(context_ops) = remote_doc.engine.maybe_package_local_ops(doc_id) {
            // Send to the owner (could be ourselves or a remote)
            // Don't attach req_id since this isn’t a direct response
            // (not that it matters much).
            next.send_to_remote_no_req_id(&host_id, Msg::OpFromClient { ops: context_ops })
                .await;
        }

        // Send response with processed remote ops
        let resp = SendOpsResp {
            ops: lean_ops,
            last_global_seq: remote_doc.engine.current_gseq(),
            pending_local_ops: remote_doc.engine.pending_local_ops(),
            doc_len: remote_doc.buffer.len() as u64,
        };
        next.send_resp(resp, None).await;
        Ok(())
    }

    fn handle_undo_from_editor(&mut self, params: UndoParams) -> anyhow::Result<UndoResp> {
        let (_doc_id, remote_doc) = self
            .get_remote_doc_mut(&params.file)
            .ok_or_else(|| anyhow::anyhow!("Remote doc not found: {}", params.file))?;

        let (ops, context) = match params.kind {
            UndoKind::Undo => remote_doc.engine.generate_undo_op(),
            UndoKind::Redo => remote_doc.engine.generate_redo_op(),
        };
        let resp = UndoResp { ops, context };
        Ok(resp)
    }

    async fn handle_print_history_from_editor<'a>(
        &self,
        next: &Next<'a>,
        params: PrintHistoryParams,
    ) -> () {
        let res = self.get_remote_doc(&params.file);
        if res.is_none() {
            next.send_resp(
                (),
                Some(lsp_server::ResponseError {
                    code: ErrorCode::IoError as i32,
                    message: format!("File not found: {}", params.file),
                    data: None,
                }),
            )
            .await;
            return;
        }

        let remote_doc = res.unwrap().1;
        let history = remote_doc.engine.print_history(params.debug);
        let resp = PrintHistoryResp { history };
        next.send_resp(resp, None).await;
    }

    async fn handle_connection_state_from_editor<'a>(&self, next: &Next<'a>) -> () {
        // Collect current remotes for the wire response.
        let mut connections = Vec::new();
        let now = Instant::now();
        for (host_id, remote) in &self.current_world.remotes {
            let state = match &remote.status {
                RemoteStatus::Pending => ConnectionState::Pending,
                RemoteStatus::ConnectingStage1 { .. } => {
                    ConnectionState::ConnectingStage1(unix_epoch_secs(), !remote.ever_connected)
                }
                RemoteStatus::ConnectingStage2 { .. } => {
                    ConnectionState::ConnectingStage2(unix_epoch_secs(), !remote.ever_connected)
                }
                RemoteStatus::Connected => ConnectionState::Connected,
                RemoteStatus::Disconnected { .. } => ConnectionState::Disconnected,
                RemoteStatus::Failed { .. } => ConnectionState::FailedToConnect,
            };
            let next_retry_in_secs =
                if let RemoteStatus::Disconnected { next_retry_at, .. } = &remote.status {
                    Some(next_retry_at.saturating_duration_since(now).as_secs())
                } else {
                    None
                };
            connections.push(message::ConnectionStateEntry {
                host_id: host_id.clone(),
                state,
                transport: remote.transport.clone(),
                next_retry_in_secs,
            });
        }

        // Collect accepting signaling addresses (only Bound servers).
        let accepting: Vec<AcceptingEntry> = self
            .current_world
            .signaling
            .iter()
            .filter(|(_addr, state)| matches!(state.status, SignalingStatus::Bound))
            .map(|(addr, _)| AcceptingEntry { addr: addr.clone() })
            .collect();

        // Collect owned docs from self.docs.
        let mut live = Vec::new();
        for (_doc_id, doc) in &self.docs {
            let file = EditorFileDesc::new(doc.file_desc.clone(), self.host_id.clone());
            let subscribers: Vec<ServerId> = doc.subscribers.keys().cloned().collect();
            live.push(message::LiveDocEntry {
                file,
                subscribers,
                filename: doc.name.clone(),
                meta: doc.meta.clone(),
                seq: doc.engine.current_seq(),
            });
        }

        // Collect docs from self.remote_docs.
        let mut connected = Vec::new();
        for ((host_id, _doc_id), remote_doc) in &self.remote_docs.docs {
            let file = EditorFileDesc::new(remote_doc.file_desc.clone(), host_id.clone());
            connected.push(message::ConnectedDocEntry {
                file,
                filename: remote_doc.name.clone(),
                meta: remote_doc.meta.clone(),
            });
        }

        let projects: Vec<ConfigProject> = self
            .projects
            .iter()
            .map(|(_, proj)| ConfigProject {
                name: proj.name.clone(),
                path: proj.root.clone(),
            })
            .collect();

        let config = self.config.config();
        let msg = message::ConnectionStateResp {
            connections,
            accepting,
            live,
            connected,
            projects,
            trusted_hosts: self.ideal_world.trusted.clone(),
            permission: config.permission,
            cert_hash: self.key_cert.cert_der_hash(),
        };
        next.send_resp(msg, None).await;
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
            engine: ClientEngine::new(
                self.site_id,
                0,
                InternalDoc::new(params.content.chars().count() as u64),
            ),
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
            next.send_to_remote(
                &host_id,
                Msg::MoveFile {
                    project,
                    source: old_path,
                    dest: new_path,
                },
            )
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
                                Msg::FileMoved {
                                    project: project.clone(),
                                    source: old_path.clone(),
                                    dest: new_path.clone(),
                                },
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
                next.send_to_remote(&host_id, Msg::SaveFile { doc: doc_id })
                    .await;
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
            next.send_to_remote(&host_id, Msg::StopSendingOps { doc: doc_id })
                .await;
        } else {
            tracing::info!("Doc {} not found, nothing to disconnect", file);
        }

        // Send empty response to indicate success
        next.send_resp((), None).await;
        Ok(())
    }

    async fn handle_close_file_from_editor<'a>(
        &mut self,
        next: &Next<'a>,
        params: DeleteFileParams,
        delete: bool,
    ) -> anyhow::Result<()> {
        let DeleteFileParams { file: editor_file } = params;
        let host_id = editor_file.host_id().clone();
        let file: FileDesc = editor_file.into();

        // Remote.
        if host_id != self.host_id {
            next.send_to_remote(&host_id, Msg::CloseFile { file, delete })
                .await;
            return Ok(());
        }

        // Local.
        match self.handle_close_file_locally(&file, delete).await {
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
                            if delete {
                                Msg::FileDeleted { file: file.clone() }
                            } else {
                                Msg::FileClosed { file: file.clone() }
                            },
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

    async fn handle_close_file_from_remote<'a>(
        &mut self,
        next: Next<'a>,
        file_desc: FileDesc,
        requester: ServerId,
        delete: bool,
    ) -> anyhow::Result<()> {
        // Check delete permission
        if !self.config.delete_allowed(&requester) {
            next.send_to_remote(
                &requester,
                Msg::PermissionDenied {
                    reason: "Permission denied for deleting/closing file".to_string(),
                },
            )
            .await;
            return Ok(());
        }

        match self.handle_close_file_locally(&file_desc, delete).await {
            Ok(subscribers) => {
                let mut requester_notified = false;

                // Send FileDeleted message to all subscribers.
                for subscriber_id in &subscribers {
                    let msg = if delete {
                        Msg::FileDeleted {
                            file: file_desc.clone(),
                        }
                    } else {
                        Msg::FileClosed {
                            file: file_desc.clone(),
                        }
                    };
                    if subscriber_id == &requester {
                        next.send_to_remote(subscriber_id, msg).await;
                        requester_notified = true;
                    } else if subscriber_id != &self.host_id {
                        next.send_to_remote_no_req_id(subscriber_id, msg).await;
                    }
                }

                // If the requester is not a subscriber, still send them a response.
                let msg = if delete {
                    Msg::FileDeleted {
                        file: file_desc.clone(),
                    }
                } else {
                    Msg::FileClosed {
                        file: file_desc.clone(),
                    }
                };
                if !requester_notified {
                    next.send_to_remote(&requester, msg).await;
                }
            }
            Err(err) => {
                next.send_to_remote(
                    &requester,
                    Msg::ErrorResp {
                        code: ErrorCode::IoError,
                        file: Some(file_desc.clone()),
                        message: err.to_string(),
                    },
                )
                .await;
            }
        }
        Ok(())
    }

    async fn handle_close_file_locally(
        &mut self,
        file: &FileDesc,
        delete: bool,
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
            self.remote_docs.remove(&self.host_id, doc_id);
        }

        // Delete the file or directory from disk if there's a path to delete.
        if delete && path_to_delete.is_some() {
            let path = path_to_delete.unwrap();
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

    /// Broadcast `info`, excluding the sender of it.
    async fn broadcast_info<'a>(&mut self, next: &Next<'a>, info: Info) -> anyhow::Result<()> {
        let doc_id = info.doc_id;
        let doc = self.docs.get(&doc_id);
        if doc.is_none() {
            return Err(anyhow!("Doc {} not found", doc_id));
        }
        let doc = doc.unwrap();
        let sender = &info.sender;

        // Send InfoFromServer to all subscribers except the excluded host.
        for (subscriber_host_id, _) in &doc.subscribers {
            if subscriber_host_id != sender {
                next.send_to_remote(
                    subscriber_host_id,
                    Msg::InfoFromServer { info: info.clone() },
                )
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
        let mut config = self.config.config();

        config.trusted_hosts = params.config.trusted_hosts.clone();
        self.ideal_world.trusted = params.config.trusted_hosts;

        config.permission = params.config.permission;

        self.config.replace_and_save(config)?;
        Ok(())
    }

    fn remote_connected(&self, host_id: &ServerId) -> bool {
        matches!(
            self.current_world.remotes.get(host_id).map(|r| &r.status),
            Some(RemoteStatus::Connected)
        )
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

    /// Handle SignalingMsg from signaling client.
    async fn handle_signaling_msg<'a>(
        &mut self,
        signaling_addr: String,
        msg: crate::signaling::SignalingMsg,
        next: &Next<'a>,
        msg_tx: &mpsc::Sender<webchannel::Message>,
        signaling_channel: &crate::signaling::client::SignalingChannel,
    ) -> anyhow::Result<()> {
        tracing::info!("From signaling server: {:?}", &msg);
        match msg {
            SignalingMsg::Bound { id: _, trusted } => {
                self.handle_signaling_bound_msg(
                    signaling_addr,
                    trusted,
                    next,
                    msg_tx,
                    signaling_channel,
                )
                .await;
                Ok(())
            }

            SignalingMsg::Trusted { id: _, trusted } => {
                if let Some(s) = self.current_world.signaling.get_mut(&signaling_addr) {
                    s.trusted = trusted.into_iter().collect();
                    s.trust_sent_at = None;
                }
                Ok(())
            }

            SignalingMsg::Connect {
                sender: peer_id,
                receiver: _,
                initiator,
            } => {
                self.handle_signaling_connect_msg(
                    signaling_addr,
                    peer_id,
                    initiator,
                    next,
                    msg_tx,
                    signaling_channel,
                )
                .await;
                Ok(())
            }

            // Our connect message is rejected by remote_id. Reasons
            // can be invalid certificate, etc. Mark the connection as
            // failed so we don’t retry.
            SignalingMsg::Rejected {
                id: remote_id,
                reason,
            } => {
                self.current_world
                    .mark_remote_failed(&remote_id, format!("Rejected by signaling: {}", reason));
                let msg = ErrorResponseNote {
                    code: ErrorCode::NetworkError,
                    file: None,
                    message: format!("Connection to {} rejected: {}", remote_id, reason),
                };
                next.send_notif(NotificationCode::FailedToConnect, msg)
                    .await;
                Ok(())
            }

            SignalingMsg::IdNotFound {
                id: endpoint_id,
                reason,
            } => {
                self.current_world
                    .mark_remote_failed(&endpoint_id, format!("IdNotFound: {}", reason));
                next.send_notif(
                    NotificationCode::ErrorResponse,
                    ErrorResponseNote {
                        code: ErrorCode::NetworkError,
                        file: None,
                        message: format!("Signaling error: {}", reason),
                    },
                )
                .await;
                Ok(())
            }

            _ => {
                // Other variants are no-op
                Ok(())
            }
        }
    }

    /// Handle SignalingMsg::Bound message.
    async fn handle_signaling_bound_msg<'a>(
        &mut self,
        signaling_addr: String,
        trusted: Vec<ServerId>,
        next: &Next<'a>,
        msg_tx: &mpsc::Sender<webchannel::Message>,
        signaling_channel: &crate::signaling::client::SignalingChannel,
    ) {
        let _ = msg_tx;
        let _ = signaling_channel;
        self.current_world.mark_signaling_bound(&signaling_addr);
        // Record the trust list the signaling server confirmed.
        if let Some(s) = self.current_world.signaling.get_mut(&signaling_addr) {
            s.trusted = trusted.into_iter().collect();
            s.trust_sent_at = None;
        }
        tracing::info!("Successfully bound to signaling server {}", signaling_addr);
        // fix_world picks up any Pending remotes for this signaling
        // server on the next tick.
        self.fix_world_next();

        // Notify editor that we’re accepting.
        next.send_notif(
            NotificationCode::AcceptingConnection,
            AcceptingConnectionNote {
                addr: signaling_addr.clone(),
            },
        )
        .await;
    }

    /// Handle SignalingMsg::Connect message.
    async fn handle_signaling_connect_msg<'a>(
        &mut self,
        signaling_addr: String,
        peer_id: ServerId,
        initiator: bool,
        next: &Next<'a>,
        msg_tx: &mpsc::Sender<webchannel::Message>,
        signaling_channel: &crate::signaling::client::SignalingChannel,
    ) {
        // Check if we're already connecting or connected to this peer.
        let is_connecting_or_connected =
            self.current_world.remotes.get(&peer_id).map_or(false, |r| {
                matches!(
                    r.status,
                    RemoteStatus::ConnectingStage2 { .. } | RemoteStatus::Connected
                )
            });
        tracing::info!(
            "Received Connect message from {}, is_connecting_or_connected={}",
            peer_id,
            is_connecting_or_connected
        );
        if is_connecting_or_connected && !initiator {
            tracing::info!(
                "Already connecting/connected to {}, ignoring duplicate connection request",
                peer_id
            );
            return;
        }

        let peer_cert_hash = id_hash(&peer_id).to_string();
        let trusted = self.is_cert_trusted(&peer_cert_hash);
        if trusted {
            self.ideal_world.trusted.insert(peer_id.clone());
        }
        if !trusted {
            let msg = ErrorResponseNote {
                code: ErrorCode::PermissionDenied,
                file: None,
                message: format!(
                    "Refusing connection from {}: certificate not trusted",
                    peer_id
                ),
            };
            next.send_notif(NotificationCode::ErrorResponse, msg).await;
            let reject_msg = SignalingMsg::Rejected {
                id: peer_id.clone(),
                reason: "Provided certificate not in my trusted list".to_string(),
            };
            let _ = signaling_channel.send(&signaling_addr, reject_msg).await;
            self.current_world
                .mark_remote_failed(&peer_id, "certificate not trusted".to_string());
            return;
        }

        // Declare the peer in ideal_world so fix_world doesn’t tear
        // down the connection we’re about to build, and mark
        // current_world connecting to prevent duplicate attempts.
        let sctp_transport = webchannel::TransportConfig::SCTP {
            signaling_addr: signaling_addr.clone(),
        };
        self.ideal_world.remotes.insert(
            peer_id.clone(),
            RemoteState {
                transport: sctp_transport.clone(),
                status: RemoteStatus::Connected,
                ever_connected: true,
                retry_stride_secs: INITIAL_RETRY_STRIDE_SECS,
            },
        );
        self.current_world
            .set_remote_connecting2(&peer_id, sctp_transport);

        // If the remote initiated the connection, reply with
        // a connect message.
        if initiator {
            self.send_connect_message(
                next,
                peer_id.clone(),
                &signaling_addr,
                signaling_channel,
                false,
            )
            .await;
        }

        // Create a Sock for this peer connection. Sock
        // handles sending receiving SDP and ICE candidate
        // to/from the remote peer through signaling server.
        let sock = match signaling_channel
            .create_sock(&signaling_addr, peer_id.clone())
            .await
        {
            Ok(sock) => sock,
            Err(e) => {
                tracing::warn!(
                    "Failed to create sock for peer {}: {}",
                    id_short(&peer_id),
                    e
                );
                return;
            }
        };

        // Establish WebChannel connection using the sock.
        let key_cert = self.key_cert.clone();
        let webchannel_clone = next.webchannel.clone();
        let peer_id_clone = peer_id.clone();
        let msg_tx_clone = msg_tx.clone();
        let my_host_id = self.host_id.clone();

        tokio::spawn(async move {
            match webchannel_clone
                .connect(
                    peer_id_clone.clone(),
                    webchannel::Transport::Sock(sock),
                    key_cert,
                )
                .await
            {
                Ok(()) => {
                    tracing::info!(
                        "Successfully connected to peer {}",
                        id_short(&peer_id_clone)
                    );
                    // Send Hey to the peer; the existing handler at the
                    // other end fires `mark_connected`. We do this here
                    // (rather than inside the webchannel) so the SSH
                    // transport gets the same handshake.
                    let hey = Msg::Hey {
                        hey: message::HeyMessage {
                            message: "Nice to meet ya".to_string(),
                            credentials: "".to_string(),
                            version: "v1.0.0".to_string(),
                        },
                    };
                    if let Err(err) = webchannel_clone.send(&peer_id_clone, None, hey).await {
                        tracing::warn!(
                            "Failed to send Hey to {}: {}",
                            id_short(&peer_id_clone),
                            err
                        );
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        "Failed to connect to peer {}: {}",
                        id_short(&peer_id_clone),
                        err
                    );
                    let condition: Option<&WebChannelError> = err.downcast_ref();
                    match condition {
                        Some(WebChannelError::ConnectionExists(peer_id)) => {
                            let msg = webchannel::Message {
                                host: my_host_id,
                                body: Msg::IceProgress {
                                    peer: peer_id.to_string(),
                                    message: "Already connected".to_string(),
                                },
                                req_id: None,
                            };
                            let _ = msg_tx_clone.send(msg).await;
                        }
                        _ => {
                            // Send FailedToConnect message to main loop
                            let msg = webchannel::Message {
                                host: my_host_id,
                                body: Msg::FailedToConnect {
                                    peer: peer_id_clone.clone(),
                                    reason: err.to_string(),
                                },
                                req_id: None,
                            };
                            let _ = msg_tx_clone.send(msg).await;
                        }
                    }
                }
            }
        });
    }

    /// Handle AcceptConnection notification from editor. Declares
    /// intent in `ideal_world` and kicks off the bind inline so the
    /// caller’s next request (e.g. Connect) sees the binding underway.
    /// `fix_world` handles subsequent retries on disconnect.
    async fn handle_accept_connection<'a>(
        &mut self,
        next: &Next<'a>,
        params: AcceptConnectionParams,
        signaling_channel: &crate::signaling::client::SignalingChannel,
    ) {
        tracing::info!("AcceptConnection: addr={}", params.addr);

        self.ideal_world.signaling.insert(
            params.addr.clone(),
            SignalingState {
                status: SignalingStatus::Bound,
                ..Default::default()
            },
        );

        // Skip if we’re already attempting or have an active bind.
        if let Some(state) = self.current_world.signaling.get(&params.addr) {
            match &state.status {
                SignalingStatus::Bound | SignalingStatus::Binding { .. } => {
                    if matches!(state.status, SignalingStatus::Bound) {
                        next.send_notif(
                            NotificationCode::AcceptingConnection,
                            serde_json::json!({ "signaling_addr": params.addr }),
                        )
                        .await;
                    }
                    return;
                }
                _ => {}
            }
        }

        // Start binding inline. Pass the current global trust list.
        self.current_world.set_signaling_binding(&params.addr);
        let trusted_vec: Vec<ServerId> = self.ideal_world.trusted.iter().cloned().collect();
        let result = signaling_channel
            .bind(
                params.addr.clone(),
                self.host_id.clone(),
                self.key_cert.clone(),
                trusted_vec,
            )
            .await;
        if let Err(err) = result {
            tracing::warn!("Failed to bind SignalingClient: {}", err);
            self.current_world
                .mark_signaling_failed(&params.addr, err.to_string());
            next.send_notif(
                NotificationCode::ErrorResponse,
                ErrorResponseNote {
                    code: ErrorCode::InternalError,
                    file: None,
                    message: format!("Failed to bind to signaling server: {}", err),
                },
            )
            .await;
        }
    }
}

// *** Helper functions

/// Install SIGINT/SIGTERM handlers and notify `shutdown` when either
/// signal arrives. Caller is responsible for owning the `Arc<Notify>`
/// and for spawning this future inside a tokio runtime context.
pub async fn listen_for_signal(shutdown: Arc<tokio::sync::Notify>) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("Failed to install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT received"),
            _ = term.recv() => tracing::info!("SIGTERM received"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("Ctrl-C received");
    }
    shutdown.notify_waiters();
}

/// The [Next] object stores references to channels and have a borrow
/// lifetime, but the channels don’t really have to be references
/// since they are Send + Sync. I only made [Next] store references
/// since most of the time a reference is enough and I need to create
/// a new [Next] for each handler (because of the `req_id` depends on
/// each message) and I don’t want to clone the channel every time.
struct Next<'a> {
    editor_tx: &'a mpsc::Sender<lsp_server::Message>,
    req_id: Option<lsp_server::RequestId>,
    pub webchannel: &'a WebChannel,
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
        tracing::warn!("Failed to send notification to editor: {}", err);
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
        tracing::warn!("Failed to send response to editor: {}", err);
    }
}

/// Send a message to a remote host via WebChannel with error logging.
async fn send_to_remote(
    webchannel: &WebChannel,
    host_id: &ServerId,
    req_id: Option<lsp_server::RequestId>,
    msg: Msg,
) {
    tracing::info!("Send to {}: {:?}", id_short(host_id), &msg);
    // We don’t let caller handle disconnect errors. Because
    // disconnect error is caught by the receiving end of each
    // connection, and it’ll send a ConnectionBroke message.
    if let Err(err) = webchannel.send(host_id, req_id, msg.clone()).await {
        tracing::warn!(
            "Failed to send to remote host {}: {}, msg={:?}",
            host_id,
            err,
            msg,
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
//
// Mostly ai-generated. Could be improved, but I don’t want to spend
// too much time on this.

/// Convert lsp_server::Request to a human-readable string
pub fn request_to_string(req: &lsp_server::Request) -> String {
    let params_str = if req.method == "SendOps" {
        // Use Debug which truncates op content.
        serde_json::from_value::<SendOpsParams>(req.params.clone())
            .map(|p| format!("{:?}", p))
            .unwrap_or_else(|_| "?".to_string())
    } else {
        serde_json::to_string(&req.params).unwrap_or_else(|_| "?".to_string())
    };
    format!("Req {} {} {}", &req.id, req.method, params_str)
}

/// Convert lsp_server::Response to a human-readable string
pub fn response_to_string(resp: &lsp_server::Response) -> String {
    let result_str = result_to_string(&resp.result);
    let error_str = resp
        .error
        .as_ref()
        .map(|e| format!(" Error({}: {})", e.code, e.message))
        .unwrap_or_default();
    format!("Resp {} {}{}", &resp.id, result_str, error_str)
}

fn result_to_string(result: &Option<serde_json::Value>) -> String {
    // Return null for None.
    let result = match result {
        Some(r) => r,
        None => return "null".to_string(),
    };
    // Return JSON serialization for non-object.
    let obj = match result.as_object() {
        Some(o) => o,
        None => return serde_json::to_string(result).unwrap_or_else(|_| "?".to_string()),
    };
    // Object:
    // OpenFileResp: truncate the content field.
    if obj.contains_key("content") && obj.contains_key("siteId") {
        let mut truncated = obj.clone();
        if let Some(content) = obj.get("content").and_then(|v| v.as_str()) {
            truncated.insert("content".to_string(), truncate_for_log(content, 50).into());
        }
        return serde_json::to_string(&truncated).unwrap_or_else(|_| "?".to_string());
    }
    // SendOpsResp: use Debug which truncates op content.
    if obj.contains_key("ops") && obj.contains_key("lastGlobalSeq") {
        return serde_json::from_value::<SendOpsResp>(result.clone())
            .map(|r| format!("{:?}", r))
            .unwrap_or_else(|_| "?".to_string());
    }
    // Any other object.
    serde_json::to_string(result).unwrap_or_else(|_| "?".to_string())
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

/// Convert webchannel::Message to a human-readable string, truncating large content fields
pub fn remote_message_to_string(msg: &webchannel::Message) -> String {
    let req_id_str = msg
        .req_id
        .as_ref()
        .map(|id| format!(" req_id: {:?}", id))
        .unwrap_or_default();
    format!(
        "Message {{ host: {}, body: {:?}{} }}",
        msg.host, msg.body, req_id_str
    )
}

// *** Tests

#[cfg(test)]
pub mod tests;

#[cfg(test)]
pub mod transcript_tests;
