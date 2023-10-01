//! This module exposes the [DocServer] trait that abstracts over a
//!local and remote server.

use crate::collab_server::LocalServer;
use crate::error::CollabResult;
// use crate::grpc_client::GrpcClient;
use crate::types::*;
use crate::webrpc_client::WebrpcClient;
use async_trait::async_trait;
use enum_dispatch::enum_dispatch;
use std::pin::Pin;
use tokio_stream::Stream;

/// Functions one can expect from a server, implemented by both
/// [crate::collab_server::LocalServer] and
/// [crate::grpc_client::GrpcClient].
#[async_trait]
#[enum_dispatch]
pub trait DocServer: Send {
    fn site_id(&self) -> SiteId;
    fn server_id(&self) -> ServerId;
    async fn share_file(&mut self, file_name: &str, file: &str) -> CollabResult<DocId>;
    async fn list_files(&mut self) -> CollabResult<Vec<DocInfo>>;
    async fn request_file(&mut self, doc_id: &DocId) -> CollabResult<Snapshot>;
    async fn delete_file(&mut self, doc_id: &DocId) -> CollabResult<()>;
    async fn send_op(&mut self, ops: ContextOps) -> CollabResult<()>;
    async fn recv_op(
        &mut self,
        doc_id: &DocId,
        mut after: GlobalSeq,
    ) -> CollabResult<Pin<Box<dyn Stream<Item = CollabResult<FatOp>> + Send>>>;
}

/// Either a local server or a gRPC client connect to a remote server.
#[enum_dispatch(DocServer)]
#[derive(Debug, Clone)]
pub enum ClientEnum {
    LocalServer,
    // GrpcClient,
    WebrpcClient,
}
