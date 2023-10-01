//! This module provides [LocalServer], the server that linearizes ops
//! and handle requests from sites. A collab process will start a
//! [LocalServer] and access it through the
//! [crate::abstract_server::DocServer] trait, and access remote
//! servers with [crate::grpc_client::GrpcClient], also through
//! [crate::DocServer] crate. At the same time, we expose the local
//! server to remote sites with gRPC, by implementing the
//! [crate::rpc::doc_server_server::DocServer] trait.

use crate::abstract_server::DocServer;
use crate::error::{CollabError, CollabResult};
use crate::op::{DocId, GlobalSeq, Op, SiteId};
use crate::rpc::doc_server_server::DocServerServer;
use crate::types::*;
use crate::webrpc::{self, Endpoint, Listener};
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

use crate::rpc::{self, doc_server_server};
use tonic::{Request, Response};

// *** Types

type TResult<T> = tonic::Result<T>;

#[async_trait]
impl DocServer for LocalServer {
    fn site_id(&self) -> SiteId {
        self.self_site_id.clone()
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

/// The server object.
#[derive(Debug, Clone)]
pub struct LocalServer {
    /// SiteId given to ourselves.
    self_site_id: SiteId,
    /// SiteId given to the next connected client.
    next_site_id: Arc<Mutex<SiteId>>,
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
        LocalServer {
            self_site_id: 0,
            next_site_id: Arc::new(Mutex::new(1)),
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
                    .send(CollabError::ServerDied(err.to_string()))
                    .await
                    .unwrap();
            }
        });
        Ok(())
    }

    pub async fn share_file_1(&self, file_name: &str, file: &str) -> CollabResult<DocId> {
        // TODO permission check.
        let doc_id: DocId = rand::random();
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
            // TODO: report error to error channel.
            doc.tx.send(()).unwrap();
            Ok(())
        } else {
            Err(CollabError::DocNotFound(doc_id))
        }
    }

    // Receive ops after `after` from server. Returns a stream of ops.
    // TODO access control.
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
                    after += ops.len() as GlobalSeq;
                    for op in ops {
                        log::debug!(
                            "recv_op() Local server sends op to gRPC server or local client: {:?}",
                            &op
                        );
                        if let Err(_) = tx.send(op).await {
                            // When the local or remote
                            // [crate::collab_client::Doc] is dropped,
                            // its connection to us is dropped. This
                            // is not an error.
                            log::info!("Internal channel (local server --op--> local client or grpc server) closed.");
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
                doc_id: doc_id.clone(),
                file_name: doc.lock().await.name.clone(),
            });
        }
        res
    }

    pub async fn request_file_1(&self, doc_id: &DocId) -> CollabResult<Snapshot> {
        if let Some(doc) = self.docs.read().await.get(doc_id) {
            Ok(doc.lock().await.snapshot())
        } else {
            Err(CollabError::DocNotFound(doc_id.clone()))
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
            site_id.clone()
        } else {
            let mut next_site_id = self.next_site_id.lock().await;
            let site_id = next_site_id.clone();
            *next_site_id += 1;
            user_list.insert(cred.clone(), site_id);
            site_id
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
                    log::warn!(
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

// *** Webrpc

pub async fn run_webrpc_server(
    server_id: ServerId,
    signaling_addr: String,
    server: LocalServer,
) -> CollabResult<()> {
    let mut listener = Listener::bind(server_id.clone(), &signaling_addr).await?;
    log::info!(
        "Registered as {} at signaling server {}",
        server_id,
        signaling_addr
    );
    loop {
        let endpoint = listener.accept().await?;
        log::info!("Received connection, endpoint.name={}", &endpoint.name);
        let server_1 = server.clone();
        let _ = tokio::spawn(async move {
            let res = handle_connection(endpoint.clone(), server_1).await;
            if let Err(err) = res {
                log::warn!(
                    "Error occurred when handling remote client: {:?} endpoint.name={}",
                    err,
                    endpoint.name
                )
            } else {
                log::info!(
                    "Remote client closed connection, endpoint.name={}",
                    endpoint.name
                );
            }
        });
    }
}

async fn handle_connection(mut endpoint: Endpoint, mut server: LocalServer) -> CollabResult<()> {
    let mut rx = endpoint.read_requests()?;
    while let Some(msg) = rx.recv().await {
        let msg = msg?;
        let req_id = msg.req_id;
        let res = handle_request(msg, &mut server, &mut endpoint).await;
        match res {
            Err(err) => {
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

async fn handle_request(
    msg: webrpc::Message,
    server: &mut LocalServer,
    endpoint: &mut Endpoint,
) -> CollabResult<Option<DocServerResp>> {
    let req = msg.unpack()?;
    match req {
        DocServerReq::ShareFile { file_name, content } => {
            let doc_id = server.share_file_1(&file_name, &content).await?;
            Ok(Some(DocServerResp::ShareFile(doc_id)))
        }
        DocServerReq::ListFiles => {
            let doc_info = server.list_files_1().await;
            Ok(Some(DocServerResp::ListFiles(doc_info)))
        }
        DocServerReq::SendOp(ops) => {
            server.send_op_1(ops).await?;
            Ok(Some(DocServerResp::SendOp))
        }
        DocServerReq::RecvOp { doc_id, after } => {
            let mut stream = server.recv_op_1(&doc_id, after).await?;
            let endpoint_1 = endpoint.clone();
            let req_id = msg.req_id;
            let _ = tokio::spawn(async move {
                while let Some(op) = stream.next().await {
                    let res = endpoint_1
                        .send_response(req_id, &DocServerResp::RecvOp(op))
                        .await;
                    if let Err(err) = res {
                        log::warn!("Error sending ops to remote client: {:?}", err);
                        return;
                    }
                }
            });
            Ok(None)
        }
        DocServerReq::RequestFile(doc_id) => {
            let snapshot = server.request_file_1(&doc_id).await?;
            Ok(Some(DocServerResp::RequestFile(snapshot)))
        }
        DocServerReq::DeleteFile(doc_id) => {
            server.delete_file_1(&doc_id).await?;
            Ok(Some(DocServerResp::DeleteFile))
        }
        DocServerReq::Login(credential) => {
            let mut user_list = server.user_list.lock().await;
            let site_id = if let Some(site_id) = user_list.get(&credential) {
                site_id.clone()
            } else {
                let mut next_site_id = server.next_site_id.lock().await;
                let site_id = next_site_id.clone();
                *next_site_id += 1;
                user_list.insert(credential.clone(), site_id);
                site_id
            };
            Ok(Some(DocServerResp::Login(site_id)))
        }
    }
}
