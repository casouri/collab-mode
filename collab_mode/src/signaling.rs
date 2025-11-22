//! This module contains a signaling server and a client that can be
//! used for establishing a webrtc connection. The signaling server
//! and clients treat SDP and ICE candidates as raw strings. Signaling
//! server and clients communicate using websockets.
//! [SignalingMessage] contains different messages server and client
//! exchange.
//!
//! A typical session looks like this:
//!
//! ```text
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

pub mod client_new;
mod key_store;
pub mod server;

/// We treat SDP as a black box.
pub type SDP = String;
/// We treat ICE candidate as a black box too.
pub type ICECandidate = String;
/// Id of an endpoint, should be an UUID string.
pub type EndpointId = String;
/// Certificate in DER format hashed by SHA-256 and printed in hex.
pub type CertDerHash = String;

pub type SignalingResult<T> = Result<T, SignalingError>;

#[derive(Debug, Clone, Error, Serialize, Deserialize)]
pub enum SignalingError {
    /// Connection to signaling server broke
    #[error("Connection broke")]
    ConnectionBroke,
    /// Other error with description
    #[error("Error: {0}")]
    OtherError(String),
}

impl From<tung::tungstenite::Error> for SignalingError {
    fn from(value: tung::tungstenite::Error) -> Self {
        match value {
            tung::tungstenite::Error::ConnectionClosed => SignalingError::ConnectionBroke,
            tung::tungstenite::Error::AlreadyClosed => SignalingError::ConnectionBroke,
            err => SignalingError::OtherError(format!("Websocket error: {}", err)),
        }
    }
}

impl From<rusqlite::Error> for SignalingError {
    fn from(value: rusqlite::Error) -> Self {
        SignalingError::OtherError(format!("Database error: {}", value))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum SignalingMsg {
    /// An endpoint sends this message to bind to the id on the
    /// signaling server. The second argument is the certificate hash.
    Bind(EndpointId, CertDerHash),
    /// Acknowledge bind request.
    Bound(EndpointId),
    /// Connect request. (sender_id, receiver_id, sender_key, initiator).
    /// The initiator field indicates whether the sender initiated the connection.
    Connect(EndpointId, EndpointId, CertDerHash, bool),
    /// Send SDP. (sender_id, receiver_id, sender_sdp).
    SDP(EndpointId, EndpointId, SDP),
    /// Send candidate. (sender_id, receiver_id, sender_candidate).
    Candidate(EndpointId, EndpointId, ICECandidate),
    /// ID already taken error. (endpoint_id, error_description).
    IdTaken(EndpointId, String),
    /// ID not found error. (endpoint_id, error_description).
    IdNotFound(EndpointId, String),
    /// Time's up error. (endpoint_id, error_description).
    TimeUp(EndpointId, String),
}

impl Into<tung::tungstenite::Message> for SignalingMsg {
    fn into(self) -> tung::tungstenite::Message {
        tung::tungstenite::Message::Text(serde_json::to_string(&self).unwrap())
    }
}
