//! A websocket interface used for testing.
//! Actual connections need to be webrtc which can traverse NAT.

use futures_util::{SinkExt, StreamExt};
use lsp_server::Message;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite as tung;

use crate::message::err_msg;

/// A connection to a remote peer.
#[derive(Debug)]
pub struct Sock {
    pub msg_tx: mpsc::Sender<Message>,
    pub msg_rx: mpsc::Receiver<Message>,
}

/// Run a websocket receptor on `addr`. Returns a receiver that caller
/// can read connections from.
pub async fn accept(addr: &str) -> anyhow::Result<mpsc::Receiver<Sock>> {
    let listener = TcpListener::bind(addr).await?;
    let (sock_tx, sock_rx) = mpsc::channel(1);
    while let Ok((stream, client_addr)) = listener.accept().await {
        tracing::info!("Accepted a connection from {}", client_addr);
        let (in_tx, mut in_rx) = mpsc::channel(16);
        let (out_tx, out_rx) = mpsc::channel(16);
        let mut stream = tung::accept_async(stream).await?;

        let res = sock_tx
            .send(Sock {
                msg_tx: in_tx,
                msg_rx: out_rx,
            })
            .await;
        if res.is_err() {
            // Caller dropped connection.
            break;
        }

        let _ = tokio::spawn(async move {
            loop {
                tokio::select! {
                    msg = stream.next() => {
                        if msg.is_none() {
                            tracing::info!("Websocket stream closed by remote ({})", client_addr);
                            break;
                        }
                        let msg = msg.unwrap();
                        if msg.is_err() {
                            let _ = out_tx.send(err_msg(format!("Connection with {} broke: {}", client_addr, msg.err().unwrap()))).await;
                            break;
                        }
                        let msg = msg.unwrap();
                        let msg = msg.to_text();
                        if msg.is_err() {
                            tracing::warn!("Received non-text message from {}", client_addr);
                            continue;
                        }
                        let msg = msg.unwrap();
                        let msg = serde_json::from_str::<Message>(msg);
                        if msg.is_err() {
                            let _ = out_tx.send(err_msg(format!("Can't parse message: {:?}", msg))).await;
                            continue;
                        }
                        let msg = msg.unwrap();
                        let res = out_tx.send(msg).await;
                        if res.is_err() {
                            // Caller wants to drop the connection.
                            break;
                        }
                    }
                    msg = in_rx.recv() => {
                        if msg.is_none() {
                            // Caller wants to drop the connection.
                            break;
                        }
                        let msg = msg.unwrap();
                        let res = stream.send(tokio_tungstenite::tungstenite::Message::Text(serde_json::to_string(&msg).unwrap())).await;
                        if res.is_err() {
                            let _ = out_tx.send(err_msg(format!("Connection with {} broke: {}", client_addr, res.err().unwrap()))).await;
                            break;
                        }
                    }
                }
            }
        });
    }
    Ok(sock_rx)
}
