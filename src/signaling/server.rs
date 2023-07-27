//! This module provides a signaling server. Use
//! [run_signaling_server] to run it.

//! The server is pretty straightforward, it listens for websocket
//! connections, spawns a task for each new connection and handles
//! them with [handle_connection]. The server keeps a map mapping
//! endpoint ids to the channel that sends messages to that endpoint.
//! Serving requests mostly consists of adding entries to the map, and
//! relaying messages to the endpoint with the target id.

use crate::signaling::{EndpointId, SignalingMessage, SDP};
use futures_util::{SinkExt, StreamExt};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::sync::RwLock;
use tokio::time;
use tokio_tungstenite as tung;
use tokio_tungstenite::tungstenite::{
    protocol::{frame::coding::CloseCode, CloseFrame},
    Message,
};
use tokio_tungstenite::WebSocketStream;

/// Run the signaling server on `addr`. Addr should be of the form
/// "ip:port".
pub async fn run_signaling_server(addr: &str) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    let server = Server::new();
    while let Ok((stream, client_addr)) = listener.accept().await {
        let server = server.clone();
        let _ = tokio::spawn(async move {
            let mut endpoint_id = None;
            let res = handle_connection(&server, stream, &mut endpoint_id).await;
            if let Some(id) = endpoint_id {
                server.remove_endpoint(&id).await;
            }
            if let Err(err) = res {
                log::warn!(
                    "Error occurred when serving request from {}: {:?}",
                    client_addr,
                    err
                );
            }
        });
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct EndpointInfo {
    public: bool,
    sdp: SDP,
    /// Connection request and client candidate are sent to this
    /// channel.
    msg_tx: mpsc::Sender<Message>,
}

/// Signaling server.
#[derive(Debug, Clone)]
struct Server {
    /// Maps collab server id to their SDP.
    endpoint_map: Arc<RwLock<HashMap<EndpointId, EndpointInfo>>>,
}

impl Server {
    fn new() -> Server {
        Server {
            endpoint_map: Arc::new(RwLock::new(HashMap::new())),
        }
    }
    /// Bind a collab server to `id`, return the allocated id.
    async fn bind_endpoint(
        &self,
        public: bool,
        id: EndpointId,
        sdp: SDP,
        msg_tx: mpsc::Sender<Message>,
    ) {
        let server_info = EndpointInfo {
            public,
            sdp,
            msg_tx,
        };
        self.endpoint_map.write().await.insert(id, server_info);
    }
    /// Remove binding for serser with `id`.
    async fn remove_endpoint(&self, id: &EndpointId) {
        self.endpoint_map.write().await.remove(id);
    }
    /// Get the server info for the server with `id`.
    async fn get_endpoint_info(&self, id: &EndpointId) -> Option<EndpointInfo> {
        self.endpoint_map
            .read()
            .await
            .get(id)
            .map(|info| info.clone())
    }
}

/// Handle a connection from a collab server or client. If the
/// connected endpoint is a collab server and requested for listen,
/// `id` is set to the allocated id.
async fn handle_connection(
    server: &Server,
    stream: TcpStream,
    endpoint_id: &mut Option<EndpointId>,
) -> anyhow::Result<()> {
    let stream = tung::accept_async(stream).await?;

    let (req_tx, mut req_rx) = mpsc::channel(1);
    let (resp_tx, resp_rx) = mpsc::channel(1);

    let _ = tokio::spawn(send_receive_stream(stream, req_tx, resp_rx));

    while let Some(msg) = req_rx.recv().await {
        match msg? {
            Message::Text(msg) => {
                let msg: SignalingMessage = serde_json::from_str(&msg)?;
                match msg {
                    SignalingMessage::BindRequest(id, sdp) => {
                        // Remove the previous allocation before
                        // allocating a new id.
                        if let Some(existing_id) = endpoint_id {
                            server.remove_endpoint(existing_id).await;
                        }
                        // Allocate a new id.
                        server
                            .bind_endpoint(true, id.clone(), sdp, resp_tx.clone())
                            .await;
                        *endpoint_id = Some(id.clone());
                    }
                    SignalingMessage::ConnectRequest(my_id, their_id, my_sdp) => {
                        // Look for the endpoint corresponds to the id.
                        let endpoint_info = server.get_endpoint_info(&their_id).await;
                        // Found a server, send server's SDP to client
                        // and client's SDP to server.
                        let is_public = if let Some(info) = &endpoint_info {
                            info.public
                        } else {
                            false
                        };
                        if endpoint_info.is_none() || !is_public {
                            // Didn't find the endpoint with this id.
                            let resp = SignalingMessage::NoEndpointForId(their_id);
                            resp_tx.send(resp.into()).await?;
                            resp_tx.send(Message::Close(None)).await?;
                            return Ok(());
                        }
                        let endpoint_info = endpoint_info.unwrap();

                        // Bind connection initializer.
                        server
                            .bind_endpoint(false, my_id.clone(), my_sdp.clone(), resp_tx.clone())
                            .await;
                        *endpoint_id = Some(my_id.clone());

                        // Response to connection initializer.
                        let resp = SignalingMessage::ConnectionInvitation(
                            their_id,
                            endpoint_info.sdp.clone(),
                        );
                        resp_tx.send(resp.into()).await?;

                        // Notify connection listener.
                        let connect_req = SignalingMessage::ConnectionInvitation(my_id, my_sdp);
                        endpoint_info
                            .msg_tx
                            .send(Message::Text(serde_json::to_string(&connect_req).unwrap()))
                            .await?;
                    }
                    SignalingMessage::SendCandidateTo(their_id, candidate) => {
                        if let Some(my_id) = endpoint_id {
                            if let Some(their_info) = server.get_endpoint_info(&their_id).await {
                                let msg = SignalingMessage::CandidateFrom(my_id.clone(), candidate);
                                their_info.msg_tx.send(msg.into()).await?;
                            } else {
                                let msg = SignalingMessage::NoEndpointForId(their_id);
                                resp_tx.send(msg.into()).await?;
                            }
                        } else {
                            let resp = resp_unsupported("You should send a ConnectRequest or BindRequest before sending SendCandidate");
                            resp_tx.send(resp).await?;
                            continue;
                        }
                    }
                    _ => {
                        let resp = resp_unsupported("You should only send BindRequest or ConnectRequest to the signal server");
                        resp_tx.send(resp).await?;
                    }
                }
            }
            _ => {
                let resp = Message::Close(Some(CloseFrame {
                    code: CloseCode::Unsupported,
                    reason: Cow::Owned("We only support text message".to_string()),
                }));
                resp_tx.send(resp).await?;
            }
        }
    }
    Ok(())
}

// *** Subroutines for handle_connection

/// Sends and receives messages from `stream`.
async fn send_receive_stream(
    mut stream: WebSocketStream<TcpStream>,
    req_tx: mpsc::Sender<Result<Message, tung::tungstenite::Error>>,
    mut resp_rx: mpsc::Receiver<Message>,
) {
    // TCP sockets aren't free, close the connection after 3 min.
    let (time_tx, mut time_rx) = mpsc::channel(1);
    tokio::spawn(async move {
        time::sleep(Duration::from_secs(180)).await;
        if time_tx.send(()).await.is_err() {
            return;
        }
    });

    loop {
        tokio::select! {
            _ = time_rx.recv() => {
                stream.send(Message::Close(Some(CloseFrame {
                    code: CloseCode::Policy,
                    reason: Cow::Owned("It's been 3 min, time is up".to_string())
                }))).await.unwrap();
                return;
            }
            msg = stream.next() => {
                if let Some(msg) = msg {
                    if req_tx.send(msg).await.is_err() {
                        return;
                    }
                } else {
                    return;
                }
            }
            resp = resp_rx.recv() => {
                if let Some(msg) = resp {
                    if stream.send(msg).await.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

/// Returns a "Unsupported" close message, where the close reason is
/// `msg`.
fn resp_unsupported(msg: &str) -> Message {
    Message::Close(Some(CloseFrame {
        code: CloseCode::Unsupported,
        reason: Cow::Owned(msg.to_string()),
    }))
}
