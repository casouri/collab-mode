//! [`MsgChannel`] implementation that uses ssh's stdio as the data
//! channel. Each peer is backed by a spawned ssh subprocess (via the
//! `openssh` crate) and an [`IoMsgChannel`] wrapping the child's
//! stdin/stdout.
//!
//! The session-and-child lifetime is managed inside a tokio task that
//! owns both for their entire lifetime; the [`ReaderWriter`] returned
//! here carries a `oneshot::Sender<()>` whose drop signals that task to
//! exit and clean up.

use crate::config_man::ArcKeyCert;
use crate::io_channel::{IoMsgChannel, ReaderWriter};
use crate::message::Msg;
use crate::types::ServerId;
use crate::webchannel::{Message, MsgChannel, Transport};
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use lsp_server::RequestId;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

pub struct SshMsgChannel {
    my_hostid: ServerId,
    remote_msg_tx: mpsc::Sender<Message>,
    loopback_tx: mpsc::UnboundedSender<Message>,
    peers: Arc<Mutex<HashMap<ServerId, IoMsgChannel>>>,
}

impl SshMsgChannel {
    pub fn new(
        my_hostid: ServerId,
        remote_msg_tx: mpsc::Sender<Message>,
        loopback_tx: mpsc::UnboundedSender<Message>,
    ) -> Self {
        Self {
            my_hostid,
            remote_msg_tx,
            loopback_tx,
            peers: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl MsgChannel for SshMsgChannel {
    async fn send(
        &self,
        recipient: &ServerId,
        req_id: Option<RequestId>,
        msg: Msg,
    ) -> anyhow::Result<()> {
        // Loopback short-circuit, mirroring `WebChannel::send`.
        if recipient == &self.my_hostid {
            let message = Message {
                host: self.my_hostid.clone(),
                body: msg,
                req_id,
            };
            self.loopback_tx
                .send(message)
                .map_err(|_| anyhow!("Loopback channel closed"))?;
            return Ok(());
        }

        let io = {
            let peers = self.peers.lock().unwrap();
            peers
                .get(recipient)
                .map(|io| io.clone())
                .ok_or_else(|| anyhow!("Not connected to {}", recipient))?
        };
        io.send(recipient, req_id, msg).await
    }

    async fn connect(
        &self,
        remote_id: ServerId,
        transport: Transport,
        _my_key_cert: ArcKeyCert,
    ) -> anyhow::Result<()> {
        let (ssh_host, command) = match transport {
            Transport::Ssh { ssh_host, command } => (ssh_host, command),
            Transport::Sock(_) => {
                return Err(anyhow!("SshMsgChannel does not handle Sock transports"))
            }
            Transport::Dummy => {
                return Err(anyhow!("SshMsgChannel does not handle Dummy transport"))
            }
        };

        let rw = ssh_reader_writer(&ssh_host, &command)
            .await
            .with_context(|| format!("Spawning ssh to {}", ssh_host))?;

        let io = IoMsgChannel::new(
            self.my_hostid.clone(),
            remote_id.clone(),
            rw,
            self.remote_msg_tx.clone(),
            self.loopback_tx.clone(),
        );

        self.peers.lock().unwrap().insert(remote_id, io);
        Ok(())
    }

    async fn broadcast(&self, req_id: Option<RequestId>, msg: Msg) -> anyhow::Result<()> {
        let peer_ids: Vec<ServerId> = {
            let peers = self.peers.lock().unwrap();
            peers.keys().cloned().collect()
        };
        for peer in peer_ids {
            self.send(&peer, req_id.clone(), msg.clone()).await?;
        }
        Ok(())
    }

    fn disconnect(&self, peer: &ServerId) {
        // Removing the entry drops the `IoMsgChannel`, which (via the
        // split halves of its `ReaderWriter`) eventually drops the
        // openssh cleanup, closing ssh.
        self.peers.lock().unwrap().remove(peer);
    }

    async fn shutdown(&self) -> anyhow::Result<()> {
        // Clear the peers map. Dropping each `IoMsgChannel` causes its
        // tasks to exit, the `ReaderWriter` halves to drop, the cleanup
        // carrier to drop, and the openssh background task to wake and
        // run `session.close()`.
        self.peers.lock().unwrap().clear();
        Ok(())
    }
}

/// Spawn ssh to `ssh_host` and run `command`, returning a
/// [`ReaderWriter`] over the child's stdio. All segments in `command`
/// are shell-escaped. Dropping the returned `ReaderWriter` causes a
/// background task to wake and run `session.close().await` for a
/// graceful SSH teardown.
///
/// We create a separate ssh session for each connection, so closing
/// the session also kills the remote process it spawned.
///
/// Honors `~/.ssh/config`, ssh-agent, ProxyJump, etc. via the `openssh`
/// crate.
pub async fn ssh_reader_writer(ssh_host: &str, command: &[String]) -> anyhow::Result<ReaderWriter> {
    if command.is_empty() {
        return Err(anyhow!("command must not be empty"));
    }
    let command: Vec<String> = command.to_vec();

    // Spawn a background task that owns the `Session` and `RemoteChild`
    // (the child borrows the session) for their entire lifetime. The
    // task waits on `shutdown_rx` so both stay alive until the caller
    // drops the `ReaderWriter` (and thus the held `shutdown_tx`).
    let (init_tx, init_rx) =
        oneshot::channel::<anyhow::Result<(openssh::ChildStdin, openssh::ChildStdout)>>();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let ssh_host_owned = ssh_host.to_string();

    tokio::spawn(async move {
        let session =
            match openssh::Session::connect(&ssh_host_owned, openssh::KnownHosts::Strict).await {
                Ok(s) => s,
                Err(e) => {
                    let _ = init_tx.send(Err(anyhow::Error::new(e)
                        .context(format!("openssh connect to {}", ssh_host_owned))));
                    return;
                }
            };

        let mut cmd = session.command(&command[0]);
        for arg in &command[1..] {
            // .arg() shell-escapes each arg (vs raw_arg which does not).
            cmd.arg(arg);
        }
        cmd.stdin(openssh::Stdio::piped())
            .stdout(openssh::Stdio::piped())
            .stderr(openssh::Stdio::inherit());

        let mut child = match cmd.spawn().await {
            Ok(c) => c,
            Err(e) => {
                let _ = init_tx.send(Err(anyhow::Error::new(e).context(format!(
                    "openssh spawn {} on {}",
                    command[0], ssh_host_owned
                ))));
                return;
            }
        };

        let stdin = match child.stdin().take() {
            Some(s) => s,
            None => {
                let _ = init_tx.send(Err(anyhow!("openssh child missing stdin")));
                return;
            }
        };
        let stdout = match child.stdout().take() {
            Some(s) => s,
            None => {
                let _ = init_tx.send(Err(anyhow!("openssh child missing stdout")));
                return;
            }
        };

        if init_tx.send(Ok((stdin, stdout))).is_err() {
            // Caller dropped the future; nothing to keep alive.
            return;
        }

        // Hold `child` and `session` here until the caller drops the
        // `ReaderWriter` (which drops `shutdown_tx`).
        let _ = shutdown_rx.await;

        // Drop the child first (kills the remote command), then call
        // `session.close().await` for a graceful SSH teardown that
        // flushes any in-flight writes before returning. No timeout
        // here — the server bounds the visible shutdown window with
        // its own timeout; this task is fire-and-forget and is
        // cancelled when the runtime drops at process exit.
        drop(child);
        if let Err(err) = session.close().await {
            tracing::warn!(
                "openssh session close failed for {}: {}",
                ssh_host_owned,
                err
            );
        }
    });

    let (stdin, stdout) = init_rx
        .await
        .map_err(|_| anyhow!("openssh spawn task ended before sending result"))??;

    /// Cleanup carrier: dropping it signals the background task to
    /// wake from `shutdown_rx` and run `session.close()`.
    struct Cleanup {
        _shutdown: oneshot::Sender<()>,
    }

    Ok(ReaderWriter::new(
        stdout,
        stdin,
        Cleanup {
            _shutdown: shutdown_tx,
        },
    ))
}

#[cfg(test)]
mod tests;
