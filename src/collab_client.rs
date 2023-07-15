//! This module provides a [Doc] object, used by [crate::jspnrpc],
//! that encapsulates communicating to local and remote server and
//! sending and receiving ops.

//! [Doc] can be created by either sharing
//! a local file to a local or remote server [Doc::new_share_file], or
//! connecting to a document on a local or remote server by
//! [Doc::new_connect_file]. Editor can send local ops to the Doc and
//! receive (transformed) remotes ops to be applied.

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

type OpStream = Pin<Box<dyn Stream<Item = CollabResult<FatOp>> + Send>>;

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
        external_notifier: std::sync::mpsc::Sender<DocDesignator>,
    ) -> CollabResult<Doc> {
        // At most 2 errors from two worker threads.
        let (err_tx, err_rx) = mpsc::channel(2);
        let doc_id = server.share_file(file_name, file).await?;

        let mut doc = make_doc(server.site_id(), doc_id.clone(), err_rx);

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
        external_notifier: std::sync::mpsc::Sender<DocDesignator>,
    ) -> CollabResult<(Doc, String)> {
        // At most 2 errors from two worker threads.
        let (err_tx, err_rx) = mpsc::channel(2);
        let site_id = server.site_id();
        let mut doc = make_doc(site_id.clone(), doc_id.clone(), err_rx);

        let remote_op_buffer = Arc::clone(&doc.remote_op_buffer);
        let notifier_tx = Arc::clone(&doc.new_ops_tx);
        let notifier_rx = doc.new_ops_rx.clone();
        let engine = Arc::clone(&doc.engine);

        // Download file.
        let snapshot = server.request_file(&doc_id).await?;
        let stream = server.recv_op(&doc_id, snapshot.seq).await?;

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
    /// remote ops.
    pub async fn send_op(&mut self, ops: Vec<Op>, kind: OpKind) -> CollabResult<Vec<Op>> {
        self.check_async_errors()?;

        // 1. Package ops.
        let mut site_seq = self.site_seq;
        let mut engine = self.engine.lock().await;
        let fatops: Vec<FatOp> = ops
            .into_iter()
            .map(|op| {
                site_seq += 1;
                FatOp {
                    seq: None,
                    doc: self.doc_id.clone(),
                    site: engine.site_id(),
                    op,
                    site_seq,
                }
            })
            .collect();
        self.site_seq = site_seq;

        // 2. Process local ops.
        for op in &fatops {
            engine.process_local_op(op.clone(), kind)?;
        }

        // 3. Process pending remote ops.
        let mut remote_ops = self.remote_op_buffer.lock().await;
        let remote_ops = remote_ops.drain(..);
        let mut transformed_remote_ops: Vec<FatOp> = vec![];
        for op in remote_ops {
            if let Some(op) = engine.process_remote_op(op)? {
                transformed_remote_ops.push(op);
            }
        }

        // Try sending the new local ops.
        self.new_ops_tx.send(()).unwrap();

        Ok(transformed_remote_ops.into_iter().map(|op| op.op).collect())
    }

    /// Return `n` consecutive undo ops from the current undo tip.
    pub async fn undo(&self) -> Option<Op> {
        self.engine.lock().await.generate_undo_op()
    }

    /// Return `n` consecutive redo ops from the current undo tip.
    pub async fn redo(&self) -> Option<Op> {
        self.engine.lock().await.generate_redo_op()
    }

    /// Handle errors created by worker threads.
    fn check_async_errors(&mut self) -> CollabResult<()> {
        if let Ok(err) = self.err_rx.try_recv() {
            return Err(err);
        } else {
            return Ok(());
        }
    }
}

// *** Subroutines for Doc::new_share_file and new_connect_file

fn make_doc(site_id: SiteId, doc_id: DocId, err_rx: mpsc::Receiver<CollabError>) -> Doc {
    let engine = Arc::new(Mutex::new(ClientEngine::new(site_id.clone())));
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
    external_notifier: std::sync::mpsc::Sender<DocDesignator>,
) -> tokio::task::JoinHandle<()> {
    let msg = DocDesignator {
        doc: doc_id.clone(),
        server: server_id,
    };
    tokio::spawn(async move {
        loop {
            let new_remote_op = remote_op_stream.next().await;
            if new_remote_op.is_none() {
                let err = CollabError::ChannelClosed(format!(
                    "Doc({}) Internal channel (local server --op--> local client) broke",
                    &doc_id
                ));
                error_channel.send(err).await.unwrap();
                return;
            };
            let new_remote_op = new_remote_op.unwrap();
            if let Err(err) = new_remote_op {
                error_channel.send(err).await.unwrap();
                return;
            }
            let op = new_remote_op.unwrap();
            log::debug!("Doc({}) Received op from local server: {:?}", &doc_id, &op);

            let truly_remote_op_arrived = op.site != site_id;
            remote_op_buffer.lock().await.push(op);

            // Got some op from server, maybe we can send
            // new local ops now?
            notifier_tx.send(()).unwrap();

            // Notify JSONRPC server to notify the editor.
            if truly_remote_op_arrived {
                let res = external_notifier.send(msg.clone());
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
