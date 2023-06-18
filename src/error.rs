use crate::engine::EngineError;
use crate::types::*;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tonic::Status;

#[derive(Debug, Clone, Error, Serialize, Deserialize)]
pub enum CollabError {
    #[error("Cannot parse request: {0}")]
    ParseError(String),
    #[error("Fatal OT engine error: {err:?}")]
    EngineError {
        #[from]
        err: EngineError,
    },
    #[error("A document with this id already exists: {0:?}")]
    DocAlreadyExists(DocId),
    #[error("Cannot find the document with this id: {0:?}")]
    DocNotFound(DocId),
    #[error("Not connected to the server: {0:?}")]
    ServerNotConnected(ServerId),
    #[error("Unexpected closed channel: {0}")]
    ChannelClosed(String),
    #[error("Operation out of bounds: {0:?}, doc size: {1:?}")]
    OpOutOfBound(Op, usize),
    #[error("gRPC transport error: {0}")]
    TransportErr(String),
    #[error("IO error: {0}")]
    IOErr(String),
}

pub type CollabResult<T> = Result<T, CollabError>;

impl From<Status> for CollabError {
    fn from(value: Status) -> Self {
        if let tonic::Code::Internal = value.code() {
            serde_json::from_str::<CollabError>(&value.message()).unwrap()
        } else {
            CollabError::TransportErr(value.to_string())
        }
    }
}

impl From<CollabError> for Status {
    fn from(value: CollabError) -> Self {
        let str = serde_json::to_string(&value).unwrap();
        Status::internal(str)
    }
}

impl From<serde_json::Error> for CollabError {
    fn from(value: serde_json::Error) -> Self {
        CollabError::ParseError(format!("{:#}", value))
    }
}
