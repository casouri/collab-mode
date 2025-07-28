use crate::{
    collab_doc::Doc,
    engine::InternalDoc,
    error::CollabError,
    types::{ClientEngine, DocId, FatOp, GlobalSeq, LocalSeq, SiteId},
};
use anyhow::{anyhow, Context};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc};
use webrtc_util::sync::Mutex;

// *** Struct

/// A shared directory, basically. Remote user can freely browse and
/// open files under a shared project.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    /// Id of the project.
    id: u64,
    /// Name of the project, default to the base name of `root`.
    name: String,
    /// Absolute path of the root of the project.
    root: String,
    /// Metadata for the project. A JSON string.
    meta: String,
}

/// File in a project.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectFile {
    /// Relative path of this file under its project.
    rel_path: String,
    /// Whether this file is a directory. This is only for showing in
    /// the UI, we still check the actual file ourselves.
    is_dir: bool,
    /// Whether this file is a symlink. This is only for showing in
    /// the UI, we still check the actual file ourselves.
    is_symlink: bool,
}

// *** Const

const CREATE_PROJECT_TABLE: &'static str = "CREATE TABLE IF NOT EXISTS project (
id INTEGER PRIMARY KEY AUTOINCREMENT,
name TEXT NOT NULL,
root TEXT NOT NULL UNIQUE,
meta TEXT NOT NULL
)";

// Snapshot can lag behind the ops, since saving snapshot to DB can be
// a bit expensive (with serialization). As long as we have the ops,
// we can always bring snapshot up-to-date by appling ops on it.
const CREATE_DOC_TABLE: &'static str = "CREATE TABLE IF NOT EXISTS doc (
id INTEGER PRIMARY KEY AUTOINCREMENT,
abs_path TEXT NOT NULL,
current_site_seq INTEGER NOT NULL,
snapshot_internal_doc TEXT NOT NULL,
snapshot_content TEXT NOT NULL,
snapshot_seq INTEGER NOT NULL,
meta TEXT NOT NULL
)";

// Both global op and local op live in this table. Global ops have
// global_seq, local ops only have site_seq.
const CREATE_OP_TABLE: &'static str = "CREATE TABLE IF NOT EXISTS op (
doc_id INTEGER NOT NULL,
global_seq INTEGER,
site_seq INTEGER NOT NULL,
group_seq INTEGER NOT NULL,
kind INTEGER NOT NULL,
op TEXT NOT NULL,
FOREIGN KEY(doc_id) REFERENCES doc(id)
PRIMARY KEY(doc_id, seq)
)";

// *** Helper Fn

/// Get the basename of the file.
fn basename(rel_path: &str) -> String {
    if let Some(name) = PathBuf::from(rel_path).file_name() {
        name.to_string_lossy().to_string()
    } else {
        "".to_string()
    }
}

/// Get the absolute filename of the `rel_path` in `project`. Symlinks
/// are resolved.
fn abs_filename(rel_path: &str, project: &Project) -> std::io::Result<PathBuf> {
    let proj_filename = PathBuf::from(&project.root);
    let filename = proj_filename.join(rel_path);
    let stat = std::fs::metadata(&filename)?;
    if stat.is_symlink() {
        std::fs::read_link(filename)
    } else {
        Ok(filename)
    }
}

// *** Fn

/// Initialize the DB `conn`. Create necessary tables.
pub fn init_db(conn: &Connection) -> anyhow::Result<()> {
    conn.execute(CREATE_PROJECT_TABLE, ())
        .with_context(|| format!("Failed to initialize DB"))?;
    conn.execute(CREATE_DOC_TABLE, ())
        .with_context(|| format!("Failed to initialize DB"))?;
    Ok(())
}

pub fn list_projects(conn: &Connection) -> anyhow::Result<Vec<Project>> {
    let mut stmt = conn.prepare("SELECT id, name, root, meta FROM project")?;
    let projects_iter = stmt
        .query_map([], |row| {
            Ok(Project {
                id: row.get(0)?,
                name: row.get(1)?,
                root: row.get(2)?,
                meta: row.get(3)?,
            })
        })
        .with_context(|| format!("Failed to query DB for projects"))?;

    let mut projects: Vec<Project> = vec![];
    for proj in projects_iter {
        projects.push(proj.with_context(|| format!("Failed to query DB"))?);
    }
    Ok(projects)
}

/// Get the project with id `project_id`.
fn get_project_by_id(project_id: u64, conn: &Connection) -> anyhow::Result<Project> {
    conn.query_row_and_then(
        "SELECT id, name, root, meta FROM project WHERE id = ?",
        [project_id],
        |row| {
            Ok(Project {
                id: row.get(0)?,
                name: row.get(1)?,
                root: row.get(2)?,
                meta: row.get(3)?,
            }) as anyhow::Result<Project>
        },
    )
    .with_context(|| format!("Failed to get the project (id = {}) from DB", project_id))
}

/// List the files under the directory at `rel_path` in the project
/// with `project_id`.
pub fn list_files(
    project_id: u64,
    rel_path: &str,
    conn: &Connection,
) -> anyhow::Result<Vec<ProjectFile>> {
    let project = get_project_by_id(project_id, conn)?;
    let rel_path = rel_path.trim_start_matches('/');
    let filename = std::path::Path::new(&project.root).join(rel_path);
    let filename_str = filename.to_string_lossy().to_string();
    let stat = std::fs::metadata(&filename)
        .with_context(|| format!("Can't access file {}", &filename_str))?;
    if !stat.is_dir() {
        return Err(anyhow!(format!("Not a directory: {}", filename_str)));
    }
    let mut result: Vec<ProjectFile> = vec![];
    let files = std::fs::read_dir(&filename)
        .with_context(|| format!("Can't access file {}", &filename_str))?;
    for file in files {
        let file = file.with_context(|| format!("Can't access files in {}", &filename_str))?;
        let file_str = file.file_name().to_string_lossy().to_string();
        let stat = file
            .metadata()
            .with_context(|| format!("Can't access file {}", &file_str))?;
        result.push(ProjectFile {
            rel_path: filename.join(&file_str).to_string_lossy().to_string(),
            is_dir: stat.is_dir(),
            is_symlink: stat.is_symlink(),
        })
    }

    Ok(result)
}

/// Get file content of the file at `rel_path` under `project`.
pub fn file_content(project_id: u64, rel_path: &str, conn: &Connection) -> anyhow::Result<String> {
    let project = get_project_by_id(project_id, &conn)?;
    let filename = abs_filename(rel_path, &project)
        .with_context(|| format!("Failed to access file {}", rel_path))?;
    std::fs::read_to_string(filename).with_context(|| format!("Can't read file {}", rel_path))
}

/// Create a project at `path`.
pub fn add_project(path: &str, conn: &Connection) -> anyhow::Result<Project> {
    let abs_path = expanduser::expanduser(path)
        .with_context(|| format!("Can't expand filename to absolute path"))?;

    let abs_path_str = abs_path.to_string_lossy().to_string();
    let res = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 from project WHERE root = ?) AS res",
            [abs_path_str.clone()],
            |row| Ok(row.get::<&str, u64>("res")?),
        )
        .with_context(|| {
            format!(
                "Failed to check if project (path={}) already exists in DB",
                &abs_path_str
            )
        })?;
    if res == 1 {
        return Err(anyhow!(
            "Project with the same path ({}) already exists",
            abs_path_str
        ));
    }

    let basename = if let Some(name) = abs_path.file_name() {
        name.to_string_lossy().to_string()
    } else {
        return Err(anyhow!("Can't resolve the filename of {}", path));
    };
    conn.execute(
        "INSERT INTO project (name, root, meta) VALUES (?, ?, ?)",
        (&basename, abs_path.to_str(), "{}"),
    )
    .with_context(|| format!("Can't insert new project to DB"))?;
    Ok(Project {
        id: conn.last_insert_rowid() as u64,
        name: basename,
        root: abs_path.to_string_lossy().to_string(),
        meta: "{}".to_string(),
    })
}

// // Create a new doc, insert into DB, and return a `NewDoc` object.
// pub fn new_doc(
//     conn: &mut Connection,
//     site_id: SiteId,
//     abs_path: PathBuf,
//     content: String,
//     base_seq: GlobalSeq,
// ) -> anyhow::Result<NewDoc> {
//     let path_str = abs_path.to_string_lossy().to_string();
//     let mut stmt = conn.prepare("SELECT id FROM doc WHERE abs_path = ?")?;
//     let existing_doc = stmt
//         .query_map([path_str.clone()], |row| row.get(0))
//         .with_context(|| format!("Failed to find doc in DB"))?;
//     if let Some(doc_id) = existing_doc.into_iter().next() {
//         let doc_id = doc_id?;
//         return Err(CollabError::DocAlreadyExists(doc_id).into());
//     }

//     let engine = ClientEngine::new(site_id, base_seq, content.len() as u64);

//     // Insert doc to DB.

//     conn.execute("INSERT INTO doc (abs_path, next_site_seq, snapshot_internal_doc, snapshot_content, meta) VALUES (?, ?, ?, ?, ?)", [
//         path_str,
//         "1".to_string(),
//         serde_json::to_string(&internal_doc).expect("Serializing InternalDoc to JSON shouldn't fail"),
//         file_content ])?;
// }

// /// Start tracking a file as a doc.
// pub fn new_doc_from_file(
//     file: &ProjectFile,
//     project: &Project,
//     conn: &mut Connection,
// ) -> anyhow::Result<NewDoc> {
//     let abs_path = abs_filename(&file.rel_path, project).with_context(|| {
//         format!(
//             "Failed to resolve absolute path of file {} in project {}",
//             file.rel_path, project.name
//         )
//     })?;
//     let path_str = abs_path.to_string_lossy().to_string();

//     // Read file content.
//     let file_content = std::fs::read_to_string(&abs_path)
//         .with_context(|| format!("Failed to read file {}", path_str))?;
//     let file_len = file_content.len() as u64;
//     let internal_doc = InternalDoc::new(file_len);

//     Ok(NewDoc {
//         doc_id: conn.last_insert_rowid() as u32,
//         abs_path,
//         engine: Arc::new(Mutex::new(ClientEngine::new(0, 0, file_len))),
//         remote_op_buffer: Arc::new(Mutex::new(Vec::new())),
//         site_seq: 1,
//     })
// }
