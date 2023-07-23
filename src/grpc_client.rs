//! This module provides a gRPC client that implements
//! [crate::abstract_server::DocServer] trait, users can access remote
//! servers with this client.

use crate::abstract_server::DocServer;
use crate::collab_server::Snapshot;
use crate::error::{CollabError, CollabResult};
use crate::types::*;
use async_trait::async_trait;
use std::pin::Pin;
use tokio_stream::{Stream, StreamExt};
use tonic::Request;

use crate::rpc;
use crate::rpc::doc_server_client::DocServerClient;

// *** Types

type OpStream = Pin<Box<dyn Stream<Item = CollabResult<FatOp>> + Send>>;

// *** Structs

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
}

// *** Impl Server for GrpcClient

#[async_trait]
impl DocServer for GrpcClient {
    fn site_id(&self) -> SiteId {
        self.site_id.clone()
    }
    fn server_id(&self) -> ServerId {
        self.server_id.clone()
    }
    async fn share_file(&mut self, file_name: &str, file: &str) -> CollabResult<DocId> {
        let req = Request::new(rpc::FileToShare {
            file_name: file_name.to_string(),
            content: file.to_string(),
        });
        let resp = self.client.share_file(req).await?;
        return Ok(resp.into_inner().doc_id);
    }

    async fn list_files(&mut self) -> CollabResult<Vec<DocInfo>> {
        let resp = self.client.list_files(Request::new(rpc::Empty {})).await?;
        let files = resp.into_inner().files;
        let files = files
            .into_iter()
            .map(|info| DocInfo {
                doc_id: info.doc_id,
                file_name: info.file_name,
            })
            .collect();
        Ok(files)
    }

    async fn request_file(&mut self, doc_id: &DocId) -> CollabResult<Snapshot> {
        let resp = self
            .client
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

    async fn send_op(&mut self, ops: Vec<ContextOps>) -> CollabResult<()> {
        let ops = ops
            .iter()
            .map(|op| bincode::serialize(op).unwrap())
            .collect();
        self.client
            .send_op(Request::new(rpc::VecContextOps {
                vec_context_ops: ops,
            }))
            .await?;
        Ok(())
    }
    async fn recv_op(&mut self, doc_id: &DocId, after: GlobalSeq) -> CollabResult<OpStream> {
        let resp = self
            .client
            .recv_op(Request::new(rpc::FileOps {
                doc_id: doc_id.clone(),
                after,
            }))
            .await?;

        let stream = resp.into_inner();
        let stream = stream.map(|op| match op {
            Ok(op) => Ok(bincode::deserialize::<FatOp>(&op.op[..]).unwrap()),
            Err(status) => Err(CollabError::from(status)),
        });
        Ok(Box::pin(stream))
    }
    async fn delete_file(&mut self, doc_id: &DocId) -> CollabResult<()> {
        self.client
            .delete_file(Request::new(rpc::DocId {
                doc_id: doc_id.clone(),
            }))
            .await?;
        Ok(())
    }
}
