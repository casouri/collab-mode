//! This module provides a [Doc] object, used by [crate::jspnrpc],
//! that encapsulates communicating to local and remote server and
//! sending and receiving ops.

//! [Doc] can be created by either sharing
//! a local file to a local or remote server [Doc::new_share_file], or
//! connecting to a document on a local or remote server by
//! [Doc::new_connect_file]. Editor can send local ops to the Doc and
//! receive (transformed) remotes ops to be applied.

//! [Doc] starts background threads to send/receive ops, if error
//! occurs, the error is captured and saved, the next time we caller
//! uses one of [Doc]'s methods (sendop, undo, and redo), the error
//! will be returned immediately. The caller should report the error
//! to the editor and drop the [Doc], which will clean up the
//! background threads.

use crate::abstract_server::{ClientEnum, DocServer};
use crate::error::{CollabError, CollabResult};
use crate::types::*;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::{mpsc, watch};
use tokio_stream::Stream;
use tokio_stream::StreamExt;

// *** Types

type OpStream = Pin<Box<dyn Stream<Item = CollabResult<Vec<FatOp>>> + Send>>;

// *** Structs

/// A client that connects to the local server and remote server.
/// Represents a local or remote document.
pub struct Doc {
    doc_id: DocId,
    engine: Arc<Mutex<ClientEngine>>,
    remote_op_buffer: Arc<Mutex<Vec<FatOp>>>,
    /// server: CollabServer,
    site_seq: LocalSeq,
    /// Notifier sender for remote op arrival AND local op arrival.
    new_ops_tx: Arc<watch::Sender<()>>,
    /// Notifier receiver for remote op arrival AND local op arrival.
    new_ops_rx: watch::Receiver<()>,
    /// Channel that receives async errors produced by worker threads.
    err_rx: mpsc::Receiver<CollabError>,
    /// Thread handlers used for terminate worker threads.
    thread_handlers: Vec<tokio::task::JoinHandle<()>>,
}

// *** Doc creation functions

impl Doc {
    /// Create a new Doc by sharing a file with the local server.
    pub async fn new_share_file(
        mut server: ClientEnum,
        file_name: &str,
        file: &str,
        external_notifier: std::sync::mpsc::Sender<NewOpNotification>,
    ) -> CollabResult<Doc> {
        // At most 2 errors from two worker threads.
        let (err_tx, err_rx) = mpsc::channel(2);
        let doc_id = server.share_file(file_name, file).await?;

        let mut doc = make_doc(server.site_id(), doc_id.clone(), 0, err_rx);

        let remote_op_buffer = Arc::clone(&doc.remote_op_buffer);
        let notifier_tx = Arc::clone(&doc.new_ops_tx);
        let notifier_rx = doc.new_ops_rx.clone();
        let engine = Arc::clone(&doc.engine);

        // Now server will spawn a thread that keeps sending new ops
        // to our channel.
        let stream = server.recv_op(&doc_id, 0).await?;

        // Spawn a thread that retrieves remote ops and add to
        // `remote_op_buffer`
        let handle = spawn_thread_receive_remote_op(
            doc_id.clone(),
            server.site_id(),
            server.server_id(),
            stream,
            remote_op_buffer,
            notifier_tx,
            err_tx.clone(),
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
            err_tx,
        );
        doc.thread_handlers.push(handle);

        Ok(doc)
    }

    /// Create a new Doc by connecting to a remote file through
    /// `client`.
    pub async fn new_connect_file(
        mut server: ClientEnum,
        doc_id: DocId,
        external_notifier: std::sync::mpsc::Sender<NewOpNotification>,
    ) -> CollabResult<(Doc, String)> {
        // Download file.
        let snapshot = server.request_file(&doc_id).await?;
        let stream = server.recv_op(&doc_id, snapshot.seq).await?;

        // At most 2 errors from two worker threads.
        let (err_tx, err_rx) = mpsc::channel(2);
        let site_id = server.site_id();
        let mut doc = make_doc(site_id.clone(), doc_id.clone(), snapshot.seq, err_rx);

        let remote_op_buffer = Arc::clone(&doc.remote_op_buffer);
        let notifier_tx = Arc::clone(&doc.new_ops_tx);
        let notifier_rx = doc.new_ops_rx.clone();
        let engine = Arc::clone(&doc.engine);

        // Spawn a thread that receives remote ops.
        let handle = spawn_thread_receive_remote_op(
            doc_id.clone(),
            server.site_id(),
            server.server_id(),
            stream,
            Arc::clone(&remote_op_buffer),
            notifier_tx,
            err_tx.clone(),
            external_notifier,
        );
        doc.thread_handlers.push(handle);

        // Spawn a thread that sends local ops.
        let handle = spawn_thread_send_local_op(doc_id, engine, server, notifier_rx, err_tx);
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
    pub async fn send_op(
        &mut self,
        ops: Vec<EditorFatOp>,
    ) -> CollabResult<(Vec<EditorLeanOp>, GlobalSeq)> {
        self.check_async_errors()?;

        // Get remote ops before locking engine.
        let remote_ops: Vec<FatOp> = {
            let mut remote_ops = self.remote_op_buffer.lock().await;
            remote_ops.drain(..).collect()
        };

        // 1. Process local ops.
        let mut engine = self.engine.lock().await;
        for op in ops {
            self.site_seq += 1;
            let fatop = FatOp {
                seq: None,
                doc: self.doc_id.clone(),
                site: engine.site_id(),
                op: op.op.into(),
                site_seq: self.site_seq,
                kind: OpKind::Original, // Use a dummy value.
                group_seq: op.group_seq,
            };
            engine.process_local_op(fatop, op.kind)?;
        }

        // 2. Process pending remote ops.
        let mut transformed_remote_ops: Vec<FatOp> = vec![];
        for op in remote_ops {
            if let Some(op) = engine.process_remote_op(op)? {
                transformed_remote_ops.push(op);
            }
        }

        // Try sending the new local ops. `spawn_thread_send_local_op`
        // will be waked by this signal and try to send the new local
        // ops.
        self.new_ops_tx.send(()).unwrap();

        let last_op = get_last_global_seq(&transformed_remote_ops)?.unwrap_or(0);

        Ok((
            transformed_remote_ops
                .into_iter()
                .map(|op| op.into())
                .collect(),
            last_op,
        ))
    }

    /// Return `n` consecutive undo ops from the current undo tip.
    pub async fn undo(&mut self) -> CollabResult<Vec<Op>> {
        self.check_async_errors()?;
        Ok(self.engine.lock().await.generate_undo_op())
    }

    /// Return `n` consecutive redo ops from the current undo tip.
    pub async fn redo(&mut self) -> CollabResult<Vec<Op>> {
        self.check_async_errors()?;
        Ok(self.engine.lock().await.generate_redo_op())
    }

    /// Handle errors created by worker threads.
    fn check_async_errors(&mut self) -> CollabResult<()> {
        if let Ok(err) = self.err_rx.try_recv() {
            return Err(err);
        } else {
            return Ok(());
        }
    }

    /// Display a log of the local history for debugging purpose. If
    /// `debug` is true, print more detailed (but more verbose)
    /// information.
    pub async fn print_history(&self, debug: bool) -> String {
        // self.check_async_errors()?;
        let mut output = String::new();
        output += self.engine.lock().await.print_history(debug).as_str();
        output
    }
}

// *** Subroutines for Doc::new_share_file and new_connect_file

fn make_doc(
    site_id: SiteId,
    doc_id: DocId,
    base_seq: GlobalSeq,
    err_rx: mpsc::Receiver<CollabError>,
) -> Doc {
    let engine = Arc::new(Mutex::new(ClientEngine::new(site_id.clone(), base_seq)));
    let remote_op_buffer = Arc::new(Mutex::new(vec![]));
    let (notifier_tx, notifier_rx) = watch::channel(());
    let thread_handlers = vec![];
    Doc {
        doc_id: doc_id.clone(),
        engine: engine.clone(),
        remote_op_buffer,
        site_seq: 0,
        new_ops_tx: Arc::new(notifier_tx),
        new_ops_rx: notifier_rx.clone(),
        err_rx,
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
    remote_op_buffer: Arc<Mutex<Vec<FatOp>>>,
    notifier_tx: Arc<watch::Sender<()>>,
    error_channel: mpsc::Sender<CollabError>,
    external_notifier: std::sync::mpsc::Sender<NewOpNotification>,
) -> tokio::task::JoinHandle<()> {
    // Draining remote_op_stream and storing ops into a buffer seems
    // redundant, but is easier to write and understand in the big
    // picture.
    tokio::spawn(async move {
        loop {
            let new_remote_ops = remote_op_stream.next().await;
            if new_remote_ops.is_none() {
                let err = CollabError::ChannelClosed(format!(
                    "Doc({}) Internal channel (local server --op--> local client) broke",
                    &doc_id
                ));
                error_channel.send(err).await.unwrap();
                return;
            };
            let new_remote_ops = new_remote_ops.unwrap();
            if let Err(err) = new_remote_ops {
                error_channel.send(err).await.unwrap();
                return;
            }
            let ops = new_remote_ops.unwrap();
            log::debug!(
                "Doc({}) Received ops from local server: {:?}",
                &doc_id,
                &ops
            );
            let truly_remote_op_arrived = ops
                .iter()
                .map(|op| op.site != site_id)
                .reduce(|acc, flag| acc || flag);
            if truly_remote_op_arrived.is_none() {
                continue;
            }
            let truly_remote_op_arrived = truly_remote_op_arrived.unwrap();

            let mut last_global_seq = 0;
            match get_last_global_seq(&ops) {
                Err(err) => {
                    error_channel.send(err).await.unwrap();
                }
                Ok(seq) => {
                    if let Some(seq) = seq {
                        last_global_seq = seq;
                    } else {
                        continue;
                    }
                }
            };

            remote_op_buffer.lock().await.extend(ops);

            // Got some op from server, maybe we can send
            // new local ops now?
            notifier_tx.send(()).unwrap();

            // Notify JSONRPC server to notify the editor.
            if truly_remote_op_arrived {
                let res = external_notifier.send(NewOpNotification {
                    doc_id: doc_id.clone(),
                    server_id: server_id.clone(),
                    last_seq: last_global_seq,
                });
                if let Err(err) = res {
                    let err = CollabError::ChannelClosed(format!(
                    "Doc({}) Internal notification channel (Doc --(doc,server)--> jsonrpc) broke: {:#}",
                    &doc_id, err
                ));
                    error_channel.send(err).await.unwrap();
                    return;
                }
            }
        }
    })
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
    error_channel: mpsc::Sender<CollabError>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            notifier_rx.changed().await.unwrap();
            let ops = {
                let mut engine = engine.lock().await;
                engine.maybe_package_local_ops()
            };
            if let Some(ops) = ops {
                log::debug!("Doc({}) Sending op to server: {:?}", &doc_id, &ops);
                if let Err(err) = server.send_op(ops).await {
                    error_channel.send(err).await.unwrap();
                    return;
                }
            }
        }
    })
}

fn get_last_global_seq(ops: &[FatOp]) -> CollabResult<Option<GlobalSeq>> {
    if let Some(last_op) = ops.last() {
        if let Some(last_global_seq) = last_op.seq {
            Ok(Some(last_global_seq))
        } else {
            let err = CollabError::TransportErr(format!(
                "Remote op doesn't have a global seq {:?}",
                last_op
            ));
            Err(err)
        }
    } else {
        Ok(None)
    }
}
