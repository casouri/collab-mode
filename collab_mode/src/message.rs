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

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitParams {
    pub peer_id: ServerId,
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

pub fn err_msg(message: String) -> Message {
    Message::Notification(lsp_server::Notification {
        method: NotificationCode::ConnectionBroke.as_string(),
        params: serde_json::json!({
            "desc": message,
        }),
    })
}
