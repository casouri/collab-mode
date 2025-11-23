use crate::{config_man::ConfigProject, server, types::*, webchannel::TransportType};
use lsp_server::Message;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// *** Notifications

#[derive(Clone, Copy, fmt_derive::Debug, fmt_derive::Display)]
#[non_exhaustive]
#[allow(dead_code)]
pub enum NotificationCode {
    RemoteOpsArrived,    // Editor should fetch remote ops.
    FileListUpdated,     // Editor should update the file list.
    ConnectionBroke,     // Editor should indicate connection is broken.
    FailedToConnect,     // Editor should show reason for failed connection attempt.
    Connecting,          // Editor should set connection state of the host to "connecting".
    ConnectionProgress,  // Editor should show connection progress.
    AcceptingConnection, // Editor should show that we’re accepting connections.
    AcceptStopped,       // Editor should show that we’re not accepting connections anymore.
    Connected,           // Editor should mark the remote as connected.
    FileMoved,           // A file moved, editor should update it accordingly.
    FileDeleted,         // A file/directory was deleted.
    FileClosed,          // A file is closed.
    InfoReceived,        // Info received from remote for a file.

    UnimportantError, // Editor should log it but not display it.
    InternalError,    // Error that user should see, display it.
    ErrorResponse,    // Async request error, comes with error code.
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[non_exhaustive]
#[repr(i32)]
pub enum ErrorCode {
    // Defined by JSON RPC.
    ParseError = -32700,
    InvalidRequest = -32600,
    MethodNotFound = -32601,
    InvalidParams = -32602,
    InternalError = -32603,
    ServerErrorStart = -32099,
    ServerErrorEnd = -32000,

    /// LSP error. Happens when editor didn’t send the initialize
    /// request before other requests.
    NotInitialized = -32002,

    // Collab-mode errors.
    /// Server is unaffected, but editor must reconnect/recreate the doc.
    DocFatal = 103,
    /// Permission denied.
    PermissionDenied = 104,
    /// IO error, like file not found or permission denied.
    IoError = 105,
    /// Network error, like connection timeout.
    NetworkError = 106,
    /// Wrong arguments like empty filename.
    BadRequest = 113,
    /// Not connected to the remote.
    NotConnected = 114,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteOpsArrivedNote {
    pub file: EditorFileDesc,
    pub last_seq: GlobalSeq,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionBrokeNote {
    pub host_id: ServerId,
    pub reason: String,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorResponseNote {
    pub code: ErrorCode,
    pub file: Option<FileDesc>,
    pub message: String,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostAndMessageNote {
    pub host_id: ServerId,
    pub message: String,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectingNote {
    pub host_id: ServerId,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileClosedNote {
    pub file: EditorFileDesc,
}

// *** Requests and responses

// **** Init

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitResp {
    pub host_id: ServerId,
}

// **** Accept

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AcceptConnectionParams {
    pub signaling_addr: String,
    pub transport_type: TransportType,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StopAcceptingParams {
    pub signaling_addr: String,
}

// **** Update config (accept mode and trusted hosts)

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateConfigParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub add_trusted_hosts: Option<HashMap<ServerId, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remove_trusted_hosts: Option<Vec<ServerId>>,
}

// **** Connect

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectParams {
    pub host_id: ServerId,
    pub signaling_addr: String,
    pub transport_type: TransportType,
}

// **** ShareFile

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareFileParams {
    pub filename: String,
    pub content: String,
    pub meta: JsonMap,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareFileResp {
    pub file: EditorFileDesc,
    pub site_id: SiteId,
}

// **** DeclareProjects

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeclareProjectsParams {
    pub projects: Vec<ConfigProject>,
}

// **** ListProjects and ListFiles

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListProjectsParams {
    pub host_id: ServerId,
    pub signaling_addr: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListFilesParams {
    pub dir: EditorFileDesc,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListFilesResp {
    pub files: Vec<ListFilesEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListFilesEntry {
    pub file: EditorFileDesc,
    // The non-directory filename (buffer/document name in editor).
    // Must match `file` if it's a project file.
    pub filename: String,
    pub is_directory: bool,
    pub meta: JsonMap,
}

// **** OpenFile & CreateFile

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum OpenMode {
    Create,
    Open,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenFileParams {
    pub file: EditorFileDesc,
    pub mode: OpenMode,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenFileResp {
    pub content: String,
    pub site_id: SiteId,
    pub filename: String,
    pub file: EditorFileDesc,
}

// **** MoveFile

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveFileParams {
    pub host_id: ServerId,
    pub project: ProjectId,
    pub old_path: String,
    pub new_path: String,
}

pub type MoveFileResp = MoveFileParams;
pub type FileMovedNotif = MoveFileParams;

// **** SaveFile

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveFileParams {
    pub file: EditorFileDesc,
}

pub type SaveFileResp = SaveFileParams;

// **** DeleteFile & CloseFile

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteFileParams {
    pub file: EditorFileDesc,
}

pub type DeleteFileResp = DeleteFileParams;
pub type FileDeletedNotif = DeleteFileParams;

// **** DisconnectFile

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisconnectFileParams {
    pub file: EditorFileDesc,
}

// **** SendOpEditor

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendOpsParams {
    pub ops: Vec<EditorFatOp>,
    pub file: EditorFileDesc,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendOpsResp {
    pub ops: Vec<EditorLeanOp>,
    pub last_global_seq: GlobalSeq,
    pub pending_local_ops: usize,
    pub doc_len: u64,
}

// **** SendInfo

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendInfoParams {
    pub file: EditorFileDesc,
    pub info: JsonMap,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendInfoNote {
    pub file: EditorFileDesc,
    pub info: serde_json::Value,
    pub sender: ServerId,
}

// **** Undo

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
pub enum UndoKind {
    Undo,
    Redo,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UndoParams {
    pub file: EditorFileDesc,
    pub kind: UndoKind,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UndoResp {
    pub ops: Vec<EditInstruction>,
    /// Context in which the ops were generated. It’s the total
    /// history length at the time of generation. Editor must send
    /// back the undo ops under the same context or else an error is
    /// thrown. (Basically, don’t apply any other ops between getting
    /// the undo op and sending the undo ops back.)
    pub context: u64,
}

// **** ConnectionState

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionStateEntry {
    pub host_id: ServerId,
    pub state: server::ConnectionState,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveDocEntry {
    pub file: EditorFileDesc,
    pub subscribers: Vec<ServerId>,
    pub filename: String,
    pub meta: JsonMap,
    pub seq: GlobalSeq,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectedDocEntry {
    pub file: EditorFileDesc,
    pub filename: String,
    pub meta: JsonMap,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionStateResp {
    /// Connected remotes.
    pub connections: Vec<ConnectionStateEntry>,
    /// Signal addresses that we're accepting connection on. Usually
    /// just one.
    pub accepting: Vec<String>,
    /// Live documents hosted by this server.
    pub live: Vec<LiveDocEntry>,
    /// Connected documents from remote servers.
    pub connected: Vec<ConnectedDocEntry>,
    /// Local projects.
    pub projects: Vec<ConfigProject>,
}

// **** Etc

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrintHistoryParams {
    pub file: EditorFileDesc,
    pub debug: bool,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrintHistoryResp {
    pub history: String,
}

// *** Peer messages

#[derive(Debug, Clone, Serialize, Deserialize, fmt_derive::Display)]
#[non_exhaustive]
pub struct HeyMessage {
    pub message: String,
    pub credentials: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, fmt_derive::Display)]
#[non_exhaustive]
pub enum Msg {
    ShareSingleFile {
        filename: String,
        meta: String,
        content: FileContentOrPath,
    },
    FileShared(DocId),
    ListFiles {
        dir: Option<FileDesc>,
    },
    FileList(Vec<ListFilesEntry>),
    OpFromClient(ContextOps),
    OpFromServer {
        doc: DocId,
        ops: Vec<FatOp>,
    },
    RequestOps {
        doc: DocId,
        after: GlobalSeq,
    },
    RequestFile(FileDesc, OpenMode),
    MoveFile {
        project: ProjectId,
        source: String,
        dest: String,
    },
    FileMoved {
        project: ProjectId,
        source: String,
        dest: String,
    },
    SaveFile(DocId),
    FileSaved(DocId),
    CloseFile {
        file: FileDesc,
        delete: bool,
    },
    FileDeleted(FileDesc),
    FileClosed(FileDesc),
    Snapshot(NewSnapshot),
    ResetFile(DocId),
    Login(Credential),
    LoggedYouIn(SiteId),
    Info(Info),
    InfoFromClient(Info),
    InfoFromServer(Info),

    // Misc
    IceProgress(ServerId, String),
    Hey(HeyMessage),

    // Errors
    FailedToConnect(ServerId, String),
    ConnectionBroke(ServerId),
    StopSendingOps(DocId),
    SerializationErr(String),
    PermissionDenied(String),
    BadRequest(String),
    // Fatal error that shouldn't happen (not a fault, ie, a bug in
    // code), must reset the doc.
    DocFatal(DocId, String),
    // If we need to respond to a request from a remote's editor with
    // an error, use this message.
    ErrorResp {
        code: ErrorCode,
        file: Option<FileDesc>,
        message: String,
    },
}

// *** Functions

pub fn err_msg(message: String) -> Message {
    Message::Notification(lsp_server::Notification {
        method: NotificationCode::ConnectionBroke.to_string(),
        params: serde_json::json!({
            "desc": message,
        }),
    })
}
