use crate::{collab_doc::Doc, types::JsonMap};
use anyhow::{anyhow, Context};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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

// *** Impl

impl ProjectFile {
    /// Get the basename of the file.
    pub fn basename(&self) -> String {
        if let Some(name) = PathBuf::from(&self.rel_path).file_name() {
            name.to_string_lossy().to_string()
        } else {
            "".to_string()
        }
    }

    /// Get the absolute filename of the file. Symlinks are resolved.
    pub fn abs_filename(&self, project: &Project) -> std::io::Result<PathBuf> {
        let proj_filename = PathBuf::from(&project.root);
        let filename = proj_filename.join(&self.rel_path);
        let stat = std::fs::metadata(&filename)?;
        if stat.is_symlink() {
            std::fs::read_link(filename)
        } else {
            Ok(filename)
        }
    }
}

// *** Const

const CREATE_PROJECT_TABLE: &'static str = "CREATE TABLE IF NOT EXISTS project (
id INTEGER PRIMARY KEY AUTOINCREMENT,
name TEXT NOT NULL,
root TEXT NOT NULL UNIQUE,
meta TEXT NOT NULL
)";

const CREATE_DOC_TABLE: &'static str = "CREATE TABLE IF NOT EXISTS doc (
id INTEGER PRIMARY KEY AUTOINCREMENT,
abs_path TEXT NOT NULL,
site_seq INTEGER NOT NULL,
snapshot_internal_doc TEXT NOT NULL,
snapshot_content TEXT NOT NULL,
snapshot_seq INTEGER NOT NULL,
meta TEXT NOT NULL
)";

const CREATE_OP_TABLE: &'static str = "CREATE TABLE IF NOT EXISTS op (
doc_id INTEGER NOT NULL,
seq INTEGER NOT NULL,
site_seq INTEGER NOT NULL,
group_seq INTEGER NOT NULL,
kind INTEGER NOT NULL,
op TEXT NOT NULL,
remote INTEGER NOT NULL,
FOREIGN KEY(doc_id) REFERENCES doc(id)
)";

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

pub fn list_files(
    project_id: u64,
    rel_path: &str,
    conn: &Connection,
) -> anyhow::Result<Vec<ProjectFile>> {
    let project = conn
        .query_row_and_then(
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
        .with_context(|| format!("Failed to get the project (id = {}) from DB", project_id))?;
    list_files_1(project, rel_path)
}

/// List the files under the directory at `rel_path` in `project`.
pub fn list_files_1(project: Project, rel_path: &str) -> anyhow::Result<Vec<ProjectFile>> {
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

/// Get file content of `file` under `project`.
pub fn file_content(project: &Project, file: &ProjectFile) -> anyhow::Result<String> {
    let filename = file
        .abs_filename(project)
        .with_context(|| format!("Failed to access file {}", file.rel_path))?;
    std::fs::read_to_string(filename).with_context(|| format!("Can't read {}", file.rel_path))
}

/// Create a project at `path`.
pub fn add_project(path: &str, conn: &Connection) -> anyhow::Result<Project> {
    let abs_path = expanduser::expanduser(path)
        .with_context(|| format!("Can't expand filename to absolute path"))?;
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

/// Starting tracking a file as a doc.
pub fn add_doc(
    file: &ProjectFile,
    project: &Project,
    conn: &mut Connection,
) -> anyhow::Result<Doc> {
    // let abs_path = file.abs_filename(project).with_context(|| {
    //     format!(
    //         "Failed to resolve absolute path of file {} in project {}",
    //         file.rel_path, project.name
    //     )
    // })?;
    // let stmt = conn.prepare(
    //     "SELECT id, abs_path, site_seq, snapshot_interal_doc, snapshot_content, snapshot_seq, meta FROM doc WHERE abs_path = ?",
    // );
    // stmt.query_map(vec![abs_path], |row| {})
    //     .with_context(|| format!("Failed to find doc in DB"));
    todo!()
}
