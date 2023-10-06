//! This module contains a signaling server and a client that can be
//! used for establishing a webrtc connection. The signaling server
//! and clients treat SDP and ICE candidates as raw strings. Signaling
//! server and clients communicate using websockets.
//! [SignalingMessage] contains different messages server and client
//! exchange.
//!
//! A typical session looks like this:
//!
//! ```ignore
//! endpoint      signal server      endpoint
//! | -------Bind-----> |                   |
//! |                   | <----Connect----- |
//! | <----Connect----- |                   |
//! | -----Connect----> |                   |
//! |                   | -----Connect----> |
//! |                   |                   |
//! | -----Candidate--> | <---Candidate---- |
//! | <----Candidate--- | ----Candidate---> |
//! ```

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_tungstenite as tung;

pub mod client;
pub mod server;

/// We treat SDP as a black box.
pub type SDP = String;
/// We treat ICE candidate as a black box too.
pub type ICECandidate = String;
/// Id of an endpoint, should be an UUID string.
pub type EndpointId = String;

pub type SignalingResult<T> = Result<T, SignalingError>;

#[derive(Debug, Clone, Error, Serialize, Deserialize)]
pub enum SignalingError {
    /// Allocated listening time is up.
    #[error("Allocated time of {0} seconds is up")]
    TimesUp(u16),
    /// The connection is closed normally.
    #[error("The other endpoint closed the connection")]
    Closed,
    /// Some websocket error.
    #[error("Websocket error: {0}")]
    WebsocketError(String),
    /// Received an unexpected message.
    #[error("Expected: {0}, received: {1:?}")]
    UnexpectedMessage(String, SignalingMessage),
    /// Couldn't parse the websocket message.
    #[error("Couldn't parse the message")]
    ParseError(String),
    /// Cannot find the endpoint with that id.
    #[error("No endpoint is listening on this id: {0}")]
    NoEndpointForId(EndpointId),
    #[error("This id is already binded: {0}")]
    IdTaken(EndpointId),
}

impl From<tung::tungstenite::Error> for SignalingError {
    fn from(value: tung::tungstenite::Error) -> Self {
        match value {
            tung::tungstenite::Error::ConnectionClosed => SignalingError::Closed,
            tung::tungstenite::Error::AlreadyClosed => SignalingError::Closed,
            err => SignalingError::WebsocketError(err.to_string()),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum SignalingMessage {
    /// An endpoint sends this message to bind to the id on the
    /// signaling server.
    Bind(EndpointId),
    /// Connect request. (sender_id, receiver_id, sender_sdp).
    Connect(EndpointId, EndpointId, SDP),
    /// Send candidate. (sender_id, receiver_id, sender_candidate).
    Candidate(EndpointId, EndpointId, ICECandidate),
    /// Cannot find the corresponding endpoint for the provided id.
    NoEndpointForId(String),
    /// Id is already binded.
    IdTaken(EndpointId),
    /// Allocated time (3min) is up.
    TimesUp(u16),
}

impl Into<tung::tungstenite::Message> for SignalingMessage {
    fn into(self) -> tung::tungstenite::Message {
        tung::tungstenite::Message::Text(serde_json::to_string(&self).unwrap())
    }
}

#[cfg(test)]
mod tests {
    use crate::signaling::{client, server};

    #[test]
    #[ignore]
    pub fn signaling_test() -> anyhow::Result<()> {
        let sdp_server = "server sdp".to_string();
        let sdp_client = "client sdp".to_string();
        let candidate_server = vec!["cs1".to_string(), "cs2".to_string()];
        let candidate_client = vec!["cc1".to_string(), "cc2".to_string()];
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _ = runtime.spawn(server::run_signaling_server("127.0.0.1:9000"));

        // Server
        let handle = runtime.spawn(async move {
            let mut listener = client::Listener::new("ws://127.0.0.1:9000", "1".to_string())
                .await
                .unwrap();
            listener.bind().await.unwrap();
            println!("Server binded to id = 1");
            if let Ok(mut sock) = listener.accept().await {
                assert!(sock.sdp() == "client sdp");
                println!("Server received client sdp: {}", sock.sdp());
                sock.send_sdp(sdp_server).await.unwrap();
                println!("Server sent sdp answer");

                for candidate in candidate_server {
                    sock.send_candidate(candidate).await.unwrap();
                    println!("Sent server candidate");
                }
                let c1 = sock.recv_candidate().await.unwrap();
                println!("Received client candidate: {}", c1);
                assert!(c1 == "cc1");
                let c2 = sock.recv_candidate().await.unwrap();
                println!("Received client candidate: {}", c2);
                assert!(c2 == "cc2");
            } else {
                panic!("Server failed to accept");
            }
        });

        runtime.block_on(async move {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let mut listener = client::Listener::new("ws://127.0.0.1:9000", "2".to_string())
                .await
                .unwrap();
            let mut sock = listener.connect("1".to_string(), sdp_client).await.unwrap();
            assert!(sock.sdp() == "server sdp");
            for candidate in candidate_client {
                sock.send_candidate(candidate).await.unwrap();
                println!("Sent client candidate");
            }
            let c1 = sock.recv_candidate().await.unwrap();
            println!("Received server candidate: {}", c1);
            assert!(c1 == "cs1");
            let c2 = sock.recv_candidate().await.unwrap();
            println!("Received server candidate: {}", c2);
            assert!(c2 == "cs2")
        });

        runtime.block_on(async { handle.await }).unwrap();

        Ok(())
    }
}
