//! This module provides a webrpc client that implements
//! [crate::abstract_server::DocServer] trait, users can access remote
//! servers with this client.

use crate::abstract_server::{DocServer, InfoStream, OpStream};
use crate::error::{CollabError, CollabResult};
use crate::types::*;
use crate::webrpc::Endpoint;
use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

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

    async fn list_files(&mut self, dir_path: Option<FilePath>) -> CollabResult<Vec<DocInfo>> {
        let resp = self
            .endpoint
            .send_request_oneshot(&DocServerReq::ListFiles { dir_path })
            .await?
            .unpack()?;
        log::debug!("list_files() => {:?}", &resp);
        match resp {
            DocServerResp::ListFiles(doc_info) => Ok(doc_info),
            DocServerResp::Err(err) => Err(CollabError::RemoteErr(Box::new(err))),
            resp => Err(unexpected_resp("ListFiles", resp)),
        }
    }

    async fn share_file(
        &mut self,
        file_name: &str,
        file: FileContentOrPath,
    ) -> CollabResult<DocId> {
        if let FileContentOrPath::Path(_) = &file {
            return Err(CollabError::UnsupportedOperation(
                "Sharing directory to a remote host".to_string(),
            ));
        }
        let file_debug = format!("{:?}", &file);
        let resp = self
            .endpoint
            .send_request_oneshot(&DocServerReq::ShareFile {
                file_name: file_name.to_string(),
                content: file,
            })
            .await?
            .unpack()?;
        log::debug!(
            "share_file(file_name={file_name}, file={file_debug}) => {:?}",
            &resp
        );
        match resp {
            DocServerResp::ShareFile(doc_id) => Ok(doc_id),
            DocServerResp::Err(err) => Err(CollabError::RemoteErr(Box::new(err))),
            resp => Err(unexpected_resp("ShareFile", resp)),
        }
    }

    async fn request_file(&mut self, doc_file: &DocFile) -> CollabResult<Snapshot> {
        let resp = self
            .endpoint
            .send_request_oneshot(&DocServerReq::RequestFile(doc_file.clone()))
            .await?
            .unpack()?;
        log::debug!("request_file(doc_file={:?}) => {:?}", doc_file, &resp);
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

    async fn recv_op_and_info(
        &mut self,
        doc_id: &DocId,
        after: GlobalSeq,
    ) -> CollabResult<(OpStream, InfoStream)> {
        let mut rx = self
            .endpoint
            .send_request(&DocServerReq::RecvOpAndInfo {
                doc_id: doc_id.clone(),
                after,
            })
            .await?;

        let (tx_op, rx_op) = mpsc::channel::<CollabResult<Vec<FatOp>>>(1);
        let (tx_info, rx_info) = mpsc::channel::<CollabResult<String>>(1);
        let doc_id = doc_id.clone();
        let _ = tokio::spawn(async move {
            while let Some(res) = rx.recv().await {
                match res {
                    Err(err) => {
                        tx_op.send(Err(err.into())).await.unwrap();
                        return;
                    }
                    Ok(msg) => match msg.unpack::<DocServerResp>() {
                        Err(err) => {
                            tx_op.send(Err(err.into())).await.unwrap();
                            return;
                        }
                        Ok(resp) => {
                            log::debug!(
                                "rev_op_and_info(doc_id={doc_id}, after={after}) =streaming=> {:?}",
                                &resp
                            );
                            match resp {
                                DocServerResp::RecvOp(ops) => {
                                    tx_op.send(Ok(ops)).await.unwrap();
                                }
                                DocServerResp::RecvInfo(info) => {
                                    tx_info.send(Ok(info)).await.unwrap();
                                }
                                resp => {
                                    tx_op
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
        Ok((
            Box::pin(ReceiverStream::from(rx_op)),
            Box::pin(ReceiverStream::from(rx_info)),
        ))
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

    async fn send_info(&mut self, doc_id: &DocId, info: String) -> CollabResult<()> {
        let debug_msg = format!("send_info(info={info})");
        let resp = self
            .endpoint
            .send_request_oneshot(&DocServerReq::SendInfo {
                doc_id: doc_id.clone(),
                info,
            })
            .await?
            .unpack()?;
        log::debug!("{debug_msg} => {:?}", &resp);
        match resp {
            DocServerResp::SendInfo => Ok(()),
            DocServerResp::Err(err) => Err(CollabError::RemoteErr(Box::new(err))),
            resp => Err(unexpected_resp("SendInfo", resp)),
        }
    }
}

/// Create an error stating that `resp` wasn't the expected type.
fn unexpected_resp(expected_resp: &str, resp: DocServerResp) -> CollabError {
    CollabError::RpcError(format!(
        "Expecting {expected_resp} response but got {:?}",
        resp
    ))
}
