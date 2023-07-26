use crate::signaling::{SignalingError, SignalingMessage, SignalingResult, SDP};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite as tung;
use tokio_tungstenite::tungstenite::Message;

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

/// Send a bind request (`id`, `sdp`) to the signaling server at
/// `addr`. Return a stream of SDP's. If the connection is closed, the
/// stream reads a `SignalingError::Closed`. `addr` should be a vaoid
/// URL like "ws://...".
pub async fn bind(
    addr: &str,
    id: String,
    sdp: SDP,
) -> SignalingResult<mpsc::Receiver<SignalingResult<SDP>>> {
    let (mut stream, _addr) = tung::connect_async(addr).await?;

    let msg = SignalingMessage::BindRequest(id, sdp);
    stream
        .send(Message::Text(serde_json::to_string(&msg).unwrap()))
        .await?;

    let mut stream = stream.map(|msg| {
        let msg = expect_text(msg?)?;
        let msg: SignalingMessage = serde_json::from_str(&msg)
            .map_err(|err| SignalingError::ParseError(err.to_string()))?;
        match msg {
            SignalingMessage::BindResponse(sdp) => Ok(sdp),
            msg => Err(SignalingError::UnexpectedMessage(
                "BindResponse".to_string(),
                msg,
            )),
        }
    });
    let (tx, rx) = mpsc::channel(1);
    tokio::spawn(async move {
        while let Some(msg) = stream.next().await {
            if tx.send(msg).await.is_err() {
                return;
            }
        }
    });
    Ok(rx)
}

/// Send a connect request (`id`, `sdp`) to the signaling server at
/// `addr`. `addr` should be a vaoid URL like "ws://...".
pub async fn connect(addr: &str, id: String, sdp: SDP) -> SignalingResult<SDP> {
    let (mut stream, _addr) = tung::connect_async(addr).await?;

    let msg = SignalingMessage::ConnectRequest(id, sdp);
    stream
        .send(Message::Text(serde_json::to_string(&msg).unwrap()))
        .await?;
    if let Some(msg) = stream.next().await {
        let text = expect_text(msg?)?;
        let msg = serde_json::from_str(&text)
            .map_err(|err| SignalingError::ParseError(err.to_string()))?;
        match msg {
            SignalingMessage::ConnectResponse(sdp) => {
                let _ = stream.send(Message::Close(None)).await;
                Ok(sdp)
            }
            msg => Err(SignalingError::UnexpectedMessage(
                "ConnectResponse".to_string(),
                msg,
            )),
        }
    } else {
        Err(SignalingError::Closed)
    }
}
