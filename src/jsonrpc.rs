//! This module is the frontend of a collab process. It exposes two
//! functions, [run_stdio] and [run_socket], that communicates with
//! the editor with either pipe or socket.
//!
//! Then entry function will start two threads, one listens for
//! "remote arrived" notification, and sends that notification to the
//! editor; the other reads JSONRPC requests and serves them.
//!
//! Error handling: Error handling is synchronous, if error occurs
//! when serving a request, the error is captured and packaged into a
//! JSONRPC error and send back as the response.
//!
// Non-fatal errors occurred in [crate::collab_server::LocalServer]
// are also communicated to the editor, they are sent to the editor in
// the form of JSONRPC notifications.

use crate::abstract_server::{ClientEnum, DocServer};
use crate::collab_doc::Doc;
use crate::collab_server::{run_webrpc_server, LocalServer};
use crate::config_man::ConfigManager;
use crate::error::{CollabError, CollabResult};
// use crate::grpc_client::GrpcClient;
use crate::collab_webrpc_client::WebrpcClient;
use crate::types::*;
use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::exit;
use tokio::task::JoinHandle;
use tracing::instrument;

pub mod types;
use types::*;

// *** Structs

/// Stores data that the jsonrpc frontend uses.
pub struct JSONRPCServer {
    /// The host id (server uuid) for this host.
    host_id: String,
    /// Private key and self-signed certificate.
    key_cert: ArcKeyCert,
    /// Maps (Doc, Server) to the Doc object.
    doc_map: HashMap<DocDesignator, Doc>,
    /// Maps ServerId (URL) to the server object, which can be either
    /// the local server or a gRPC client connected to a remote
    /// server.
    client_map: HashMap<ServerId, ClientEnum>,
    /// A notifier sender, JSONRPCServer clone it and pass it to the
    /// Doc when we create one, and the Doc will send remote op
    /// notification to the notification channel, which JSONRPCServer
    /// will receive and send notification to the editor.
    notifier_tx: std::sync::mpsc::Sender<CollabNotification>,
    /// The webrpc server that handles remote requests.
    rpc_server: Option<JoinHandle<()>>,
}

// *** Entry functions

/// Run the JSONRPC server on stdio.
pub fn run_stdio(
    runtime: tokio::runtime::Runtime,
    config_man: ConfigManager,
) -> anyhow::Result<()> {
    let (connection, io_threads) = Connection::stdio();
    main_loop(connection, runtime, config_man);
    io_threads.join()?;
    Ok(())
}

/// Run the JSONRPC server on a socket listening on `addr` (host:port).
pub fn run_socket(
    addr: &str,
    runtime: tokio::runtime::Runtime,
    config_man: ConfigManager,
) -> anyhow::Result<()> {
    let (connection, io_threads) = Connection::listen(addr)?;
    main_loop(connection, runtime, config_man);
    io_threads.join()?;
    Ok(())
}

// Handles JSONRPC requests, blocks.
//
// Overview of all the threads:
//
//        Sync world                         Async world
//
// +------------+ <--(1)-- rx notifier tx <--- Doc threads
// |            |
// |            | <--(2)-- rx errors   tx <--- Doc threads
// | Connection |
// |            | ------->
// |            |           (3)  <---block_on--->
// +------------+ <-------
//
fn main_loop(connection: Connection, runtime: tokio::runtime::Runtime, config_man: ConfigManager) {
    let (notifier_tx, notifier_rx) = std::sync::mpsc::channel();
    // This channel is used by both the doc server (for autosave
    // errors) and client (for accept connection error). The client
    // side is synchronous, so it won't generate many errors; the
    // server will take care not to generate too many errors at once.
    // So a modest buffer size should be enough.
    let (async_err_tx, mut async_err_rx) = tokio::sync::mpsc::channel(16);

    let doc_server = LocalServer::new(async_err_tx.clone());
    let mut jsonrpc_server = None;

    let connection = std::sync::Arc::new(connection);
    let connection_1 = std::sync::Arc::clone(&connection);
    let connection_2 = std::sync::Arc::clone(&connection);

    // Instead of using select, just start four threads in the sync
    // world.

    // 1. Send remote op notifications. TODO: Maybe we should
    // throttle-control notifications?
    std::thread::spawn(move || {
        for notif in notifier_rx.iter() {
            let msg = match notif {
                CollabNotification::Op(notif) => lsp_server::Notification {
                    method: NotificationCode::RemoteOpArrived.into(),
                    params: serde_json::to_value(notif).unwrap(),
                },
                CollabNotification::Info(info) => lsp_server::Notification {
                    method: NotificationCode::Info.into(),
                    params: serde_json::to_value(info).unwrap(),
                },
                CollabNotification::HarmlessErr(err) => lsp_server::Notification {
                    method: NotificationCode::HarmlessErr.into(),
                    params: serde_json::to_value(err).unwrap(),
                },
            };
            if let Err(err) = connection_1.sender.send(Message::Notification(msg)) {
                tracing::error!(?err, "Error sending jsonrpc notification to editor");
                // FIXME: More graceful shutdown
                exit(-1);
            }
        }
    });

    // 2. Send non-fatal collab server errors.
    std::thread::spawn(move || loop {
        let err: Option<CollabError> = async_err_rx.blocking_recv();
        if err.is_none() {
            tracing::error!("JSONRPC server error channel broke");
            // FIXME: More graceful shutdown.
            exit(-1);
        }
        let msg = match err.unwrap() {
            CollabError::SignalingTimesUp(time) => lsp_server::Notification {
                method: NotificationCode::SignalingTimesUp.into(),
                params: serde_json::to_value(time).unwrap(),
            },
            CollabError::AcceptConnectionErr(err) => lsp_server::Notification {
                method: NotificationCode::AcceptConnectionErr.into(),
                params: serde_json::to_value(err).unwrap(),
            },
            CollabError::AutoSaveErr(doc_id, msg) => lsp_server::Notification {
                method: NotificationCode::AutoSaveErr.into(),
                params: serde_json::to_value(AutoSaveErrParams { doc_id, msg }).unwrap(),
            },
            err => lsp_server::Notification {
                method: NotificationCode::ServerError.into(),
                params: serde_json::to_value(err.to_string()).unwrap(),
            },
        };
        connection_2
            .sender
            .send(Message::Notification(msg))
            .unwrap();
    });

    // 3. Handle requests on the main thread.
    loop {
        let msg = connection.receiver.recv();
        if msg.is_err() {
            tracing::error!("JSONRPC request channel broke");
            // FIXME: Save everything and exit.
            return;
        }
        match msg.unwrap() {
            Message::Request(req) => {
                let id = req.id.clone();
                let msg: Message;
                if req.method == "Initialize" {
                    let res = handle_init_request(
                        req,
                        doc_server.clone(),
                        notifier_tx.clone(),
                        &config_man,
                    );
                    msg = match res {
                        Ok(server) => {
                            jsonrpc_server = Some(server);
                            make_resp(id, ())
                        }
                        Err(err) => {
                            let code = error_code(&err);
                            make_err(id, code, format!("{:#}", err))
                        }
                    };
                } else if let Some(ref mut jsonrpc_server) = jsonrpc_server {
                    tracing::debug!(?req, "handle_request");

                    let res = runtime.block_on(jsonrpc_server.handle_request(
                        req,
                        doc_server.clone(),
                        async_err_tx.clone(),
                    ));

                    tracing::debug!(?res, "handle_request");

                    msg = match res {
                        Ok(msg) => msg,
                        Err(err) => {
                            let code = error_code(&err);
                            make_err(id, code, format!("{:#}", err))
                        }
                    };
                } else {
                    msg = make_err(
                        req.id,
                        error_code(&CollabError::NotInitialized),
                        "Server not initialized".to_string(),
                    );
                }

                let res = connection.sender.send(msg);
                if res.is_err() {
                    tracing::error!(?res, "JSONRPC response channel broke");
                    // FIXME: More graceful shutdown
                    exit(-1);
                }
            }
            Message::Response(_) => {
                tracing::warn!("Received a response");
            }
            Message::Notification(notif) => {
                if let Some(ref mut jsonrpc_server) = jsonrpc_server {
                    let res = runtime.block_on(
                        jsonrpc_server.handle_notification(notif.clone(), doc_server.clone()),
                    );
                    if let Err(err) = res {
                        tracing::error!(?notif, ?err, "Error handling notification");
                    }
                }
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
        CollabError::Fatal(_) => ErrorCode::ServerFatal,
        CollabError::DocFatal(_) => ErrorCode::DocFatal,
        CollabError::EngineError(_) => ErrorCode::DocFatal,
        CollabError::RpcError(_) => ErrorCode::DocNonFatal,

        CollabError::BadRequest(_) => ErrorCode::BadRequest,
        CollabError::DocNotFound(_) => ErrorCode::DocNotFound,
        CollabError::FileNotFound(_) => ErrorCode::FileNotFound,
        CollabError::DocAlreadyExists(_) => ErrorCode::DocAlreadyExists,
        CollabError::ServerNotConnected(_) => ErrorCode::DocNonFatal,
        CollabError::NotRegularFile(_) => ErrorCode::NotRegularFile,
        CollabError::NotDirectory(_) => ErrorCode::NotDirectory,
        CollabError::UnsupportedOperation(_) => ErrorCode::UnsupportedOperation,
        CollabError::NotInitialized => ErrorCode::NotInitialized,
        CollabError::AutoSaveErr(_, _) => ErrorCode::ErrAutoSave,

        // This shouldn't be ever needed, because they are converted
        // into notifications.
        CollabError::SignalingTimesUp(_) => ErrorCode::DocNonFatal,
        CollabError::AcceptConnectionErr(_) => ErrorCode::DocNonFatal,
    }
}

// *** Functions for JSONRPCServer

impl JSONRPCServer {
    pub fn new(
        server: LocalServer,
        notifier_tx: std::sync::mpsc::Sender<CollabNotification>,
        host_id: String,
        key_cert: KeyCert,
    ) -> JSONRPCServer {
        let mut client_map = HashMap::new();
        client_map.insert(SERVER_ID_SELF.to_string(), server.into());
        JSONRPCServer {
            host_id,
            key_cert: std::sync::Arc::new(key_cert),
            doc_map: HashMap::new(),
            client_map,
            notifier_tx,
            rpc_server: None,
        }
    }

    /// Handle `request`, return response message.
    async fn handle_request(
        &mut self,
        request: Request,
        server: LocalServer,
        async_err_tx: tokio::sync::mpsc::Sender<CollabError>,
    ) -> CollabResult<Message> {
        let msg = if request.method == "ShareFile" {
            let params: ShareFileParams = serde_json::from_value(request.params)?;
            let resp = self.handle_share_file_request(params).await?;
            make_resp(request.id, resp)
        } else if request.method == "ShareDir" {
            let params: ShareDirParams = serde_json::from_value(request.params)?;
            let resp = self.handle_share_dir_request(params).await?;
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
        } else if request.method == "DisconnectFromFile" {
            let params: DocIdParams = serde_json::from_value(request.params)?;
            let resp = self.handle_disconnect_from_file_request(params).await?;
            make_resp(request.id, resp)
        } else if request.method == "DeleteFile" {
            let params: DocIdParams = serde_json::from_value(request.params)?;
            let resp = self.handle_delete_file_request(params).await?;
            make_resp(request.id, resp)
        } else if request.method == "Undo" {
            let params: UndoParams = serde_json::from_value(request.params)?;
            let resp = self.handle_undo_request(params)?;
            make_resp(request.id, resp)
        } else if request.method == "AcceptConnection" {
            let params: AcceptConnectionParams = serde_json::from_value(request.params)?;
            let resp = self
                .handle_accept_connection_request(params, server, async_err_tx)
                .await?;
            make_resp(request.id, resp)
        } else if request.method == "PrintHistory" {
            let params: PrintHistoryParams = serde_json::from_value(request.params)?;
            let resp = self.handle_print_history_request(params)?;
            make_resp(request.id, resp)
        } else {
            // FIXME: Replace with proper handling.
            todo!()
        };
        Ok(msg)
    }

    /// Handle `notif`.
    #[instrument(skip(self, _server))]
    async fn handle_notification(
        &mut self,
        notif: Notification,
        _server: LocalServer,
    ) -> CollabResult<()> {
        if notif.method == "SendInfo" {
            let params: SendInfoParams = serde_json::from_value(notif.params)?;
            self.handle_send_info_notif(params).await?;
            Ok(())
        } else {
            // FIXME: Replace with proper handling.
            todo!()
        }
    }
}

// *** Subroutines for handle_request

fn handle_init_request(
    req: Request,
    doc_server: LocalServer,
    notifier_tx: std::sync::mpsc::Sender<CollabNotification>,
    config_man: &ConfigManager,
) -> CollabResult<JSONRPCServer> {
    let params: InitParams = serde_json::from_value(req.params)?;
    let host_id = params.host_id;
    let key_cert = config_man.get_key_and_cert(host_id.clone())?;

    Ok(JSONRPCServer::new(
        doc_server,
        notifier_tx,
        host_id,
        key_cert,
    ))
}

// Request handler that operates on documents must handle errors by
// removing the doc from doc_map, and returning the error.

impl JSONRPCServer {
    pub fn get_client(&mut self, server_id: &ServerId) -> CollabResult<&mut ClientEnum> {
        if let Some(cli) = self.client_map.get_mut(server_id) {
            Ok(cli)
        } else {
            Err(CollabError::ServerNotConnected(server_id.to_string()))
        }
    }

    pub fn get_doc(&mut self, doc_id: &DocId, server_id: &ServerId) -> CollabResult<&mut Doc> {
        let key = DocDesignator {
            doc: doc_id.clone(),
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
            doc: doc_id.clone(),
            server: server_id.to_string(),
        };
        self.doc_map.remove(&key);
    }

    #[instrument(skip(self))]
    pub async fn handle_share_file_request(
        &mut self,
        params: ShareFileParams,
    ) -> CollabResult<ShareFileResp> {
        if params.file_name == "" {
            return Err(CollabError::BadRequest(
                "File name can’t be empty".to_string(),
            ));
        }
        let client = self.get_client(&params.host_id)?;
        let site_id = client.site_id();
        let doc = Doc::new_share_file(
            client.clone(),
            &params.file_name,
            &params.file_meta,
            params.content,
            self.notifier_tx.clone(),
        )
        .await?;

        let resp = ShareFileResp {
            doc_id: doc.doc_id(),
            site_id,
        };
        let key = DocDesignator {
            server: params.host_id,
            doc: doc.doc_id(),
        };

        self.doc_map.insert(key, doc);
        Ok(resp)
    }

    #[instrument(skip(self))]
    pub async fn handle_share_dir_request(
        &mut self,
        params: ShareDirParams,
    ) -> CollabResult<ShareDirResp> {
        let client = self.get_client(&params.host_id)?;
        let path = expanduser::expanduser(&params.path)
            .map_err(|_| CollabError::FileNotFound(params.path))?;
        if params.dir_name == "" {
            return Err(CollabError::BadRequest(
                "Dir name can’t be empty".to_string(),
            ));
        }
        let doc_id = client
            .share_file(
                &params.dir_name,
                &params.dir_meta,
                FileContentOrPath::Path(PathBuf::from(path)),
            )
            .await?;

        let resp = ShareDirResp { doc_id };
        Ok(resp)
    }

    #[instrument(skip(self))]
    pub async fn handle_send_op_request(
        &mut self,
        params: SendOpParams,
    ) -> CollabResult<Option<SendOpResp>> {
        let doc = self.get_doc(&params.doc_id, &params.host_id)?;
        let res = doc.send_op(params.ops);
        if res.is_err() {
            self.remove_doc(&params.doc_id, &params.host_id);
        }
        if let Some((remote_ops, last_seq)) = res? {
            let resp = SendOpResp {
                ops: remote_ops,
                last_seq,
            };
            Ok(Some(resp))
        } else {
            Ok(None)
        }
    }

    #[instrument(skip(self))]
    pub async fn handle_send_info_notif(&mut self, params: SendInfoParams) -> CollabResult<()> {
        let client = self.get_client(&params.host_id)?;
        client
            .send_info(&params.doc_id, serde_json::to_string(&params.info)?)
            .await?;
        Ok(())
    }

    #[instrument(skip(self))]
    pub async fn handle_list_files_request(
        &mut self,
        params: ListFilesParams,
    ) -> CollabResult<ListFilesResp> {
        let dir_path = match &params.dir {
            Some(DocDesc::Dir((dir_id, rel_path))) => Some((*dir_id, PathBuf::from(rel_path))),
            None => None,
            _ => {
                return Err(CollabError::NotDirectory(format!(
                    "{:?}",
                    &params.dir.unwrap()
                )));
            }
        };

        let res = if let Ok(client) = self.get_client(&params.host_id) {
            client.list_files(dir_path).await
        } else {
            let client = WebrpcClient::new(
                params.host_id.clone(),
                self.key_cert.clone(),
                &params.signaling_addr,
                params.credential,
            )
            .await?;
            self.client_map
                .insert(params.host_id.clone(), client.into());
            let client = self.client_map.get_mut(&params.host_id).unwrap();
            client.list_files(dir_path).await
        };
        if res.is_err() {
            // If a client has problems, all it's associated docs must
            // have problems too, but no need to clean them up right
            // here. They will cleanup themselves.
            self.client_map.remove(&params.host_id);
        }
        let files = res?;
        let resp = ListFilesResp { files };
        Ok(resp)
    }

    #[instrument(skip(self))]
    pub async fn handle_connect_to_file_request(
        &mut self,
        params: ConnectToFileParams,
    ) -> CollabResult<ConnectToFileResp> {
        match &params.doc_desc {
            DocDesc::Doc(doc_id) => {
                if self.get_doc(doc_id, &params.host_id).is_ok() {
                    self.remove_doc(doc_id, &params.host_id)
                }
            }
            _ => (),
        }

        let client = self.get_client(&params.host_id)?;
        let site_id = client.site_id();
        let (doc, content) = Doc::new_connect_file(
            client.clone(),
            params.doc_desc.clone(),
            self.notifier_tx.clone(),
        )
        .await?;
        let file_name = doc.file_name();
        let doc_id = doc.doc_id();
        self.doc_map
            .insert(DocDesignator::new(&doc_id, &params.host_id), doc);
        let resp = ConnectToFileResp {
            content,
            site_id,
            file_name,
            doc_id,
        };
        Ok(resp)
    }

    #[instrument(skip(self))]
    pub async fn handle_disconnect_from_file_request(
        &mut self,
        params: DocIdParams,
    ) -> CollabResult<DocIdParams> {
        if self.get_doc(&params.doc_id, &params.host_id).is_ok() {
            self.remove_doc(&params.doc_id, &params.host_id);
        }
        Ok(params)
    }

    #[instrument(skip(self))]
    pub async fn handle_delete_file_request(
        &mut self,
        params: DocIdParams,
    ) -> CollabResult<DocIdParams> {
        if self.get_doc(&params.doc_id, &params.host_id).is_ok() {
            self.remove_doc(&params.doc_id, &params.host_id);
        }
        if let Ok(cli) = self.get_client(&params.host_id) {
            cli.delete_file(&params.doc_id).await?;
        }
        Ok(params)
    }

    #[instrument(skip(self))]
    pub fn handle_undo_request(&mut self, params: UndoParams) -> CollabResult<UndoResp> {
        let doc = self.get_doc(&params.doc_id, &params.host_id)?;
        let res = match params.kind {
            UndoKind::Undo => doc.undo(),
            UndoKind::Redo => doc.redo(),
        };
        match res {
            Ok(ops) => {
                let resp = UndoResp { ops };
                Ok(resp)
            }
            Err(err) => {
                self.remove_doc(&params.doc_id, &params.host_id);
                Err(err)
            }
        }
    }

    #[instrument(skip(self))]
    pub async fn handle_accept_connection_request(
        &mut self,
        params: AcceptConnectionParams,
        server: LocalServer,
        async_err_tx: tokio::sync::mpsc::Sender<CollabError>,
    ) -> CollabResult<()> {
        if let Some(rpc_server) = &self.rpc_server {
            if !rpc_server.is_finished() {
                return Ok(());
            }
        }
        let key_cert = self.key_cert.clone();
        let handle = tokio::spawn(async move {
            let res =
                run_webrpc_server(params.host_id, key_cert, params.signaling_addr, server).await;
            if let Err(err) = res {
                let err = match err {
                    CollabError::SignalingTimesUp(time) => CollabError::SignalingTimesUp(time),
                    err => CollabError::AcceptConnectionErr(err.to_string()),
                };
                async_err_tx.send(err).await.unwrap();
            }
        });
        self.rpc_server = Some(handle);
        Ok(())
    }

    #[instrument(skip(self))]
    pub fn handle_print_history_request(
        &mut self,
        params: PrintHistoryParams,
    ) -> CollabResult<String> {
        let doc = self.get_doc(&params.doc_id, &params.host_id)?;
        Ok(doc.print_history(params.debug))
    }
}
