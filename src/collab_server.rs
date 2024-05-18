//! This module provides [LocalServer], the server that linearizes ops
//! and handle requests from sites. A collab process will start a
//! [LocalServer] and access it through the
//! [crate::abstract_server::DocServer] trait, and access remote
//! servers with [crate::grpc_client::GrpcClient], also through
//! [crate::DocServer] crate. At the same time, we expose the local
//! server to remote sites with gRPC, by implementing the
//! [crate::rpc::doc_server_server::DocServer] trait.

use crate::abstract_server::{DocServer, InfoStream, OpStream};
use crate::error::{CollabError, CollabResult};
use crate::types::*;
use crate::webrpc::{self, Endpoint, Listener};
use async_trait::async_trait;
use gapbuf::GapBuffer;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::select;
use tokio::sync::{broadcast, mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use tracing::{instrument, Instrument};

const RETAINED_INFO_MSG_MAX: usize = 32;

// *** Types

#[async_trait]
impl DocServer for LocalServer {
    fn site_id(&self) -> SiteId {
        self.self_site_id.clone()
    }
    fn server_id(&self) -> ServerId {
        SERVER_ID_SELF.to_string()
    }
    async fn share_file(
        &mut self,
        file_name: &str,
        file_meta: &JsonMap,
        file: FileContentOrPath,
    ) -> CollabResult<DocId> {
        self.share_file_1(file_name, file_meta, file, None).await
    }
    async fn list_files(&mut self, dir_path: Option<FilePath>) -> CollabResult<Vec<DocInfo>> {
        self.list_files_1(dir_path).await
    }
    async fn request_file(&mut self, doc_file: &DocDesc) -> CollabResult<Snapshot> {
        self.request_file_1(doc_file).await
    }
    async fn delete_file(&mut self, doc_id: &DocId) -> CollabResult<()> {
        self.delete_file_1(doc_id).await
    }
    async fn send_op(&mut self, ops: ContextOps) -> CollabResult<()> {
        self.send_op_1(ops).await
    }
    async fn recv_op_and_info(
        &mut self,
        doc_id: &DocId,
        after: GlobalSeq,
    ) -> CollabResult<(OpStream, InfoStream)> {
        let (op_stream, info_stream) = self.recv_op_1(doc_id, after).await?;
        Ok((
            Box::pin(op_stream.map(|ops| Ok(ops))),
            Box::pin(info_stream.map(|ops| Ok(ops))),
        ))
    }
    async fn send_info(&mut self, doc_id: &DocId, info: String) -> CollabResult<()> {
        self.send_info_1(
            doc_id,
            Info {
                sender: self.self_site_id,
                value: info,
            },
        )
        .await
    }
}

// *** Structs

/// The server object.
#[derive(Debug, Clone)]
pub struct LocalServer {
    /// SiteId given to ourselves.
    self_site_id: SiteId,
    /// SiteId given to the next connected client.
    next_site_id: Arc<Mutex<SiteId>>,
    /// Docs hosted by this server.
    docs: Arc<Mutex<HashMap<DocId, Arc<Mutex<Doc>>>>>,
    /// Like `docs` but stores directories.
    dirs: Arc<Mutex<HashMap<DocId, Arc<Mutex<Dir>>>>>,
    /// Map of recognized users.
    user_list: Arc<Mutex<HashMap<Credential, SiteId>>>,
    /// Channel for sending errors to the editor.
    err_tx: mpsc::Sender<CollabError>,
}

/// Stores relevant data for a document, used by the server.
#[derive(Debug)]
struct Doc {
    /// Human-readable name for the doc.
    name: String,
    /// Metadata for this doc.
    meta: JsonMap,
    /// The local file path, if exists.
    file_path: Option<FilePath>,
    /// The full path of this doc on disk, if exists. This is used for
    /// automatically saving the file to disk.
    full_path: Arc<Mutex<Option<PathBuf>>>,
    /// The server engine that transforms and stores ops for this doc.
    engine: ServerEngine,
    /// The document itself.
    buffer: Arc<Mutex<GapBuffer<char>>>,
    /// Engine sends a Unit to tx when there are new ops.
    tx: watch::Sender<()>,
    /// Clone a rx from this rx, and check for new op notification.
    rx: watch::Receiver<()>,
    /// Incoming info are sent to this channel.
    tx_info: broadcast::Sender<Info>,
    /// Retain this receiver so the channel is not closed.
    _rx_info: broadcast::Receiver<Info>,
    /// If this is not None, some error happend to this Doc and any
    /// further access should simply return an error. This way every
    /// client sees the error rather than seeing DocNotFound.
    error: Option<CollabError>,
    /// Dropping this rx terminates autosave thread.
    _auto_save_shutdown_rx: mpsc::Receiver<()>,
}

/// Stores relevant data for a shared dir, used by the server.
#[derive(Debug)]
struct Dir {
    /// Human-readable name for the dir.
    name: String,
    /// Metadata for the directory.
    meta: JsonMap,
    /// Absolute path of the directory on disk.
    path: PathBuf,
    /// Children of this dir (rel_path, doc_id). Note that this
    /// doesn't include all the files in this dir on the disk; it only
    /// includes dirs and docs included in dir and doc maps in
    /// [LocalServer].
    children: Vec<(PathBuf, DocId)>,
    /// If this is not None, some error happend to this Doc and any
    /// further access should simply return an error.
    error: Option<CollabError>,
}

// *** Functions for Doc

impl Doc {
    pub fn new(
        doc_id: DocId,
        file_name: &str,
        file_meta: &JsonMap,
        content: &str,
        file_path: Option<FilePath>,
        full_path: Arc<Mutex<Option<PathBuf>>>,
        err_tx: mpsc::Sender<CollabError>,
    ) -> Doc {
        let (tx, rx) = watch::channel(());
        let (tx_info, rx_info) = broadcast::channel(RETAINED_INFO_MSG_MAX);
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
        let mut buffer = GapBuffer::new();
        buffer.insert_many(0, content.chars());
        let buffer = Arc::new(Mutex::new(buffer));

        tokio::spawn(run_auto_save(
            doc_id,
            buffer.clone(),
            full_path.clone(),
            shutdown_tx,
            err_tx,
        ));

        Doc {
            name: file_name.to_string(),
            file_path,
            full_path,
            meta: file_meta.clone(),
            engine: ServerEngine::new(content.chars().count() as u64),
            buffer,
            tx,
            rx,
            tx_info,
            _rx_info: rx_info,
            error: None,
            _auto_save_shutdown_rx: shutdown_rx,
        }
    }

    /// Apply `op` to the document.
    pub fn apply_op(&mut self, op: FatOp) -> CollabResult<()> {
        let instr = self.engine.convert_internal_op_and_apply(op)?;
        match instr {
            EditInstruction::Ins(edits) => {
                for (pos, str) in edits.into_iter().rev() {
                    self.buffer
                        .lock()
                        .unwrap()
                        .insert_many(pos as usize, str.chars());
                }
            }
            EditInstruction::Del(edits) => {
                for (pos, str) in edits.into_iter().rev() {
                    self.buffer
                        .lock()
                        .unwrap()
                        .drain((pos as usize)..(pos as usize + str.chars().count()));
                }
            }
        }
        Ok(())
    }

    /// Return the current snapshot of the document.
    pub fn snapshot(&self, doc_id: DocId) -> Snapshot {
        Snapshot {
            buffer: self.buffer.lock().unwrap().iter().collect::<String>(),
            file_name: self.name.clone(),
            seq: self.engine.current_seq(),
            doc_id,
        }
    }

    /// Check if the doc already has an error, if so, return that
    /// error.
    pub fn check_for_existing_error(&self) -> CollabResult<()> {
        match self.error {
            Some(ref err) => Err(err.clone()),
            None => Ok(()),
        }
    }
}

/// Save `text` to disk at `path` for `doc_id`.
fn save_to_disk(doc_id: &DocId, path: &Path, text: &str) -> CollabResult<()> {
    let file = std::fs::File::open(path).map_err(|err| {
        CollabError::AutoSaveErr(
            doc_id.clone(),
            format!("Can’t open file for autosaving: {:#}", err),
        )
    })?;
    let mut writer = std::io::BufWriter::new(file);
    writer.write_all(text.as_bytes()).map_err(|err| {
        CollabError::AutoSaveErr(
            doc_id.clone(),
            format!("Can’t write the doc to disk: {:#}", err),
        )
    })?;
    Ok(())
}

async fn run_auto_save(
    doc_id: DocId,
    buffer: Arc<Mutex<GapBuffer<char>>>,
    path: Arc<Mutex<Option<PathBuf>>>,
    shutdown_tx: mpsc::Sender<()>,
    err_tx: mpsc::Sender<CollabError>,
) {
    let mut timer = tokio::time::interval(std::time::Duration::from_secs(30));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        select! {
            _ = shutdown_tx.closed() => {
                return;
            }
            _ = timer.tick() => {
                let path = path.lock().unwrap().clone();
                if let Some(path) = path {
                    let text = buffer.lock().unwrap().iter().collect::<String>();
                    let res = save_to_disk(&doc_id, &path, &text);
                    if let Err(err) = res {
                        tracing::error!(?err, "Autosave failed");
                        let _ = err_tx.try_send(err);
                    }
                    tracing::debug!("Autosave");
                }
            }
        }
    }
}

// *** Functions for Dir

impl Dir {
    /// Check if the dir already has an error, if so, return that
    /// error.
    pub fn check_for_existing_error(&self) -> CollabResult<()> {
        match self.error {
            Some(ref err) => Err(err.clone()),
            None => Ok(()),
        }
    }
}

// *** Functions for CollabServer

impl LocalServer {
    pub fn new(err_tx: mpsc::Sender<CollabError>) -> LocalServer {
        LocalServer {
            self_site_id: 0,
            next_site_id: Arc::new(Mutex::new(1)),
            docs: Arc::new(Mutex::new(HashMap::new())),
            dirs: Arc::new(Mutex::new(HashMap::new())),
            user_list: Arc::new(Mutex::new(HashMap::new())),
            err_tx,
        }
    }

    /// Get the `Doc` with `doc_id`. Getting the `Doc` using this
    /// function doesn't locks `self.docs`.
    fn get_doc(&self, doc_id: &DocId) -> Option<Arc<Mutex<Doc>>> {
        let docs = self.docs.lock().unwrap();
        if let Some(doc) = docs.get(&doc_id) {
            let doc = doc.clone();
            drop(docs);
            Some(doc)
        } else {
            None
        }
    }

    /// Get the `Dir` with `doc_id`. Getting the `Dir` using this
    /// function doesn't locks `self.docs`.
    fn get_dir(&self, doc_id: &DocId) -> Option<Arc<Mutex<Dir>>> {
        let dirs = self.dirs.lock().unwrap();
        if let Some(dir) = dirs.get(&doc_id) {
            let dir = dir.clone();
            drop(dirs);
            Some(dir)
        } else {
            None
        }
    }

    /// Attach `err` to doc with `doc_id`.
    fn attach_error(&mut self, doc_id: &DocId, err: CollabError) {
        if let Some(doc) = self.get_doc(doc_id) {
            let mut doc = doc.lock().unwrap();
            match &mut doc.error {
                None => doc.error = Some(err),
                Some(_) => (),
            }
        }
    }

    /// Attach `err` to doc with `doc_id`.
    fn attach_dir_error(&mut self, dir_id: &DocId, err: CollabError) {
        if let Some(dir) = self.get_dir(dir_id) {
            let mut dir = dir.lock().unwrap();
            match &mut dir.error {
                None => dir.error = Some(err),
                Some(_) => (),
            }
        }
    }

    /// Create a doc out of the file at `rel_path` in `dir`, and
    /// return its snapshot.
    async fn get_dir_file(
        &self,
        dir_id: &DocId,
        dir: &Arc<Mutex<Dir>>,
        rel_path: &Path,
    ) -> CollabResult<Snapshot> {
        let full_path = {
            let dir = dir.lock().unwrap();
            dir.check_for_existing_error()?;
            dir.path.join(rel_path)
        };
        // We return full path here and let clients handle displaying
        // more friendly names.
        let file_name = full_path.to_string_lossy().to_string();
        let mut file = std::fs::File::open(full_path).map_err(|err| {
            CollabError::DocFatal(format!("Can't open file {}: {:?}", file_name, err))
        })?;
        let mut buf = String::new();
        file.read_to_string(&mut buf).map_err(|err| {
            CollabError::DocFatal(format!("Can't read file {}: {:?}", file_name, err))
        })?;
        let doc_id = self
            .share_file_1(
                &file_name,
                &empty_json_map(),
                FileContentOrPath::Content(buf.clone()),
                Some((*dir_id, rel_path.to_path_buf())),
            )
            .await?;
        let doc = self.get_doc(&doc_id).unwrap();
        let snapshot = doc.lock().unwrap().snapshot(doc_id);
        Ok(snapshot)
    }

    /// Realize `path` into a full path.
    fn realize_full_path(&self, path: &FilePath) -> PathBuf {
        let (dir_id, rel_path) = path;
        let dirs = self.dirs.lock().unwrap();
        let dir = dirs.get(&dir_id).unwrap().clone();
        drop(dirs);
        let dir = dir.lock().unwrap();
        dir.path.join(rel_path)
    }

    // `file_path` is only for `get_dir_file`. Other callers always
    // pass a `None`. There are three use-cases for this function:
    // 1. Share a buffer: file = content, file_path = None
    // 2. Share a directory: file = path, file_path = None
    // 3. Share a file that's in a shared directory: file = content,
    //    file_path = file's dir and rel_path.
    #[instrument(skip(self, file))]
    pub async fn share_file_1(
        &self,
        file_name: &str,
        file_meta: &JsonMap,
        file: FileContentOrPath,
        file_path: Option<FilePath>,
    ) -> CollabResult<DocId> {
        tracing::debug!("Entered share_file");

        // TODO permission check.
        let doc_id: DocId = rand::random();
        match file {
            FileContentOrPath::Content(content) => {
                // realize_full_path locks self.dirs, so we call it
                // before locking self.docs.
                let full_path = if let Some(ref file_path) = file_path {
                    Some(self.realize_full_path(file_path))
                } else {
                    None
                };
                let mut docs = self.docs.lock().unwrap();

                docs.insert(
                    doc_id.clone(),
                    Arc::new(Mutex::new(Doc::new(
                        doc_id,
                        file_name,
                        file_meta,
                        &content,
                        file_path.clone(),
                        Arc::new(Mutex::new(full_path)),
                        self.err_tx.clone(),
                    ))),
                );
                drop(docs);

                // Add doc before adding doc to the dir.
                if let Some((dir_id, rel_path)) = file_path {
                    let mut dirs = self.dirs.lock().unwrap();
                    let maybe_dir = dirs.get_mut(&dir_id);
                    if let Some(dir) = maybe_dir {
                        let mut dir = dir.lock().unwrap();
                        dir.check_for_existing_error()?;
                        dir.children.push((rel_path, doc_id));
                    } else {
                        drop(dirs);
                        let mut docs = self.docs.lock().unwrap();
                        let _ = docs.remove(&doc_id);
                        return Err(CollabError::DocNotFound(dir_id));
                    }
                }
            }
            FileContentOrPath::Path(path) => {
                let mut dirs = self.dirs.lock().unwrap();

                dirs.insert(
                    doc_id.clone(),
                    Arc::new(Mutex::new(Dir {
                        name: file_name.to_string(),
                        path,
                        children: vec![],
                        meta: empty_json_map(),
                        error: None,
                    })),
                );
            }
        }
        Ok(doc_id)
    }

    /// Send local ops to the server. TODO: access control.
    #[instrument(skip(self))]
    pub async fn send_op_1(&self, ops: ContextOps) -> CollabResult<()> {
        tracing::debug!("Entered send_op");

        let doc_id = ops.doc();
        if let Some(doc) = self.get_doc(&doc_id) {
            let mut doc = doc.lock().unwrap();
            doc.check_for_existing_error()?;

            tracing::debug!(?ops, "Local server receive ops");

            let ops = doc.engine.process_ops(ops.ops, ops.context)?;

            for op in ops {
                doc.apply_op(op)?;
            }

            tracing::trace!(
                "doc = \"{}\"",
                doc.buffer.lock().unwrap().iter().collect::<String>()
            );

            // Notification channel are never closed.
            // TODO: report error to error channel.
            doc.tx.send(()).unwrap();
            Ok(())
        } else {
            Err(CollabError::DocNotFound(doc_id))
        }
    }

    // Receive ops after `after` from server. Returns a stream of ops.
    // TODO access control.
    #[instrument(skip(self))]
    pub async fn recv_op_1(
        &self,
        doc_id: &DocId,
        mut after: GlobalSeq,
    ) -> CollabResult<(ReceiverStream<Vec<FatOp>>, ReceiverStream<Info>)> {
        tracing::debug!("Entered recv_op");

        let (tx_op, rx_op) = mpsc::channel(1);
        let (tx_info, rx_info) = mpsc::channel(1);
        // Clone a notification channel and a reference to the doc,
        // listen for notification and get new ops from the doc, and
        // feed into the channel.
        let maybe_doc = self.get_doc(doc_id);
        if maybe_doc.is_none() {
            return Err(CollabError::DocNotFound(doc_id.clone()));
        }

        let doc = maybe_doc.unwrap();
        let doc1 = doc.clone();
        let locked_doc = doc.lock().unwrap();
        locked_doc.check_for_existing_error()?;
        let mut notifier = locked_doc.rx.clone();
        let mut inner_rx_info = locked_doc.tx_info.subscribe();
        drop(locked_doc);

        let _ = tokio::spawn(async move {
            while notifier.changed().await.is_ok() {
                tracing::debug!(
                    "Local server collects global ops after seq#{} to send out",
                    after
                );
                let ops = doc1.lock().unwrap().engine.global_ops_after(after);
                after += ops.len() as GlobalSeq;

                tracing::debug!(?ops, "Local server sends out ops");

                if let Err(err) = tx_op.send(ops).await {
                    // When the local or remote
                    // [crate::collab_doc::Doc] is dropped,
                    // its connection to us is dropped. This
                    // is not an error.
                    tracing::info!(?err, "Op channel closed.");
                    return;
                }
            }
        });

        // Using select creates too much indent, might as well start
        // two tasks.
        let _ = tokio::spawn(async move {
            while let Ok(info) = inner_rx_info.recv().await {
                if let Err(err) = tx_info.send(info).await {
                    tracing::info!(?err, "Info channel closed");
                    return;
                }
            }
        });

        Ok((ReceiverStream::from(rx_op), ReceiverStream::from(rx_info)))
    }

    async fn list_directory(&self, dir_path: FilePath) -> CollabResult<Vec<DocInfo>> {
        let (doc_id, rel_path) = dir_path;
        if let Some(dir) = self.get_dir(&doc_id) {
            let (path, children) = {
                let dir = dir.lock().unwrap();
                dir.check_for_existing_error()?;
                (Path::new(&dir.path).join(rel_path), dir.children.clone())
            };

            if !path.is_dir() {
                // If there are error access the file, return that
                // error instead of NotDirectory.
                path.metadata().map_err(|err| {
                    CollabError::DocFatal(format!(
                        "Can't access file {}: {:?}",
                        path.to_string_lossy(),
                        err
                    ))
                })?;
                return Err(CollabError::NotDirectory(
                    path.to_string_lossy().to_string(),
                ));
            }
            let mut res = vec![];

            for file in std::fs::read_dir(&path).map_err(|err| {
                CollabError::DocFatal(format!(
                    "Can't read directory {}: {:?}",
                    path.to_string_lossy(),
                    err
                ))
            })? {
                let file = file.map_err(|err| {
                    CollabError::DocFatal(format!(
                        "Can't read directory {}: {:?}",
                        path.to_string_lossy(),
                        err
                    ))
                })?;
                let abs_path = file.path();
                let rel_path = pathdiff::diff_paths(&abs_path, &path).unwrap();
                // This traverses symlink.
                let meta = std::fs::metadata(&abs_path).map_err(|err| {
                    CollabError::DocFatal(format!(
                        "Can't access file {}: {:?}",
                        abs_path.to_string_lossy(),
                        err
                    ))
                })?;

                // We got the child file, now let's see if it's
                // already in the system as a doc.
                let included_doc = if let Some(idx) = children
                    .iter()
                    .position(|(child_rel_path, _)| *child_rel_path == rel_path)
                {
                    let (_, child_doc_id) = children[idx];
                    let doc = self
                        .docs
                        .lock()
                        .unwrap()
                        .get(&child_doc_id)
                        .map(|x| x.clone());
                    doc.map(|doc| (child_doc_id, doc.lock().unwrap().meta.clone()))
                } else {
                    None
                };

                let file_name = file.file_name().to_string_lossy().into();

                if let Some((child_doc_id, meta)) = included_doc {
                    res.push(DocInfo {
                        doc_desc: DocDesc::Doc(child_doc_id),
                        file_name,
                        file_meta: meta,
                    });
                } else {
                    let doc_desc = if meta.is_file() {
                        DocDesc::File((doc_id, rel_path))
                    } else {
                        DocDesc::Dir((doc_id, rel_path))
                    };
                    res.push(DocInfo {
                        doc_desc,
                        file_name,
                        file_meta: empty_json_map(),
                    });
                }
            }
            return Ok(res);
        } else {
            return Err(CollabError::DocNotFound(doc_id));
        };
    }

    #[instrument(skip(self))]
    pub async fn list_files_1(&self, dir_path: Option<FilePath>) -> CollabResult<Vec<DocInfo>> {
        tracing::debug!("Entered list_files");

        if let Some(dir_path) = dir_path {
            return self.list_directory(dir_path).await;
        }
        let mut res = vec![];
        // Clone the data out, because we don't want deadlocks, do we?
        // I'm not sure if there are places where we lock a doc first
        // and then lock self.docs, but let's just not take any risk.
        let docs: Vec<(u32, Arc<Mutex<Doc>>)> = self
            .docs
            .lock()
            .unwrap()
            .iter()
            .map(|(doc_id, doc)| (*doc_id, doc.clone()))
            .collect();
        for (doc_id, doc) in docs {
            // TODO permission check.
            // Only include top-level docs.
            let doc = doc.lock().unwrap();
            let no_path = doc.file_path.is_none();
            let meta = doc.meta.clone();
            if no_path {
                res.push(DocInfo {
                    doc_desc: DocDesc::Doc(doc_id.clone()),
                    file_name: doc.name.clone(),
                    file_meta: meta,
                })
            };
        }

        tracing::debug!("Got docs");

        let dirs: Vec<(u32, Arc<Mutex<Dir>>)> = self
            .dirs
            .lock()
            .unwrap()
            .iter()
            .map(|(dir_id, dir)| (*dir_id, dir.clone()))
            .collect();

        tracing::debug!("Got dirs");

        for (dir_id, dir) in dirs {
            let dir = dir.lock().unwrap();
            res.push(DocInfo {
                doc_desc: DocDesc::Dir((dir_id, PathBuf::from("."))),
                file_name: dir.name.clone(),
                file_meta: dir.meta.clone(),
            })
        }
        Ok(res)
    }

    #[instrument(skip(self))]
    pub async fn request_file_1(&self, doc_file: &DocDesc) -> CollabResult<Snapshot> {
        tracing::debug!("Entered request_file");

        match doc_file {
            DocDesc::Doc(doc_id) => {
                if let Some(doc) = self.get_doc(doc_id) {
                    let doc = doc.lock().unwrap();
                    doc.check_for_existing_error()?;
                    Ok(doc.snapshot(*doc_id))
                } else {
                    Err(CollabError::DocNotFound(doc_id.clone()))
                }
            }
            DocDesc::File((dir_id, rel_path)) => {
                if let Some(dir) = self.get_dir(dir_id) {
                    self.get_dir_file(&dir_id, &dir, rel_path).await
                } else {
                    // TODO: DirNotFound?
                    Err(CollabError::DocNotFound(*dir_id))
                }
            }
            DocDesc::Dir((dir_id, rel_path)) => {
                let path = format!("{dir_id}/{}", rel_path.to_string_lossy());
                Err(CollabError::NotRegularFile(path))
            }
        }
    }

    #[instrument(skip(self))]
    async fn delete_file_1(&self, doc_id: &DocId) -> CollabResult<()> {
        tracing::debug!("Entered delete_file");
        // Try remove it as a doc.
        let affected_dir = {
            let mut docs = self.docs.lock().unwrap();
            let maybe_doc = docs.remove(doc_id);
            if let Some(doc) = maybe_doc {
                let path = doc.lock().unwrap().file_path.clone();
                path.map(|(dir_id, _)| dir_id)
            } else {
                None
            }
        };
        // If it's a doc, remove it from its parent dir.
        if let Some(dir_id) = affected_dir {
            if let Some(dir) = self.dirs.lock().unwrap().get(&dir_id) {
                let mut dir = dir.lock().unwrap();
                if let Some(idx) = dir.children.iter().position(|(_, id)| *id == *doc_id) {
                    dir.children.remove(idx);
                }
            }
        }
        // Try remove it as a dir.
        {
            let mut dirs = self.dirs.lock().unwrap();
            dirs.remove(doc_id);
        }
        Ok(())
    }

    #[instrument(skip(self))]
    async fn send_info_1(&mut self, doc_id: &DocId, info: Info) -> CollabResult<()> {
        tracing::debug!("Entered send_info");

        if let Some(doc) = self.get_doc(doc_id) {
            let doc = doc.lock().unwrap();
            doc.check_for_existing_error()?;
            doc.tx_info.send(info).unwrap();
            Ok(())
        } else {
            Err(CollabError::DocNotFound(doc_id.clone()))
        }
    }
}

// *** Webrpc

#[instrument(skip(server_key_cert, server))]
pub async fn run_webrpc_server(
    server_id: ServerId,
    server_key_cert: ArcKeyCert,
    signaling_addr: String,
    server: LocalServer,
) -> CollabResult<()> {
    let mut listener = Listener::bind(server_id.clone(), server_key_cert, &signaling_addr).await?;

    tracing::info!(
        "Registered as {} at signaling server {}",
        server_id,
        signaling_addr
    );
    loop {
        let endpoint = listener.accept().await?;

        tracing::info!(endpoint.name, "Received connection");

        let span = tracing::info_span!("Handle connection", endpoint.name);
        let server_1 = server.clone();
        let _ = tokio::spawn(
            async move {
                let res = handle_connection(endpoint.clone(), server_1).await;
                if let Err(err) = res {
                    tracing::warn!(?err, "Error occurred when handling remote client")
                } else {
                    tracing::info!("Remote client closed connection");
                }
            }
            .instrument(span),
        );
    }
}

async fn handle_connection(mut endpoint: Endpoint, mut server: LocalServer) -> CollabResult<()> {
    let mut rx = endpoint.process_incoming_messages()?;
    while let Some(msg) = rx.recv().await {
        let msg = msg?;
        let req_id = msg.req_id;
        let res = handle_request(msg, &mut server, &mut endpoint).await;
        match res {
            Err(err) => {
                tracing::warn!(?err, "Non-fatal error when handling request");
                endpoint
                    .send_response(req_id, &DocServerResp::Err(err))
                    .await?;
            }
            Ok(Some(resp)) => {
                endpoint.send_response(req_id, &resp).await?;
            }
            Ok(None) => (),
        }
    }
    Ok(())
}

#[instrument(skip(msg, server, endpoint))]
async fn handle_request(
    msg: webrpc::Message,
    server: &mut LocalServer,
    endpoint: &mut Endpoint,
) -> CollabResult<Option<DocServerResp>> {
    let req = msg.unpack()?;

    tracing::debug!(?req, "handle_request");

    match req {
        DocServerReq::ShareFile {
            file_name,
            file_meta,
            content,
        } => {
            if let FileContentOrPath::Path(_) = &content {
                return Err(CollabError::UnsupportedOperation(
                    "Sharing directory to remote host".to_string(),
                ));
            }
            let file_meta = serde_json::from_str(&file_meta).unwrap();
            let doc_id = server
                .share_file_1(&file_name, &file_meta, content, None)
                .await?;
            Ok(Some(DocServerResp::ShareFile(doc_id)))
        }
        DocServerReq::ListFiles { dir_path } => {
            let dir_id = match dir_path {
                Some((dir_id, _)) => Some(dir_id),
                None => None,
            };
            let res = server.list_files_1(dir_path).await;
            match res {
                Ok(doc_info) => Ok(Some(DocServerResp::ListFiles(
                    doc_info.into_iter().map(|info| info.into()).collect(),
                ))),
                Err(err) => {
                    if let Some(dir_id) = dir_id {
                        server.attach_dir_error(&dir_id, err.clone());
                    }
                    Err(err)
                }
            }
        }
        DocServerReq::SendOp(ops) => {
            let doc_id = ops.doc();
            let res = server.send_op_1(ops).await;
            if let Err(err) = res {
                // FIXME: When do we garbage collect errored docs?
                server.attach_error(&doc_id, err.clone());
                return Err(err);
            }
            Ok(Some(DocServerResp::SendOp))
        }
        DocServerReq::SendInfo { doc_id, info } => {
            let res = server.send_info_1(&doc_id, info).await;
            if let Err(err) = res {
                // FIXME: When do we garbage collect errored docs?
                server.attach_error(&doc_id, err.clone());
                return Err(err);
            }
            Ok(Some(DocServerResp::SendInfo))
        }
        DocServerReq::RecvOpAndInfo { doc_id, after } => {
            let res = server.recv_op_1(&doc_id, after).await;
            if let Err(err) = &res {
                server.attach_error(&doc_id, err.clone());
            }
            let (mut stream_op, mut stream_info) = res?;
            let endpoint_1 = endpoint.clone();
            let endpoint_2 = endpoint.clone();
            let req_id = msg.req_id;
            // Send ops.
            let span = tracing::info_span!("Send ops for recv_op_info", doc_id);
            let _ = tokio::spawn(
                async move {
                    while let Some(op) = stream_op.next().await {
                        let res = endpoint_1
                            .send_response(req_id, &DocServerResp::RecvOp(op))
                            .await;
                        if let Err(err) = res {
                            tracing::warn!(?err, "Error sending ops to remote client");
                            return;
                        }
                    }
                    tracing::info!("Channel to remote client closed")
                }
                .instrument(span),
            );
            // Send info.
            let span = tracing::info_span!("Send info for recv_op_info", doc_id);
            let _ = tokio::spawn(
                async move {
                    while let Some(info) = stream_info.next().await {
                        let res = endpoint_2
                            .send_response(req_id, &DocServerResp::RecvInfo(info))
                            .await;
                        if let Err(err) = res {
                            tracing::warn!(?err, "Error sending info to remote client");
                            return;
                        }
                    }
                    tracing::info!("Channel to remote client closed")
                }
                .instrument(span),
            );
            Ok(None)
        }
        DocServerReq::RequestFile(doc_file) => {
            let snapshot = server.request_file_1(&doc_file).await?;
            Ok(Some(DocServerResp::RequestFile(snapshot)))
        }
        DocServerReq::DeleteFile(doc_id) => {
            server.delete_file_1(&doc_id).await?;
            Ok(Some(DocServerResp::DeleteFile))
        }
        DocServerReq::Login(credential) => {
            let mut user_list = server.user_list.lock().unwrap();
            let site_id = if let Some(site_id) = user_list.get(&credential) {
                site_id.clone()
            } else {
                // DLWARN: At this point we are still holding user_list.
                let mut next_site_id = server.next_site_id.lock().unwrap();
                let site_id = next_site_id.clone();
                *next_site_id += 1;
                user_list.insert(credential.clone(), site_id);
                site_id
            };
            Ok(Some(DocServerResp::Login(site_id)))
        }
    }
}
