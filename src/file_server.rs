use crate::{error::CollabResult, fatal, types::JsonMap};
use rusqlite;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::os::unix::fs::MetadataExt;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    /// Name of the project, default to the base name of `root`.
    name: String,
    /// Absolute path of the root of the project.
    root: String,
    /// Metadata for the project.
    meta: JsonMap,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirEntry {
    /// Filename of this entry.
    name: String,
    /// Whether this file is a directory.
    is_dir: bool,
    /// Whether this file is a symlink.
    is_symlink: bool,
    /// Size of the file in bytes.
    size: u64,
}

const CREATE_PROJECTS_TABLE: (&'static str) = "CREATE TABLE IF NOT EXISTS projects (
id INTEGER PRIMARY KEY AUTOINCREMENT,
root TEXT NOT NULL UNIQUE,
name TEXT NOT NULL,
meta TEXT NOT NULL
)";

const CREATE_FILES_TABLE: (&'static str) = "CREATE TABLE IF NOT EXISTS files (
id INTEGER PRIMARY KEY AUTOINCREMENT,
rel_path TEXT NOT NULL,
content TEXT,
)";

const CREATE_PROJECT_FILES_TABLE: (&'static str) = "CREATE TABLE IF NOT EXISTS project_files (
project_id INTEGER,
file_id INTEGER,
UNIQUE (project_id, file_id),
FOREIGN KEY (project_id) REFERENCES projects (id)
FOREIGN KEY (file_id) REFERENCES files (id)
)";

/// Initialize the DB `conn`. Create necessary tables.
pub fn init_db(conn: Connection) -> CollabResult<()> {
    conn.execute(CREATE_PROJECTS_TABLE, ())
        .map_err(|err| fatal!("Failed to create projects table:\n {:#?}", err))?;
    conn.execute(CREATE_FILES_TABLE, ())
        .map_err(|err| fatal!("Failed to create files table:\n {:#?}", err))?;
    conn.execute(CREATE_PROJECT_FILES_TABLE, ())
        .map_err(|err| fatal!("Failed to create project_files table:\n {:#?}", err))?;

    Ok(())
}

pub fn listdir(project: Project, rel_path: String) -> CollabResult<Vec<DirEntry>> {
    let filename = std::path::Path::new(&project.root).join(rel_path);
    let filename_str = filename.to_string_lossy().to_string();
    let stat = std::fs::metadata(&filename)
        .map_err(|err| fatal!("Can't access file {}:\n{:#?}", &filename_str, err))?;
    if !stat.is_dir() {
        return Err(crate::error::CollabError::NotDirectory(filename_str));
    }
    let mut result: Vec<DirEntry> = vec![];
    let files = std::fs::read_dir(&filename)
        .map_err(|err| fatal!("Can't access file {}:\n {:#?}", &filename_str, err))?;
    for file in files {
        let file =
            file.map_err(|err| fatal!("Can't access files in {}:\n {:#?}", &filename_str, err))?;
        let filename_str = file.file_name().to_string_lossy().to_string();
        let stat = file
            .metadata()
            .map_err(|err| fatal!("Can't access file {}:\n {:#?}", &filename_str, err))?;
        result.push(DirEntry {
            name: filename_str,
            is_dir: stat.is_dir(),
            is_symlink: stat.is_symlink(),
            size: stat.size(),
        })
    }

    Ok(result)
}
