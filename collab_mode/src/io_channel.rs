//! Single-peer framed message channel over arbitrary `AsyncRead`/`AsyncWrite`.
//!
//! The connection is set up before construction; [`MsgChannel::connect`] is a
//! no-op. [`MsgChannel::send`] ignores the recipient parameter — there is
//! exactly one peer for the lifetime of an [`IoMsgChannel`].
//!
//! Used directly by the envoy (which has stdin/stdout already wired) and
//! instantiated per-peer by `SshMsgChannel`.

use crate::config_man::ArcKeyCert;
use crate::message::Msg;
use crate::types::ServerId;
use crate::webchannel::{Message, MsgChannel, Transport};
use anyhow::anyhow;
use async_trait::async_trait;
use lsp_server::RequestId;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

struct State {
    out_tx: Option<mpsc::Sender<Message>>,
    /// When this drops (via `disconnect` or `Drop` on `IoMsgChannel`), the
    /// spawned tasks observe `cancel_tx.closed()` and exit.
    cancel_rx: Option<mpsc::Receiver<()>>,
}

pub struct IoMsgChannel {
    my_hostid: ServerId,
    peer_id: ServerId,
    loopback_tx: mpsc::UnboundedSender<Message>,
    state: Arc<Mutex<State>>,
}

impl IoMsgChannel {
    /// Wrap a pre-connected reader/writer pair. Spawns inbound and outbound
    /// framing tasks immediately. There is exactly one peer for the lifetime
    /// of this channel.
    pub fn new<R, W>(
        my_hostid: ServerId,
        peer_id: ServerId,
        reader: R,
        writer: W,
        remote_msg_tx: mpsc::Sender<Message>,
        loopback_tx: mpsc::UnboundedSender<Message>,
    ) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (out_tx, out_rx) = mpsc::channel::<Message>(16);
        let (cancel_tx, cancel_rx) = mpsc::channel::<()>(1);

        // We only send out ConnectionBroke message on inbound error,
        // because when outbound errors out, I expect inbound to also
        // error.

        // Outbound task: drain out_rx, frame, write.
        let cancel_tx_out = cancel_tx.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = run_outbound(out_rx, writer) => {}
                _ = cancel_tx_out.closed() => {}
            }
        });

        // Inbound task: read frames, push to remote_msg_tx; emit
        // ConnectionBroke on EOF.
        let peer_id_in = peer_id.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = run_inbound(reader, remote_msg_tx, peer_id_in) => {}
                _ = cancel_tx.closed() => {}
            }
        });

        Self {
            my_hostid,
            peer_id,
            loopback_tx,
            state: Arc::new(Mutex::new(State {
                out_tx: Some(out_tx),
                cancel_rx: Some(cancel_rx),
            })),
        }
    }

    /// The peer this channel is configured to talk to.
    pub fn peer_id(&self) -> &ServerId {
        &self.peer_id
    }
}

#[async_trait]
impl MsgChannel for IoMsgChannel {
    async fn send(
        &self,
        recipient: &ServerId,
        req_id: Option<RequestId>,
        msg: Msg,
    ) -> anyhow::Result<()> {
        let message = Message {
            host: self.my_hostid.clone(),
            body: msg,
            req_id,
        };

        // Loopback short-circuit, mirroring `WebChannel::send`.
        if recipient == &self.my_hostid {
            self.loopback_tx
                .send(message)
                .map_err(|_| anyhow!("Loopback channel closed"))?;
            return Ok(());
        }

        // Otherwise route to the single peer; recipient id is ignored.
        let tx = self
            .state
            .lock()
            .unwrap()
            .out_tx
            .clone()
            .ok_or_else(|| anyhow!("Not connected"))?;
        tx.send(message)
            .await
            .map_err(|_| anyhow!("Channel broke"))?;
        Ok(())
    }

    async fn connect(
        &self,
        _remote_id: ServerId,
        _transport: Transport,
        _my_key_cert: ArcKeyCert,
    ) -> anyhow::Result<()> {
        // IoMsgChannel never initiates; the connection is set up at
        // construction time.
        Ok(())
    }

    async fn broadcast(&self, req_id: Option<RequestId>, msg: Msg) -> anyhow::Result<()> {
        let peer_id = self.peer_id.clone();
        self.send(&peer_id, req_id, msg).await
    }

    fn disconnect(&self, _peer: &ServerId) {
        // Drop both halves so the outbound task's `out_rx.recv` returns
        // None and the inbound task's `cancel_tx.closed()` fires.
        let mut s = self.state.lock().unwrap();
        s.out_tx = None;
        s.cancel_rx = None;
    }
}

async fn run_outbound<W>(mut out_rx: mpsc::Receiver<Message>, mut writer: W)
where
    W: AsyncWrite + Unpin,
{
    while let Some(message) = out_rx.recv().await {
        if let Err(err) = frame_write(&mut writer, &message).await {
            tracing::warn!("IoMsgChannel outbound write failed: {}", err);
            return;
        }
    }
}

async fn run_inbound<R>(mut reader: R, remote_msg_tx: mpsc::Sender<Message>, peer_id: ServerId)
where
    R: AsyncRead + Unpin,
{
    loop {
        match frame_read(&mut reader).await {
            Ok(message) => {
                if remote_msg_tx.send(message).await.is_err() {
                    tracing::info!("IoMsgChannel inbound: receiver gone, exiting");
                    return;
                }
            }
            Err(err) => {
                tracing::info!(
                    peer = %peer_id,
                    error = %err,
                    "IoMsgChannel inbound: stream ended"
                );
                let _ = remote_msg_tx
                    .send(Message {
                        host: peer_id.clone(),
                        body: Msg::ConnectionBroke(peer_id.clone()),
                        req_id: None,
                    })
                    .await;
                return;
            }
        }
    }
}

/// Write a length-prefixed serialized [`Message`] to `writer`. Format is
/// `[8-byte big-endian length][serde_json payload]`, matching the existing
/// SCTP framing.
pub async fn frame_write<W>(writer: &mut W, message: &Message) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let bytes = serde_json::to_vec(message)?;
    let len = (bytes.len() as u64).to_be_bytes();
    writer.write_all(&len).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

/// Read a length-prefixed serialized [`Message`] from `reader`.
pub async fn frame_read<R>(reader: &mut R) -> anyhow::Result<Message>
where
    R: AsyncRead + Unpin,
{
    let mut len_bytes = [0u8; 8];
    reader.read_exact(&mut len_bytes).await?;
    let len = u64::from_be_bytes(len_bytes) as usize;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    let message: Message = serde_json::from_slice(&buf)?;
    Ok(message)
}

#[cfg(test)]
mod tests;
