use crate::types::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub enum ErrorCode {
    // Defined by JSON RPC. Generally means editor has programming
    // error and sent a malformed request.
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
    //
    /// Doc non-fatal. Transient error, will retry again later, editor
    /// doesn't need to do anything (except for informing the user).
    DocNonFatal = 100,
    /// Server fatal error, editor must restart the server.
    ServerFatal = 102,
    /// Server is unaffected, but editor must reconnect/recreate the doc.
    DocFatal = 103,

    /// For errors below, editor must retry the operation or give up.

    /// Permission denied.
    PermissionDenied = 104,
    /// Doc not found.
    DocNotFound = 105,
    /// Doc already exists.
    DocAlreadyExists = 106,
    /// Local IO error.
    // IOError = 107,
    /// Not a regular file
    NotRegularFile = 108,
    /// Not a directory
    NotDirectory = 109,
    /// Unsupported operation like sharing directory to remote host.
    UnsupportedOperation = 110,
    /// Can't auto-save a file.
    ErrAutoSave = 111,
    /// Can't find this file on disk.
    FileNotFound = 112,
    /// Wrong arguments like empty filename.
    BadRequest = 113,
}

#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub enum NotificationCode {
    RemoteOpArrived,
    ServerError,
    SignalingTimesUp,
    AcceptConnectionErr,
    Info,
    HarmlessErr,
    AutoSaveErr,
}

impl Into<String> for NotificationCode {
    fn into(self) -> String {
        match self {
            NotificationCode::RemoteOpArrived => "RemoteOpArrived".to_string(),
            NotificationCode::ServerError => "ServerError".to_string(),
            NotificationCode::SignalingTimesUp => "SignalingTimesUp".to_string(),
            NotificationCode::AcceptConnectionErr => "AcceptConnectionErr".to_string(),
            NotificationCode::Info => "Info".to_string(),
            NotificationCode::HarmlessErr => "HarmlessErr".to_string(),
            NotificationCode::AutoSaveErr => "AutoSaveErr".to_string(),
        }
    }
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoSaveErrParams {
    pub doc_id: DocId,
    pub msg: String,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitParams {
    pub host_id: ServerId,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareFileParams {
    pub host_id: ServerId,
    pub file_name: String,
    pub content: String,
    pub file_meta: JsonMap,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareFileResp {
    pub doc_id: DocId,
    pub site_id: SiteId,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareDirParams {
    pub host_id: ServerId,
    pub dir_name: String,
    pub dir_meta: JsonMap,
    pub path: String,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareDirResp {
    pub doc_id: DocId,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendOpParams {
    pub ops: Vec<EditorFatOp>,
    pub doc_id: DocId,
    pub host_id: ServerId,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendOpResp {
    pub ops: Vec<EditorLeanOp>,
    pub last_seq: GlobalSeq,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendInfoParams {
    pub info: JsonMap,
    pub doc_id: DocId,
    pub host_id: ServerId,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UndoResp {
    pub ops: Vec<EditInstruction>,
}

/// If both `doc_id` and `path` are None, list top-level docs and
/// directories. If both are non-null, list files inside that
/// particular directory.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListFilesParams {
    pub dir: Option<DocDesc>,
    pub host_id: ServerId,
    pub signaling_addr: String,
    pub credential: Credential,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListFilesResp {
    pub files: Vec<DocInfo>,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocIdParams {
    pub doc_id: DocId,
    pub host_id: ServerId,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectToFileParams {
    pub host_id: ServerId,
    pub doc_desc: DocDesc,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectToFileResp {
    pub content: String,
    pub site_id: SiteId,
    pub file_name: String,
    pub doc_id: DocId,
}

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
pub struct AcceptConnectionParams {
    pub host_id: ServerId,
    pub signaling_addr: String,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrintHistoryParams {
    pub doc_id: DocId,
    pub host_id: ServerId,
    pub debug: bool,
}
