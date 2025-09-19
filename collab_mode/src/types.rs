//! This module re-exports some common types for convenience, and
//! define a couple more.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;

pub use crate::config_man::{ArcKeyCert, KeyCert};
pub use crate::engine::ContextOps;
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

// Reserved virtual projects used to represent standalone buffers/files.
// - `_buffers`: in-memory buffers (non-absolute identifiers)
// - `_files`:   standalone files referenced by absolute paths
pub const RESERVED_BUFFERS_PROJECT: &str = "_buffers";
pub const RESERVED_FILES_PROJECT: &str = "_files";

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Info {
    pub doc_id: DocId,
    pub sender: ServerId,
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

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum FileDesc {
    /// A project.
    Project { id: ProjectId },
    /// A file in a project.
    ProjectFile {
        project: ProjectId,
        /// Relative path to the file in the project.
        file: String,
    },
}

impl fmt::Display for FileDesc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FileDesc::Project { id } => write!(f, "{}", id),
            FileDesc::ProjectFile { project, file } => write!(f, "{}/{}", project, file),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub struct EditorFileDesc {
    #[serde(rename = "hostId")]
    pub host_id: ServerId,
    pub project: ProjectId,
    /// Relative path to the file in the project. Empty string means it's a project directory.
    pub file: String,
}

impl fmt::Display for EditorFileDesc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.file.is_empty() {
            write!(f, "{}/{}", self.host_id, self.project)
        } else {
            write!(f, "{}/{}/{}", self.host_id, self.project, self.file)
        }
    }
}

impl EditorFileDesc {
    /// Create an EditorFileDesc from a FileDesc and a host_id
    pub fn new(file_desc: FileDesc, host_id: ServerId) -> Self {
        match file_desc {
            FileDesc::Project { id } => EditorFileDesc {
                host_id,
                project: id,
                file: String::new(),
            },
            FileDesc::ProjectFile { project, file } => EditorFileDesc {
                host_id,
                project,
                file,
            },
        }
    }

    /// Get the host_id from this EditorFileDesc
    pub fn host_id(&self) -> &ServerId {
        &self.host_id
    }

    /// Check if this descriptor represents a project (directory)
    pub fn is_project(&self) -> bool {
        self.file.is_empty()
    }

    /// Check if this descriptor represents a file
    pub fn is_file(&self) -> bool {
        !self.file.is_empty()
    }
}

impl From<EditorFileDesc> for FileDesc {
    fn from(editor_file: EditorFileDesc) -> Self {
        if editor_file.file.is_empty() {
            FileDesc::Project {
                id: editor_file.project,
            }
        } else {
            FileDesc::ProjectFile {
                project: editor_file.project,
                file: editor_file.file,
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
pub fn empty_json_map() -> JsonMap {
    serde_json::Map::new()
}
