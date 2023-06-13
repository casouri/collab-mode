pub type ServerId = String;
pub const SERVER_ID_SELF: &str = "self";
pub const SITE_ID_SELF: &str = "self";
pub type Credential = String;

pub use crate::op::{DocId, GlobalSeq, LocalSeq, Op, SiteId};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DocDesignator {
    pub server: ServerId,
    pub doc: DocId,
}
