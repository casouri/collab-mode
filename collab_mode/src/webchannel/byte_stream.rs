//! Byte-stream framing utilities used by the io and ssh transports
//! plus the envoy bootstrap. All three of `ReaderWriter`,
//! `frame_read`, and `frame_write` lived in the old `io_channel.rs`;
//! that module is gone now, the shapes here are unchanged.

use crate::webchannel::Message;
use anyhow::Result;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

/// A bidirectional byte-stream paired with a cleanup value that drops
/// with the stream. Implements both [`AsyncRead`] and [`AsyncWrite`].
///
/// Producers (eg. an SSH spawner, the envoy stdio path) construct one
/// with [`ReaderWriter::new`], stashing whatever they need to clean
/// up — child process handles, ssh sessions, kill-on-drop guards — in
/// the `cleanup` argument. When the `ReaderWriter` is dropped (or
/// both halves of a `tokio::io::split` of it are dropped), the
/// cleanup value drops.
pub struct ReaderWriter {
    reader: Box<dyn AsyncRead + Send + Unpin>,
    writer: Box<dyn AsyncWrite + Send + Unpin>,
    _cleanup: Box<dyn Send + Sync>,
}

impl ReaderWriter {
    pub fn new<R, W, C>(reader: R, writer: W, cleanup: C) -> Self
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
        C: Send + Sync + 'static,
    {
        Self {
            reader: Box::new(reader),
            writer: Box::new(writer),
            _cleanup: Box::new(cleanup),
        }
    }

    /// Convenience: wrap any single duplex stream (e.g.
    /// `tokio::io::duplex`'s halves) as a `ReaderWriter` with no
    /// extra cleanup.
    pub fn from_duplex<S>(stream: S) -> Self
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let (r, w) = tokio::io::split(stream);
        Self::new(r, w, ())
    }
}

impl AsyncRead for ReaderWriter {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.reader).poll_read(cx, buf)
    }
}

impl AsyncWrite for ReaderWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.writer).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.writer).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.writer).poll_shutdown(cx)
    }
}

/// Write a length-prefixed serialized [`Message`] to `writer`. Format
/// is `[8-byte big-endian length][serde_json payload]`.
pub async fn frame_write<W>(writer: &mut W, message: &Message) -> Result<()>
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
pub async fn frame_read<R>(reader: &mut R) -> Result<Message>
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
