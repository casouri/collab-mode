//! Per-peer actor for SSH transport. Owns the ssh-specific setup
//! (spawning the remote process via openssh) and then delegates the
//! byte-stream actor work to [`super::IoRemote`].
//!
//! Lifecycle: `SshRemote::run` is itself the long-lived task. It opens
//! the ssh `Session`, spawns the remote command, wraps the child’s
//! stdio in a [`ReaderWriter`], and hands that to an `IoRemote`. The
//! `RemoteChild` and `Session` live inside an [`SshGuard`] whose `Drop`
//! kills the child and detaches a task that closes the session, so
//! teardown happens uniformly on success, error, and future
//! cancellation.

use super::{Command, IoRemote, Message, ReaderWriter};
use crate::cancel::CancelManager;
use crate::message::Msg;
use crate::types::{id_short, ServerId};
use openssh::Session;
use tokio::sync::mpsc;

/// Drop guard for the ssh session object, so that the drop logic is
/// ran async and doesn’t block tokio.
struct SshSessionDropGuard {
    session: Option<Session>,
    ssh_host: String,
    cancel: CancelManager,
}

impl Drop for SshSessionDropGuard {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            let ssh_host = self.ssh_host.clone();
            self.cancel.spawn_uncancelled(async move {
                if let Err(err) = session.close().await {
                    tracing::warn!("openssh session close failed for {ssh_host}: {err}");
                    // TODO: Send the error to editor as well.
                }
            });
        }
    }
}

pub(super) struct SshRemote {
    peer_id: ServerId,
    ssh_host: String,
    command: Vec<String>,
    remote_msg_tx: mpsc::Sender<Message>,
    rx: mpsc::Receiver<Command>,
    connection_id: u128,
    cancel: CancelManager,
}

impl SshRemote {
    pub(super) fn new(
        peer_id: ServerId,
        ssh_host: String,
        command: Vec<String>,
        remote_msg_tx: mpsc::Sender<Message>,
        rx: mpsc::Receiver<Command>,
        connection_id: u128,
        cancel: CancelManager,
    ) -> Self {
        Self {
            peer_id,
            ssh_host,
            command,
            remote_msg_tx,
            rx,
            connection_id,
            cancel,
        }
    }

    /// Connect ssh, spawn the remote command, run an `IoRemote` over
    /// its stdio. All ssh session teardown happens in
    /// [`SshSessionGuard::drop`]; the `RemoteChild`'s own `Drop` kills
    /// the remote process. Both fire on success, error, and future
    /// cancellation.
    pub(super) async fn run(self) {
        let SshRemote {
            peer_id,
            ssh_host,
            command,
            remote_msg_tx,
            rx,
            connection_id,
            cancel,
        } = self;

        if command.is_empty() {
            tracing::warn!("SshRemote for {} got empty command", id_short(&peer_id));
            send_connection_broke(&remote_msg_tx, &peer_id, connection_id).await;
            return;
        }

        // Open ssh session, wrap in guard immediately so any later
        // early-return path closes the session via Drop.
        let session = match Session::connect(&ssh_host, openssh::KnownHosts::Strict).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "openssh connect to {ssh_host} for {} failed: {e}",
                    id_short(&peer_id)
                );
                send_connection_broke(&remote_msg_tx, &peer_id, connection_id).await;
                return;
            }
        };
        let session_guard = SshSessionDropGuard {
            session: Some(session),
            ssh_host: ssh_host.clone(),
            cancel: cancel.clone(),
        };
        // RemoteChild borrows from Session, so we access via &session.
        let session_ref = session_guard.session.as_ref().unwrap();

        // Spawn the remote command.
        let mut cmd = session_ref.command(&command[0]);
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
                send_connection_broke(&remote_msg_tx, &peer_id, connection_id).await;
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
                send_connection_broke(&remote_msg_tx, &peer_id, connection_id).await;
                return;
            }
        };

        let rw = ReaderWriter::new(stdout, stdin, ());
        IoRemote::new(peer_id, rw, remote_msg_tx, rx, connection_id)
            .run()
            .await;
        // child and session guard are dropped and their Drop runs,
        // child first, session guard second.
    }
}

async fn send_connection_broke(
    tx: &mpsc::Sender<Message>,
    peer_id: &ServerId,
    connection_id: u128,
) {
    let _ = tx
        .send(Message {
            host: peer_id.clone(),
            body: Msg::ConnectionBroke {
                peer: peer_id.clone(),
                connection_id,
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
            crate::cancel::CancelManager::new(),
        );

        chan.connect(
            "peer".into(),
            Transport::Ssh {
                ssh_host: "yuan@localhost".into(),
                command: vec!["cat".into()],
            },
            key_cert(),
            1,
            crate::cancel::CancelManager::new(),
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
