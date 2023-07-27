//! This module contains the client part of the signaling process.
//! Collab server and collab client uses this modeule to establish
//! connection between each other.

//! A collab server would bind itself to an id on the signaling
//! server, and a collab client knowing that id can establish a
//! connection to the collab server.

//! Specifically, a collab server creates a [Listener], and uses the
//! [Listener::bind] method to send its id and SDP to the signaling
//! server. Then it calls [Listener::accept] in a loop to accept
//! connection requests from collab clients, in the form of a
//! [Socket].

//! A collab client creates a [Listener], and uses the
//! [Listener::connect] method to connect to the collab server
//! associated with the id, and gets the [Socket] connected to the
//! collab server.

//! On both sides, the collab server and client can get each other's
//! SDP by [Socket::sdp], and send and receive ICE candidates through
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
pub struct Listener {
    /// Channel used to send messages out to the signal server.
    out_tx: mpsc::Sender<Message>,
    /// When the listener receives a `BindResponse(their_id,
    /// their_sdp)`, it sends `(their_id, their_sdp)` to this channel.
    sock_rx: mpsc::UnboundedReceiver<Socket>,
}

/// A socket that can be used to exchange ICE candidates.
pub struct Socket {
    msg_rx: mpsc::UnboundedReceiver<ICECandidate>,
    msg_tx: mpsc::Sender<Message>,
    their_sdp: SDP,
    their_id: EndpointId,
}

impl Listener {
    /// Connect to the signaling server at `addr`.
    pub async fn new(addr: &str) -> SignalingResult<Listener> {
        let (stream, _addr) = tung::connect_async(addr).await?;

        let (in_tx, mut in_rx) = mpsc::channel(1);
        let (out_tx, out_rx) = mpsc::channel(1);
        let (sock_tx, sock_rx) = mpsc::unbounded_channel();

        let _ = tokio::spawn(send_receive_stream(stream, in_tx, out_rx));

        let listener = Listener {
            sock_rx,
            out_tx: out_tx.clone(),
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
    pub async fn bind(&mut self, id: EndpointId, sdp: SDP) -> SignalingResult<()> {
        let msg = SignalingMessage::BindRequest(id, sdp);
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
        let msg = SignalingMessage::ConnectRequest(my_id, their_id, sdp);
        self.out_tx
            .send(msg.into())
            .await
            .map_err(|_err| SignalingError::Closed)?;
        let sock = self.accept().await?;
        Ok(sock)
    }

    /// Receive the next incoming connection invitation (as the result
    /// of [Listener::bind]).
    pub async fn accept(&mut self) -> SignalingResult<Socket> {
        if let Some(sock) = self.sock_rx.recv().await {
            Ok(sock)
        } else {
            Err(SignalingError::Closed)
        }
    }
}

impl Socket {
    /// Send `candidate` to the other endpoint.
    pub async fn send_candidate(&self, candidate: ICECandidate) -> SignalingResult<()> {
        let msg = SignalingMessage::SendCandidateTo(self.their_id.clone(), candidate);
        self.msg_tx
            .send(msg.into())
            .await
            .map_err(|_err| SignalingError::Closed)?;
        Ok(())
    }

    /// Receive candidate from the other endpoint.
    pub async fn recv_candidate(&mut self) -> Option<ICECandidate> {
        self.msg_rx.recv().await
    }

    /// Return the SDP of the other endpoint.
    pub fn sdp(&self) -> SDP {
        self.their_sdp.clone()
    }
}

// *** Subroutines

async fn listener_process_message(
    msg: Result<Message, tung::tungstenite::Error>,
    out_tx: mpsc::Sender<Message>,
    sock_tx: mpsc::UnboundedSender<Socket>,
    endpoint_map: &mut HashMap<EndpointId, mpsc::UnboundedSender<ICECandidate>>,
) -> SignalingResult<()> {
    let msg = expect_text(msg?)?;
    let msg: SignalingMessage =
        serde_json::from_str(&msg).map_err(|err| SignalingError::ParseError(err.to_string()))?;
    match msg {
        SignalingMessage::ConnectionInvitation(their_id, their_sdp) => {
            let (msg_tx, msg_rx) = mpsc::unbounded_channel();
            endpoint_map.insert(their_id.clone(), msg_tx);
            let sock = Socket {
                their_id,
                their_sdp,
                msg_rx,
                msg_tx: out_tx.clone(),
            };
            sock_tx.send(sock).map_err(|_err| SignalingError::Closed)?;
            Ok(())
        }
        SignalingMessage::CandidateFrom(their_id, their_candidate) => {
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
        msg => Err(SignalingError::UnexpectedMessage(
            "BindResponse".to_string(),
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
