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
//! | ---WS upgrade---> |                   |
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

/// HTTP header names carrying the params during the WebSocket
/// upgrade.
pub const BIND_HEADER_ID: &str = "X-Collab-Endpoint-Id";
pub const BIND_HEADER_IDENTITY: &str = "X-Collab-Identity";
pub const BIND_HEADER_SIGNATURE: &str = "X-Collab-Signature";
pub const BIND_HEADER_TRUSTED: &str = "X-Collab-Trusted";

/// Encode a trust list as a comma-separated string suitable for use
/// as an HTTP header value. Endpoint ids have the shape
/// `<name>::<cert_hash>` so they don't contain commas, but as a
/// defensive measure any comma is rejected.
pub fn encode_trusted_header(trusted: &[EndpointId]) -> anyhow::Result<String> {
    for id in trusted {
        if id.contains(',') {
            return Err(anyhow::anyhow!(
                "endpoint id {id:?} contains a comma; cannot encode in header"
            ));
        }
    }
    Ok(trusted.join(","))
}

/// Inverse of [`encode_trusted_header`]. An empty header value decodes
/// to an empty list (rather than a list with a single empty string).
pub fn decode_trusted_header(value: &str) -> anyhow::Result<Vec<EndpointId>> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    Ok(value.split(',').map(|s| s.to_string()).collect())
}

/// Parse the `<timestamp>:<base64-cert>` form of an [`Identity`] from a
/// header string. Matches `Identity`'s `Serialize`/`Deserialize` shape.
pub fn parse_identity_header(s: &str) -> anyhow::Result<Identity> {
    let (ts_str, cert_b64) = s
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("Identity missing ':' separator"))?;
    let timestamp: u64 = ts_str.parse()?;
    let cert = B64
        .decode(cert_b64)
        .map_err(|e| anyhow::anyhow!("cert base64: {e}"))?;
    Ok(Identity { timestamp, cert })
}

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
#[serde(tag = "kind")]
pub enum SignalingMsg {
    /// Internal notification message that the connection is
    /// established. Created by the signaling client.
    Bound {
        id: EndpointId,
        trusted: Vec<EndpointId>,
    },
    /// Connect request. The `initiator` field indicates whether the
    /// sender initiated the connection. The receiver verifies the cert
    /// hash in `sender` matches the provided cert during DTLS.
    Connect {
        sender: EndpointId,
        receiver: EndpointId,
        initiator: bool,
    },
    /// Send SDP from sender to receiver.
    SDP {
        sender: EndpointId,
        receiver: EndpointId,
        sdp: SDP,
    },
    /// Send ICE candidate from sender to receiver.
    Candidate {
        sender: EndpointId,
        receiver: EndpointId,
        candidate: ICECandidate,
    },
    /// Override the sender's trust list on the signaling server. The
    /// server replies with [`SignalingMsg::Trusted`] echoing the new
    /// list. Trust is overriding, the new list replaces the old list.
    Trust {
        sender: EndpointId,
        trusted: Vec<EndpointId>,
    },
    /// Confirms the sender's trust list now stored on the server.
    Trusted {
        id: EndpointId,
        trusted: Vec<EndpointId>,
    },
    /// ID not found error.
    IdNotFound { id: EndpointId, reason: String },
    /// Connection rejected by the receiver.
    Rejected { id: EndpointId, reason: String },
    /// Client sends this to keep connection alive.
    Ping,
    /// Keep-alive reply from the Cloudflare worker’s auto-response. The
    /// in-process Rust server doesn’t send this; the client treats it as a
    /// no-op.
    Pong,
    /// Server sends this when client has been inactive too long.
    Terminate { reason: String },
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
