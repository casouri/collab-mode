//! Per-peer actor for byte-stream transports (envoy stdio, ssh stdio).
//!
//! Just like [`super::SctpRemote`], use one spawned task that owns
//! reader, writer, and the `Command` receiver, and runs a single
//! `select!` loop. Outgoing messages arrive as `Command::Msg` and are
//! framed onto the writer; inbound frames are deserialized and
//! forwarded to the server mailbox.

use super::{frame_read, frame_write, Command, Message, ReaderWriter};
use crate::message::Msg;
use crate::types::ServerId;
use tokio::io::{ReadHalf, WriteHalf};
use tokio::sync::mpsc;

pub(super) struct IoRemote {
    peer_id: ServerId,
    reader: ReadHalf<ReaderWriter>,
    writer: WriteHalf<ReaderWriter>,
    remote_msg_tx: mpsc::Sender<Message>,
    rx: mpsc::Receiver<Command>,
}

/// Why the actor’s main loop exited.
enum Exit {
    Shutdown(tokio::sync::oneshot::Sender<anyhow::Result<()>>),
    OutgoingErr,
    InboundClosed,
    SenderDropped,
}

impl IoRemote {
    pub(super) fn new(
        peer_id: ServerId,
        rw: ReaderWriter,
        remote_msg_tx: mpsc::Sender<Message>,
        rx: mpsc::Receiver<Command>,
    ) -> Self {
        let (reader, writer) = tokio::io::split(rw);
        Self {
            peer_id,
            reader,
            writer,
            remote_msg_tx,
            rx,
        }
    }

    /// Run select in a loop, and handle incoming/outgoing messages,
    /// as well as shutdown. Cleanup after loop exits.
    pub(super) async fn run(mut self) {
        let exit = self.run_inner().await;
        match exit {
            Exit::Shutdown(tx) => {
                let _ = tx.send(Ok(()));
            }
            Exit::OutgoingErr | Exit::InboundClosed | Exit::SenderDropped => {
                let _ = self
                    .remote_msg_tx
                    .send(Message {
                        host: self.peer_id.clone(),
                        body: Msg::ConnectionBroke {
                            peer: self.peer_id.clone(),
                        },
                        req_id: None,
                    })
                    .await;
            }
        }
    }

    /// Single select loop over outgoing commands and inbound frames.
    /// Returns the reason we’re exiting.
    async fn run_inner(&mut self) -> Exit {
        loop {
            tokio::select! {
                cmd = self.rx.recv() => match cmd {
                    Some(Command::Shutdown(tx)) => return Exit::Shutdown(tx),
                    Some(Command::Msg(msg)) => {
                        if let Err(e) = frame_write(&mut self.writer, &msg).await {
                            tracing::warn!("IoRemote write to {} failed: {e}", self.peer_id);
                            return Exit::OutgoingErr;
                        }
                    }
                    None => return Exit::SenderDropped,
                },
                frame = frame_read(&mut self.reader) => match frame {
                    Ok(msg) => {
                        if self.remote_msg_tx.send(msg).await.is_err() {
                            return Exit::InboundClosed;
                        }
                    }
                    Err(e) => {
                        tracing::info!("IoRemote inbound for {} ended: {e}", self.peer_id);
                        return Exit::InboundClosed;
                    }
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::message::{HeyMessage, Msg};
    use std::time::Duration;
    use tokio::time::timeout;

    fn hey() -> Msg {
        Msg::Hey {
            hey: HeyMessage {
                message: "hi".into(),
                credentials: "".into(),
                version: "v1.0.0".into(),
            },
        }
    }

    fn key_cert() -> ArcKeyCert {
        std::sync::Arc::new(crate::config_man::create_key_cert("test"))
    }

    /// Build two WebChannels connected via a `tokio::io::duplex` and
    /// hand back each side’s `DualReceiver` for inbound assertions.
    async fn pair() -> (WebChannel, DualReceiver, WebChannel, DualReceiver) {
        let (a, b) = tokio::io::duplex(8192);
        let (dual_a, tx_a, self_tx_a) = DualReceiver::new();
        let (dual_b, tx_b, self_tx_b) = DualReceiver::new();
        let chan_a = WebChannel::new("a".into(), tx_a, self_tx_a);
        let chan_b = WebChannel::new("b".into(), tx_b, self_tx_b);
        chan_a
            .connect(
                "b".into(),
                Transport::Io(ReaderWriter::from_duplex(a)),
                key_cert(),
            )
            .await
            .unwrap();
        chan_b
            .connect(
                "a".into(),
                Transport::Io(ReaderWriter::from_duplex(b)),
                key_cert(),
            )
            .await
            .unwrap();
        (chan_a, dual_a, chan_b, dual_b)
    }

    #[tokio::test]
    async fn roundtrip() {
        let (chan_a, _dual_a, _chan_b, mut dual_b) = pair().await;
        chan_a.send(&"b".into(), None, hey()).await.unwrap();
        let msg = timeout(Duration::from_secs(2), dual_b.recv())
            .await
            .expect("recv timed out")
            .expect("dual_b closed");
        assert_eq!(msg.host, "a");
        assert!(matches!(msg.body, Msg::Hey { hey: _ }));
    }

    #[tokio::test]
    async fn eof_emits_connection_broke() {
        let (_chan_a, mut dual_a, chan_b, _dual_b) = pair().await;
        // Shutdown on B drives its IoRemote to exit cleanly; dropping
        // its writer half closes the duplex; A’s frame_read hits EOF.
        chan_b.shutdown().await.unwrap();
        let msg = timeout(Duration::from_secs(2), dual_a.recv())
            .await
            .expect("recv timed out")
            .expect("dual_a closed");
        assert!(matches!(msg.body, Msg::ConnectionBroke { peer: _ }));
    }

    #[tokio::test]
    async fn send_to_unconnected_fails() {
        let (_dual_a, tx_a, self_tx_a) = DualReceiver::new();
        let chan_a = WebChannel::new("a".into(), tx_a, self_tx_a);
        let res = chan_a.send(&"b".into(), None, hey()).await;
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("Not connected"));
    }
}
