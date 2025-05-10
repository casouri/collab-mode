//! This module exposes the [DocServer] trait that abstracts over a
//! local and remote server.

use crate::collab_server::LocalServer;
use crate::error::CollabResult;
// use crate::grpc_client::GrpcClient;
use crate::collab_webrpc_client::WebrpcClient;
use crate::types::*;
use async_trait::async_trait;
use enum_dispatch::enum_dispatch;
use std::pin::Pin;
use tokio_stream::Stream;

pub type OpStream = Pin<Box<dyn Stream<Item = CollabResult<Vec<FatOp>>> + Send>>;
pub type InfoStream = Pin<Box<dyn Stream<Item = CollabResult<Info>> + Send>>;

/// Functions one can expect from a server, implemented by both
/// [crate::collab_server::LocalServer] and
/// [crate::grpc_client::GrpcClient].
#[async_trait]
#[enum_dispatch]
pub trait DocServer: Send {
    fn site_id(&self) -> SiteId;
    fn server_id(&self) -> ServerId;
    async fn share_file(
        &mut self,
        file_name: &str,
        file_meta: &JsonMap,
        file: FileContentOrPath,
    ) -> CollabResult<DocId>;
    async fn list_files(&mut self, dir_path: Option<FilePath>) -> CollabResult<Vec<DocInfo>>;
    async fn request_file(&mut self, doc_file: &DocDesc) -> CollabResult<Snapshot>;
    async fn delete_file(&mut self, doc_id: &DocId) -> CollabResult<()>;
    async fn send_op(&mut self, ops: ContextOps) -> CollabResult<()>;
    async fn recv_op_and_info(
        &mut self,
        doc_id: &DocId,
        mut after: GlobalSeq,
    ) -> CollabResult<(OpStream, InfoStream)>;
    async fn send_info(&mut self, doc_id: &DocId, info: String) -> CollabResult<()>;
}

/// Either a local server or a gRPC client connect to a remote server.
#[enum_dispatch(DocServer)]
#[derive(Debug, Clone)]
pub enum ClientEnum {
    LocalServer,
    // GrpcClient,
    WebrpcClient,
}
