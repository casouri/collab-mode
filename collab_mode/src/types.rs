//! This module re-exports some common types for convenience, and
//! define a couple more.

use crate::error::CollabError;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub use crate::config_man::{ArcKeyCert, KeyCert};
pub use crate::engine::{ClientEngine, ContextOps, ServerEngine};
pub use crate::op::{
    replace_whitespace_char, DocId, GlobalSeq, GroupSeq, LocalSeq, Op, OpKind, SiteId,
};

pub type ServerId = String;
pub const SERVER_ID_SELF: &str = "self";
pub type Credential = String;
/// A file in a directory that’s not yet a doc. The path is a relative
/// path. We don’t want to use full path since some people might
/// consider the full path sensitive?
pub type FilePath = (DocId, PathBuf);
/// Project’s id is a descriptive name.
pub type ProjectId = String;

pub type FatOp = crate::op::FatOp<Op>;

pub type JsonMap = serde_json::Map<String, serde_json::Value>;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Info {
    pub sender: SiteId,
    pub value: String,
}

#[derive(Eq, PartialEq, Clone, Deserialize, Serialize)]
pub enum EditorOp {
    Ins(u64, String),
    Del(u64, String),
    Undo,
    Redo,
}

impl std::fmt::Debug for EditorOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Undo => write!(f, "undo"),
            Self::Redo => write!(f, "redo"),
            Self::Ins(pos, content) => {
                let content = replace_whitespace_char(content.to_string());
                write!(f, "ins({pos}, {content})")
            }
            Self::Del(pos, content) => {
                let content = replace_whitespace_char(content.to_string());
                write!(f, "del({pos}, {content})")
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum DocDesc {
    /// A shared doc.
    Doc(DocId),
    /// A file on disk, not yet shared.
    File(FilePath),
    /// A subdir on disk under a shared dir.
    Dir(FilePath),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum FileDesc {
    /// A shared doc that isn’t in a project. `id` should be the
    /// absolute filename for disk files, and buffer name for buffers.
    File { id: String },
    /// A project.
    Project { id: ProjectId },
    /// A file in a project.
    ProjectFile {
        project: ProjectId,
        /// Relative path to the file in the project.
        file: String,
    },
}

impl PartialEq for FileDesc {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (FileDesc::File { id: id1 }, FileDesc::File { id: id2 }) => id1 == id2,
            (FileDesc::Project { id: id1 }, FileDesc::Project { id: id2 }) => {
                // Normalize and compare project paths
                std::path::Path::new(id1)
                    .canonicalize()
                    .unwrap_or_else(|_| std::path::PathBuf::from(id1))
                    == std::path::Path::new(id2)
                        .canonicalize()
                        .unwrap_or_else(|_| std::path::PathBuf::from(id2))
            }
            (
                FileDesc::ProjectFile {
                    project: proj1,
                    file: path1,
                },
                FileDesc::ProjectFile {
                    project: proj2,
                    file: path2,
                },
            ) => {
                // Compare normalized full paths
                let full_path1 = std::path::Path::new(proj1).join(path1);
                let full_path2 = std::path::Path::new(proj2).join(path2);
                full_path1.canonicalize().unwrap_or(full_path1.clone())
                    == full_path2.canonicalize().unwrap_or(full_path2.clone())
            }
            _ => false,
        }
    }
}

impl Eq for FileDesc {}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum EditorFileDesc {
    /// A shared doc that isn't in a project.
    File {
        #[serde(rename = "hostId")]
        host_id: ServerId,
        id: String,
    },
    /// A project.
    Project {
        #[serde(rename = "hostId")]
        host_id: ServerId,
        id: ProjectId,
    },
    /// A file in a project.
    ProjectFile {
        #[serde(rename = "hostId")]
        host_id: ServerId,
        project: ProjectId,
        /// Relative path to the file in the project.
        file: String,
    },
}

impl EditorFileDesc {
    /// Create an EditorFileDesc from a FileDesc and a host_id
    pub fn new(file_desc: FileDesc, host_id: ServerId) -> Self {
        match file_desc {
            FileDesc::File { id } => EditorFileDesc::File { host_id, id },
            FileDesc::Project { id } => EditorFileDesc::Project { host_id, id },
            FileDesc::ProjectFile { project, file } => EditorFileDesc::ProjectFile {
                host_id,
                project,
                file,
            },
        }
    }

    /// Get the host_id from this EditorFileDesc
    pub fn host_id(&self) -> &ServerId {
        match self {
            EditorFileDesc::File { host_id, .. } => host_id,
            EditorFileDesc::Project { host_id, .. } => host_id,
            EditorFileDesc::ProjectFile { host_id, .. } => host_id,
        }
    }
}

impl From<EditorFileDesc> for FileDesc {
    fn from(editor_file: EditorFileDesc) -> Self {
        match editor_file {
            EditorFileDesc::File { id, .. } => FileDesc::File { id },
            EditorFileDesc::Project { id, .. } => FileDesc::Project { id },
            EditorFileDesc::ProjectFile { project, file, .. } => {
                FileDesc::ProjectFile { project, file }
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum FileContentOrPath {
    /// File content.
    Content(String),
    /// File path.
    Path(PathBuf),
}

/// A shared directory, basically. Remote user can freely browse and
/// open files under a shared project.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    /// Name of the project, default to the base name of `root`.
    pub name: String,
    /// Absolute filename of the root of the project.
    pub root: String,
    /// Canonicalized absolute root for stable comparisons.
    pub root_canon: String,
    /// Metadata for the project. A serialized JSON object.
    pub meta: JsonMap,
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

/// File name plus doc id for a document.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocInfo {
    pub doc_desc: DocDesc,
    pub file_name: String,
    pub file_meta: JsonMap,
}

/// DocInfo with JsonMap encoded in string.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcDocInfo {
    pub doc_desc: DocDesc,
    pub file_name: String,
    pub file_meta: String,
}

impl From<DocInfo> for RpcDocInfo {
    fn from(value: DocInfo) -> Self {
        RpcDocInfo {
            doc_desc: value.doc_desc,
            file_name: value.file_name,
            file_meta: serde_json::to_string(&value.file_meta).unwrap(),
        }
    }
}

impl From<RpcDocInfo> for DocInfo {
    fn from(value: RpcDocInfo) -> Self {
        DocInfo {
            doc_desc: value.doc_desc,
            file_name: value.file_name,
            file_meta: serde_json::from_str(&value.file_meta).unwrap(),
        }
    }
}

/// Meaning: editor needs to keep trying to get ops until they got
/// every op before and including `last_seq`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewOpNotification {
    pub doc_id: DocId,
    pub host_id: ServerId,
    pub last_seq: GlobalSeq,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InfoNotification {
    pub doc_id: DocId,
    pub host_id: ServerId,
    pub sender_site_id: SiteId,
    pub value: JsonMap,
}

#[derive(Debug, Clone)]
pub enum CollabNotification {
    Op(NewOpNotification),
    Info(InfoNotification),
    HarmlessErr(String),
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
    /// Doc id for this doc, useful for requesting a (dir_id,
    /// rel_path).
    pub doc_id: DocId,
}

/// A snapshot of a document. Returned by the server when a site
/// requests a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewSnapshot {
    /// The file content.
    pub content: String,
    /// File name of the doc.
    pub name: String,
    /// Sequence number of the last op.
    pub seq: GlobalSeq,
    /// Assigned site id for the receiver.
    pub site_id: SiteId,
    /// File desc used by the original RequestFile request.
    pub file_desc: FileDesc,
    /// Doc id on the remote server.
    pub doc_id: DocId,
}

/// Requests sent to webrpc server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DocServerReq {
    ShareFile {
        file_name: String,
        file_meta: String,
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
    RequestFile(DocDesc),
    DeleteFile(DocId),
    Login(Credential),
    SendInfo {
        doc_id: DocId,
        info: Info,
    },
}

/// Reponses sent to webrpc client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DocServerResp {
    ShareFile(DocId),
    ListFiles(Vec<RpcDocInfo>),
    SendOp,
    RecvOp(Vec<FatOp>),
    RequestFile(Snapshot),
    DeleteFile,
    Login(SiteId),
    SendInfo,
    RecvInfo(Info),
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

pub fn empty_json_map() -> JsonMap {
    serde_json::Map::new()
}
