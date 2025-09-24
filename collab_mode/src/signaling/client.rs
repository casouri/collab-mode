//! This module contains the client part of the signaling process.
//! Collab server and collab client uses this modeule to establish
//! connection between each other.
//!
//! A collab server would bind itself to an id on the signaling
//! server, and a collab client knowing that id can establish a
//! connection to the collab server.
//!
//! Specifically, a collab server creates a [Listener], and uses the
//! [Listener::bind] method to bind to an id on the signaling server.
//! Then it calls [Listener::accept] in a loop to accept connection
//! requests from collab clients, in the form of a [Socket]. When
//! [Listener::accept] returns a socket, the collab servers gets the
//! client's sdp by [Socket::sdp], and send back its sdp with
//! [Socket::send_sdp].
//!
//! A collab client creates a [Listener], and uses the
//! [Listener::connect] method to connect to the collab server
//! associated with the id and send its sdp to the server, and gets
//! back a [Socket]. I can get the server's sdp by [Socket::sdp].
//!
//! Then, both sides send and receive ICE candidates through
//! [Socket::send_candidate] and [Socket::recv_candidate], until the
//! webrtc connection is established.

use crate::config_man::ArcKeyCert;

use super::{
    CertDerHash, EndpointId, ICECandidate, SignalingError, SignalingMessage, SignalingResult, SDP,
};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite as tung;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::Instrument;

/// Return Ok if `msg` is a text message, Err otherwise.
fn expect_text(msg: Message) -> SignalingResult<String> {
    match msg {
        Message::Text(text) => Ok(text),
        msg => Err(SignalingError::WebsocketError(format!(
            "Expected text ws message but got {:?}",
            msg
        ))),
    }
}

/// A listener that can be used to either listen for connections from
/// other endpoints or connect to other endpoints.
pub struct Listener {
    /// The id for this endpoint.
    pub my_id: EndpointId,
    /// The private key and self-signed certificate for this endpoint.
    pub my_key_cert: ArcKeyCert,
    /// Channel used to send messages out to the signal server.
    out_tx: mpsc::Sender<Message>,
    /// When the listener receives a `BindResponse(their_id,
    /// their_sdp)`, it sends `(their_id, their_sdp)` to this channel.
    sock_rx: mpsc::UnboundedReceiver<SignalingResult<Socket>>,
    /// Handle to the send/receive stream task
    send_receive_task: JoinHandle<()>,
    /// Handle to the message processing task
    process_message_task: JoinHandle<()>,
}

/// A socket that can be used to exchange ICE candidates.
#[derive(Debug)]
pub struct Socket {
    msg_rx: mpsc::UnboundedReceiver<ICECandidate>,
    msg_tx: mpsc::Sender<Message>,
    their_sdp: SDP,
    their_id: EndpointId,
    my_id: EndpointId,
    my_cert: CertDerHash,
    their_cert: CertDerHash,
}

#[derive(Debug, Clone)]
pub struct CandidateSender {
    my_id: EndpointId,
    msg_tx: mpsc::Sender<Message>,
    their_id: EndpointId,
}

impl Listener {
    /// Create a listener that connects to the signaling server at
    /// `addr`. This endpoint has `id` and `key`.
    pub async fn new(
        addr: &str,
        id: EndpointId,
        my_key_cert: ArcKeyCert,
    ) -> SignalingResult<Listener> {
        let (stream, _addr) = tung::connect_async(addr).await?;

        let (in_tx, mut in_rx) = mpsc::channel(1);
        let (out_tx, out_rx) = mpsc::channel(1);
        let (sock_tx, sock_rx) = mpsc::unbounded_channel();

        let span = tracing::info_span!(
            "Listener send_receive_stream",
            endpoint = id,
            signaling_addr = addr
        );
        let send_receive_task =
            tokio::spawn(send_receive_stream(stream, in_tx, out_rx).instrument(span));

        let der_hash = my_key_cert.cert_der_hash();

        let span = tracing::info_span!(
            "listener_process_message",
            endpoint = id,
            signaling_addr = addr
        );
        let out_tx_clone = out_tx.clone();
        let process_message_task = tokio::spawn(
            async move {
                let mut endpoint_map = HashMap::new();
                while let Some(msg) = in_rx.recv().await {
                    // Ignore errors as long as in_rx hasn't closed.
                    // This thread must keep running and process
                    // incoming connect requests from other collab
                    // hosts.
                    let res = listener_process_message(
                        der_hash.clone(),
                        msg,
                        out_tx_clone.clone(),
                        sock_tx.clone(),
                        &mut endpoint_map,
                    )
                    .await;
                    if let Err(err) = res {
                        tracing::warn!(?err, "Error processing incoming message");
                    }
                }
                tracing::info!("ws stream closed, ending bg thread");
            }
            .instrument(span),
        );

        let listener = Listener {
            my_id: id.clone(),
            my_key_cert,
            sock_rx,
            out_tx,
            send_receive_task,
            process_message_task,
        };

        Ok(listener)
    }

    /// Share `sdp` on the signal server under `id`, and start
    /// listening for incoming connections.
    pub async fn bind(&mut self) -> SignalingResult<()> {
        let cert_hash = self.my_key_cert.cert_der_hash();
        let msg = SignalingMessage::Bind(self.my_id.clone(), cert_hash);
        self.out_tx
            .send(msg.into())
            .await
            .map_err(|_err| SignalingError::Closed)?;
        Ok(())
    }

    /// Share our `sdp`, connect to the endpoint that has the id
    /// `their_id`.
    pub async fn connect(&mut self, their_id: EndpointId, sdp: SDP) -> SignalingResult<Socket> {
        let cert_hash = self.my_key_cert.cert_der_hash();
        let msg = SignalingMessage::Connect(self.my_id.clone(), their_id, sdp, cert_hash);
        self.out_tx
            .send(msg.into())
            .await
            .map_err(|_err| SignalingError::Closed)?;
        self.accept().await
    }

    /// Receive the next incoming connection invitation (as the result
    /// of [Listener::bind]).
    pub async fn accept(&mut self) -> SignalingResult<Socket> {
        if let Some(sock) = self.sock_rx.recv().await {
            sock
        } else {
            Err(SignalingError::Closed)
        }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // Abort background tasks to close WebSocket connection
        tracing::info!("Closing signaling connection for endpoint {}", self.my_id);
        self.send_receive_task.abort();
        self.process_message_task.abort();
    }
}

impl Socket {
    /// Send the answer SDP to the other endpoint. This should happend
    /// immediately after accepting a connection request.
    pub async fn send_sdp(&self, sdp: SDP) -> SignalingResult<()> {
        let msg = SignalingMessage::Connect(
            self.my_id.clone(),
            self.their_id.clone(),
            sdp,
            self.my_cert.clone(),
        );
        self.msg_tx
            .send(msg.into())
            .await
            .map_err(|_err| SignalingError::Closed)?;
        Ok(())
    }

    /// Send `candidate` to the other endpoint.
    pub async fn send_candidate(&self, candidate: ICECandidate) -> SignalingResult<()> {
        let msg = SignalingMessage::Candidate(self.my_id.clone(), self.their_id.clone(), candidate);
        self.msg_tx
            .send(msg.into())
            .await
            .map_err(|_err| SignalingError::Closed)?;
        Ok(())
    }

    /// Return a candidate sender that can do what `send_candidate`
    /// does, but without holding reference to the socket.
    pub fn candidate_sender(&self) -> CandidateSender {
        CandidateSender {
            my_id: self.my_id.clone(),
            msg_tx: self.msg_tx.clone(),
            their_id: self.their_id.clone(),
        }
    }

    /// Receive candidate from the other endpoint.
    pub async fn recv_candidate(&mut self) -> Option<ICECandidate> {
        self.msg_rx.recv().await
    }

    /// Return the SDP of the other endpoint.
    pub fn sdp(&self) -> SDP {
        self.their_sdp.clone()
    }

    /// Return the id of the other end.
    pub fn id(&self) -> String {
        self.their_id.clone()
    }

    /// Return their certificate hash.
    pub fn cert_hash(&self) -> CertDerHash {
        self.their_cert.clone()
    }
}

impl CandidateSender {
    /// Send `candidate` to the other endpoint.
    pub async fn send_candidate(&self, candidate: ICECandidate) -> SignalingResult<()> {
        let msg = SignalingMessage::Candidate(self.my_id.clone(), self.their_id.clone(), candidate);
        self.msg_tx
            .send(msg.into())
            .await
            .map_err(|_err| SignalingError::Closed)?;
        Ok(())
    }
}

// *** Subroutines

async fn listener_process_message(
    my_cert: CertDerHash,
    msg: Result<Message, tung::tungstenite::Error>,
    out_tx: mpsc::Sender<Message>,
    sock_tx: mpsc::UnboundedSender<SignalingResult<Socket>>,
    endpoint_map: &mut HashMap<EndpointId, mpsc::UnboundedSender<ICECandidate>>,
) -> SignalingResult<()> {
    let msg = expect_text(msg?)?;
    let msg: SignalingMessage =
        serde_json::from_str(&msg).map_err(|err| SignalingError::ParseError(err.to_string()))?;
    match msg {
        SignalingMessage::Connect(their_id, my_id, their_sdp, their_cert) => {
            let (msg_tx, msg_rx) = mpsc::unbounded_channel();
            endpoint_map.insert(their_id.clone(), msg_tx);
            let sock = Socket {
                my_id: my_id.clone(),
                my_cert,
                their_id,
                their_sdp,
                msg_rx,
                msg_tx: out_tx.clone(),
                their_cert,
            };
            // Receiver is never dropped while program is running.
            sock_tx.send(Ok(sock)).unwrap();
            Ok(())
        }
        SignalingMessage::NoEndpointForId(id) => {
            sock_tx
                .send(Err(SignalingError::NoEndpointForId(id)))
                // Receiver is never dropped while Listener is live.
                .unwrap();
            Ok(())
        }
        SignalingMessage::IdTaken(id) => {
            // Receiver is never dropped while Listener is live.
            sock_tx.send(Err(SignalingError::IdTaken(id))).unwrap();
            Ok(())
        }
        SignalingMessage::TimesUp(time) => {
            // Receiver is never dropped while Listener is live.
            sock_tx.send(Err(SignalingError::TimesUp(time))).unwrap();
            Ok(())
        }
        SignalingMessage::Candidate(their_id, _my_id, their_candidate) => {
            let tx = endpoint_map.get(&their_id).map(|tx| tx.clone());
            if let Some(tx) = tx {
                let res = tx.send(their_candidate);

                if let Err(err) = res {
                    tracing::info!(
                        their_id,
                        ?err,
                        "Can't send received remote candidate for this endpoint to their channel"
                    );
                    endpoint_map.remove(&their_id);
                }
            } else {
                tracing::warn!(
                    their_id,
                    "Received a candidate but we don't have that client in the client_map"
                );
            }
            Ok(())
        }
        msg => Err(SignalingError::UnexpectedMessage(
            "SignalingMessage".to_string(),
            msg,
        )),
    }
}

/// Sends and receives messages from `stream`.
async fn send_receive_stream(
    mut stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
    req_tx: mpsc::Sender<Result<Message, tung::tungstenite::Error>>,
    mut resp_rx: mpsc::Receiver<Message>,
) {
    loop {
        tokio::select! {
            msg = stream.next() => {
                if let Some(msg) = msg {
                    if req_tx.send(msg).await.is_err() {
                        tracing::info!("Req channel closed, ending bg thread");
                        break;
                    }
                } else {
                    tracing::info!("Req stream closed, ending bg thread");
                    break;
                }
            }
            resp = resp_rx.recv() => {
                if let Some(msg) = resp {
                    if stream.send(msg).await.is_err() {
                        tracing::info!("Resp stream closed, ending bg thread");
                        break;
                    }
                } else {
                    tracing::info!("Resp channel closed, ending bg thread");
                    break;
                }
            }
        }
    }
    let res = stream.close(Option::None).await;
    if let Err(err) = res {
        tracing::warn!("Error closing ws stream: {}", err);
    }
}
