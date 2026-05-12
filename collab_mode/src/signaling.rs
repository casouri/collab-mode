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

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{de::Error, Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use tokio_tungstenite as tung;

pub mod auth;
pub mod client_new;
pub mod server;

/// We treat SDP as a black box.
pub type SDP = String;
/// We treat ICE candidate as a black box too.
pub type ICECandidate = String;
/// Id of an endpoint — same shape as [`crate::types::ServerId`].
pub type EndpointId = String;
/// Certificate in DER format hashed by SHA-256 and printed in hex.
pub type CertDerHash = String;

/// Authentication payload sent in [`SignalingMsg::Bind`]. Carries the
/// endpoint's cert (base64‑encoded DER) and a unix epoch timestamp;
/// the signaling server verifies the timestamp is within 30s and
/// checks the accompanying [`Signature`] against the cert's public
/// key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// Unix epoch of creation time.
    pub timestamp: u64,
    /// X.509 cert DER.
    pub cert: Vec<u8>,
}

/// ECDSA P-256 SHA-256 signature over [`Identity::to_signed_bytes`],
/// produced with the private key matching the cert in [`Identity`].
/// Encoded to string in base64.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature(pub String);

impl Identity {
    /// Build an [`Identity`] from raw cert DER bytes and a timestamp.
    pub fn from_cert_der(timestamp: u64, cert_der: &[u8]) -> Self {
        Identity {
            timestamp,
            cert: cert_der.to_vec(),
        }
    }

    /// Serialize to bytes for signing. Use a custom format
    /// (<timestamp>:<cert>) instead of json to keep it simple and
    /// predictable.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.to_string().into_bytes()
    }
}

// Easier to just implement the methods instead of From/To.
impl Signature {
    /// Build a [`Signature`] from raw bytes.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Signature(B64.encode(bytes))
    }

    /// Decode the signature back to raw bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>, base64::DecodeError> {
        B64.decode(&self.0)
    }
}

impl ToString for Identity {
    fn to_string(&self) -> String {
        format!("{}:{}", self.timestamp, B64.encode(&self.cert))
    }
}

impl Serialize for Identity {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Identity {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        let (ts_str, cert_b64) = s
            .split_once(':')
            .ok_or_else(|| D::Error::custom("Identity missing ':' separator"))?;
        let timestamp: u64 = ts_str.parse().map_err(|err| {
            D::Error::custom(format!("Failed to parse identity timestamp: {err}"))
        })?;
        let cert = B64
            .decode(cert_b64)
            .map_err(|err| D::Error::custom(format!("Failed to parse cert: {err}")))?;
        Ok(Identity { timestamp, cert })
    }
}

impl Serialize for Signature {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Signature {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        Ok(Signature(String::deserialize(de)?))
    }
}

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

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum SignalingMsg {
    /// An endpoint sends this message to bind to the id on the
    /// signaling server. The id's hash portion must match
    /// `hash_der(identity.cert)`, and the [`Signature`] must verify
    /// against the cert's public key over [`Identity::to_signed_bytes`].
    Bind(EndpointId, Identity, Signature),
    /// Acknowledge bind request, echoing the bound id back.
    Bound(EndpointId),
    /// Connect request. (sender_id, receiver_id, initiator). The
    /// initiator field indicates whether the sender initiated the
    /// connection. The receiver verifies the cert hash in `sender_id`
    /// matches the provided cert during DTLS.
    Connect(EndpointId, EndpointId, bool),
    /// Send SDP. (sender_id, receiver_id, sender_sdp).
    SDP(EndpointId, EndpointId, SDP),
    /// Send candidate. (sender_id, receiver_id, sender_candidate).
    Candidate(EndpointId, EndpointId, ICECandidate),
    /// Bind error. (endpoint_id, error_description).
    BindErr(EndpointId, String),
    /// ID not found error. (endpoint_id, error_description).
    IdNotFound(EndpointId, String),
    /// Connection rejected by the receiver. (endpoint_id, error_description).
    Rejected(EndpointId, String),
    /// Client sends this to keep connection alive.
    Ping,
    /// Server sends this when client has been inactive too long.
    Inactive(String),
}

impl Into<tung::tungstenite::Message> for SignalingMsg {
    fn into(self) -> tung::tungstenite::Message {
        tung::tungstenite::Message::Text(serde_json::to_string(&self).unwrap())
    }
}

/// Error indicating that accepting connections has stopped.
/// Contains the reason for stopping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptStopped(pub String);

/// Message from signaling channel. Contains either a successful
/// signaling message or an error indicating connection stopped.
#[derive(Debug, Clone)]
pub struct SignalingMessage {
    pub signaling_addr: String,
    pub msg: Result<SignalingMsg, AcceptStopped>,
}
