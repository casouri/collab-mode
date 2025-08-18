use crate::{types::*, webchannel::TransportType};
use lsp_server::Message;
use serde::{Deserialize, Serialize};

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
    Hey,                 // A ping from a remote, editor should mark the remote as connected.

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
}

// Non-simple notifications

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
pub struct RemoteOpsArrivedParams {
    pub host_id: ServerId,
    pub doc_id: DocId,
}

// Requests and responses

// *** Init

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitParams {
    pub host_id: ServerId,
}

// *** Accept

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AcceptConnectionParams {
    pub host_id: ServerId,
    pub signaling_addr: String,
    pub transport_type: TransportType,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionBrokeParams {
    /// Description of how kind of connection broke and why.
    pub desc: String,
}

// *** Connect

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectParams {
    pub host_id: ServerId,
    pub signaling_addr: String,
    pub transport_type: TransportType,
}

// *** ShareFile

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

// *** DeclareProjects

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeclareProjectsParams {
    pub projects: Vec<DeclareProjectEntry>,
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

// *** ListFiles

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListFilesParams {
    pub dir: Option<FileDesc>,
    pub host_id: ServerId,
    pub signaling_addr: String,
    pub credential: Credential,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListFilesResp {
    pub files: Vec<ListFileEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListFileEntry {
    pub file: FileDesc,
    // The non-directory filename (buffer/document name in editor).
    pub filename: String,
    pub is_directory: bool,
    pub meta: JsonMap,
}

// *** OpenFile

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenFileParams {
    pub host_id: ServerId,
    pub file_desc: FileDesc,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenFileResp {
    pub content: String,
    pub site_id: SiteId,
    pub filename: String,
    pub doc_id: DocId,
}

// *** WriteFile

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteFileParams {
    pub host_id: ServerId,
    pub file_desc: FileDesc,
}

// *** SendOpEditor

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

// *** SendInfo

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendInfoParams {
    pub info: JsonMap,
    pub doc_id: DocId,
    pub host_id: ServerId,
}

// *** Undo

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

// *** Etc

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrintHistoryParams {
    pub doc_id: DocId,
    pub host_id: ServerId,
    pub debug: bool,
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
