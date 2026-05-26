#![doc = include_str!("README.md")]

pub mod config_man;
pub mod editor_receptor;
mod engine;
mod error;
pub mod filewatch_receptor;
mod ice;
pub mod message;
mod op;
pub mod server;
pub mod signaling;
pub mod types;
pub mod webchannel;
pub mod websocket_receptor;

/// Server implementation details — subscriber bookkeeping, op routing,
/// document lifecycle, etc. See the page contents for the full write-up.
#[doc = include_str!("implementation.md")]
pub mod implementation_doc {}

/// Editor ↔ collab-process JSON-RPC request and response flows. Each
/// editor request is documented end-to-end (params, response, server
/// processing).
#[doc = include_str!("../../docs/editor-request-flows.md")]
pub mod editor_request_flows_doc {}
