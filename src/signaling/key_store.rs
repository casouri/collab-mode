use rusqlite::Connection;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::SignalingResult;

#[derive(Debug)]
pub struct PubKeyStore {
    /// SQLite connection.
    conn: Connection,
}

/// Returns the current unix epoch in seconds.
fn current_unix_epoch_in_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

impl PubKeyStore {
    /// Return a `PubKeyStore` connected to the sqlite database at
    /// `db_path`.
    pub fn new(db_path: &Path) -> SignalingResult<PubKeyStore> {
        let conn = Connection::open(db_path)?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS PubKeys (
    uuid TEXT PRIMARY KEY,
    key TEXT NOT NULL,
    atime INTEGER NOT NULL
)",
            (),
        )?;
        Ok(PubKeyStore { conn })
    }

    /// Return the certificate hash for `uuid` if it exists.
    pub fn get_key_for(&self, uuid: &str) -> SignalingResult<Option<String>> {
        let res = self
            .conn
            .query_row("SELECT key FROM PubKeys WHERE uuid = ?", (uuid,), |row| {
                row.get(0)
            });
        match res {
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Ok(key) => Ok(Some(key)),
            Err(err) => Err(err.into()),
        }
    }

    /// Set `key` (certificate in DER format hashed by SHA-256 and
    /// printed in hex) for `uuid`. If a key aready exists for `uuid`,
    /// override it.
    pub fn set_key_for(&self, uuid: &str, key: &str) -> SignalingResult<()> {
        let atime = current_unix_epoch_in_secs();
        self.conn.execute(
            "INSERT OR REPLACE
INTO PubKeys (uuid, key, atime)
VALUES (?1, ?2, ?3)",
            (uuid, key, atime),
        )?;
        Ok(())
    }
}
