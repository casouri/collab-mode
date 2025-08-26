// The JSONRPC interface with editor. This module provides two
// functions that that starts an actor on either a pipe or a socket.
// And the actor simply moves JSONRPC messages around.

use lsp_server::{Connection, Message};
use tokio::sync::mpsc;
use tracing::trace;

/// Run the editor receptor on stdio. Returns when fatal error occurs.
pub fn run_stdio(msg_tx: mpsc::Sender<Message>, msg_rx: mpsc::Receiver<Message>) {
    tracing::info!("Started listening for messages from editor");
    // We don't need to join the io_threads, as the main_loop will get
    // io errors anyway. Blocks until a connection is made.
    let (connection, _io_threads) = Connection::stdio();
    main_loop(connection, msg_tx, msg_rx);
}

/// Run the editor receptor on socket. Returns when fatal error occurs.
pub fn run_socket(
    addr: &str,
    msg_tx: mpsc::Sender<Message>,
    msg_rx: mpsc::Receiver<Message>,
) -> anyhow::Result<()> {
    tracing::info!("Started listening for messages from editor on {}", &addr);
    // We don't need to join the io_threads, as the main_loop will get
    // io errors anyway. Blocks until a connection is made.
    let (connection, _io_threads) = Connection::listen(addr)?;
    main_loop(connection, msg_tx, msg_rx);
    Ok(())
}

// Starts sync threads carring messages between the editor and the
// collab peer, blocks on errors. Return when fatal error occurs.
fn main_loop(
    connection: Connection,
    msg_tx: mpsc::Sender<Message>,
    mut msg_rx: mpsc::Receiver<Message>,
) {
    let (err_tx, mut err_rx) = mpsc::channel::<String>(1);

    let connection = std::sync::Arc::new(connection);
    // Starts two threads, one read from `connection` and send to
    // `msg_tx`, the other reads from `msg_rx` and sends to
    // `connection`.
    let connection_1 = connection.clone();
    let err_tx_1 = err_tx.clone();
    let msg_tx_1 = msg_tx.clone();

    // Read from editor and send to msg_tx.
    std::thread::spawn(move || loop {
        let msg = connection.receiver.recv();
        if msg.is_err() {
            let err = msg.err().unwrap();
            let _ =
                err_tx.blocking_send(format!("Failed to receive message from editor: {:#?}", err));
            break;
        }
        let msg = msg.unwrap();
        trace!("Received message from editor: {:?}", msg);

        if let Message::Request(req) = &msg {
            if req.method == "Initialize" {
                let res =
                    connection
                        .sender
                        .send(lsp_server::Message::Response(lsp_server::Response {
                            id: req.id.clone(),
                            result: Some(serde_json::json!({})),
                            error: None,
                        }));
                if let Err(err) = res {
                    tracing::error!("Failed to send Initialize response to editor: {:#?}", err);
                    break;
                }
                continue;
            }
        }

        let res = msg_tx.blocking_send(msg);
        if let Err(err) = res {
            let _ = err_tx.blocking_send(format!(
                "Failed to send editor message to collab peer: {:#?}",
                err
            ));
            break;
        }
    });

    // Read from msg_rx and send to editor.
    std::thread::spawn(move || loop {
        let msg = msg_rx.blocking_recv();
        if msg.is_none() {
            let _ = err_tx_1.blocking_send("Message rx channel from peer closed".to_string());
            break;
        }
        let msg = msg.unwrap();
        trace!("Received message from collab peer: {:?}", msg);
        if let Err(err) = connection_1.sender.send(msg) {
            let _ = err_tx_1.blocking_send(format!("Failed to send message to editor: {:#?}", err));
            break;
        }
    });

    // Block on error channel. If received an fatal error, exit program.
    // All errors sent to err_rx are fatal errors.
    let err = err_rx.blocking_recv();
    if let Some(err) = err {
        tracing::error!("Collab peer terminated due to error: {:#?}", err);
    }
}
