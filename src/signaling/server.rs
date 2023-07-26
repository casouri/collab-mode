use crate::signaling::{SignalingMessage, SDP};
use futures_util::{SinkExt, StreamExt};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::sync::{Mutex, RwLock};
use tokio::time;
use tokio_tungstenite as tung;
use tokio_tungstenite::tungstenite::{
    protocol::{frame::coding::CloseCode, CloseFrame},
    Message,
};

use super::ClientId;

/// Run the signaling server on `addr`. Addr should be of the form
/// "ip:port".
pub async fn run_signaling_server(addr: &str) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    let server = Server::new();
    while let Ok((stream, client_addr)) = listener.accept().await {
        let server = server.clone();
        let _ = tokio::spawn(async move {
            let mut server_id = None;
            let res = handle_connection(&server, stream, &mut server_id).await;
            if let Some(server_id) = server_id {
                server.remove_server(&server_id).await;
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
struct ServerInfo {
    sdp: SDP,
    /// Connection request and client candidate are sent to this
    /// channel.
    msg_tx: mpsc::Sender<Message>,
}

impl Server {
    fn new() -> Server {
        Server {
            server_map: Arc::new(RwLock::new(HashMap::new())),
            client_map: Arc::new(RwLock::new(HashMap::new())),
        }
    }
    /// Bind a collab server to `id`, return the allocated id.
    async fn bind(&self, id: String, sdp: SDP, msg_tx: mpsc::Sender<Message>) {
        let server_info = ServerInfo { sdp, msg_tx };
        self.server_map.write().await.insert(id, server_info);
    }
    /// Remove binding for serser with `id`.
    async fn remove_server(&self, id: &str) {
        self.server_map.write().await.remove(id);
    }
}

/// Signaling server.
#[derive(Debug, Clone)]
struct Server {
    /// Maps collab server id to their SDP.
    server_map: Arc<RwLock<HashMap<String, ServerInfo>>>,
    client_map: Arc<RwLock<HashMap<ClientId, mpsc::Sender<Message>>>>,
}

/// Handle a connection from a collab server or client. If the
/// connected endpoint is a collab server and requested for listen,
/// `id` is set to the allocated id.
async fn handle_connection(
    server: &Server,
    stream: TcpStream,
    server_id: &mut Option<String>,
) -> anyhow::Result<()> {
    let mut stream = tung::accept_async(stream).await?;
    let (req_tx, mut req_rx) = mpsc::channel(1);
    let (resp_tx, mut resp_rx) = mpsc::channel(1);

    // Spawn a thread that sends and receives messages from the
    // stream;
    let _ = tokio::spawn(async move {
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
                    if req_tx.send(msg).await.is_err() {
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
    });

    while let Some(Some(msg)) = req_rx.recv().await {
        match msg? {
            Message::Text(msg) => {
                let msg: SignalingMessage = serde_json::from_str(&msg)?;
                match msg {
                    SignalingMessage::BindRequest(uuid, sdp) => {
                        // Remove the previous allocation before
                        // allocating a new id.
                        if let Some(server_id) = server_id {
                            server.remove_server(server_id).await;
                        }
                        // Allocate a new id.
                        server.bind(uuid.clone(), sdp, resp_tx.clone()).await;
                        *server_id = Some(uuid.clone());
                    }
                    SignalingMessage::ConnectRequest(id, client_sdp) => {
                        // Look for the server corresponds to the id.
                        let server_info = server
                            .server_map
                            .read()
                            .await
                            .get(&id)
                            .map(|info| info.clone());
                        // Found a server, send server's SDP to client
                        // and client's SDP to server.
                        if let Some(server_info) = server_info {
                            // Response to collab client.
                            let resp = SignalingMessage::ConnectResponse(server_info.sdp.clone());
                            resp_tx
                                .send(Message::Text(serde_json::to_string(&resp).unwrap()))
                                .await?;

                            // Notify collab server.
                            let connect_req = SignalingMessage::BindResponse(client_sdp);
                            server_info
                                .msg_tx
                                .send(Message::Text(serde_json::to_string(&connect_req).unwrap()))
                                .await?;
                        } else {
                            // Didn't find the server with this id.
                            let resp = SignalingMessage::NoServerForId(id);
                            resp_tx
                                .send(Message::Text(serde_json::to_string(&resp).unwrap()))
                                .await?;
                        }
                    }
                    _ => {
                        let resp = Message::Close(Some(CloseFrame {
                            code: CloseCode::Unsupported,
                            reason: Cow::Owned(
                                "You should only send BindRequest or ConnectRequest to the signal server"
                                    .to_string(),
                            ),
                        }));
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
