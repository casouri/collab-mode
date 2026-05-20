//! This module provides a signaling server. Use
//! [run_signaling_server] to run it.
//!
//! The server is pretty straightforward, it listens for websocket
//! connections, spawns a task for each new connection and handles
//! them with [handle_connection]. The server keeps a map mapping
//! endpoint ids to the channel that sends messages to that endpoint.
//! Serving requests mostly consists of adding entries to the map, and
//! relaying messages to the endpoint with the target id.
//!
//! The id of each client is in the form of <name>::<cert hash>. And
//! when client tries to bind to an id on the signaling server, they
//! have to prove that they own the key of the cert. They prove if by
//! attaching a signed signature with the bind request. Which is
//! basically `<timestamp>:<cert>` encrypted using the key.
//!
//! This is just a spam-filtering check so someone doens’t claim to be
//! someone else and spams messages and get the “someone else”
//! blocked. (TODO: implement rate limiting.) When the clients
//! establish connections betwee each other, they also check the
//! validity of the provided cert.

use crate::signaling::auth::verify_identity;
use crate::signaling::{
    decode_trusted_header, parse_identity_header, EndpointId, SignalingMsg, Signature,
    BIND_HEADER_ID, BIND_HEADER_IDENTITY, BIND_HEADER_SIGNATURE, BIND_HEADER_TRUSTED,
};
use crate::types::id_hash;
use futures_util::{SinkExt, StreamExt};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::sync::RwLock;
use tokio::time;
use tokio_tungstenite as tung;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{
    response::Builder as ResponseBuilder, HeaderMap, StatusCode,
};
use tokio_tungstenite::tungstenite::{
    protocol::{frame::coding::CloseCode, CloseFrame},
    Message,
};
use tokio_tungstenite::WebSocketStream;
use tracing::{instrument, Instrument};

/// Maximum lifetime of a single WebSocket connection. After this, the
/// server closes the stream regardless of activity; the client must
/// re-bind on a fresh connection. This forces periodic re-authentication
/// via the bind headers.
const MAX_CONNECTION_LIFETIME_SECS: u64 = 12 * 60 * 60;
/// Timeout for cleaning up stale connection.
const INACTIVE_TIMEOUT_SECS: u64 = 600; // 10 minutes.

/// Run the signaling server on `addr` (form: `"ip:port"`).
pub async fn run_signaling_server(addr: &str) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    let server = Server::new();

    while let Ok((stream, client_addr)) = listener.accept().await {
        tracing::info!("Accepted a connection from {}", client_addr);
        let server = server.clone();
        let span = tracing::info_span!("Serve connection", %client_addr);
        let _ = tokio::spawn(
            async move {
                let mut endpoint_id: Option<EndpointId> = None;
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
    /// Endpoint ids allowed to send Connect/SDP/Candidate addressed
    /// to this endpoint.
    trusted: HashSet<EndpointId>,
}

/// Signaling server.
#[derive(Clone)]
struct Server {
    endpoint_map: Arc<RwLock<HashMap<EndpointId, EndpointInfo>>>,
}

impl Server {
    fn new() -> Server {
        Server {
            endpoint_map: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Override the trust list for `id`. No-op if `id` is not
    /// bound.
    async fn update_trust(&self, id: &EndpointId, trusted: HashSet<EndpointId>) {
        let mut map = self.endpoint_map.write().await;
        if let Some(info) = map.get_mut(id) {
            info.trusted = trusted;
        }
    }

    /// Remove binding for endpoint with `id`.
    async fn remove_endpoint(&self, id: &EndpointId) {
        let mut endpoint_map = self.endpoint_map.write().await;
        endpoint_map.remove(id);
    }

    /// Get the server info for the server with `id`.
    async fn get_endpoint_info(&self, id: &EndpointId) -> Option<EndpointInfo> {
        self.endpoint_map.read().await.get(id).cloned()
    }
}

/// Pull the named header out of `headers` as an owned String. Errors if the
/// header is missing or its value isn’t valid UTF-8.
fn header_str(headers: &HeaderMap, name: &str) -> Result<String, String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing {name}"))
}

/// Validate the `X-Collab-*` bind headers from a WebSocket upgrade
/// request. On success returns `(endpoint_id, trusted_set)` so the
/// outer task can register the endpoint after the upgrade completes.
fn validate_bind(headers: &HeaderMap) -> Result<(EndpointId, HashSet<EndpointId>), String> {
    let id = header_str(headers, BIND_HEADER_ID)?;

    let identity_str = header_str(headers, BIND_HEADER_IDENTITY)?;
    let identity =
        parse_identity_header(&identity_str).map_err(|e| format!("{BIND_HEADER_IDENTITY}: {e}"))?;

    let sig_str = header_str(headers, BIND_HEADER_SIGNATURE)?;
    let signature = Signature(sig_str);

    let trusted_str = header_str(headers, BIND_HEADER_TRUSTED)?;
    let trusted: Vec<EndpointId> =
        decode_trusted_header(&trusted_str).map_err(|e| format!("{BIND_HEADER_TRUSTED}: {e}"))?;

    let cert_hash =
        verify_identity(&identity, &signature).map_err(|e| format!("identity verify: {e}"))?;
    if id_hash(&id) != cert_hash {
        return Err(format!(
            "bind id hash {} != cert hash {}",
            id_hash(&id),
            cert_hash
        ));
    }
    Ok((id, trusted.into_iter().collect()))
}

/// Handle a connection from client. Read headers and validate auth,
/// if pass, start conversation with it.
async fn handle_connection(
    server: &Server,
    stream: TcpStream,
    endpoint_id: &mut Option<EndpointId>,
) -> anyhow::Result<()> {
    // Set this in the callback, read it once stream is created.
    let bind_info: Arc<StdMutex<Option<(EndpointId, HashSet<EndpointId>)>>> =
        Arc::new(StdMutex::new(None));
    let bind_info_clone = bind_info.clone();
    let callback = move |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
        match validate_bind(req.headers()) {
            Ok((id, trusted)) => {
                *bind_info_clone.lock().unwrap() = Some((id, trusted));
                Ok(resp)
            }
            Err(reason) => {
                tracing::info!(%reason, "Rejected bind during upgrade");
                let err: ErrorResponse = ResponseBuilder::new()
                    .status(StatusCode::UNAUTHORIZED)
                    .body(Some(reason))
                    .unwrap();
                Err(err)
            }
        }
    };
    let stream = tokio_tungstenite::accept_hdr_async(stream, callback).await?;
    let (id, trust_set) = bind_info
        .lock()
        .unwrap()
        .take()
        .expect("validate_bind populated this on Ok");

    let (req_tx, mut req_rx) = mpsc::channel(1);
    let (resp_tx, resp_rx) = mpsc::channel(1);

    // Register the bound endpoint. If there’s an existing entry, just
    // override it. After the auth we’re sure it’s the same client
    // making another binding request.
    {
        let mut map = server.endpoint_map.write().await;
        map.insert(
            id.clone(),
            EndpointInfo {
                msg_tx: resp_tx.clone(),
                trusted: trust_set,
            },
        );
    }
    *endpoint_id = Some(id);

    // Track last ping time for inactivity detection.
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
                SignalingMsg::Trust { sender, trusted } => {
                    check_id(&sender, endpoint_id, &resp_tx).await?;
                    server
                        .update_trust(&sender, trusted.iter().cloned().collect())
                        .await;
                    let resp = SignalingMsg::Trusted {
                        id: sender,
                        trusted,
                    };
                    resp_tx.send(resp.into()).await?;
                }
                SignalingMsg::Connect {
                    ref sender,
                    ref receiver,
                    ..
                }
                | SignalingMsg::SDP {
                    ref sender,
                    ref receiver,
                    ..
                }
                | SignalingMsg::Candidate {
                    ref sender,
                    ref receiver,
                    ..
                } => {
                    let sender = sender.clone();
                    let receiver = receiver.clone();
                    check_id(&sender, endpoint_id, &resp_tx).await?;
                    let Some(receiver_info) = server.get_endpoint_info(&receiver).await else {
                        let resp = SignalingMsg::IdNotFound {
                            id: sender,
                            reason: format!("No endpoint found for id: {}", receiver),
                        };
                        resp_tx.send(resp.into()).await?;
                        return Ok(());
                    };
                    if !receiver_info.trusted.contains(&sender) {
                        let resp = SignalingMsg::Rejected {
                            id: sender,
                            reason: format!("Sender not in {}’s trust list", receiver),
                        };
                        resp_tx.send(resp.into()).await?;
                        return Ok(());
                    }
                    receiver_info.msg_tx.send(msg.into()).await?;
                }
                SignalingMsg::Ping => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs();
                    last_ping.store(now, Ordering::Relaxed);
                    tracing::debug!("Received ping, updated last_ping");
                }
                SignalingMsg::Pong => {}
                _ => {
                    let resp =
                        resp_unsupported("Send Connect, SDP, Candidate, Trust, or Ping only");
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
    // Check inactivity every 1 minute. Closes the connection if no
    // ping for 10 minutes, or after the 12-hour lifetime cap.
    let mut check_interval = time::interval(Duration::from_secs(60));
    check_interval.tick().await; // Skip immediate first tick
    let bound_at = Instant::now();

    loop {
        tokio::select! {
            _ = check_interval.tick() => {
                let reason = if bound_at.elapsed()
                    > Duration::from_secs(MAX_CONNECTION_LIFETIME_SECS)
                {
                    Some("12-hour limit reached, need re-auth")
                } else {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs();
                    let last = last_ping.load(Ordering::Relaxed);
                    if now - last > INACTIVE_TIMEOUT_SECS {
                        Some("No ping received in 10 minutes")
                    } else {
                        None
                    }
                };
                if let Some(reason) = reason {
                    tracing::info!(%reason, "Closing connection");
                    let msg = SignalingMsg::Terminate { reason: reason.to_string() };
                    // Use a timeout because if the peer is unreachable
                    // (e.g. laptop asleep), the close handshake reads
                    // forever and would prevent remove_endpoint from
                    // running.
                    let _ = time::timeout(Duration::from_secs(5), async {
                        let _ = stream.send(msg.into()).await;
                        let _ = stream.close(None).await;
                    }).await;
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
