//! This module re-exports some common types for convenience, and
//! define a couple more.

use serde::{Deserialize, Serialize};

pub type ServerId = String;
pub const SERVER_ID_SELF: &str = "self";
pub type Credential = String;

pub use crate::engine::{ClientEngine, ContextOps, ServerEngine};
use crate::error::CollabError;
pub use crate::op::{DocId, GlobalSeq, GroupSeq, LocalSeq, Op, OpKind, SiteId};

pub type FatOp = crate::op::FatOp<Op>;

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
pub enum EditorOp {
    Ins((u64, String)),
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
    pub kind: EditorOpKind,
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
#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocInfo {
    pub doc_id: DocId,
    pub file_name: String,
}

/// A snapshot of a document. Returned by the server when a site
/// requests a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    /// The file content.
    pub buffer: String,
    /// Sequence number of the last op.
    pub seq: GlobalSeq,
}

/// Requests sent to webrpc server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DocServerReq {
    ShareFile { file_name: String, content: String },
    ListFiles,
    SendOp(ContextOps),
    RecvOp { doc_id: DocId, after: GlobalSeq },
    RequestFile(DocId),
    DeleteFile(DocId),
    Login(Credential),
}

/// Reponses sent to webrpc client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DocServerResp {
    ShareFile(DocId),
    ListFiles(Vec<DocInfo>),
    SendOp,
    RecvOp(FatOp),
    RequestFile(Snapshot),
    DeleteFile,
    Login(SiteId),
    Err(CollabError),
}

impl Into<Op> for EditorOp {
    fn into(self) -> Op {
        match self {
            Self::Ins(op) => Op::Ins(op),
            Self::Del(ops) => Op::Del(ops, vec![]),
        }
    }
}

impl From<Op> for EditorOp {
    fn from(value: Op) -> Self {
        match value {
            Op::Ins(op) => Self::Ins(op),
            Op::Del(ops, _) => Self::Del(ops),
        }
    }
}
