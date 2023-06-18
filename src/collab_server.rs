use crate::abstract_server::DocServer;
use crate::error::{CollabError, CollabResult};
use crate::op::{DocId, GlobalSeq, Op, SiteId};
use crate::rpc::doc_server_server::DocServerServer;
use crate::types::*;
use async_trait::async_trait;
use jumprope::JumpRope;
use std::cmp::min;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tokio::sync::{Mutex, RwLock};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tokio_stream::{Stream, StreamExt};
use uuid::Uuid;

use crate::rpc::{self, doc_server_server};
use tonic::{Request, Response};

// *** Types

type TResult<T> = tonic::Result<T>;

#[async_trait]
impl DocServer for LocalServer {
    fn site_id(&self) -> SiteId {
        self.site_id.clone()
    }
    fn server_id(&self) -> ServerId {
        SERVER_ID_SELF.to_string()
    }
    async fn share_file(&mut self, file_name: &str, file: &str) -> CollabResult<DocId> {
        self.share_file_1(file_name, file).await
    }
    async fn list_files(&mut self) -> CollabResult<Vec<DocInfo>> {
        Ok(self.list_files_1().await)
    }
    async fn request_file(&mut self, doc_id: &DocId) -> CollabResult<Snapshot> {
        self.request_file_1(doc_id).await
    }
    async fn delete_file(&mut self, doc_id: &DocId) -> CollabResult<()> {
        self.delete_file_1(doc_id).await
    }
    async fn send_op(&mut self, ops: ContextOps) -> CollabResult<()> {
        self.send_op_1(ops).await
    }
    async fn recv_op(
        &mut self,
        doc_id: &DocId,
        after: GlobalSeq,
    ) -> CollabResult<Pin<Box<dyn Stream<Item = CollabResult<FatOp>> + Send>>> {
        let stream = self.recv_op_1(doc_id, after).await?;
        Ok(Box::pin(stream.map(|op| Ok(op))))
    }
}

// *** Structs

/// A snapshot of a document.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// The file content.
    pub buffer: String,
    /// Sequence number of the last op.
    pub seq: GlobalSeq,
}

#[derive(Debug, Clone)]
pub struct LocalServer {
    site_id: SiteId,
    docs: Arc<RwLock<HashMap<DocId, Arc<Mutex<Doc>>>>>,
    user_list: Arc<Mutex<HashMap<Credential, SiteId>>>,
}

/// Stores relevant data for a document, used by the server.
#[derive(Debug)]
struct Doc {
    /// Human-readable name for the doc.
    name: String,
    /// The server engine that transforms and stores ops for this doc.
    engine: ServerEngine,
    /// The document itself.
    buffer: JumpRope,
    /// Engine sends a Unit to tx when there are new ops.
    tx: watch::Sender<()>,
    /// Clone a rx from this rx, and check for new op notification.
    rx: watch::Receiver<()>,
}

// *** Functions for Doc

impl Doc {
    pub fn new(file_name: &str, content: &str) -> Doc {
        let (tx, rx) = watch::channel(());
        Doc {
            name: file_name.to_string(),
            engine: ServerEngine::new(),
            buffer: JumpRope::from(content),
            tx,
            rx,
        }
    }

    /// Apply `op` to the document.
    pub fn apply_op(&mut self, op: Op) -> CollabResult<()> {
        match &op {
            Op::Ins((pos, content)) => {
                if *pos > self.buffer.len_chars() as u64 {
                    Err(CollabError::OpOutOfBound(op, self.buffer.len_chars()))
                } else {
                    self.buffer.insert(*pos as usize, &content);
                    Ok(())
                }
            }
            Op::Del(ops) => {
                for dop in ops {
                    let start = dop.0 as usize;
                    let end = dop.0 as usize + dop.1.len();
                    if end > self.buffer.len_chars() {
                        return Err(CollabError::OpOutOfBound(op, self.buffer.len_chars()));
                    }
                    self.buffer.replace(start..end, "");
                }
                Ok(())
            }
        }
    }

    /// Return the current snapshot of the document.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            buffer: self.buffer.to_string(),
            seq: self.engine.current_seq(),
        }
    }
}

// *** Functions for CollabServer

impl LocalServer {
    pub fn new() -> LocalServer {
        let uuid = Uuid::new_v4();
        LocalServer {
            site_id: uuid.to_string(),
            docs: Arc::new(RwLock::new(HashMap::new())),
            user_list: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn start_grpc_server(
        self,
        port: u16,
        error_channel: mpsc::Sender<CollabError>,
    ) -> CollabResult<()> {
        let service = DocServerServer::new(self);
        let server = tonic::transport::Server::builder().add_service(service);

        let addr4 = format!("0.0.0.0:{}", port);
        let addr6 = format!("[::1]:{}", port);

        let listener4 = TcpListener::bind(addr4)
            .await
            .map_err(|err| CollabError::IOErr(err.to_string()))?;
        let listener6 = TcpListener::bind(addr6)
            .await
            .map_err(|err| CollabError::IOErr(err.to_string()))?;
        let incoming4 = TcpListenerStream::new(listener4);
        let incoming6 = TcpListenerStream::new(listener6);
        let incoming46 = tokio_stream::StreamExt::merge(incoming4, incoming6);

        let _ = tokio::spawn(async move {
            if let Err(err) = server.serve_with_incoming(incoming46).await {
                error_channel
                    .send(CollabError::TransportErr(err.to_string()))
                    .await
                    .unwrap();
            }
        });
        Ok(())
    }

    pub async fn share_file_1(&self, file_name: &str, file: &str) -> CollabResult<DocId> {
        // TODO permission check.
        let uuid = Uuid::new_v4();
        let doc_id = uuid.to_string();
        let mut docs = self.docs.write().await;

        docs.insert(
            doc_id.clone(),
            Arc::new(Mutex::new(Doc::new(file_name, file))),
        );
        Ok(doc_id)
    }

    /// Send local ops to the server. TODO: access control.
    pub async fn send_op_1(&self, ops: ContextOps) -> CollabResult<()> {
        let doc_id = ops.doc();
        if let Some(doc) = self.docs.read().await.get(&doc_id) {
            let mut doc = doc.lock().await;

            log::debug!("send_op() Local server receive ops, processing: {:?}", &ops);
            let ops = doc.engine.process_ops(ops.ops, ops.context)?;

            for op in ops {
                doc.apply_op(op.op)?;
            }
            log::debug!("send_op() doc: \"{}\"", doc.buffer.to_string());

            // Notification channel are never closed.
            doc.tx.send(()).unwrap();
            Ok(())
        } else {
            Err(CollabError::DocNotFound(doc_id))
        }
    }

    // Receive ops after `after`. from server. Server will send ops to
    // `channel`. TODO access control.
    pub async fn recv_op_1(
        &self,
        doc_id: &DocId,
        mut after: GlobalSeq,
    ) -> CollabResult<ReceiverStream<FatOp>> {
        let (tx, rx) = mpsc::channel(1);
        // Clone a notification channel and a reference to the doc,
        // listen for notification and get new ops from the doc, and
        // feed into the channel.
        let res = if let Some(doc) = self.docs.read().await.get(doc_id) {
            Ok((doc.lock().await.rx.clone(), doc.clone()))
        } else {
            Err(CollabError::DocNotFound(doc_id.clone()))
        };
        let (mut notifier, doc) = res?;

        let _ = tokio::spawn(async move {
            loop {
                if notifier.changed().await.is_ok() {
                    let doc = doc.lock().await;
                    log::debug!(
                        "recv_op() Local server collects global ops after seq#{} to send out",
                        after
                    );
                    let ops = doc.engine.global_ops_after(after);
                    after += ops.len() as u64;
                    for op in ops {
                        log::debug!(
                            "recv_op() Local server sends op to gRPC server or local client: {:?}",
                            &op
                        );
                        if let Err(_) = tx.send(op).await {
                            log::error!("Internal channel (local server --op--> local client or grpc server) closed. Maybe remove closed connection.");
                            return;
                        }
                    }
                }
            }
        });
        Ok(ReceiverStream::from(rx))
    }

    pub async fn list_files_1(&self) -> Vec<DocInfo> {
        let mut res = vec![];
        for (doc_id, doc) in self.docs.read().await.iter() {
            res.push(DocInfo {
                doc_id: doc_id.to_string(),
                file_name: doc.lock().await.name.clone(),
            });
        }
        res
    }

    pub async fn request_file_1(&self, doc_id: &DocId) -> CollabResult<Snapshot> {
        if let Some(doc) = self.docs.read().await.get(doc_id) {
            Ok(doc.lock().await.snapshot())
        } else {
            Err(CollabError::DocNotFound(doc_id.to_string()))
        }
    }

    async fn delete_file_1(&self, doc_id: &DocId) -> CollabResult<()> {
        let mut docs = self.docs.write().await;
        docs.remove(doc_id);
        Ok(())
    }
}

// *** RPC

#[async_trait]
impl doc_server_server::DocServer for LocalServer {
    type RecvOpStream = Pin<Box<dyn Stream<Item = TResult<rpc::FatOp>> + Send>>;
    type RequestFileStream = ReceiverStream<TResult<rpc::SnapshotChunk>>;

    async fn login(&self, request: Request<rpc::Credential>) -> TResult<Response<rpc::SiteId>> {
        let cred = request.into_inner().cred;

        let mut user_list = self.user_list.lock().await;
        let site_id = if let Some(site_id) = user_list.get(&cred) {
            site_id.to_string()
        } else {
            let uuid = Uuid::new_v4();
            user_list.insert(cred.clone(), uuid.to_string());
            cred
        };
        Ok(Response::new(rpc::SiteId { id: site_id }))
    }

    async fn share_file(
        &self,
        request: Request<rpc::FileToShare>,
    ) -> TResult<Response<rpc::DocId>> {
        let file = request.into_inner();
        let doc_id = self.share_file_1(&file.file_name, &file.content).await?;
        Ok(Response::new(rpc::DocId { doc_id }))
    }

    async fn send_op(&self, request: Request<rpc::ContextOps>) -> TResult<Response<rpc::Empty>> {
        let inner = request.into_inner();
        let context = inner.context;
        let ops: Vec<FatOp> = inner
            .ops
            .into_iter()
            .map(|op| bincode::deserialize(&op[..]).unwrap())
            .collect();
        log::debug!(
            "send_op() gRPC server receives ops, passing to local server to process: {:?}",
            &ops
        );
        self.send_op_1(ContextOps { context, ops }).await?;
        Ok(Response::new(rpc::Empty {}))
    }

    async fn recv_op(
        &self,
        request: Request<rpc::FileOps>,
    ) -> TResult<Response<Self::RecvOpStream>> {
        let inner = request.into_inner();
        let stream = self.recv_op_1(&inner.doc_id, inner.after).await?;

        let stream = stream.map(|op| {
            let op = rpc::FatOp {
                op: bincode::serialize(&op).unwrap(),
            };
            Ok(op)
        });
        Ok(Response::new(Box::pin(stream)))
    }

    async fn list_files(&self, _request: Request<rpc::Empty>) -> TResult<Response<rpc::FileList>> {
        let files = self.list_files_1().await;
        let files = files
            .into_iter()
            .map(|info| rpc::DocInfo {
                doc_id: info.doc_id,
                file_name: info.file_name,
            })
            .collect();
        log::debug!("gRPC list_files request -> {:?}", &files);
        Ok(Response::new(rpc::FileList { files }))
    }

    async fn request_file(
        &self,
        request: Request<rpc::DocId>,
    ) -> TResult<Response<Self::RequestFileStream>> {
        let snapshot = self.request_file_1(&request.into_inner().doc_id).await?;
        let buffer = snapshot.buffer;
        let seq = snapshot.seq;

        let (tx, rx) = mpsc::channel(1);

        tokio::spawn(async move {
            let len = 32 * 1024 * 1024;
            let mut start = 0;
            loop {
                if start >= buffer.len() {
                    break;
                }
                let buf = &buffer[start..(min(start + len, buffer.len()))];
                if let Err(err) = tx
                    .send(Ok(rpc::SnapshotChunk {
                        seq,
                        content: buf.to_string(),
                    }))
                    .await
                {
                    log::error!(
                        "request_file() Can't send snapshot chunk to grpc channel: {:#}",
                        err
                    );
                    return;
                }
                start += len;
            }
        });
        Ok(Response::new(ReceiverStream::from(rx)))
    }

    async fn delete_file(&self, request: Request<rpc::DocId>) -> TResult<Response<rpc::Empty>> {
        self.delete_file_1(&request.into_inner().doc_id).await?;
        Ok(Response::new(rpc::Empty {}))
    }
}
