//! This module re-exports some common types for convenience, and
//! define a couple more.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;

pub use crate::config_man::{ArcKeyCert, KeyCert};
pub use crate::engine::{ContextOps, InternalDoc};
pub use crate::op::{
    replace_whitespace_char, DocId, GlobalSeq, GroupSeq, LocalSeq, Op, OpKind, SiteId,
};

/// If `addr` has no URL scheme, prepend `wss://`. Existing schemes
/// (`wss://`, `ws://`, `https://`, etc) are preserved.
pub fn normalize_signaling_addr(addr: &str) -> String {
    match addr.find("://") {
        Some(idx) if idx > 0 => addr.to_string(),
        _ => format!("wss://{}", addr),
    }
}

/// Truncate string for logging: show head…tail (N chars) if too long.
pub fn truncate_for_log(s: &str, max_len: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_len {
        replace_whitespace_char(s.to_string())
    } else {
        let head: String = chars[..10].iter().collect();
        let tail: String = chars[chars.len() - 10..].iter().collect();
        format!(
            "{}...{} ({} chars)",
            replace_whitespace_char(head),
            replace_whitespace_char(tail),
            chars.len()
        )
    }
}

/// A server's identity. Format is `<name>::<cert-hash>` but also
/// accept (grudgingly) `<cert-hash>`. The hash portion is the SHA-256
/// of the DER cert in colon-separated upper-hex. The name is for
/// convenience.
pub type ServerId = String;

/// Returns the cert-hash portion of a [`ServerId`]. Splits on the
/// first `::`; if there's no `::`, treat the whole id as the hash.
pub fn id_hash(id: &str) -> &str {
    match id.find("::") {
        Some(idx) => &id[idx + 2..],
        None => id,
    }
}

/// Returns the name portion of a [`ServerId`], or `None` if there
/// isn't one.
pub fn id_name(id: &str) -> Option<&str> {
    id.find("::").map(|idx| &id[..idx])
}

/// Compose a [`ServerId`] from an optional name and a cert hash.
pub fn make_id(name: Option<&str>, cert_hash: &str) -> ServerId {
    match name {
        Some(name) if !name.is_empty() => format!("{name}::{cert_hash}"),
        _ => cert_hash.to_string(),
    }
}
/// A file in a directory that’s not yet a doc. The path is a relative
/// path. We don’t want to use full path since some people might
/// consider the full path sensitive?
#[allow(dead_code)]
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
#[serde(tag = "kind")]
pub enum EditorOp {
    /// Insert op, with the position and content.
    Ins { pos: u64, content: String },
    /// Delete op, with the position and content.
    Del { pos: u64, content: String },
    /// Undo op, with the context number.
    Undo { context: u64 },
    /// Redo op, with the context number.
    Redo { context: u64 },
}

impl std::fmt::Debug for EditorOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Undo { context } => write!(f, "undo({context})"),
            Self::Redo { context } => write!(f, "redo({context})"),
            Self::Ins { pos, content } => {
                write!(f, "ins({}, {})", pos, truncate_for_log(content, 30))
            }
            Self::Del { pos, content } => {
                write!(f, "del({}, {})", pos, truncate_for_log(content, 30))
            }
        }
    }
}

impl std::fmt::Display for EditorOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Undo { context } => write!(f, "undo({context})"),
            Self::Redo { context } => write!(f, "redo({context})"),
            Self::Ins { pos, content } => {
                write!(f, "ins({}, {})", pos, truncate_for_log(content, 30))
            }
            Self::Del { pos, content } => {
                write!(f, "del({}, {})", pos, truncate_for_log(content, 30))
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
            FileDesc::ProjectFile { project, file } => write!(f, "/{}/{}", project, file),
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
    #[allow(dead_code)]
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
                file: editor_file.file.trim_matches('/').to_string(),
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
            EditorOp::Ins { .. } => EditorOpKind::Original,
            EditorOp::Del { .. } => EditorOpKind::Original,
            EditorOp::Undo { .. } => EditorOpKind::Undo,
            EditorOp::Redo { .. } => EditorOpKind::Redo,
        }
    }
}

#[derive(Eq, PartialEq, Clone, Deserialize, Serialize)]
pub struct Edit {
    pub pos: u64,
    pub content: String,
}

impl std::fmt::Debug for Edit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Edit")
            .field("pos", &self.pos)
            .field("content", &truncate_for_log(&self.content, 30))
            .finish()
    }
}

impl std::fmt::Display for Edit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "edit({}, {})",
            self.pos,
            truncate_for_log(&self.content, 30)
        )
    }
}

/// The segments inside each op are to be applied in the same time, so
/// you’ll see things that doesn’t make sense at first glance, like
/// Ins[(0, “a”), (0, “b”)], meaning insert “ab” at 0. So for both
/// insert and delete, apply the segments in reverse order to preserve
/// the positions.
#[derive(Debug, Eq, PartialEq, Clone, Deserialize, Serialize)]
#[serde(tag = "kind")]
pub enum EditInstruction {
    Ins { edits: Vec<Edit> },
    Del { edits: Vec<Edit> },
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
#[derive(Clone, Serialize, Deserialize)]
pub struct NewSnapshot {
    /// The file content.
    pub content: String,
    /// The internal document state (includes tombstones). This is
    /// necessary because if A shares a file, deletes a big portion of
    /// it, B requests that (redacted) file, creates a fresh internal
    /// doc for it, then A undo the delete, the op’s range will
    /// overflow B’s internal doc.
    pub internal_doc: InternalDoc,
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

impl std::fmt::Debug for NewSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let chars: Vec<char> = self.content.chars().collect();
        let len = chars.len();

        let content_str = if len <= 50 {
            self.content.clone()
        } else {
            let head: String = chars[..20].iter().collect();
            let tail: String = chars[len - 20..].iter().collect();
            format!("'{}'\u{2026}'{}' ({} chars)", head, tail, len)
        };

        f.debug_struct("NewSnapshot")
            .field("content", &content_str)
            .field("internal_doc", &self.internal_doc)
            .field("name", &self.name)
            .field("seq", &self.seq)
            .field("site_id", &self.site_id)
            .field("file_desc", &self.file_desc)
            .field("doc_id", &self.doc_id)
            .finish()
    }
}

#[allow(dead_code)]
pub fn empty_json_map() -> JsonMap {
    serde_json::Map::new()
}
