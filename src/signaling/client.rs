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

use super::{EndpointId, ICECandidate, SignalingError, SignalingMessage, SignalingResult, SDP};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite as tung;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

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
#[derive(Debug)]
pub struct Listener {
    /// The id for this endpoint.
    my_id: EndpointId,
    /// Channel used to send messages out to the signal server.
    out_tx: mpsc::Sender<Message>,
    /// When the listener receives a `BindResponse(their_id,
    /// their_sdp)`, it sends `(their_id, their_sdp)` to this channel.
    sock_rx: mpsc::UnboundedReceiver<SignalingResult<Socket>>,
    /// Use for cleaning up.
    shutdown_rx: mpsc::Receiver<()>,
}

/// A socket that can be used to exchange ICE candidates.
#[derive(Debug)]
pub struct Socket {
    msg_rx: mpsc::UnboundedReceiver<ICECandidate>,
    msg_tx: mpsc::Sender<Message>,
    their_sdp: SDP,
    their_id: EndpointId,
    my_id: EndpointId,
}

#[derive(Debug, Clone)]
pub struct CandidateSender {
    my_id: EndpointId,
    msg_tx: mpsc::Sender<Message>,
    their_id: EndpointId,
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.shutdown_rx.close();
    }
}

impl Listener {
    /// Connect to the signaling server at `addr`.
    pub async fn new(addr: &str, id: EndpointId) -> SignalingResult<Listener> {
        let (stream, _addr) = tung::connect_async(addr).await?;

        let (in_tx, mut in_rx) = mpsc::channel(1);
        let (out_tx, out_rx) = mpsc::channel(1);
        let (sock_tx, sock_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);

        let _ = tokio::spawn(send_receive_stream(stream, in_tx, out_rx, shutdown_tx));

        let listener = Listener {
            my_id: id.clone(),
            sock_rx,
            out_tx: out_tx.clone(),
            shutdown_rx,
        };

        let _ = tokio::spawn(async move {
            let mut endpoint_map = HashMap::new();
            while let Some(msg) = in_rx.recv().await {
                // Ignore errors as long as in_rx hasn't closed.
                let _todo = listener_process_message(
                    msg,
                    out_tx.clone(),
                    sock_tx.clone(),
                    &mut endpoint_map,
                )
                .await;
            }
        });
        Ok(listener)
    }

    /// Share `sdp` on the signal server under `id`, and start
    /// listening for incoming connections.
    pub async fn bind(&mut self) -> SignalingResult<()> {
        let msg = SignalingMessage::Bind(self.my_id.clone());
        self.out_tx
            .send(msg.into())
            .await
            .map_err(|_err| SignalingError::Closed)?;
        Ok(())
    }

    /// Share our `sdp`, connect to the endpoint that has the id
    /// `their_id`.
    pub async fn connect(&mut self, their_id: EndpointId, sdp: SDP) -> SignalingResult<Socket> {
        let my_id = uuid::Uuid::new_v4().to_string();
        let msg = SignalingMessage::Connect(my_id, their_id, sdp);
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

impl Socket {
    /// Send the answer SDP to the other endpoint. This should happend
    /// immediately after accepting a connection request.
    pub async fn send_sdp(&self, sdp: SDP) -> SignalingResult<()> {
        let msg = SignalingMessage::Connect(self.my_id.clone(), self.their_id.clone(), sdp);
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
    msg: Result<Message, tung::tungstenite::Error>,
    out_tx: mpsc::Sender<Message>,
    sock_tx: mpsc::UnboundedSender<SignalingResult<Socket>>,
    endpoint_map: &mut HashMap<EndpointId, mpsc::UnboundedSender<ICECandidate>>,
) -> SignalingResult<()> {
    let msg = expect_text(msg?)?;
    let msg: SignalingMessage =
        serde_json::from_str(&msg).map_err(|err| SignalingError::ParseError(err.to_string()))?;
    match msg {
        SignalingMessage::Connect(their_id, my_id, their_sdp) => {
            let (msg_tx, msg_rx) = mpsc::unbounded_channel();
            endpoint_map.insert(their_id.clone(), msg_tx);
            let sock = Socket {
                my_id: my_id.clone(),
                their_id,
                their_sdp,
                msg_rx,
                msg_tx: out_tx.clone(),
            };
            sock_tx.send(Ok(sock)).unwrap(); // Receiver is never dropped.
            Ok(())
        }
        SignalingMessage::NoEndpointForId(id) => {
            sock_tx
                .send(Err(SignalingError::NoEndpointForId(id)))
                .unwrap(); // Receiver is never dropped.
            Ok(())
        }
        SignalingMessage::IdTaken(id) => {
            sock_tx.send(Err(SignalingError::IdTaken(id))).unwrap();
            Ok(())
        }
        SignalingMessage::TimesUp(time) => {
            sock_tx.send(Err(SignalingError::TimesUp(time))).unwrap();
            Ok(())
        }
        SignalingMessage::Candidate(their_id, _my_id, their_candidate) => {
            let tx = endpoint_map.get(&their_id).map(|tx| tx.clone());
            if let Some(tx) = tx {
                let _todo = tx.send(their_candidate);
            } else {
                log::warn!(
                    "Received a candidate for client id {} but we don't have that client in the client_map",
                    their_id
                );
            }
            Ok(())
        }
        msg => Err(SignalingError::UnexpectedMessage("".to_string(), msg)),
    }
}

/// Sends and receives messages from `stream`.
async fn send_receive_stream(
    mut stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
    req_tx: mpsc::Sender<Result<Message, tung::tungstenite::Error>>,
    mut resp_rx: mpsc::Receiver<Message>,
    shutdown_tx: mpsc::Sender<()>,
) {
    loop {
        tokio::select! {
            _ = shutdown_tx.closed() => {
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
