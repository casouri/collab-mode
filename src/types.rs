//! This module re-exports some common types for convenience, and
//! define a couple more.

use crate::error::CollabError;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub use crate::engine::{ClientEngine, ContextOps, ServerEngine};
pub use crate::op::{DocId, GlobalSeq, GroupSeq, LocalSeq, Op, OpKind, SiteId};

pub type ServerId = String;
pub const SERVER_ID_SELF: &str = "self";
pub type Credential = String;
pub type FilePath = (DocId, PathBuf);

pub type FatOp = crate::op::FatOp<Op>;

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
pub enum EditorOp {
    Ins(u64, String),
    Del(u64, String),
    Undo,
    Redo,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum DocFile {
    /// A shared doc.
    Doc(DocId),
    /// A file on disk, not yet shared.
    File(FilePath),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum FileContentOrPath {
    /// File content.
    Content(String),
    /// File path.
    Path(PathBuf),
}

impl EditorOp {
    pub fn kind(&self) -> EditorOpKind {
        match self {
            EditorOp::Ins(_, _) => EditorOpKind::Original,
            EditorOp::Del(_, _) => EditorOpKind::Original,
            EditorOp::Undo => EditorOpKind::Undo,
            EditorOp::Redo => EditorOpKind::Redo,
        }
    }
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
pub enum EditInstruction {
    Ins(Vec<(u64, String)>),
    Del(Vec<(u64, String)>),
}

#[derive(Debug, Eq, PartialEq, Clone, Copy, Deserialize, Serialize)]
pub enum EditorOpKind {
    Original,
    Undo,
    Redo,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorFatOp {
    pub op: EditorOp,
    pub group_seq: GroupSeq,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorLeanOp {
    pub op: EditInstruction,
    pub site_id: SiteId,
}

/// Basically (Doc, Server). Uniquely identifies a document.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DocDesignator {
    pub server: ServerId,
    pub doc: DocId,
}

impl DocDesignator {
    pub fn new(doc_id: &DocId, server_id: &ServerId) -> DocDesignator {
        DocDesignator {
            doc: doc_id.clone(),
            server: server_id.clone(),
        }
    }
}

/// File name plus doc id for a document. We might later add more meta
/// info to a doc.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocInfo {
    pub doc: DocFile,
    pub file_name: String,
}

/// Meaning: editor needs to keep trying to get ops until they got
/// every op before and including `last_seq`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewOpNotification {
    pub doc_id: DocId,
    pub server_id: ServerId,
    pub last_seq: GlobalSeq,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InfoNotification {
    pub doc_id: DocId,
    pub server_id: ServerId,
    pub value: serde_json::Value,
}

#[derive(Debug, Clone)]
pub enum CollabNotification {
    Op(NewOpNotification),
    Info(InfoNotification),
}

/// A snapshot of a document. Returned by the server when a site
/// requests a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    /// The file content.
    pub buffer: String,
    /// File name of the doc.
    pub file_name: String,
    /// Sequence number of the last op.
    pub seq: GlobalSeq,
}

/// Requests sent to webrpc server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DocServerReq {
    ShareFile {
        file_name: String,
        content: FileContentOrPath,
    },
    ListFiles {
        dir_path: Option<FilePath>,
    },
    SendOp(ContextOps),
    RecvOpAndInfo {
        doc_id: DocId,
        after: GlobalSeq,
    },
    RequestFile(DocId),
    DeleteFile(DocId),
    Login(Credential),
    SendInfo {
        doc_id: DocId,
        info: String,
    },
}

/// Reponses sent to webrpc client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DocServerResp {
    ShareFile(DocId),
    ListFiles(Vec<DocInfo>),
    SendOp,
    RecvOp(Vec<FatOp>),
    RequestFile(Snapshot),
    DeleteFile,
    Login(SiteId),
    SendInfo,
    RecvInfo(String),
    Err(CollabError),
}

// impl From<FatOp> for EditorLeanOp {
//     fn from(value: FatOp) -> Self {
//         EditorLeanOp {
//             op: value.op.into(),
//             site_id: value.site,
//         }
//     }
// }
