use enum_dispatch::enum_dispatch;
use serde::{Deserialize, Serialize};

pub type ServerId = String;
pub const SERVER_ID_SELF: &str = "self";
pub const SITE_ID_SELF: &str = "self";
pub type Credential = String;

pub use crate::engine::ContextOps;
pub use crate::op::{DocId, FatOp, GlobalSeq, LocalSeq, Op, SiteId};

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

#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocInfo {
    pub doc_id: DocId,
    pub file_name: String,
}
