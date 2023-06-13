use crate::op::{DocId, Op, SiteId};
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
    pub server: ServerId,
    pub file_name: String,
    pub file: String,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareFileResp {
    pub doc_id: DocId,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendOpParams {
    pub ops: Vec<Op>,
    pub doc: DocId,
    pub server: ServerId,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendOpResp {
    pub ops: Vec<Op>,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListFilesParams {
    pub server: ServerId,
    pub credential: Credential,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListFilesResp {
    pub files: Vec<DocId>,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectToFileParams {
    pub doc: DocId,
    pub server: ServerId,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectToFileResp {
    pub content: String,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteOpArrivedNotification {
    pub doc: DocId,
    pub server: ServerId,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisconnectFromFileParams {
    pub doc: DocId,
    pub server: ServerId,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisconnectFromFileResp {
    pub doc: DocId,
    pub server: ServerId,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StopSharingFileParams {
    pub doc: DocId,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StopSharingFileResp {
    pub doc: DocId,
}
