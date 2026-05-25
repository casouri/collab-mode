//! Per-peer actor for SSH transport. Owns the ssh-specific setup
//! (spawning the remote process via openssh) and then delegates the
//! byte-stream actor work to [`super::IoRemote`].
//!
//! Lifecycle: `SshRemote::run` is itself the long-lived task. It opens
//! the ssh `Session`, spawns the remote command, wraps the child’s
//! stdio in a [`ReaderWriter`], hands that to an `IoRemote`, and
//! awaits the actor loop. When `IoRemote` returns, the `ReaderWriter`
//! and its half-streams drop, the child is dropped (killing the
//! remote process), and `session.close().await` runs a graceful SSH
//! teardown — all in the same task, no helper spawn needed.

use super::{Command, IoRemote, Message, ReaderWriter};
use crate::message::Msg;
use crate::types::{id_short, ServerId};
use tokio::sync::mpsc;

pub(super) struct SshRemote {
    peer_id: ServerId,
    ssh_host: String,
    command: Vec<String>,
    remote_msg_tx: mpsc::Sender<Message>,
    rx: mpsc::Receiver<Command>,
}

impl SshRemote {
    pub(super) fn new(
        peer_id: ServerId,
        ssh_host: String,
        command: Vec<String>,
        remote_msg_tx: mpsc::Sender<Message>,
        rx: mpsc::Receiver<Command>,
    ) -> Self {
        Self {
            peer_id,
            ssh_host,
            command,
            remote_msg_tx,
            rx,
        }
    }

    /// Connect ssh, spawn the remote command, run an `IoRemote` over
    /// its stdio, then close the session. No nested `tokio::spawn` —
    /// `SshRemote::run` is already on a spawned task, so the `Session`
    /// + `RemoteChild` can just live as locals across the whole run.
    /// On any setup error, push `ConnectionBroke` and return.
    pub(super) async fn run(self) {
        let SshRemote {
            peer_id,
            ssh_host,
            command,
            remote_msg_tx,
            rx,
        } = self;

        if command.is_empty() {
            tracing::warn!("SshRemote for {} got empty command", id_short(&peer_id));
            send_connection_broke(&remote_msg_tx, &peer_id).await;
            return;
        }

        // Open ssh session.
        let session = match openssh::Session::connect(&ssh_host, openssh::KnownHosts::Strict).await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "openssh connect to {ssh_host} for {} failed: {e}",
                    id_short(&peer_id)
                );
                send_connection_broke(&remote_msg_tx, &peer_id).await;
                return;
            }
        };

        // Spawn the remote command.
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
                tracing::warn!(
                    "openssh spawn {} on {ssh_host} for {} failed: {e}",
                    command[0],
                    id_short(&peer_id)
                );
                let _ = session.close().await;
                send_connection_broke(&remote_msg_tx, &peer_id).await;
                return;
            }
        };

        let (stdin, stdout) = match (child.stdin().take(), child.stdout().take()) {
            (Some(i), Some(o)) => (i, o),
            _ => {
                tracing::warn!(
                    "openssh child missing stdin/stdout for {}",
                    id_short(&peer_id)
                );
                drop(child);
                let _ = session.close().await;
                send_connection_broke(&remote_msg_tx, &peer_id).await;
                return;
            }
        };

        let rw = ReaderWriter::new(stdout, stdin, ());
        IoRemote::new(peer_id, rw, remote_msg_tx, rx).run().await;

        // IoRemote::run will do cleanup; here we just need to cleanup
        // the ssh-related stuff.
        drop(child);
        if let Err(err) = session.close().await {
            tracing::warn!("openssh session close failed for {ssh_host}: {err}");
        }
    }
}

async fn send_connection_broke(tx: &mpsc::Sender<Message>, peer_id: &ServerId) {
    let _ = tx
        .send(Message {
            host: peer_id.clone(),
            body: Msg::ConnectionBroke {
                peer: peer_id.clone(),
            },
            req_id: None,
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::message::{HeyMessage, Msg};
    use std::time::Duration;
    use tokio::time::timeout;

    fn key_cert() -> ArcKeyCert {
        std::sync::Arc::new(crate::config_man::create_key_cert("me-test"))
    }

    fn hey() -> Msg {
        Msg::Hey {
            hey: HeyMessage {
                message: "hi".into(),
                credentials: "".into(),
                version: "v1.0.0".into(),
            },
        }
    }

    /// Requires setting up `ssh yuan@localhost` to work. Run with
    /// `cargo test ssh_remote::tests::openssh_localhost --
    /// --ignored`. Uses `cat` as the remote command so the framed Hey
    /// we send is echoed back through our inbound channel.
    #[tokio::test]
    #[ignore]
    async fn openssh_localhost_roundtrip() {
        let (mut dual, tx, self_tx) = DualReceiver::new();
        let chan = WebChannel::new(
            "me".into(),
            tx,
            self_tx,
            std::sync::Arc::new(tokio::sync::Notify::new()),
        );

        chan.connect(
            "peer".into(),
            Transport::Ssh {
                ssh_host: "yuan@localhost".into(),
                command: vec!["cat".into()],
            },
            key_cert(),
        )
        .await
        .expect("connect_ssh should return Ok (ssh spawn is async)");

        chan.send(&"peer".into(), None, hey())
            .await
            .expect("send should succeed");

        let msg = timeout(Duration::from_secs(10), dual.recv())
            .await
            .expect("recv timed out")
            .expect("dual closed");
        assert_eq!(msg.host, "me");
        assert!(matches!(msg.body, Msg::Hey { hey: _ }));
    }
}
