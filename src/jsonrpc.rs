use crate::abstract_server::{ClientEnum, DocServer};
use crate::collab_client::Doc;
use crate::collab_server::LocalServer;
use crate::error::{CollabError, CollabResult};
use crate::grpc_client::GrpcClient;
use crate::types::*;
use lsp_server::{Connection, Message, Request, RequestId, Response};
use serde::Serialize;
use std::collections::HashMap;

pub mod types;
use types::*;

// *** Structs

pub struct JSONRPCServer {
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
            let params = DocIdParams {
                doc_id: doc.doc,
                server_id: doc.server,
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
        let mut client_map = HashMap::new();
        client_map.insert(SERVER_ID_SELF.to_string(), server.into());
        JSONRPCServer {
            doc_map: HashMap::new(),
            client_map,
            notifier_tx,
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
            let params: DocIdParams = serde_json::from_value(request.params)?;
            let resp = self.handle_connect_to_file_request(params).await?;
            make_resp(request.id, resp)
        } else if request.method == "DisconnectFromFile" {
            let params: DocIdParams = serde_json::from_value(request.params)?;
            let resp = self.handle_disconnect_from_file_request(params).await?;
            make_resp(request.id, resp)
        } else if request.method == "DeleteFile" {
            let params: DocIdParams = serde_json::from_value(request.params)?;
            let resp = self.handle_delete_file_request(params).await?;
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

    pub fn get_client(&mut self, server_id: &ServerId) -> CollabResult<&mut ClientEnum> {
        if let Some(cli) = self.client_map.get_mut(server_id) {
            Ok(cli)
        } else {
            Err(CollabError::ServerNotConnected(server_id.to_string()))
        }
    }

    pub fn get_doc(&mut self, doc_id: &DocId, server_id: &ServerId) -> CollabResult<&mut Doc> {
        let key = DocDesignator {
            doc: doc_id.to_string(),
            server: server_id.to_string(),
        };
        if let Some(doc) = self.doc_map.get_mut(&key) {
            Ok(doc)
        } else {
            Err(CollabError::DocNotFound(doc_id.clone()))
        }
    }

    pub fn remove_client(&mut self, server_id: &ServerId) {
        self.client_map.remove(server_id);
    }

    pub fn remove_doc(&mut self, doc_id: &DocId, server_id: &ServerId) {
        let key = DocDesignator {
            doc: doc_id.to_string(),
            server: server_id.to_string(),
        };
        self.doc_map.remove(&key);
    }

    pub async fn handle_share_file_request(
        &mut self,
        params: ShareFileParams,
    ) -> CollabResult<ShareFileResp> {
        let client = self.get_client(&params.server_id)?;
        let doc = Doc::new_share_file(
            client.clone(),
            &params.file_name,
            &params.content,
            self.notifier_tx.clone(),
        )
        .await?;

        let resp = ShareFileResp {
            doc_id: doc.doc_id(),
        };
        let key = DocDesignator {
            server: params.server_id,
            doc: doc.doc_id(),
        };

        self.doc_map.insert(key, doc);
        Ok(resp)
    }

    pub async fn handle_send_op_request(
        &mut self,
        params: SendOpParams,
    ) -> CollabResult<SendOpResp> {
        let doc = self.get_doc(&params.doc_id, &params.server_id)?;
        let res = doc.send_op(params.ops).await;
        if res.is_err() {
            self.remove_doc(&params.doc_id, &params.server_id);
        }
        let remote_ops = res?;
        let resp = SendOpResp { ops: remote_ops };
        Ok(resp)
    }

    pub async fn handle_list_files_request(
        &mut self,
        params: ListFilesParams,
    ) -> CollabResult<ListFilesResp> {
        let res = if let Ok(client) = self.get_client(&params.server_id) {
            client.list_files().await
        } else {
            let client = GrpcClient::new(params.server_id.clone(), params.credential).await?;
            self.client_map
                .insert(params.server_id.clone(), client.into());
            let client = self.client_map.get_mut(&params.server_id).unwrap();
            client.list_files().await
        };
        if res.is_err() {
            // If a client has problems, all it's associated docs must
            // have problems too, but no need to clean them up right
            // here. They will cleanup themselves.
            self.client_map.remove(&params.server_id);
        }
        let files = res?;
        let resp = ListFilesResp { files };
        Ok(resp)
    }

    pub async fn handle_connect_to_file_request(
        &mut self,
        params: DocIdParams,
    ) -> CollabResult<ConnectToFileResp> {
        if self.get_doc(&params.doc_id, &params.server_id).is_ok() {
            self.remove_doc(&params.doc_id, &params.server_id)
        }
        let client = self.get_client(&params.server_id)?;
        let (doc, content) = Doc::new_connect_file(
            client.clone(),
            params.doc_id.clone(),
            self.notifier_tx.clone(),
        )
        .await?;
        self.doc_map
            .insert(DocDesignator::new(&params.doc_id, &params.server_id), doc);
        let resp = ConnectToFileResp { content };
        Ok(resp)
    }

    pub async fn handle_disconnect_from_file_request(
        &mut self,
        params: DocIdParams,
    ) -> CollabResult<DocIdParams> {
        if self.get_doc(&params.doc_id, &params.server_id).is_ok() {
            self.remove_doc(&params.doc_id, &params.server_id);
        }
        Ok(params)
    }

    pub async fn handle_delete_file_request(
        &mut self,
        params: DocIdParams,
    ) -> CollabResult<DocIdParams> {
        if self.get_doc(&params.doc_id, &params.server_id).is_ok() {
            self.remove_doc(&params.doc_id, &params.server_id);
        }
        if let Ok(cli) = self.get_client(&params.server_id) {
            cli.delete_file(&params.doc_id).await?;
        }
        Ok(params)
    }
}
