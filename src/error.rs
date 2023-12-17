//! This module defines [CollabError], the error types used throughout
//! the program.

use crate::types::*;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Error, Serialize, Deserialize)]
pub enum CollabError {
    // Program error.
    #[error("Fatal server error ({0})")]
    ServerFatal(String),
    #[error("Rpc error ({0})")]
    RpcError(String),
    #[error("Doc fatal error ({0})")]
    DocFatal(String),
    #[error("Fatal OT engine error ({0})")]
    EngineError(String),

    // User error.
    #[error("Doc({0}) already exists")]
    DocAlreadyExists(DocId),
    #[error("Cannot find Doc({0})")]
    DocNotFound(DocId),
    #[error("Not connected to Server({0})")]
    ServerNotConnected(ServerId),
    #[error("{0} is not a regular file")]
    NotRegularFile(String),
    #[error("{0} is not a directory")]
    NotDirectory(String),
    #[error("{0} is not supported")]
    UnsupportedOperation(String),

    // Notification.
    #[error("Error accepting remote connections ({0})")]
    AcceptConnectionErr(String),
    #[error("Allocated time ({0}s) on the signaling server is up")]
    SignalingTimesUp(u16),

    #[error("Error returned from remote server ({0})")]
    RemoteErr(Box<CollabError>),

    #[error("IO error ({0})")]
    IOErr(String),
}

pub type CollabResult<T> = Result<T, CollabError>;

impl From<std::io::Error> for CollabError {
    fn from(value: std::io::Error) -> Self {
        CollabError::IOErr(value.to_string())
    }
}

impl From<crate::engine::EngineError> for CollabError {
    fn from(value: crate::engine::EngineError) -> Self {
        CollabError::EngineError(format!("{:#?}", value))
    }
}

impl From<serde_json::Error> for CollabError {
    fn from(value: serde_json::Error) -> Self {
        CollabError::RpcError(format!("{:#}", value))
    }
}

impl From<WebrpcError> for CollabError {
    fn from(value: WebrpcError) -> Self {
        match value {
            WebrpcError::SignalingTimesUp(time) => CollabError::SignalingTimesUp(time),
            value => CollabError::RpcError(format!("{:#}", value)),
        }
    }
}

pub type WebrpcResult<T> = Result<T, WebrpcError>;

#[derive(Debug, Clone, thiserror::Error, Serialize, Deserialize)]
pub enum WebrpcError {
    #[error("Signaling error {0}")]
    SignalingError(String),
    #[error("Allocated time ({0}s) on the signaling server is up")]
    SignalingTimesUp(u16),
    #[error("ICE error {0}")]
    ICEError(String),
    #[error("SCTP error {0}")]
    SCTPError(String),
    #[error("Can't parse message: {0}")]
    ParseError(String),
    #[error("Data channel error {0}")]
    DataChannelError(String),
    #[error("The other end stopped listening for responses for this request")]
    RequestClosed(),
}

impl From<bincode::Error> for WebrpcError {
    fn from(value: bincode::Error) -> Self {
        WebrpcError::ParseError(value.to_string())
    }
}

impl From<crate::signaling::SignalingError> for WebrpcError {
    fn from(value: crate::signaling::SignalingError) -> Self {
        match value {
            crate::signaling::SignalingError::TimesUp(time) => WebrpcError::SignalingTimesUp(time),
            value => WebrpcError::SignalingError(value.to_string()),
        }
    }
}

impl From<webrtc_ice::Error> for WebrpcError {
    fn from(value: webrtc_ice::Error) -> Self {
        Self::ICEError(value.to_string())
    }
}

impl From<webrtc_sctp::Error> for WebrpcError {
    fn from(value: webrtc_sctp::Error) -> Self {
        Self::SCTPError(value.to_string())
    }
}

impl From<webrtc_data::Error> for WebrpcError {
    fn from(value: webrtc_data::Error) -> Self {
        Self::DataChannelError(value.to_string())
    }
}
