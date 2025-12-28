//! This module provides a signaling server. Use
//! [run_signaling_server] to run it.
//!
//! The server is pretty straightforward, it listens for websocket
//! connections, spawns a task for each new connection and handles
//! them with [handle_connection]. The server keeps a map mapping
//! endpoint ids to the channel that sends messages to that endpoint.
//! Serving requests mostly consists of adding entries to the map, and
//! relaying messages to the endpoint with the target id.

use crate::signaling::{EndpointId, SignalingMsg};
use futures_util::{SinkExt, StreamExt};
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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
use tracing::{instrument, Instrument};

use super::key_store::{self, PubKeyStore};
use super::{CertDerHash, SignalingError, SignalingResult};

/// Run the signaling server on `addr`. Addr should be of the form
/// "ip:port".
pub async fn run_signaling_server(addr: &str, db_path: &Path) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    let key_store = key_store::PubKeyStore::new(db_path)?;
    let server = Server::new(key_store);
    while let Ok((stream, client_addr)) = listener.accept().await {
        tracing::info!("Accepted a connection from {}", client_addr);
        let server = server.clone();
        let span = tracing::info_span!("Serve connection", %client_addr);
        let _ = tokio::spawn(
            async move {
                let mut endpoint_id = None;
                let res = handle_connection(&server, stream, &mut endpoint_id).await;
                tracing::info!(?endpoint_id);
                if let Some(id) = endpoint_id {
                    server.remove_endpoint(&id).await;
                }
                {
                    let map = server.endpoint_map.read().await;
                    tracing::info!(?map);
                }
                if let Err(err) = res {
                    tracing::info!(
                        %err,
                        "Stopped serving connection due to error",
                    );
                }
            }
            .instrument(span),
        );
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct EndpointInfo {
    /// Connection request and client candidate are sent to this
    /// channel.
    msg_tx: mpsc::Sender<Message>,
}

/// Signaling server.
#[derive(Clone)]
struct Server {
    /// Maps collab server id to their SDP.
    endpoint_map: Arc<RwLock<HashMap<EndpointId, EndpointInfo>>>,
    key_store: Arc<Mutex<PubKeyStore>>,
}

impl Server {
    fn new(key_store: PubKeyStore) -> Server {
        Server {
            endpoint_map: Arc::new(RwLock::new(HashMap::new())),
            key_store: Arc::new(Mutex::new(key_store)),
        }
    }

    /// Bind a collab server to `id`. Return error if some endpoint
    /// is already connected with that id, or some endpoint has
    /// connected with that id and the pub key doesn't match.
    async fn bind_endpoint(
        &self,
        id: EndpointId,
        key: CertDerHash,
        msg_tx: mpsc::Sender<Message>,
    ) -> SignalingResult<()> {
        tracing::info!(id, key, "Handle req: bind()");
        // First check pub key.
        let id_taken = {
            let key_store = self.key_store.lock().unwrap();
            if let Some(saved_key) = key_store.get_key_for(&id)? {
                if saved_key != key {
                    tracing::info!(saved_key, key, "key mismatch");
                }
                saved_key != key
            } else {
                key_store.set_key_for(&id, &key)?;
                false
            }
        };
        if id_taken {
            let _ = msg_tx
                .send(
                    SignalingMsg::IdTaken(
                        id.clone(),
                        "ID already taken (key mismatch)".to_string(),
                    )
                    .into(),
                )
                .await;
            return Err(SignalingError::OtherError(
                "ID already taken (key mismatch)".to_string(),
            ));
        }

        // Try to insert connection to endpoint map, if there’s
        // already a connection using that id, fail.
        let server_info = EndpointInfo {
            msg_tx: msg_tx.clone(),
        };
        let success = self.insert_endpoint(id.clone(), server_info).await;
        if !success {
            let _ = msg_tx
                .send(SignalingMsg::IdTaken(id.clone(), "ID already taken".to_string()).into())
                .await;
            return Err(SignalingError::OtherError("ID already taken".to_string()));
        }

        let _ = msg_tx.send(SignalingMsg::Bound(id.clone()).into()).await;
        Ok(())
    }

    // Send connect request to the endpoint with `receiver_id`.
    async fn connect_to_endpoint(
        &self,
        sender_id: &EndpointId,
        receiver_id: EndpointId,
        sender_key: CertDerHash,
        initiator: bool,
        resp_tx: &mpsc::Sender<Message>,
    ) -> SignalingResult<()> {
        tracing::info!(
            sender_id,
            receiver_id,
            sender_key,
            initiator,
            "Handle req: connect()"
        );
        let endpoint_info = self.get_endpoint_info(&receiver_id).await;
        match endpoint_info {
            Some(endpoint_info) => {
                // Notify connection listener.
                let connect_req = SignalingMsg::Connect(
                    sender_id.clone(),
                    receiver_id.clone(),
                    sender_key,
                    initiator,
                );
                let _ = endpoint_info.msg_tx.send(connect_req.into()).await;
                tracing::info!(
                    "Sent Connect message to {} on behave of {}",
                    receiver_id,
                    sender_id
                );
                Ok(())
            }
            None => {
                // Didn't find the endpoint with this id.
                let resp = SignalingMsg::IdNotFound(
                    sender_id.clone(),
                    format!("No endpoint found for id: {}", receiver_id),
                );
                let _ = resp_tx.send(resp.into()).await;
                tracing::info!("No endpoint for id: {}", receiver_id);
                Ok(())
            }
        }
    }

    /// Insert `id=info` into the endpoint map. Return false if `id`
    /// is taken in the map, return true if inserted successfully.
    async fn insert_endpoint(&self, id: EndpointId, info: EndpointInfo) -> bool {
        let mut map = self.endpoint_map.write().await;
        if map.contains_key(&id) {
            return false;
        } else {
            map.insert(id, info);
            return true;
        }
    }

    /// Remove binding for endpoint with `id`.
    async fn remove_endpoint(&self, id: &EndpointId) {
        let mut endpoint_map = self.endpoint_map.write().await;
        endpoint_map.remove(id);
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
/// connected endpoint is a collab server and requested to bind,
/// `endpoint_id` is set to the allocated id.
async fn handle_connection(
    server: &Server,
    stream: TcpStream,
    endpoint_id: &mut Option<EndpointId>,
) -> anyhow::Result<()> {
    let stream = tung::accept_async(stream).await?;

    let (req_tx, mut req_rx) = mpsc::channel(1);
    let (resp_tx, resp_rx) = mpsc::channel(1);

    // Track last ping time for inactivity detection
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let last_ping = Arc::new(AtomicU64::new(now));

    let _ = tokio::spawn(send_receive_stream(
        stream,
        req_tx,
        resp_rx,
        last_ping.clone(),
    ));

    while let Some(msg) = req_rx.recv().await {
        let res = handle_message(msg, server, &resp_tx, endpoint_id, &last_ping).await;
        if res.is_err() {
            let _ = resp_tx.send(Message::Close(None)).await;
        }
        res?;
    }
    Ok(())
}

/// Handle each client message, send response to `resp_tx`.
async fn handle_message(
    msg: Result<Message, tokio_tungstenite::tungstenite::Error>,
    server: &Server,
    resp_tx: &mpsc::Sender<Message>,
    endpoint_id: &mut Option<EndpointId>,
    last_ping: &Arc<AtomicU64>,
) -> anyhow::Result<()> {
    let msg = msg?;
    if let Ok(txt) = &msg.to_text() {
        tracing::debug!(txt, "Received ws message");
    } else {
        tracing::debug!(?msg, "Received non-text ws message");
    }
    match msg {
        Message::Text(msg) => {
            let msg: SignalingMsg = serde_json::from_str(&msg)?;
            match msg {
                SignalingMsg::Bind(id, key) => {
                    server
                        .bind_endpoint(id.clone(), key.clone(), resp_tx.clone())
                        .await?;
                    *endpoint_id = Some(id);
                }
                SignalingMsg::Connect(sender_id, receiver_id, sender_key, initiator) => {
                    check_id(&sender_id, endpoint_id, &resp_tx).await?;

                    if endpoint_id.is_none() {
                        server
                            .bind_endpoint(sender_id.clone(), sender_key.clone(), resp_tx.clone())
                            .await?;
                        *endpoint_id = Some(sender_id.clone());
                    }
                    server
                        .connect_to_endpoint(
                            &sender_id,
                            receiver_id,
                            sender_key,
                            initiator,
                            &resp_tx,
                        )
                        .await?;
                }
                SignalingMsg::SDP(sender_id, receiver_id, sdp) => {
                    check_id(&sender_id, endpoint_id, &resp_tx).await?;

                    if endpoint_id.is_none() {
                        let resp = resp_unsupported(
                            "You should send a Connect or Bind message before sending SDP message",
                        );
                        resp_tx.send(resp).await?;
                        return Ok(());
                    }
                    if let Some(their_info) = server.get_endpoint_info(&receiver_id).await {
                        let msg = SignalingMsg::SDP(sender_id.clone(), receiver_id.clone(), sdp);
                        their_info.msg_tx.send(msg.into()).await?;
                    } else {
                        let msg = SignalingMsg::IdNotFound(
                            sender_id.clone(),
                            format!("No endpoint found for id: {}", receiver_id),
                        );
                        resp_tx.send(msg.into()).await?;
                    }
                }
                SignalingMsg::Candidate(sender_id, receiver_id, candidate) => {
                    check_id(&sender_id, endpoint_id, &resp_tx).await?;

                    if endpoint_id.is_none() {
                        let resp = resp_unsupported("You should send a Connect or Bind message before sending Candidate message");
                        resp_tx.send(resp).await?;
                        return Ok(());
                    }
                    if let Some(their_info) = server.get_endpoint_info(&receiver_id).await {
                        let msg = SignalingMsg::Candidate(
                            sender_id.clone(),
                            receiver_id.clone(),
                            candidate,
                        );
                        their_info.msg_tx.send(msg.into()).await?;
                    } else {
                        let msg = SignalingMsg::IdNotFound(
                            sender_id.clone(),
                            format!("No endpoint found for id: {}", receiver_id),
                        );
                        resp_tx.send(msg.into()).await?;
                    }
                }
                SignalingMsg::Ping => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs();
                    last_ping.store(now, Ordering::Relaxed);
                    tracing::debug!("Received ping, updated last_ping");
                }
                _ => {
                    let resp = resp_unsupported(
                        "You should only send Bind, Connect, SDP, Candidate, or Ping message to the signal server",
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
    Ok(())
}

// *** Subroutines for handle_connection

/// Sends and receives messages from `stream`.
#[instrument(skip_all)]
async fn send_receive_stream(
    mut stream: WebSocketStream<TcpStream>,
    req_tx: mpsc::Sender<Result<Message, tung::tungstenite::Error>>,
    mut resp_rx: mpsc::Receiver<Message>,
    last_ping: Arc<AtomicU64>,
) {
    // Check inactivity every 1 minute.
    let mut check_interval = time::interval(Duration::from_secs(60));
    check_interval.tick().await; // Skip immediate first tick

    const INACTIVE_TIMEOUT_SECS: u64 = 600; // 10 minutes

    loop {
        tokio::select! {
            _ = check_interval.tick() => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                let last = last_ping.load(Ordering::Relaxed);
                if now - last > INACTIVE_TIMEOUT_SECS {
                    let msg = SignalingMsg::Inactive(
                        "No ping received in 10 minutes".to_string()
                    );
                    let _ = stream.send(msg.into()).await;
                    tracing::info!("Closing connection due to inactivity");
                    return;
                }
            }
            msg = stream.next() => {
                if let Some(msg) = msg {
                    if req_tx.send(msg).await.is_err() {
                        // handle_connection stopped serving the
                        // connection due to error, just return here.
                        return;
                    }
                } else {
                    tracing::info!("Stream from client closed");
                    return;
                }
            }
            resp = resp_rx.recv() => {
                if let Some(msg) = resp {
                    if stream.send(msg).await.is_err() {
                        tracing::info!("Stream to client closed");
                        return;
                    }
                } else {
                    // handle_connection stopped serving the
                    // connection due to error, just return here.
                    return;
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

/// If `provided_id` doesn’t match `recorded_id`, return an error.
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
