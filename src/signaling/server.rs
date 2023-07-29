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
    async fn bind_endpoint(&self, public: bool, id: EndpointId, msg_tx: mpsc::Sender<Message>) {
        let server_info = EndpointInfo { public, msg_tx };
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
        let msg = msg?;
        log::debug!("Received message {:?}", &msg);
        match msg {
            Message::Text(msg) => {
                let msg: SignalingMessage = serde_json::from_str(&msg)?;
                match msg {
                    SignalingMessage::Bind(id) => {
                        // Check if the id is already taken.
                        if let Some(existing_id) = endpoint_id {
                            let resp = SignalingMessage::IdTaken(existing_id.clone());
                            resp_tx.send(resp.into()).await?;
                            return Ok(());
                        }
                        // Allocate a new id.
                        server
                            .bind_endpoint(true, id.clone(), resp_tx.clone())
                            .await;
                        *endpoint_id = Some(id.clone());
                    }
                    SignalingMessage::Connect(sender_id, receiver_id, sdp) => {
                        // Look for the endpoint corresponds to the id.
                        let endpoint_info = server.get_endpoint_info(&receiver_id).await;
                        if endpoint_info.is_none() {
                            // Didn't find the endpoint with this id.
                            let resp = SignalingMessage::NoEndpointForId(receiver_id);
                            resp_tx.send(resp.into()).await?;
                            resp_tx.send(Message::Close(None)).await?;
                            return Ok(());
                        }
                        let endpoint_info = endpoint_info.unwrap();

                        check_id(&sender_id, endpoint_id, &resp_tx).await?;

                        // Bind connection initializer.
                        server
                            .bind_endpoint(false, sender_id.clone(), resp_tx.clone())
                            .await;
                        *endpoint_id = Some(sender_id.clone());

                        // Notify connection listener.
                        let connect_req = SignalingMessage::Connect(sender_id, receiver_id, sdp);
                        endpoint_info.msg_tx.send(connect_req.into()).await?;
                    }
                    SignalingMessage::Candidate(sender_id, receiver_id, candidate) => {
                        if endpoint_id.is_none() {
                            let resp = resp_unsupported("You should send a Connect or Bind message before sending Candidate message");
                            resp_tx.send(resp).await?;
                            return Ok(());
                        }
                        check_id(&sender_id, endpoint_id, &resp_tx).await?;
                        if let Some(their_info) = server.get_endpoint_info(&receiver_id).await {
                            let msg = SignalingMessage::Candidate(
                                sender_id.clone(),
                                receiver_id.clone(),
                                candidate,
                            );
                            their_info.msg_tx.send(msg.into()).await?;
                        } else {
                            let msg = SignalingMessage::NoEndpointForId(receiver_id);
                            resp_tx.send(msg.into()).await?;
                        }
                    }
                    _ => {
                        let resp = resp_unsupported(
                            "You should only send Bind, Connect, or Candidate message to the signal server",
                        );
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

/// If `provided_id` doesn't match `recorded_id`, return an error.
async fn check_id(
    provided_id: &EndpointId,
    recorded_id: &Option<EndpointId>,
    resp_tx: &mpsc::Sender<Message>,
) -> anyhow::Result<()> {
    if let Some(endpoint_id) = recorded_id {
        if endpoint_id != provided_id {
            let resp =
                resp_unsupported("You are using a endpoint id different from the one recorded");
            resp_tx.send(resp).await?;
            return Err(anyhow::Error::msg(
                "Endpoint tries to use a different endpoint as previously used",
            ));
        }
    }
    Ok(())
}
