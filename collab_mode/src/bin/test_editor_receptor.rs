fn main() {
    tracing_subscriber::fmt::init();

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (to_peer_tx, mut to_peer_rx) = tokio::sync::mpsc::channel::<lsp_server::Message>(16);
    let (_from_peer_tx, from_peer_rx) = tokio::sync::mpsc::channel::<lsp_server::Message>(16);
    runtime.spawn(async move {
        tokio::select! {
            income_msg = to_peer_rx.recv() => {
                if let Some(msg) = income_msg {
                    tracing::info!("Received message from editor: {:#?}", msg);
                }
            }
        }
    });
    let res = collab_mode::editor_receptor::run_socket(
        "localhost:7777",
        to_peer_tx,
        from_peer_rx,
    );
    if let Err(err) = res {
        tracing::error!("Failed to start collab peer: {:#?}", err);
        std::process::exit(1);
    }
}
