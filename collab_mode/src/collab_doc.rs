//! This module provides a [Doc] object, used by [crate::jspnrpc],
//! that encapsulates communicating to local and remote server and
//! sending and receiving ops.
//!
//! [Doc] can be created by either sharing
//! a local file to a local or remote server [Doc::new_share_file], or
//! connecting to a document on a local or remote server by
//! [Doc::new_connect_file]. Editor can send local ops to the Doc and
//! receive (transformed) remotes ops to be applied.
//!
//! [Doc] starts background threads to send/receive ops, if error
//! occurs, the error is captured and saved, the next time the caller
//! uses one of [Doc]'s methods (sendop, undo, and redo), the error
//! will be returned immediately. The caller should report the error
//! to the editor and drop the [Doc], which will clean up the
//! background threads.

use crate::abstract_server::{ClientEnum, DocServer, InfoStream, OpStream};
use crate::error::{convert_remote_err, CollabError, CollabResult};
use crate::types::*;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tokio_stream::StreamExt;
use tracing::{instrument, Instrument};

// *** Structs

/// The status of a doc. Usually should be live, when everything is
/// going fine, if connection broke, doc goes into offline mode, and
/// can be resurrected from offline mode to live by reconnecting to
/// remote host. If there's a fatal error, the doc goes into fatal
/// mode and can't be recovered anymore.
#[derive(Debug, Clone)]
pub enum DocStatus {
    /// Well and good.
    Live,
    /// Connection broke due to this error.
    Offline(CollabError),
    /// Fatal error.
    Fatal(CollabError),
}

impl From<CollabError> for DocStatus {
    fn from(value: CollabError) -> Self {
        match &value {
            CollabError::RpcError(_) => Self::Offline(value),
            _ => Self::Fatal(value),
        }
    }
}

impl DocStatus {
    fn is_live(&self) -> bool {
        match self {
            Self::Live => true,
            _ => false,
        }
    }
    fn is_fatal(&self) -> bool {
        match self {
            Self::Fatal(_) => true,
            _ => false,
        }
    }
}

/// A client that connects to the local server and remote server.
/// Represents a local or remote document.
#[derive(Debug)]
pub struct Doc {
    doc_id: DocId,
    file_name: String,
    engine: Arc<Mutex<ClientEngine>>,
    remote_op_buffer: Arc<Mutex<Vec<FatOp>>>,
    /// server: CollabServer,
    site_seq: LocalSeq,
    /// Notifier sender for remote op arrival AND local op arrival.
    new_ops_tx: Arc<watch::Sender<()>>,
    /// Notifier receiver for remote op arrival AND local op arrival.
    new_ops_rx: watch::Receiver<()>,
    /// Channel that receives async errors produced by worker threads.
    /// If there are errors in this channel, the doc has encountered a
    /// fatal error and must be removed and recreated.
    status: Arc<Mutex<DocStatus>>,
    /// Thread handlers used for terminating worker threads.
    thread_handlers: Vec<tokio::task::JoinHandle<()>>,
}

// *** Doc creation functions

impl Doc {
    /// Create a new Doc by sharing a file with the local server.
    #[instrument(skip(server, file_meta, file, external_notifier))]
    pub async fn new_share_file(
        mut server: ClientEnum,
        file_name: &str,
        file_meta: &JsonMap,
        file: String,
        external_notifier: std::sync::mpsc::Sender<CollabNotification>,
    ) -> CollabResult<Doc> {
        let file_len = file.chars().count() as u64;
        let doc_id = server
            .share_file(file_name, file_meta, FileContentOrPath::Content(file))
            .await?;

        let mut doc = make_doc(
            server.site_id(),
            doc_id.clone(),
            file_name.to_string(),
            file_len,
            0,
        );

        let remote_op_buffer = Arc::clone(&doc.remote_op_buffer);
        let notifier_tx = Arc::clone(&doc.new_ops_tx);
        let notifier_rx = doc.new_ops_rx.clone();
        let engine = Arc::clone(&doc.engine);

        // Now server will spawn a thread that keeps sending new ops
        // to our channel.
        let (op_stream, info_stream) = server.recv_op_and_info(&doc_id, 0).await?;

        // Spawn a thread that retrieves remote ops and add to
        // `remote_op_buffer`
        let handle = spawn_thread_receive_remote_op(
            doc_id.clone(),
            server.site_id(),
            server.server_id(),
            op_stream,
            info_stream,
            remote_op_buffer,
            notifier_tx,
            doc.status.clone(),
            external_notifier,
        );
        doc.thread_handlers.push(handle);

        // Spawn a thread that keeps trying to send out local ops when
        // appropriate.
        let handle = spawn_thread_send_local_op(
            doc_id.clone(),
            Arc::clone(&engine),
            server,
            notifier_rx.clone(),
            doc.status.clone(),
        );
        doc.thread_handlers.push(handle);

        Ok(doc)
    }

    /// Create a new Doc by connecting to a remote file through
    /// `client`.
    #[instrument(skip(server, external_notifier))]
    pub async fn new_connect_file(
        mut server: ClientEnum,
        doc_file: DocDesc,
        external_notifier: std::sync::mpsc::Sender<CollabNotification>,
    ) -> CollabResult<(Doc, String)> {
        // Download file.
        let snapshot = server.request_file(&doc_file).await?;
        let doc_id = snapshot.doc_id;
        let (op_stream, info_stream) = server.recv_op_and_info(&doc_id, snapshot.seq).await?;

        // At most 2 errors from two worker threads.

        let site_id = server.site_id();
        let mut doc = make_doc(
            site_id.clone(),
            doc_id.clone(),
            snapshot.file_name,
            snapshot.buffer.chars().count() as u64,
            snapshot.seq,
        );

        let remote_op_buffer = Arc::clone(&doc.remote_op_buffer);
        let notifier_tx = Arc::clone(&doc.new_ops_tx);
        let notifier_rx = doc.new_ops_rx.clone();
        let engine = Arc::clone(&doc.engine);

        // Spawn a thread that receives remote ops.
        let handle = spawn_thread_receive_remote_op(
            doc_id.clone(),
            server.site_id(),
            server.server_id(),
            op_stream,
            info_stream,
            Arc::clone(&remote_op_buffer),
            notifier_tx,
            doc.status.clone(),
            external_notifier,
        );
        doc.thread_handlers.push(handle);

        // Spawn a thread that sends local ops.
        let handle =
            spawn_thread_send_local_op(doc_id, engine, server, notifier_rx, doc.status.clone());
        doc.thread_handlers.push(handle);

        Ok((doc, snapshot.buffer))
    }
}

// *** Doc cleanup

impl Drop for Doc {
    fn drop(&mut self) {
        for handle in &self.thread_handlers {
            handle.abort();
        }
    }
}

// *** Doc functions

impl Doc {
    /// Get the id of this doc.
    pub fn doc_id(&self) -> DocId {
        self.doc_id.clone()
    }

    /// Get the file name of this doc.
    pub fn file_name(&self) -> String {
        self.file_name.clone()
    }

    /// Get the notifier channel for this doc. The channel is notified
    /// when remote op arrives. (This is for JSON-RPC server to send
    /// notification to the editor.)
    pub fn get_notifier(&self) -> watch::Receiver<()> {
        self.new_ops_rx.clone()
    }

    /// Send local `ops` and retrieve remote ops. `ops` can be empty,
    /// in which case the purpose is solely retrieving accumulated
    /// remote ops. Return the transformed remote ops that editor can
    /// apply to its document, plus the last seq number among the ops
    /// returned.
    #[instrument(skip(self, ops))]
    pub fn send_op(
        &mut self,
        ops: Vec<EditorFatOp>,
    ) -> CollabResult<Option<(Vec<EditorLeanOp>, GlobalSeq)>> {
        self.check_async_errors()?;

        tracing::debug!(?ops, "send_op");

        // Get remote ops before locking engine.
        let remote_ops: Vec<FatOp> = {
            let mut remote_ops = self.remote_op_buffer.lock().unwrap();
            remote_ops.drain(..).collect()
        };

        tracing::info!(?remote_ops);

        // 1. Process local ops.
        let mut engine = self.engine.lock().unwrap();
        for op in ops {
            self.site_seq += 1;
            engine.process_local_op(op, self.doc_id.clone(), self.site_seq.clone())?;
        }

        // 2. Process pending remote ops.
        let mut last_op = 0;
        let mut transformed_remote_ops: Vec<EditorLeanOp> = vec![];
        for op in remote_ops {
            if let Some((op, seq)) = engine.process_remote_op(op)? {
                transformed_remote_ops.push(op);
                last_op = seq;
            }
        }

        tracing::debug!(?transformed_remote_ops);

        // Try sending the new local ops. `spawn_thread_send_local_op`
        // will be waked by this signal and try to send the new local
        // ops.
        self.new_ops_tx.send(()).unwrap();

        // We want to avoid sending seq number = 0.
        if transformed_remote_ops.len() > 0 {
            Ok(Some((transformed_remote_ops, last_op)))
        } else {
            Ok(None)
        }
    }

    /// Return `n` consecutive undo ops from the current undo tip.
    pub fn undo(&mut self) -> CollabResult<Vec<EditInstruction>> {
        self.check_async_errors()?;
        Ok(self.engine.lock().unwrap().generate_undo_op())
    }

    /// Return `n` consecutive redo ops from the current undo tip.
    pub fn redo(&mut self) -> CollabResult<Vec<EditInstruction>> {
        self.check_async_errors()?;
        Ok(self.engine.lock().unwrap().generate_redo_op())
    }

    /// Handle errors created by background threads.
    fn check_async_errors(&mut self) -> CollabResult<()> {
        let status = self.status.lock().unwrap();
        match &*status {
            DocStatus::Live => Ok(()),
            DocStatus::Offline(err) => Err(err.clone()),
            DocStatus::Fatal(err) => Err(err.clone()),
        }
    }

    /// Display a log of the local history for debugging purpose. If
    /// `debug` is true, print more detailed (but more verbose)
    /// information.
    pub fn print_history(&self, debug: bool) -> String {
        // self.check_async_errors()?;
        let mut output = String::new();
        output += self.engine.lock().unwrap().print_history(debug).as_str();
        output
    }
}

// *** Subroutines for Doc::new_share_file and new_connect_file

fn make_doc(
    site_id: SiteId,
    doc_id: DocId,
    file_name: String,
    init_len: u64,
    base_seq: GlobalSeq,
) -> Doc {
    let engine = Arc::new(Mutex::new(ClientEngine::new(
        site_id.clone(),
        base_seq,
        init_len,
    )));
    let remote_op_buffer = Arc::new(Mutex::new(vec![]));
    let (notifier_tx, notifier_rx) = watch::channel(());
    let thread_handlers = vec![];
    Doc {
        doc_id: doc_id.clone(),
        engine: engine.clone(),
        file_name,
        remote_op_buffer,
        site_seq: 0,
        new_ops_tx: Arc::new(notifier_tx),
        new_ops_rx: notifier_rx.clone(),
        status: Arc::new(Mutex::new(DocStatus::Live)),
        thread_handlers,
    }
}

/// Spawn a thread that retrieves remote ops and add to
/// `remote_op_buffer`, whenever it receives ops, sends a unit to
/// `notifier_tx` as notification.
fn spawn_thread_receive_remote_op(
    doc_id: DocId,
    site_id: SiteId,
    server_id: ServerId,
    mut remote_op_stream: OpStream,
    mut remote_info_stream: InfoStream,
    remote_op_buffer: Arc<Mutex<Vec<FatOp>>>,
    notifier_tx: Arc<watch::Sender<()>>,
    mut status: Arc<Mutex<DocStatus>>,
    external_notifier: std::sync::mpsc::Sender<CollabNotification>,
) -> tokio::task::JoinHandle<()> {
    // Receive info.
    let mut status_1 = status.clone();
    let external_notifier_1 = external_notifier.clone();
    let doc_id_1 = doc_id.clone();
    let server_id_1 = server_id.clone();
    let span = tracing::info_span!("receiving remote info", doc_id, site_id, server_id);
    tokio::spawn(async move {
        while let Some(info) = remote_info_stream.next().await {
            match info {
                Err(err) => {
                    maybe_set_status(&mut status_1, err);
                    return;
                }
                Ok(info) => {
                    if info.sender == site_id {
                        continue;
                    }
                    match serde_json::from_str(&info.value) {
                        Ok(value) => {
                            external_notifier_1
                                .send(CollabNotification::Info(InfoNotification {
                                    doc_id: doc_id_1.clone(),
                                    host_id: server_id_1.clone(),
                                    sender_site_id: info.sender,
                                    value,
                                }))
                                // external_notifier is never dropped,
                                // so just unwrap.
                                .unwrap();
                        }
                        Err(err) => {
                            // Failure to parse an info isn't a big
                            // deal, inform the editor about it and
                            // live on.
                            let err: CollabError = err.into();
                            external_notifier_1
                                .send(CollabNotification::HarmlessErr(format!("{:#?}", err)))
                                // external_notifier is never dropped,
                                // so just unwrap.
                                .unwrap();
                        }
                    }
                }
            }
        }
        // Info stream broke, meaning connection broke.
        let mut status = status_1.lock().unwrap();
        if !status.is_live() {
            let err = CollabError::DocFatal(format!(
                "Connection for Doc({}) broke: internal channel (local server --info--> local client) broke",
                &doc_id
            ));
            *status = err.into();
        }
    }.instrument(span));

    let span = tracing::info_span!("receiving remote ops", doc_id, site_id, server_id);
    // Receive op.
    //
    // Draining remote_op_stream and storing ops into a buffer seems
    // redundant, but is easier to write and understand in the big
    // picture. Also, make sure we add the remote op into the vector
    // before sending "remote op arrived" signal to editor.
    tokio::spawn(
        async move {
            loop {
                let new_remote_ops = remote_op_stream.next().await;
                if new_remote_ops.is_none() {
                    // Connection broke.
                    let err = CollabError::RpcError(format!(
                        "Connection for receiving doc({}) ops broke",
                        &doc_id
                    ));
                    maybe_set_status(&mut status, err);
                    return;
                };
                let new_remote_ops = new_remote_ops.unwrap();

                // Remove host sent error instead of ops.
                if let Err(err) = new_remote_ops {
                    let err = convert_remote_err(err);
                    maybe_set_status(&mut status, err);
                    return;
                }
                let ops = new_remote_ops.unwrap();

                tracing::debug!(?ops, "Received ops");

                let truly_remote_op_arrived = ops
                    .iter()
                    .map(|op| op.site() != site_id)
                    .reduce(|acc, flag| acc || flag);
                if truly_remote_op_arrived.is_none() {
                    continue;
                }
                let truly_remote_op_arrived = truly_remote_op_arrived.unwrap();

                let last_global_seq;
                match get_last_global_seq(&ops) {
                    Err(err) => {
                        // Remote op doesn't have a sequence number, this
                        // is doc-fatal.
                        maybe_set_status(&mut status, err);
                        return;
                    }
                    Ok(seq) => {
                        if let Some(seq) = seq {
                            last_global_seq = seq;
                        } else {
                            // ops is an empty array, go to next iter.
                            continue;
                        }
                    }
                };

                remote_op_buffer.lock().unwrap().extend(ops);

                // Got some op from server, maybe we can send
                // new local ops now?
                notifier_tx.send(()).unwrap();

                // Notify JSONRPC server to notify the editor.
                if truly_remote_op_arrived {
                    let notification = CollabNotification::Op(NewOpNotification {
                        doc_id: doc_id.clone(),
                        host_id: server_id.clone(),
                        last_seq: last_global_seq,
                    });
                    external_notifier
                        .send(notification)
                        // external_notifier is never dropped while
                        // the program is running, so just unwrap.
                        .unwrap();
                }
            }
        }
        .instrument(span),
    )
}

/// Spawn a thread that keeps trying get local ops from `engine` and
/// send out to `server` when appropriate. It tries to get local ops
/// when ever `notifier_rx` is notified. Errors are sent to
/// `error_channel`.
fn spawn_thread_send_local_op(
    doc_id: DocId,
    engine: Arc<Mutex<ClientEngine>>,
    mut server: ClientEnum,
    mut notifier_rx: watch::Receiver<()>,
    mut status: Arc<Mutex<DocStatus>>,
) -> tokio::task::JoinHandle<()> {
    let span = tracing::info_span!("Send local ops", doc_id);
    tokio::spawn(
        async move {
            loop {
                notifier_rx.changed().await.unwrap();
                let ops = {
                    let mut engine = engine.lock().unwrap();
                    engine.maybe_package_local_ops()
                };
                if let Some(ops) = ops {
                    tracing::info!(?ops, "Sending ops to server");
                    if let Err(err) = server.send_op(ops).await {
                        maybe_set_status(&mut status, err);
                        return;
                    }
                }
            }
        }
        .instrument(span),
    )
}

fn get_last_global_seq(ops: &[FatOp]) -> CollabResult<Option<GlobalSeq>> {
    if let Some(last_op) = ops.last() {
        if let Some(last_global_seq) = last_op.seq {
            Ok(Some(last_global_seq))
        } else {
            let err =
                CollabError::DocFatal(format!("Remote op doesn't have a global seq {:?}", last_op));
            Err(err)
        }
    } else {
        Ok(None)
    }
}

/// Set `status` according to err. But if `status` is already in fatal or offline state, don't change it.
fn maybe_set_status(status: &mut Arc<Mutex<DocStatus>>, err: CollabError) {
    let mut status = status.lock().unwrap();
    if status.is_fatal() {
        return;
    }
    let new_status: DocStatus = err.into();
    tracing::info!(?new_status, "Doc status changed");
    // Don't allow going from fatal to offline, but allow going from
    // offline to fatal.
    if status.is_live() || new_status.is_fatal() {
        *status = new_status;
    }
}
