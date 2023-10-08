//! This module provides a webrpc client that implements
//! [crate::abstract_server::DocServer] trait, users can access remote
//! servers with this client.

use crate::abstract_server::DocServer;
use crate::error::{CollabError, CollabResult};
use crate::types::*;
use crate::webrpc::Endpoint;
use async_trait::async_trait;
use std::pin::Pin;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::Stream;

// *** Types

type OpStream = Pin<Box<dyn Stream<Item = CollabResult<Vec<FatOp>>> + Send>>;

// *** Structs

/// [WebrpcClient] represents a webrpc endpoint connecting to a remote
/// server. Users can list files using the client. [WebrpcClient] can be
/// shared by cloning, just like the tonic client.
#[derive(Debug, Clone)]
pub struct WebrpcClient {
    endpoint: Endpoint,
    credential: Credential,
    site_id: SiteId,
    server_id: ServerId,
}

// *** Functions for WebrpcClient

impl WebrpcClient {
    // Create a client by connecting to `server_addr` on the signaling
    // server at `signaling_addr` with `credential`.
    pub async fn new(
        server_id: ServerId,
        signaling_addr: &str,
        credential: Credential,
    ) -> CollabResult<WebrpcClient> {
        let endpoint = Endpoint::connect(server_id.clone(), signaling_addr).await?;
        let resp = endpoint
            .send_request_oneshot(&DocServerReq::Login(credential.clone()))
            .await?
            .unpack::<DocServerResp>()?;

        match resp {
            DocServerResp::Login(site_id) => Ok(WebrpcClient {
                endpoint,
                credential,
                site_id: site_id.clone(),
                server_id,
            }),
            resp => Err(unexpected_resp("Login", resp)),
        }
    }
}

#[async_trait]
impl DocServer for WebrpcClient {
    fn site_id(&self) -> SiteId {
        self.site_id.clone()
    }

    fn server_id(&self) -> ServerId {
        self.server_id.clone()
    }

    async fn list_files(&mut self) -> CollabResult<Vec<DocInfo>> {
        let resp = self
            .endpoint
            .send_request_oneshot(&DocServerReq::ListFiles)
            .await?
            .unpack()?;
        log::debug!("list_files() => {:?}", &resp);
        match resp {
            DocServerResp::ListFiles(doc_info) => Ok(doc_info),
            DocServerResp::Err(err) => Err(CollabError::RemoteErr(Box::new(err))),
            resp => Err(unexpected_resp("ListFiles", resp)),
        }
    }

    async fn share_file(&mut self, file_name: &str, file: &str) -> CollabResult<DocId> {
        let resp = self
            .endpoint
            .send_request_oneshot(&DocServerReq::ShareFile {
                file_name: file_name.to_string(),
                content: file.to_string(),
            })
            .await?
            .unpack()?;
        log::debug!(
            "share_file(file_name={file_name}, file={file}) => {:?}",
            &resp
        );
        match resp {
            DocServerResp::ShareFile(doc_id) => Ok(doc_id),
            DocServerResp::Err(err) => Err(CollabError::RemoteErr(Box::new(err))),
            resp => Err(unexpected_resp("ShareFile", resp)),
        }
    }

    async fn request_file(&mut self, doc_id: &DocId) -> CollabResult<Snapshot> {
        let resp = self
            .endpoint
            .send_request_oneshot(&DocServerReq::RequestFile(doc_id.clone()))
            .await?
            .unpack()?;
        log::debug!("request_file(doc_id={doc_id}) => {:?}", &resp);
        match resp {
            DocServerResp::RequestFile(snapshot) => Ok(snapshot),
            DocServerResp::Err(err) => Err(CollabError::RemoteErr(Box::new(err))),
            resp => Err(unexpected_resp("RequestFile", resp)),
        }
    }

    async fn send_op(&mut self, ops: ContextOps) -> CollabResult<()> {
        let ops_str = format!("{:?}", &ops);
        let resp = self
            .endpoint
            .send_request_oneshot(&DocServerReq::SendOp(ops))
            .await?
            .unpack()?;
        log::debug!("send_op(ops={ops_str}) => {:?}", &resp);
        match resp {
            DocServerResp::SendOp => Ok(()),
            DocServerResp::Err(err) => Err(CollabError::RemoteErr(Box::new(err))),
            resp => Err(unexpected_resp("SendOp", resp)),
        }
    }

    async fn recv_op(&mut self, doc_id: &DocId, after: GlobalSeq) -> CollabResult<OpStream> {
        let mut rx = self
            .endpoint
            .send_request(&DocServerReq::RecvOp {
                doc_id: doc_id.clone(),
                after,
            })
            .await?;

        let (tx_ret, rx_ret) = mpsc::channel::<CollabResult<Vec<FatOp>>>(1);
        let doc_id = doc_id.clone();
        let _ = tokio::spawn(async move {
            while let Some(res) = rx.recv().await {
                match res {
                    Err(err) => {
                        tx_ret.send(Err(err.into())).await.unwrap();
                        return;
                    }
                    Ok(msg) => match msg.unpack::<DocServerResp>() {
                        Err(err) => {
                            tx_ret.send(Err(err.into())).await.unwrap();
                            return;
                        }
                        Ok(resp) => {
                            log::debug!(
                                "rev_op(doc_id={doc_id}, after={after}) =streaming=> {:?}",
                                &resp
                            );
                            match resp {
                                DocServerResp::RecvOp(ops) => {
                                    tx_ret.send(Ok(ops)).await.unwrap();
                                }
                                resp => {
                                    tx_ret
                                        .send(Err(unexpected_resp("RecvOp", resp)))
                                        .await
                                        .unwrap();
                                    return;
                                }
                            }
                        }
                    },
                }
            }
        });
        Ok(Box::pin(ReceiverStream::from(rx_ret)))
    }

    async fn delete_file(&mut self, doc_id: &DocId) -> CollabResult<()> {
        let resp = self
            .endpoint
            .send_request_oneshot(&DocServerReq::DeleteFile(doc_id.clone()))
            .await?
            .unpack()?;
        log::debug!("delete_file(doc_id={doc_id}) => {:?}", &resp);
        match resp {
            DocServerResp::DeleteFile => Ok(()),
            DocServerResp::Err(err) => Err(CollabError::RemoteErr(Box::new(err))),
            resp => Err(unexpected_resp("DeleteFile", resp)),
        }
    }
}

/// Create an error stating that `resp` wasn't the expected type.
fn unexpected_resp(expected_resp: &str, resp: DocServerResp) -> CollabError {
    CollabError::TransportErr(format!(
        "Expecting {expected_resp} response but got {:?}",
        resp
    ))
}
