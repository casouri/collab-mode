use crate::collab_server::LocalServer;
use crate::collab_server::Snapshot;
use crate::error::{CollabError, CollabResult};
use crate::op::{DocId, GlobalSeq, LocalSeq, Op, SiteId};
use crate::types::*;
use anyhow::Context;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::{mpsc, watch};
use tonic::Request;

use crate::rpc;
use crate::rpc::doc_server_client::DocServerClient;

// *** Types

type FatOp = crate::op::FatOp<Op>;
type ContextOps = crate::engine::ContextOps<Op>;
type ServerEngine = crate::engine::ServerEngine<Op>;
type ClientEngine = crate::engine::ClientEngine<Op>;

// *** Structs

/// A client that connects to the local server and remote server.
/// Represents a local or remote document, users can send ops to the
/// Doc and receive ops from the Doc. A [Doc] can be created by
/// sharing a file ([Doc::new_share_file]), or connecting to a file
/// ([Doc::new_connect_file]).
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

/// [GrpcClient] represents a gRPC client connecting to a remote
/// server. Users can list files using the client. [GrpcClient] can be
/// shared by cloning, just like the tonic client.
#[derive(Debug, Clone)]
pub struct GrpcClient {
    client: DocServerClient<tonic::transport::Channel>,
    credential: Credential,
    site_id: SiteId,
    server_id: ServerId,
}

// *** Doc creation functions

impl Doc {
    /// Create a new Doc by sharing a file with the local server.
    pub async fn new_share_file(
        server: LocalServer,
        file_name: &str,
        file: &str,
        external_notifier: std::sync::mpsc::Sender<DocDesignator>,
    ) -> CollabResult<Doc> {
        // At most 2 errors from two worker threads.
        let (err_tx, err_rx) = mpsc::channel(2);
        let doc_id = server.share_file(file_name, file).await?;

        let mut doc = make_doc(SITE_ID_SELF.to_string(), doc_id.clone(), err_rx);

        let remote_op_buffer = Arc::clone(&doc.remote_op_buffer);
        let notifier_tx = Arc::clone(&doc.new_ops_tx);
        let notifier_rx = doc.new_ops_rx.clone();
        let engine = Arc::clone(&doc.engine);

        let (remote_op_tx, remote_op_rx) = mpsc::unbounded_channel();

        // Now server will spawn a thread that keeps sending new ops
        // to our channel.
        server.recv_op(&doc_id, 0, remote_op_tx).await?;

        // Spawn a thread that retrieves remote ops and add to
        // `remote_op_buffer`
        let handle = spawn_thread_retreive_remote_op_from_docserver(
            doc_id.clone(),
            remote_op_rx,
            remote_op_buffer,
            notifier_tx,
            err_tx.clone(),
            external_notifier,
        );
        doc.thread_handlers.push(handle);

        // Spawn a thread that keeps trying to send out local ops when
        // appropriate.
        let handle = spawn_thread_send_local_op_to_docserver(
            doc_id.clone(),
            Arc::clone(&engine),
            server.clone(),
            notifier_rx.clone(),
            err_tx,
        );
        doc.thread_handlers.push(handle);

        Ok(doc)
    }

    /// Create a new Doc by connecting to a remote file through
    /// `client`.
    pub async fn new_connect_file(
        mut client: GrpcClient,
        doc_id: DocId,
        external_notifier: std::sync::mpsc::Sender<DocDesignator>,
    ) -> CollabResult<(Doc, String)> {
        // At most 2 errors from two worker threads.
        let (err_tx, err_rx) = mpsc::channel(2);
        let site_id = client.site_id();
        let mut doc = make_doc(site_id.clone(), doc_id.clone(), err_rx);

        let remote_op_buffer = Arc::clone(&doc.remote_op_buffer);
        let notifier_tx = Arc::clone(&doc.new_ops_tx);
        let notifier_rx = doc.new_ops_rx.clone();
        let engine = Arc::clone(&doc.engine);

        // Download file.
        let snapshot = download_file_from_grpc(doc_id.clone(), &mut client.client).await?;

        // Spawn a thread that receives remote ops.
        let handle = spawn_thread_receive_remote_op_from_grpc(
            doc_id.clone(),
            client.clone(),
            snapshot.seq,
            Arc::clone(&remote_op_buffer),
            err_tx.clone(),
            notifier_tx,
            external_notifier,
        )
        .await?;
        doc.thread_handlers.push(handle);

        // Spawn a thread that sends local ops.
        let handle =
            spawn_thread_send_local_op_to_grpc(doc_id, client.client, engine, err_tx, notifier_rx);
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
    pub async fn send_op(&mut self, ops: Vec<Op>) -> CollabResult<Vec<Op>> {
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
            engine.process_local_op(op.clone())?;
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

    /// Handle errors created by worker threads.
    fn check_async_errors(&mut self) -> CollabResult<()> {
        if let Ok(err) = self.err_rx.try_recv() {
            return Err(err);
        } else {
            return Ok(());
        }
    }
}

// *** Functions for GrpcClient

impl GrpcClient {
    /// Create a client by connecting to `server_addr` with `credential`.
    pub async fn new(server_addr: ServerId, credential: Credential) -> CollabResult<GrpcClient> {
        let mut client = rpc::doc_server_client::DocServerClient::connect(server_addr.clone())
            .await
            .map_err(|err| CollabError::TransportErr(err.to_string()))?;
        let resp = client
            .login(Request::new(rpc::Credential {
                cred: credential.clone(),
            }))
            .await?;
        let site_id = resp.into_inner().id;
        Ok(GrpcClient {
            client,
            credential,
            site_id,
            server_id: server_addr,
        })
    }

    /// List files on the server.
    pub async fn list_files(&mut self) -> CollabResult<Vec<DocId>> {
        let resp = self.client.list_files(Request::new(rpc::Empty {})).await?;
        Ok(resp.into_inner().doc_id)
    }

    /// Return the site id given to us by the connected server.
    pub fn site_id(&self) -> SiteId {
        self.site_id.clone()
    }

    /// Return the server id.
    pub fn server_id(&self) -> ServerId {
        self.server_id.clone()
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
fn spawn_thread_retreive_remote_op_from_docserver(
    doc_id: DocId,
    mut remote_op_rx: mpsc::UnboundedReceiver<FatOp>,
    remote_op_buffer: Arc<Mutex<Vec<FatOp>>>,
    notifier_tx: Arc<watch::Sender<()>>,
    error_channel: mpsc::Sender<CollabError>,
    external_notifier: std::sync::mpsc::Sender<DocDesignator>,
) -> tokio::task::JoinHandle<()> {
    let msg = DocDesignator {
        doc: doc_id.clone(),
        server: SERVER_ID_SELF.to_string(),
    };
    tokio::spawn(async move {
        loop {
            let mut truly_remote_op_arrived = false;
            let new_remote_op = remote_op_rx.recv().await;
            if let Some(op) = new_remote_op {
                log::debug!("Doc({}) Received op from local server: {:?}", &doc_id, &op);
                if &op.site != SITE_ID_SELF {
                    truly_remote_op_arrived = true;
                }
                remote_op_buffer.lock().await.push(op);
            } else {
                let err = CollabError::ChannelClosed(format!(
                    "Doc({}) Internal channel (local server --op--> local client) broke",
                    &doc_id
                ));
                error_channel.send(err).await.unwrap();
                return;
            }
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
fn spawn_thread_send_local_op_to_docserver(
    doc_id: DocId,
    engine: Arc<Mutex<ClientEngine>>,
    server: LocalServer,
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
                log::debug!("Doc({}) Sending op to local server: {:?}", &doc_id, &ops);
                if let Err(err) = server.send_op(ops).await {
                    error_channel.send(err).await.unwrap();
                    return;
                }
            }
        }
    })
}

/// Download file `doc_id` from grpc `client`.
async fn download_file_from_grpc(
    doc_id: DocId,
    client: &mut DocServerClient<tonic::transport::Channel>,
) -> CollabResult<Snapshot> {
    let resp = client
        .request_file(Request::new(rpc::DocId {
            doc_id: doc_id.clone(),
        }))
        .await?;
    let mut stream = resp.into_inner();

    let mut file_content = String::new();
    let mut seq = 0;
    loop {
        let resp = stream.message().await?;
        if let Some(chunk) = resp {
            seq = chunk.seq;
            file_content.push_str(&chunk.content);
        } else {
            break;
        }
    }
    Ok(Snapshot {
        buffer: file_content,
        seq,
    })
}

/// Spawn a thread that receives remote ops for `doc_id` after
/// `after_seq` from grpc `client`, and append them into
/// `remove_op_buffer`. Whenever it receives remote ops, send
/// notification to `notifier_tx`. Errors are sent to `error_channel`.
async fn spawn_thread_receive_remote_op_from_grpc(
    doc_id: DocId,
    mut client: GrpcClient,
    after_seq: GlobalSeq,
    remote_op_buffer: Arc<Mutex<Vec<FatOp>>>,
    error_channel: mpsc::Sender<CollabError>,
    notifier_tx: Arc<watch::Sender<()>>,
    external_notifier: std::sync::mpsc::Sender<DocDesignator>,
) -> CollabResult<tokio::task::JoinHandle<()>> {
    let resp = client
        .client
        .recv_op(Request::new(rpc::FileOps {
            doc_id: doc_id.clone(),
            after: after_seq,
        }))
        .await?;

    let msg = DocDesignator {
        doc: doc_id.clone(),
        server: client.server_id(),
    };

    let mut stream = resp.into_inner();
    let handle = tokio::spawn(async move {
        loop {
            let resp = stream.message().await;
            if let Err(err) = resp {
                error_channel.send(err.into()).await.unwrap();
                return;
            }
            let op = resp.unwrap();
            if let None = op {
                let err = CollabError::TransportErr(format!(
                    "Doc({}) Remote server broke connection",
                    &doc_id
                ));
                error_channel.send(err).await.unwrap();
                return;
            }
            let op = op.unwrap();
            let op: FatOp = bincode::deserialize(&op.op[..]).unwrap();
            log::debug!("Doc({}) Received op from remote server: {:?}", &doc_id, &op);
            let truly_remote_op_arrived = op.site != client.site_id();
            remote_op_buffer.lock().await.push(op);
            notifier_tx.send(()).unwrap();

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
    });
    Ok(handle)
}

/// Spawn a thread that keeps trying get local ops from `engine` and
/// send out to grpc `client` when appropriate. It tries to get local
/// ops when ever `notifier_rx` is notified. Errors are sent to
/// `error_channel`.
fn spawn_thread_send_local_op_to_grpc(
    doc_id: DocId,
    mut client: DocServerClient<tonic::transport::Channel>,
    engine: Arc<Mutex<ClientEngine>>,
    error_channel: mpsc::Sender<CollabError>,
    mut notifier_rx: watch::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            notifier_rx.changed().await.unwrap();
            let ops = {
                let mut engine = engine.lock().await;
                engine.maybe_package_local_ops()
            };
            if let Some(ops) = ops {
                let context = ops.context;
                log::debug!("Doc({}) Sending ops to remote server: {:?}", &doc_id, &ops);
                let ops = ops
                    .ops
                    .iter()
                    .map(|op| bincode::serialize(op).unwrap())
                    .collect();
                if let Err(err) = client
                    .send_op(Request::new(rpc::ContextOps { context, ops }))
                    .await
                {
                    let err = CollabError::from(err);
                    error_channel.send(err).await.unwrap();
                    return;
                }
            }
        }
    })
}
