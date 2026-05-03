use super::*;
use crate::config_man::create_key_cert;
use crate::message::{HeyMessage, Msg};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

fn hey_msg() -> Msg {
    Msg::Hey(HeyMessage {
        message: "hi".into(),
        credentials: "".into(),
        version: "v1.0.0".into(),
    })
}

fn is_hey(msg: &Msg) -> bool {
    matches!(msg, Msg::Hey(_))
}

async fn recv_with_timeout(rx: &mut mpsc::Receiver<Message>) -> Option<Message> {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .ok()
        .flatten()
}

/// Real ssh path. Requires `ssh yuan@localhost` to work without prompts.
/// Run with: `cargo test ssh_channel openssh_localhost -- --ignored`.
///
/// Uses plain `cat` as the remote command so the framed Hey we send is
/// echoed back through the inbound channel — no envoy involved.
#[tokio::test]
#[ignore]
async fn openssh_localhost_roundtrip() {
    let (remote_tx, mut rx) = mpsc::channel::<Message>(16);
    let (loopback_tx, _) = mpsc::unbounded_channel::<Message>();

    let channel = SshMsgChannel::new(
        "me".into(),
        remote_tx,
        loopback_tx,
        vec!["cat".into()],
    );
    let key_cert = Arc::new(create_key_cert("me-test"));

    channel
        .connect(
            "peer".into(),
            Transport::Ssh {
                ssh_host: "yuan@localhost".into(),
            },
            key_cert,
        )
        .await
        .expect("openssh connect to yuan@localhost should succeed");

    channel
        .send(&"peer".into(), None, hey_msg())
        .await
        .expect("send should succeed");

    let msg = recv_with_timeout(&mut rx)
        .await
        .expect("expected echoed Hey from remote cat");
    assert_eq!(msg.host, "me");
    assert!(is_hey(&msg.body));

    channel.disconnect(&"peer".into());
}
