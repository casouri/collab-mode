use crate::abstract_server::{ClientEnum, DocServer};
use crate::collab_client::Doc;
use crate::collab_server::LocalServer;
use crate::error::{CollabError, CollabResult};
use crate::grpc_client::GrpcClient;
use crate::types::*;
use lsp_server::{Connection, ExtractError, Message, Request, RequestId, Response, ResponseError};
use serde::Serialize;
use std::collections::HashMap;
use tokio::sync::mpsc;

pub mod types;
use types::*;

// *** Structs

pub struct JSONRPCServer {
    server: LocalServer,
    doc_map: HashMap<DocDesignator, Doc>,
    client_map: HashMap<String, ClientEnum>,
    notifier_tx: std::sync::mpsc::Sender<DocDesignator>,
}

// *** Entry functions

/// Run the JSONRPC server on stdio.
pub fn run_stdio(server: LocalServer, runtime: tokio::runtime::Runtime) -> anyhow::Result<()> {
    let (connection, io_threads) = Connection::stdio();
    main_loop(connection, server, runtime);
    io_threads.join()?;
    Ok(())
}

/// Run the JSONRPC server on a socket listening on `addr` (host:port).
pub fn run_socket(
    addr: &str,
    server: LocalServer,
    runtime: tokio::runtime::Runtime,
) -> anyhow::Result<()> {
    let (connection, io_threads) = Connection::listen(addr)?;
    main_loop(connection, server, runtime);
    io_threads.join()?;
    Ok(())
}

fn main_loop(connection: Connection, server: LocalServer, runtime: tokio::runtime::Runtime) {
    let (notifier_tx, notifier_rx) = std::sync::mpsc::channel();
    let mut server = JSONRPCServer::new(server, notifier_tx);

    let connection = std::sync::Arc::new(connection);
    let connection_1 = std::sync::Arc::clone(&connection);

    // TODO: Maybe we should throttle-control notifications?
    std::thread::spawn(move || {
        for doc in notifier_rx.iter() {
            let params = RemoteOpArrivedNotification {
                doc: doc.doc,
                server: doc.server,
            };
            let msg = lsp_server::Notification {
                method: "RemoteOpArrived".to_string(),
                params: serde_json::to_value(params).unwrap(),
            };
            if let Err(err) = connection_1.sender.send(Message::Notification(msg)) {
                log::error!("Error sending jsonrpc notification: {:#}", err);
            }
        }
    });

    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                let id = req.id.clone();
                let res = runtime.block_on(Box::pin(server.handle_request(req)));
                match res {
                    Ok(msg) => {
                        if let Err(err) = connection.sender.send(msg) {
                            log::error!("Error sending jsonrpc response: {:#}", err);
                        }
                    }
                    Err(err) => {
                        let code = error_code(&err);
                        let msg = make_err(id, code, format!("{:#}", err));
                        if let Err(err) = connection.sender.send(msg) {
                            log::error!("Error sending jsonrpc response: {:#}", err);
                        }
                    }
                }
            }
            Message::Response(_) => {
                eprintln!("Received a response");
            }
            Message::Notification(_) => {
                eprintln!("Received a notification");
            }
        }
    }
}

// *** Helper functions

/// Make a response.
fn make_resp(id: RequestId, resp: impl Serialize) -> Message {
    let resp = Response {
        id,
        result: Some(serde_json::to_value(resp).unwrap()),
        error: None,
    };
    Message::Response(resp)
}

/// Make an error response.
fn make_err(id: RequestId, code: ErrorCode, message: String) -> Message {
    let resp = Response::new_err(id, code as i32, message);
    Message::Response(resp)
}

fn error_code(err: &CollabError) -> ErrorCode {
    match err {
        CollabError::ParseError(_) => ErrorCode::InvalidParams,
        CollabError::EngineError { err: _ } => ErrorCode::OTEngineError,
        CollabError::OpOutOfBound(_, _) => ErrorCode::OTEngineError,
        CollabError::DocNotFound(_) => ErrorCode::DocNotFound,
        CollabError::DocAlreadyExists(_) => ErrorCode::DocAlreadyExists,
        CollabError::ServerNotConnected(_) => ErrorCode::ConnectionBroke,
        CollabError::ChannelClosed(_) => ErrorCode::InternalError,
        CollabError::TransportErr(_) => ErrorCode::ConnectionBroke,
        CollabError::IOErr(_) => ErrorCode::InternalError,
    }
}

// *** Functions for JSONRPCServer

impl JSONRPCServer {
    pub fn new(
        server: LocalServer,
        notifier_tx: std::sync::mpsc::Sender<DocDesignator>,
    ) -> JSONRPCServer {
        JSONRPCServer {
            server,
            doc_map: HashMap::new(),
            client_map: HashMap::new(),
            notifier_tx,
        }
    }

    async fn handle_error(
        error_channel: &mut mpsc::Receiver<(DocId, CollabError)>,
    ) -> anyhow::Result<Option<Message>> {
        loop {
            let err = error_channel.recv().await;
            if let Some(err) = err {
                let error_code = match &err.1 {
                    CollabError::TransportErr(_) => ErrorCode::ConnectionBroke,
                    CollabError::EngineError { err } => ErrorCode::OTEngineError,
                    CollabError::OpOutOfBound(_, _) => ErrorCode::OTEngineError,
                    _ => ErrorCode::InternalError,
                };
                todo!()
            } else {
                return Err(
                    CollabError::ChannelClosed("Internal error_channel broke".to_string()).into(),
                );
            }
        }
    }

    /// Handle `request`, return response message.
    async fn handle_request(&mut self, request: Request) -> CollabResult<Message> {
        let msg = if request.method == "ShareFile" {
            let params: ShareFileParams = serde_json::from_value(request.params)?;
            let resp = self.handle_share_file_request(params).await?;
            make_resp(request.id, resp)
        } else if request.method == "SendOp" {
            let params: SendOpParams = serde_json::from_value(request.params)?;
            let resp = self.handle_send_op_request(params).await?;
            make_resp(request.id, resp)
        } else if request.method == "ListFiles" {
            let params: ListFilesParams = serde_json::from_value(request.params)?;
            let resp = self.handle_list_files_request(params).await?;
            make_resp(request.id, resp)
        } else if request.method == "ConnectToFile" {
            let params: ConnectToFileParams = serde_json::from_value(request.params)?;
            let resp = self.handle_connect_to_file_request(params).await?;
            make_resp(request.id, resp)
        } else {
            todo!()
        };
        Ok(msg)
    }
}

// *** Subroutines for handle_request

impl JSONRPCServer {
    // fn handle_hello_request(&self, request: Request) -> anyhow::Result<Message> {
    //     let params: HelloParams =
    //         serde_json::from_value(request.params).context("Parse Hello params")?;
    //     self.site_id = Some(params.site_id);
    //     Ok(make_resp(request.id, ()))
    // }

    pub async fn handle_share_file_request(
        &mut self,
        params: ShareFileParams,
    ) -> CollabResult<ShareFileResp> {
        let doc = Doc::new_share_file(
            self.server.clone().into(),
            &params.file_name,
            &params.file,
            self.notifier_tx.clone(),
        )
        .await?;

        let resp = ShareFileResp {
            doc_id: doc.doc_id(),
        };
        let key = DocDesignator {
            server: SERVER_ID_SELF.to_string(),
            doc: doc.doc_id(),
        };
        self.doc_map.insert(key, doc);
        Ok(resp)
    }

    pub async fn handle_send_op_request(
        &mut self,
        params: SendOpParams,
    ) -> CollabResult<SendOpResp> {
        let key = DocDesignator {
            server: params.server,
            doc: params.doc.clone(),
        };
        if let Some(doc) = self.doc_map.get_mut(&key) {
            let res = doc.send_op(params.ops).await;
            if res.is_err() {
                self.doc_map.remove(&key);
            }
            let remote_ops = res?;
            let resp = SendOpResp { ops: remote_ops };
            Ok(resp)
        } else {
            Err(CollabError::DocNotFound(params.doc))
        }
    }

    pub async fn handle_list_files_request(
        &mut self,
        params: ListFilesParams,
    ) -> CollabResult<ListFilesResp> {
        let res = if let Some(client) = self.client_map.get_mut(&params.server) {
            client.list_files().await
        } else {
            let client = GrpcClient::new(params.server.clone(), params.credential).await?;
            self.client_map.insert(params.server.clone(), client.into());
            let client = self.client_map.get_mut(&params.server).unwrap();
            client.list_files().await
        };
        if res.is_err() {
            // If a client has problems, all it's associated docs must
            // have problems too, but no need to clean them up right
            // here. They will cleanup themselves.
            self.client_map.remove(&params.server);
        }
        let files = res?;
        let resp = ListFilesResp { files };
        Ok(resp)
    }

    pub async fn handle_connect_to_file_request(
        &mut self,
        params: ConnectToFileParams,
    ) -> CollabResult<ConnectToFileResp> {
        let doc_desg = DocDesignator {
            server: params.server,
            doc: params.doc,
        };
        if self.doc_map.get(&doc_desg).is_some() {
            self.doc_map.remove(&doc_desg);
        }
        if let Some(client) = self.client_map.get(&doc_desg.server) {
            let (doc, content) = Doc::new_connect_file(
                client.clone(),
                doc_desg.doc.clone(),
                self.notifier_tx.clone(),
            )
            .await?;
            self.doc_map.insert(doc_desg, doc);
            let resp = ConnectToFileResp { content };
            Ok(resp)
        } else {
            Err(CollabError::ServerNotConnected(doc_desg.server))
        }
    }
}
