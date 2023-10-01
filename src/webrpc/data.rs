use super::ice;
use crate::error::WebrpcResult;
use crate::signaling;
use std::sync::Arc;
use tokio::sync::mpsc;
use webrtc_data::data_channel::Config;
use webrtc_data::data_channel::DataChannel;
use webrtc_data::message::message_channel_open::ChannelType;
use webrtc_ice::state::ConnectionState;
use webrtc_sctp::association;

// RFC: https://datatracker.ietf.org/doc/html/rfc8831

/// Bind to `id` on the signaling server at `signaling_addr`.
/// `signaling_addr` should be a valid websocket url.
pub async fn data_bind(
    id: signaling::EndpointId,
    signaling_addr: &str,
) -> WebrpcResult<signaling::client::Listener> {
    ice::ice_bind(id, signaling_addr).await
}

/// Accept connections from `sock`. If `progress_tx` isn't None,
/// report progress to it while establishing connection.
pub async fn data_accept(
    sock: signaling::client::Socket,
    progress_tx: Option<mpsc::Sender<ConnectionState>>,
) -> WebrpcResult<DataChannel> {
    let conn = ice::ice_accept(sock, progress_tx).await?;

    let mut config = Config::default();
    config.channel_type = ChannelType::Reliable;
    // The negotiated field isn't used in `dial`. So the value here
    // doesn't really matter.
    config.negotiated = false;

    let assoc_config = association::Config {
        net_conn: conn,
        max_receive_buffer_size: 16 * 1024,
        max_message_size: 16 * 1024,
        name: "channel".to_string(),
    };
    let assoc = Arc::new(association::Association::server(assoc_config).await?);

    let existing_channel: Vec<DataChannel> = Vec::new();
    let channel = DataChannel::accept(&assoc, config, &existing_channel).await?;
    Ok(channel)
}

/// Connect to the endpoint with `id` on the signaling server at
/// signaling_addr`. `signaling_addr` should be a valid websocket url.
/// If `progress_tx` isn't None, report progress to it while
/// establishing connection.
pub async fn data_connect(
    id: signaling::EndpointId,
    signaling_addr: &str,
    progress_tx: Option<mpsc::Sender<ConnectionState>>,
) -> WebrpcResult<DataChannel> {
    let conn = ice::ice_connect(id, signaling_addr, progress_tx).await?;

    let mut config = Config::default();
    config.channel_type = ChannelType::Reliable;
    // The negotiated field isn't used in `dial`. So the value here
    // doesn't really matter.
    config.negotiated = false;

    let assoc_config = association::Config {
        net_conn: conn,
        max_receive_buffer_size: 16 * 1024,
        max_message_size: 16 * 1024,
        name: "channel".to_string(),
    };
    let assoc = Arc::new(association::Association::client(assoc_config).await?);

    // We only open one stream for the data channel for each SCTP
    // association, so the id doesn't really matter much.
    let channel_id = 1;
    let channel = DataChannel::dial(&assoc, channel_id, config).await?;
    Ok(channel)
}

#[cfg(test)]
mod tests {
    use super::{data_accept, data_bind, data_connect};
    use crate::signaling::server::run_signaling_server;

    #[test]
    fn data_channel_test() -> anyhow::Result<()> {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _ = runtime.spawn(run_signaling_server("127.0.0.1:9000"));

        let handle = runtime.spawn(async {
            let mut listener = data_bind("1".to_string(), "ws://127.0.0.1:9000")
                .await
                .unwrap();
            let sock = listener.accept().await.unwrap();
            let channel = data_accept(sock, None).await.unwrap();
            channel.write(&bytes::Bytes::from("s1")).await.unwrap();
            let mut buf = [0u8; 150];
            let len = channel.read(&mut buf).await.unwrap();
            let msg = std::str::from_utf8(&buf[..len]).unwrap();
            println!("Server reads: {}", msg);
            assert!(msg == "c1");
        });

        runtime.block_on(async {
            let channel = data_connect("1".to_string(), "ws://127.0.0.1:9000", None)
                .await
                .unwrap();
            channel.write(&bytes::Bytes::from("c1")).await.unwrap();
            let mut buf = [0u8; 150];
            let len = channel.read(&mut buf).await.unwrap();
            let msg = std::str::from_utf8(&buf[..len]).unwrap();
            println!("Client reads: {}", msg);
            assert!(msg == "s1");
        });

        runtime.block_on(async {
            handle.await.unwrap();
        });
        Ok(())
    }
}
