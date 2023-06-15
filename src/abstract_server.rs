use crate::collab_server::{LocalServer, Snapshot};
use crate::error::CollabResult;
use crate::grpc_client::GrpcClient;
use crate::types::*;
use async_trait::async_trait;
use enum_dispatch::enum_dispatch;
use std::pin::Pin;
use tokio_stream::Stream;

type FatOp = crate::op::FatOp<Op>;
type ContextOps = crate::engine::ContextOps<Op>;
type ServerEngine = crate::engine::ServerEngine<Op>;
type ClientEngine = crate::engine::ClientEngine<Op>;

#[async_trait]
#[enum_dispatch]
pub trait DocServer: Send {
    fn site_id(&self) -> SiteId;
    fn server_id(&self) -> ServerId;
    async fn share_file(&mut self, file_name: &str, file: &str) -> CollabResult<DocId>;
    async fn list_files(&mut self) -> CollabResult<Vec<DocId>>;
    async fn request_file(&mut self, doc_id: &DocId) -> CollabResult<Snapshot>;
    async fn delete_file(&mut self, doc_id: &DocId) -> CollabResult<()>;
    async fn send_op(&mut self, ops: ContextOps) -> CollabResult<()>;
    async fn recv_op(
        &mut self,
        doc_id: &DocId,
        mut after: GlobalSeq,
    ) -> CollabResult<Pin<Box<dyn Stream<Item = CollabResult<FatOp>> + Send>>>;
}

#[enum_dispatch(DocServer)]
#[derive(Debug, Clone)]
pub enum ClientEnum {
    LocalServer,
    GrpcClient,
}
