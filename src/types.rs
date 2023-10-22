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
pub type FatOpUnprocessed = crate::op::FatOp<EditorOp>;

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
pub enum EditorOp {
    Ins(u64, String),
    Del(u64, String),
    Undo(EditInstruction),
    Redo(EditInstruction),
}

impl EditorOp {
    pub fn kind(&self) -> EditorOpKind {
        match self {
            EditorOp::Ins(_, _) => EditorOpKind::Original,
            EditorOp::Del(_, _) => EditorOpKind::Original,
            EditorOp::Undo(_) => EditorOpKind::Undo,
            EditorOp::Redo(_) => EditorOpKind::Undo,
        }
    }
}

impl Into<EditInstruction> for EditorOp {
    fn into(self) -> EditInstruction {
        match self {
            Self::Ins(pos, text) => EditInstruction::Ins(vec![(pos, text)]),
            Self::Del(pos, text) => EditInstruction::Del(vec![(pos, text)]),
            Self::Undo(instr) => instr,
            Self::Redo(instr) => instr,
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

// Dummy implementation.
impl crate::op::Operation for EditorOp {
    fn transform(&self, base: &Self, self_site: &SiteId, base_site: &SiteId) -> Self {
        panic!()
    }
    fn inverse(&mut self) {
        panic!()
    }
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
#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocInfo {
    pub doc_id: DocId,
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
    ShareFile { file_name: String, content: String },
    ListFiles,
    SendOp(ContextOps),
    RecvOpAndInfo { doc_id: DocId, after: GlobalSeq },
    RequestFile(DocId),
    DeleteFile(DocId),
    Login(Credential),
    SendInfo { doc_id: DocId, info: String },
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
