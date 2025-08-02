use crate::types::*;
use lsp_server::Message;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub enum NotificationCode {
    RemoteOpArrived,
    FileListUpdated,
    SuggestOpenFile,
    ConnectionBroke,
}

impl NotificationCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            NotificationCode::RemoteOpArrived => "remoteOpArrived",
            NotificationCode::FileListUpdated => "fileListUpdated",
            NotificationCode::SuggestOpenFile => "suggestOpenFile",
            NotificationCode::ConnectionBroke => "connectionBroke",
        }
    }

    pub fn as_string(&self) -> String {
        self.as_str().to_string()
    }
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
pub struct ConnectionBrokeParams {
    /// Description of how kind of connection broke and why.
    pub desc: String,
}

// Requests and responses

// *** Init

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitParams {
    pub host_id: ServerId,
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
    pub dir: Option<DocDesc>,
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

// *** SendOpEditor

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendOpEditorParams {
    pub ops: Vec<EditorFatOp>,
    pub doc_id: DocId,
    pub host_id: ServerId,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendOpEditorResp {
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

// *** Functions

pub fn err_msg(message: String) -> Message {
    Message::Notification(lsp_server::Notification {
        method: NotificationCode::ConnectionBroke.as_string(),
        params: serde_json::json!({
            "desc": message,
        }),
    })
}
