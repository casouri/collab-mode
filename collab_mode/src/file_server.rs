use crate::types::JsonMap;
use anyhow::{anyhow, Context};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// *** Struct

/// A shared directory, basically. Remote user can freely browse and
/// open files under a shared project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    /// Name of the project, default to the base name of `root`.
    name: String,
    /// Absolute path of the root of the project.
    root: String,
    /// Metadata for the project.
    meta: JsonMap,
}

/// File in a project.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug)]
pub struct FileServer {
    db: rusqlite::Connection,
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

const CREATE_PROJECTS_TABLE: &'static str = "CREATE TABLE IF NOT EXISTS projects (
id INTEGER PRIMARY KEY AUTOINCREMENT,
root TEXT NOT NULL UNIQUE,
name TEXT NOT NULL,
meta TEXT NOT NULL
)";

const ADD_PROJECT: &'static str = "INSERT INTO projects (root, name, meta)
VALUES (?, ?, ?)";

// *** Fn

/// Initialize the DB `conn`. Create necessary tables.
pub fn init_db(conn: Connection) -> anyhow::Result<()> {
    conn.execute(CREATE_PROJECTS_TABLE, ())
        .with_context(|| format!("Failed to create table in DB"))?;
    Ok(())
}

/// List the files under the directory at `rel_path` in `project`.
pub fn listdir(project: Project, rel_path: &str) -> anyhow::Result<Vec<ProjectFile>> {
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
pub fn add_project(path: &str, conn: Connection) -> anyhow::Result<Project> {
    let filename = PathBuf::from(path);
    let basename = if let Some(name) = filename.file_name() {
        name.to_string_lossy().to_string()
    } else {
        return Err(anyhow!("Can't resolve the filename of {}", path));
    };
    conn.execute(ADD_PROJECT, (&basename, path, "{}"))
        .with_context(|| format!("Can't insert new project to DB"))?;
    Ok(Project {
        name: basename,
        root: path.to_string(),
        meta: serde_json::from_str("{}").unwrap(),
    })
}
