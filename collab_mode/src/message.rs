use crate::{
    config_man::{AcceptMode, ConfigProject},
    types::*,
    webchannel::TransportType,
};
use lsp_server::Message;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// *** Notifications

#[derive(Clone, Copy, fmt_derive::Debug, fmt_derive::Display)]
#[non_exhaustive]
pub enum NotificationCode {
    RemoteOpsArrived,    // Editor should fetch remote ops.
    FileListUpdated,     // Editor should update the file list.
    SuggestOpenFile,     // Editor should suggest to open a file.
    ConnectionBroke,     // Editor should indicate connection is broken.
    Connecting,          // Editor should set connection state of the host to "connecting".
    ConnectionProgress,  // Editor should show connection progress.
    AcceptingConnection, // Editor should show that we're accepting connections.
    AcceptStopped,       // Editor should show that we're not accepting connections anymore.
    Connected,           // Editor should mark the remote as connected.
    FileMoved,           // A file moved, editor should update it accordingly.
    FileDeleted,         // A file/directory was deleted.

    UnimportantError, // Editor should log it but not display it.
    InternalError,    // Error that user should see, display it.
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[non_exhaustive]
pub enum ErrorCode {
    // Defined by JSON RPC.
    ParseError = -32700,
    InvalidRequest = -32600,
    MethodNotFound = -32601,
    InvalidParams = -32602,
    InternalError = -32603,
    ServerErrorStart = -32099,
    ServerErrorEnd = -32000,

    /// LSP error. Happens when editor didn't send the initialize
    /// request before other requests.
    NotInitialized = -32002,

    // Collab-mode errors.
    /// Server is unaffected, but editor must reconnect/recreate the doc.
    DocFatal = 103,
    /// Permission denied.
    PermissionDenied = 104,
    /// IO error, like file not found or permission denied.
    IoError = 105,
    /// Wrong arguments like empty filename.
    BadRequest = 113,
    /// Not connected to the remote.
    NotConnected = 114,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SuggestOpenFileParams {
    /// Absolute path to the file that the peer suggests to open.
    pub filename: String,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
/// Mostly for testing, since server auto-connect on other operations.
pub struct HeyParams {
    pub host_id: ServerId,
    pub message: String,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteOpsArrivedNote {
    // Technically don’t need host_id but I don’t want to change it
    // anymore.
    pub host_id: ServerId,
    pub doc_id: DocId,
    pub file: EditorFileDesc,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionBrokeNote {
    pub host_id: ServerId,
    pub reason: String,
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

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionBrokeParams {
    /// Description of how kind of connection broke and why.
    pub desc: String,
}

// **** Update config (accept mode and trusted hosts)

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateConfigParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accept_mode: Option<AcceptMode>,
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
    pub doc_id: DocId,
    pub site_id: SiteId,
}

// **** DeclareProjects

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeclareProjectsParams {
    pub projects: Vec<ConfigProject>,
}

// **** ListFiles

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
    pub signaling_addr: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListFilesResp {
    pub files: Vec<ListFileEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListFileEntry {
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
    pub file_desc: EditorFileDesc,
    pub mode: OpenMode,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenFileResp {
    pub content: String,
    pub site_id: SiteId,
    pub filename: String,
    pub doc_id: DocId,
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
    pub host_id: ServerId,
    pub doc_id: DocId,
}

pub type SaveFileResp = SaveFileParams;

// **** DeleteFile

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
    pub host_id: ServerId,
    pub doc_id: DocId,
}

// **** SendOpEditor

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendOpsParams {
    pub ops: Vec<EditorFatOp>,
    pub doc_id: DocId,
    pub host_id: ServerId,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendOpsResp {
    pub ops: Vec<EditorLeanOp>,
    pub last_seq: GlobalSeq,
}

// **** SendInfo

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendInfoParams {
    pub info: JsonMap,
    pub doc_id: DocId,
    pub host_id: ServerId,
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
    pub doc_id: DocId,
    pub host_id: ServerId,
    pub kind: UndoKind,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UndoResp {
    pub ops: Vec<EditInstruction>,
}

// **** Etc

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrintHistoryParams {
    pub doc_id: DocId,
    pub host_id: ServerId,
    pub debug: bool,
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
    FileList(Vec<ListFileEntry>),
    OpFromClient(ContextOps),
    OpFromServer(Vec<FatOp>),
    RequestOps {
        doc: DocId,
        after: GlobalSeq,
    },
    RequestFile(FileDesc, OpenMode),
    MoveFile(ProjectId, String, String),
    FileMoved(ProjectId, String, String),
    SaveFile(DocId),
    FileSaved(DocId),
    DeleteFile(FileDesc),
    FileDeleted(FileDesc),
    Snapshot(NewSnapshot),
    ResetFile(DocId),
    Login(Credential),
    LoggedYouIn(SiteId),
    Info(DocId, Info),

    // Misc
    IceProgress(String),
    Hey(HeyMessage),

    // Errors
    ConnectionBroke(ServerId),
    StopSendingOps(DocId),
    SerializationErr(String),
    PermissionDenied(String),
    BadRequest(String),
    // Fatal error that shouldn’t happen (not a fault, ie, a bug in
    // code), must reset the doc.
    DocFatal(DocId, String),
    // If we need to respond to a request from a remote’s editor with
    // an error, use this message.
    ErrorResp(ErrorCode, String),
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
