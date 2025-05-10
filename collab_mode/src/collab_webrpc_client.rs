//! This module provides a webrpc client that implements
//! [crate::abstract_server::DocServer] trait, users can access remote
//! servers with this client.

use crate::abstract_server::{DocServer, InfoStream, OpStream};
use crate::error::{convert_remote_err, CollabError, CollabResult};
use crate::types::*;
use crate::webrpc::Endpoint;
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::instrument;

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
        server_key_cert: ArcKeyCert,
        signaling_addr: &str,
        credential: Credential,
    ) -> CollabResult<WebrpcClient> {
        let endpoint =
            Endpoint::connect(server_id.clone(), server_key_cert, signaling_addr).await?;
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

    #[instrument(skip(self))]
    async fn list_files(&mut self, dir_path: Option<FilePath>) -> CollabResult<Vec<DocInfo>> {
        let resp = self
            .endpoint
            .send_request_oneshot(&DocServerReq::ListFiles { dir_path })
            .await?
            .unpack()?;

        tracing::debug!(?resp);

        match resp {
            DocServerResp::ListFiles(doc_info) => {
                Ok(doc_info.into_iter().map(|info| info.into()).collect())
            }
            DocServerResp::Err(err) => Err(convert_remote_err(err)),
            resp => Err(unexpected_resp("ListFiles", resp)),
        }
    }

    #[instrument(skip(self, file))]
    async fn share_file(
        &mut self,
        file_name: &str,
        file_meta: &JsonMap,
        file: FileContentOrPath,
    ) -> CollabResult<DocId> {
        if let FileContentOrPath::Path(_) = &file {
            return Err(CollabError::UnsupportedOperation(
                "Sharing directory to a remote host".to_string(),
            ));
        }

        let resp = self
            .endpoint
            .send_request_oneshot(&DocServerReq::ShareFile {
                file_name: file_name.to_string(),
                file_meta: serde_json::to_string(&file_meta)?,
                content: file,
            })
            .await?
            .unpack()?;

        tracing::debug!(?resp);

        match resp {
            DocServerResp::ShareFile(doc_id) => Ok(doc_id),
            DocServerResp::Err(err) => Err(convert_remote_err(err)),
            resp => Err(unexpected_resp("ShareFile", resp)),
        }
    }

    #[instrument(skip(self))]
    async fn request_file(&mut self, doc_file: &DocDesc) -> CollabResult<Snapshot> {
        let resp = self
            .endpoint
            .send_request_oneshot(&DocServerReq::RequestFile(doc_file.clone()))
            .await?
            .unpack()?;

        tracing::debug!(?resp);

        match resp {
            DocServerResp::RequestFile(snapshot) => Ok(snapshot),
            DocServerResp::Err(err) => Err(convert_remote_err(err)),
            resp => Err(unexpected_resp("RequestFile", resp)),
        }
    }

    #[instrument(skip(self))]
    async fn send_op(&mut self, ops: ContextOps) -> CollabResult<()> {
        let resp = self
            .endpoint
            .send_request_oneshot(&DocServerReq::SendOp(ops))
            .await?
            .unpack()?;

        tracing::debug!(?resp);

        match resp {
            DocServerResp::SendOp => Ok(()),
            DocServerResp::Err(err) => Err(convert_remote_err(err)),
            resp => Err(unexpected_resp("SendOp", resp)),
        }
    }

    #[instrument(skip(self))]
    async fn recv_op_and_info(
        &mut self,
        doc_id: &DocId,
        after: GlobalSeq,
    ) -> CollabResult<(OpStream, InfoStream)> {
        let (mut rx, _req_id) = self
            .endpoint
            .send_request(&DocServerReq::RecvOpAndInfo {
                doc_id: doc_id.clone(),
                after,
            })
            .await?;

        let (tx_op, rx_op) = mpsc::channel::<CollabResult<Vec<FatOp>>>(1);
        let (tx_info, rx_info) = mpsc::channel::<CollabResult<Info>>(1);

        let _ = tokio::spawn(async move {
            while let Some(res) = rx.recv().await {
                match res {
                    Err(err) => {
                        tracing::info!(?err, "Error receiving op and info");
                        tx_op.send(Err(err.into())).await.unwrap();
                        return;
                    }
                    Ok(msg) => match msg.unpack::<DocServerResp>() {
                        Err(err) => {
                            tracing::info!(?err, "Received err object instead of op");
                            tx_op.send(Err(err.into())).await.unwrap();
                            return;
                        }
                        Ok(resp) => match resp {
                            DocServerResp::RecvOp(ops) => {
                                tracing::info!(?ops, "Received ops");
                                tx_op.send(Ok(ops)).await.unwrap();
                            }
                            DocServerResp::RecvInfo(info) => {
                                tracing::info!(?info, "Received info");
                                tx_info.send(Ok(info)).await.unwrap();
                            }
                            resp => {
                                tracing::debug!(?resp, "Received something neither op nor info");
                                tx_op
                                    .send(Err(unexpected_resp("RecvOp", resp)))
                                    .await
                                    .unwrap();
                                return;
                            }
                        },
                    },
                }
            }
        });
        Ok((
            Box::pin(ReceiverStream::from(rx_op)),
            Box::pin(ReceiverStream::from(rx_info)),
        ))
    }

    #[instrument(skip(self))]
    async fn delete_file(&mut self, doc_id: &DocId) -> CollabResult<()> {
        let resp = self
            .endpoint
            .send_request_oneshot(&DocServerReq::DeleteFile(doc_id.clone()))
            .await?
            .unpack()?;

        tracing::debug!(?resp);

        match resp {
            DocServerResp::DeleteFile => Ok(()),
            DocServerResp::Err(err) => Err(convert_remote_err(err)),
            resp => Err(unexpected_resp("DeleteFile", resp)),
        }
    }

    #[instrument(skip(self))]
    async fn send_info(&mut self, doc_id: &DocId, info: String) -> CollabResult<()> {
        let resp = self
            .endpoint
            .send_request_oneshot(&DocServerReq::SendInfo {
                doc_id: doc_id.clone(),
                info: Info {
                    sender: self.site_id,
                    value: info,
                },
            })
            .await?
            .unpack()?;

        tracing::debug!(?resp);

        match resp {
            DocServerResp::SendInfo => Ok(()),
            DocServerResp::Err(err) => Err(convert_remote_err(err)),
            resp => Err(unexpected_resp("SendInfo", resp)),
        }
    }
}

// *** Helper functions

/// Create an error stating that `resp` wasn't the expected type.
fn unexpected_resp(expected_resp: &str, resp: DocServerResp) -> CollabError {
    CollabError::RpcError(format!(
        "Expecting {expected_resp} response but got {:?}",
        resp
    ))
}
