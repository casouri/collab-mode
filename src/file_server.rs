use crate::{error::CollabResult, fatal, nfatal, types::JsonMap};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

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
pub struct ProjectFile {
    /// Relative path of this file under its project.
    rel_path: String,
    /// Whether this file is a directory. This is only for showing in
    /// the UI, we still check the actual file ourselves.
    is_dir: bool,
    /// Whether this file is a symlink. This is only for showing in
    /// the UI, we still check the actual file ourselves.
    is_symlink: bool,
    /// Size of the file in bytes. This is only for showing in the UI,
    /// we still check the actual file ourselves.
    size: u64,
}

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

const CREATE_PROJECTS_TABLE: (&'static str) = "CREATE TABLE IF NOT EXISTS projects (
id INTEGER PRIMARY KEY AUTOINCREMENT,
root TEXT NOT NULL UNIQUE,
name TEXT NOT NULL,
meta TEXT NOT NULL
)";

/// Initialize the DB `conn`. Create necessary tables.
pub fn init_db(conn: Connection) -> CollabResult<()> {
    conn.execute(CREATE_PROJECTS_TABLE, ())
        .map_err(|err| fatal!("Failed to create projects table:\n {:#?}", err))?;
    Ok(())
}

/// List the files under the directory at `rel_path` in `project`.
pub fn listdir(project: Project, rel_path: String) -> CollabResult<Vec<ProjectFile>> {
    let filename = std::path::Path::new(&project.root).join(rel_path);
    let filename_str = filename.to_string_lossy().to_string();
    let stat = std::fs::metadata(&filename)
        .map_err(|err| nfatal!("Can't access file {}:\n{:#?}", &filename_str, err))?;
    if !stat.is_dir() {
        return Err(crate::error::CollabError::NotDirectory(filename_str));
    }
    let mut result: Vec<ProjectFile> = vec![];
    let files = std::fs::read_dir(&filename)
        .map_err(|err| nfatal!("Can't access file {}:\n {:#?}", &filename_str, err))?;
    for file in files {
        let file =
            file.map_err(|err| nfatal!("Can't access files in {}:\n {:#?}", &filename_str, err))?;
        let filename_str = file.file_name().to_string_lossy().to_string();
        let stat = file
            .metadata()
            .map_err(|err| nfatal!("Can't access file {}:\n {:#?}", &filename_str, err))?;
        result.push(ProjectFile {
            rel_path,
            is_dir: stat.is_dir(),
            is_symlink: stat.is_symlink(),
            size: stat.size(),
        })
    }

    Ok(result)
}
