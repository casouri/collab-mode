use crate::types::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub enum ErrorCode {
    // Defined by JSON RPC:
    ParseError = -32700,
    InvalidRequest = -32600,
    MethodNotFound = -32601,
    InvalidParams = -32602,
    InternalError = -32603,
    ServerErrorStart = -32099,
    ServerErrorEnd = -32000,

    // Collab-mode errors.
    //
    /// Transient error, retry again later.
    NetworkError = 100,
    /// Server fatal error, restart the server.
    ServerFatalError = 102,
    /// Server can keep running but if the operation is concerned with
    /// a doc, the doc must be reconnected.
    ServerNonFatalDocFatal = 103,

    /// Permission denied.
    PermissionDenied = 104,
    /// Doc not found.
    DocNotFound = 105,
    /// Doc already exists.
    DocAlreadyExists = 106,
    /// Local IO error.
    IOError = 107,
    /// Not a regular file
    NotRegularFile = 108,
    /// Not a directory
    NotDirectory = 109,
    /// Unsupported operation like sharing directory to remote host.
    UnsupportedOperation = 110,
}

#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub enum NotificationCode {
    RemoteOpArrived,
    ServerError,
    SignalingTimesUp,
    AcceptConnectionErr,
    Info,
}

impl Into<String> for NotificationCode {
    fn into(self) -> String {
        match self {
            NotificationCode::RemoteOpArrived => "RemoteOpArrived".to_string(),
            NotificationCode::ServerError => "ServerError".to_string(),
            NotificationCode::SignalingTimesUp => "SignalingTimesUp".to_string(),
            NotificationCode::AcceptConnectionErr => "AcceptConnectionErr".to_string(),
            NotificationCode::Info => "Info".to_string(),
        }
    }
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloParams {
    pub site_id: SiteId,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareFileParams {
    pub host_id: ServerId,
    pub file_name: String,
    pub content: String,
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
    pub info: serde_json::Value,
    pub doc_id: DocId,
    pub host_id: ServerId,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UndoResp {
    pub ops: Vec<EditInstruction>,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListFilesParams {
    pub doc_id: Option<DocId>,
    pub path: Option<String>,
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
    pub file: DocFile,
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
    pub server_id: ServerId,
    pub debug: bool,
}
