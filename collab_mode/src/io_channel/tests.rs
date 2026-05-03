use super::*;
use crate::message::{HeyMessage, Msg};
use crate::webchannel::Message;
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

fn is_connection_broke(msg: &Msg) -> bool {
    matches!(msg, Msg::ConnectionBroke(_))
}

/// Build a connected pair of `IoMsgChannel`s using a bidirectional duplex.
/// Returns `(channel_a, rx_a, channel_b, rx_b)` where `rx_*` receive
/// remote messages addressed to that side.
fn pair() -> (
    IoMsgChannel,
    mpsc::Receiver<Message>,
    IoMsgChannel,
    mpsc::Receiver<Message>,
) {
    let (a, b) = tokio::io::duplex(8192);
    let rw_a = ReaderWriter::from_duplex(a);
    let rw_b = ReaderWriter::from_duplex(b);

    let (tx_a, rx_a) = mpsc::channel::<Message>(16);
    let (tx_b, rx_b) = mpsc::channel::<Message>(16);
    // Loopback receivers are dropped here. None of the tests using `pair()`
    // exercise the loopback path, so the senders are never used.
    let (loopback_a, _) = mpsc::unbounded_channel::<Message>();
    let (loopback_b, _) = mpsc::unbounded_channel::<Message>();

    let chan_a = IoMsgChannel::new("a".into(), "b".into(), rw_a, tx_a, loopback_a);
    let chan_b = IoMsgChannel::new("b".into(), "a".into(), rw_b, tx_b, loopback_b);
    (chan_a, rx_a, chan_b, rx_b)
}

async fn recv_with_timeout(rx: &mut mpsc::Receiver<Message>) -> Option<Message> {
    tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .ok()
        .flatten()
}

#[tokio::test]
async fn roundtrip() {
    let (chan_a, _rx_a, _chan_b, mut rx_b) = pair();

    chan_a
        .send(&"b".into(), None, hey_msg())
        .await
        .expect("send should succeed");

    let msg = recv_with_timeout(&mut rx_b).await.expect("expected msg");
    assert_eq!(msg.host, "a", "host field should be the sender's id");
    assert!(is_hey(&msg.body), "body should be Hey, got {:?}", msg.body);
}

#[tokio::test]
async fn disconnect_terminates_tasks() {
    let (chan_a, _rx_a, _chan_b, mut rx_b) = pair();

    // Sanity: send works before disconnect.
    chan_a.send(&"b".into(), None, hey_msg()).await.unwrap();
    let _ = recv_with_timeout(&mut rx_b)
        .await
        .expect("first msg arrives");

    chan_a.disconnect(&"b".into());

    // After disconnect, send should fail (out_tx is gone).
    let result = chan_a.send(&"b".into(), None, hey_msg()).await;
    assert!(
        result.is_err(),
        "send after disconnect should fail, got {:?}",
        result
    );

    // The other side observes EOF and emits ConnectionBroke.
    let msg = recv_with_timeout(&mut rx_b)
        .await
        .expect("expected ConnectionBroke after disconnect");
    assert!(
        is_connection_broke(&msg.body),
        "expected ConnectionBroke, got {:?}",
        msg.body
    );
}

#[tokio::test]
async fn eof_emits_connection_broke() {
    let (chan_a, mut rx_a, chan_b, _rx_b) = pair();

    // Drop B → B's writer closes → A's inbound reader hits EOF → A emits
    // ConnectionBroke onto rx_a.
    drop(chan_b);

    let msg = recv_with_timeout(&mut rx_a)
        .await
        .expect("A should observe ConnectionBroke after B closes");
    assert!(
        is_connection_broke(&msg.body),
        "expected ConnectionBroke, got {:?}",
        msg.body
    );
    assert_eq!(msg.host, "b");

    let _ = chan_a;
}

#[tokio::test]
async fn loopback_short_circuits() {
    // Single channel; send to own host id should land on loopback_tx, not
    // on the wire.
    let (a, _b) = tokio::io::duplex(8192);
    let rw = ReaderWriter::from_duplex(a);

    let (remote_tx, mut remote_rx) = mpsc::channel::<Message>(16);
    let (loopback_tx, mut loopback_rx) = mpsc::unbounded_channel::<Message>();

    let chan = IoMsgChannel::new("self".into(), "peer".into(), rw, remote_tx, loopback_tx);

    chan.send(&"self".into(), None, hey_msg()).await.unwrap();

    // Should land on loopback, not remote.
    let lb = loopback_rx
        .try_recv()
        .expect("loopback should have a message");
    assert!(is_hey(&lb.body));
    assert_eq!(lb.host, "self");

    // remote_rx should not have anything.
    assert!(remote_rx.try_recv().is_err());
}
