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
    ConnectionBroke = -31000,
    OTEngineError = -31001,
    PermissionDenied = -31002,
    DocNotFound = -31003,
    DocAlreadyExists = -31004,
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

// #[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize, PartialOrd, Ord, Hash)]
// pub enum ServerId {
//     Local,
//     Url(String),
// }

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareFileParams {
    pub server_id: ServerId,
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
pub struct SendOpParams {
    pub ops: Vec<EditorFatOp>,
    pub doc_id: DocId,
    pub server_id: ServerId,
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
    pub server_id: ServerId,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UndoResp {
    pub ops: Vec<EditInstruction>,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListFilesParams {
    pub server_id: ServerId,
    pub signaling_addr: String,
    pub credential: Credential,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListFilesResp {
    pub files: Vec<DocInfo>,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocIdParams {
    pub doc_id: DocId,
    pub server_id: ServerId,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectToFileResp {
    pub content: String,
    pub site_id: SiteId,
    pub file_name: String,
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
    pub server_id: ServerId,
    pub kind: UndoKind,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AcceptConnectionParams {
    pub server_id: ServerId,
    pub signaling_addr: String,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrintHistoryParams {
    pub doc_id: DocId,
    pub server_id: ServerId,
    pub debug: bool,
}
