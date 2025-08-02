use collab_mode::websocket_receptor::{self, Sock};

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let runtime = tokio::runtime::Runtime::new().unwrap();

    runtime.block_on(async {
        let sock_rx = websocket_receptor::accept(&format!("127.0.0.1:7777")).await;
        if let Err(err) = sock_rx {
            tracing::error!("Error accepting connections: {}", err);
            return;
        }
        let mut sock_rx = sock_rx.unwrap();

        loop {
            let sock = sock_rx.recv().await;
            if sock.is_none() {
                tracing::error!("Error accepting connections");
                break;
            }
            let sock = sock.unwrap();
            tracing::info!("Accepted connection from");
            let _ = tokio::spawn(async move {
                let Sock { msg_tx, mut msg_rx } = sock;
                loop {
                    let msg = msg_rx.recv().await;
                    if msg.is_none() {
                        tracing::warn!("Connection with peer broke");
                        break;
                    }
                    let msg = msg.unwrap();
                    tracing::info!("Received message: {:?}", msg);

                    // Echo back.
                    if let Err(err) = msg_tx.send(msg).await {
                        tracing::warn!("Connection with peer broke: {}", err);
                        break;
                    }
                }
            });
        }
    });

    Ok(())
}
