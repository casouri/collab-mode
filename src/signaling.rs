use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio_tungstenite as tung;

pub mod client;
pub mod server;

/// We treat SDP as a black box.
pub type SDP = String;
/// We treat ICE candidate as a black box too.
pub type ICECandidate = String;
/// Client id is used for multiplexing multiple clients' requests over
/// the same connection to the signaling server.
pub type ClientId = u32;

pub type SignalingResult<T> = Result<T, SignalingError>;

#[derive(Debug, Clone, Error, Serialize, Deserialize)]
pub enum SignalingError {
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
    #[error("Couldn't parse the message")]
    ParseError(String),
    #[error("No collab server is listening on this id: {0}")]
    NoServerForId(String),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum SignalingMessage {
    /// A listen request. Collab server send this request to signaling
    /// server to listen for connection requests to the id provided.
    /// The id should be a randomly generated uuid string.
    BindRequest(String, SDP),
    /// A collab client sends this message to the signaling server to
    /// connect to the collab server with this id.
    ConnectRequest(String, SDP),
    /// The signaling server returns the collab server's SDP to the
    /// collab client.
    ConnectResponse(SDP),
    /// The signaling server also sends the collab client's SDP to the
    /// collab server so it connects to the client.
    BindResponse(SDP),
    /// Collab client uses this message to send and receive
    /// candidates.
    Candidate(ICECandidate),
    /// Collab server uses this message to send and receive
    /// candidates.
    CandidateForId(ClientId, ICECandidate),
    /// Cannot find the corresponding server for the provided id.
    NoServerForId(String),
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

/// Signaling server returns this response,
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ListenResponse {
    pub id: String,
}

/// Collab client send this request to connect to
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ConnectionRequest {
    pub id: String,
}

#[cfg(test)]
mod tests {
    use crate::signaling::{client, server};
    use std::sync::Arc;
    use webrtc::ice_transport::ice_server::RTCIceServer;
    use webrtc::peer_connection::configuration::RTCConfiguration;

    async fn test() -> anyhow::Result<()> {
        let api = webrtc::api::APIBuilder::new().build();
        let config = RTCConfiguration {
            ice_servers: vec![RTCIceServer {
                urls: vec!["stun:stun.l.google.com:19302".to_owned()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let conn = Arc::new(api.new_peer_connection(config).await?);
        let offer = conn.create_offer(None).await?;
        conn.set_local_description(offer).await?;
        let sdp = conn.local_description().await.unwrap();

        Ok(())
    }

    #[test]
    pub fn signaling_test() -> anyhow::Result<()> {
        let sdp_server = "veemo".to_string();
        let sdp_client = "woome".to_string();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _ = runtime.spawn(server::run_signaling_server("127.0.0.1:9000"));

        runtime.spawn(async move {
            let channel =
                client::bind("ws://127.0.0.1:9000", "1".to_string(), sdp_server.clone()).await;
            if let Ok(mut channel) = channel {
                while let Some(msg) = channel.recv().await {
                    match msg {
                        Ok(sdp) => {
                            println!("Got client sdp: {}", sdp);
                            assert!(sdp == sdp_server);
                        }
                        Err(err) => {
                            panic!("Bind error: {:?}", err);
                        }
                    }
                }
            } else {
                panic!("Failed to bind");
            }
        });

        runtime.block_on(async move {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            let res =
                client::connect("ws://127.0.0.1:9000", "1".to_string(), sdp_client.clone()).await;
            match res {
                Ok(sdp) => {
                    println!("Got server sdp: {}", sdp);
                    assert!(sdp == sdp_client);
                }
                Err(err) => {
                    panic!("Connect error: {:?}", err)
                }
            }
        });

        Ok(())
    }
}
