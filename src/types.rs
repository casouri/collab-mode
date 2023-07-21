//! This module re-exports some common types for convenience, and
//! define a couple more.

use serde::{Deserialize, Serialize};

pub type ServerId = String;
pub const SERVER_ID_SELF: &str = "self";
pub const SITE_ID_SELF: &str = "self";
pub type Credential = String;

pub use crate::engine::{ClientEngine, ContextOps, ServerEngine};
pub use crate::op::{DocId, GlobalSeq, GroupSeq, LocalSeq, Op, OpKind, SiteId};

pub type FatOp = crate::op::FatOp<Op>;
pub type LeanOp = crate::op::LeanOp<Op>;

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupedOp {
    pub op: Op,
    // Allow the field to be absent.
    #[serde(default)]
    pub group_seq: Option<GroupSeq>,
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
