use crate::error::{WebrpcError, WebrpcResult};
use crate::signaling::{
    client::{Listener, Socket as SignalSocket},
    EndpointId, SignalingError,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::watch;
use webrtc_ice::agent::agent_config::AgentConfig;
use webrtc_ice::agent::Agent;
use webrtc_ice::candidate::candidate_base::unmarshal_candidate;
use webrtc_ice::candidate::Candidate;
use webrtc_ice::network_type::NetworkType;
use webrtc_ice::state::ConnectionState;
use webrtc_ice::udp_network::{EphemeralUDP, UDPNetwork};
use webrtc_ice::url::Url;
use webrtc_util::Conn;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ICECredential {
    ufrag: String,
    pwd: String,
}

/// Bind to `id` on the signaling server at `signaling_addr`.
/// `signaling_addr` should be a valid websocket url.
pub async fn ice_bind(id: EndpointId, signaling_addr: &str) -> WebrpcResult<Listener> {
    let mut listener = Listener::new(signaling_addr, id).await?;
    listener.bind().await?;
    Ok(listener)
}

/// Accept connections from `sock`. If `progress_tx` isn't None,
/// report progress to it while establishing connection.
pub async fn ice_accept(
    mut sock: SignalSocket,
    progress_tx: Option<mpsc::Sender<ConnectionState>>,
) -> WebrpcResult<Arc<impl Conn + Send + Sync>> {
    let (error_tx, mut error_rx) = mpsc::channel(1);
    let (cancel_tx, cancel_rx) = mpsc::channel(1);
    let (connected_tx, connected_rx) = watch::channel(());

    let (ufrag, pwd) = get_ice_credential(&sock)?;
    let agent = Arc::new(make_ice_agent(false).await?);
    send_ice_credential(&mut sock, &agent).await?;

    ice_monitor_progress(agent.clone(), progress_tx, error_tx.clone());
    ice_exchange_candidates(agent.clone(), sock, error_tx, connected_rx)?;

    tokio::select! {
        err = error_rx.recv() => {
            drop(connected_tx);
            drop(cancel_tx);
            // We hold error_tx, this should never panic.
            Err(err.unwrap().into())
        }
        conn = agent.accept(cancel_rx, ufrag, pwd) => {
            drop(connected_tx);
            let conn = conn?;
            Ok(conn)
        }
    }
}

/// Connect to the endpoint with `id` on the signaling server at
/// signaling_addr`. `signaling_addr` should be a valid websocket url.
/// If `progress_tx` isn't None, report progress to it while
/// establishing connection.
pub async fn ice_connect(
    id: EndpointId,
    signaling_addr: &str,
    progress_tx: Option<mpsc::Sender<ConnectionState>>,
) -> WebrpcResult<Arc<impl Conn + Send + Sync>> {
    let (error_tx, mut error_rx) = mpsc::channel(1);
    let (cancel_tx, cancel_rx) = mpsc::channel(1);
    let (connected_tx, connected_rx) = watch::channel(());

    let my_id = uuid::Uuid::new_v4().to_string();
    let mut listener = Listener::new(signaling_addr, my_id).await?;
    let agent = Arc::new(make_ice_agent(true).await?);
    let cred = ice_credential(&agent).await;
    let sock = listener
        .connect(id, serde_json::to_string(&cred).unwrap())
        .await?;
    let (ufrag, pwd) = get_ice_credential(&sock)?;

    ice_monitor_progress(agent.clone(), progress_tx, error_tx.clone());
    ice_exchange_candidates(agent.clone(), sock, error_tx.clone(), connected_rx)?;

    tokio::select! {
        err = error_rx.recv() => {
            drop(connected_tx);
            drop(cancel_tx);
            // We hold error_tx, this should never panic.
            Err(err.unwrap().into())
        }
        conn = agent.dial(cancel_rx, ufrag, pwd) => {
            drop(connected_tx);
            let conn = conn?;
            Ok(conn)
        }
    }
}

/// Monitor the handshake progress. If connection failed, send an error
/// to `error_tx`. If `progress_tx` is non-none, send ConnectionState
/// to it at each step.
fn ice_monitor_progress(
    agent: Arc<Agent>,
    progress_tx: Option<mpsc::Sender<ConnectionState>>,
    error_tx: mpsc::Sender<WebrpcError>,
) {
    agent.on_connection_state_change(Box::new(move |state| {
        log::debug!("ICE state changed: {}", &state);
        if let Some(tx) = &progress_tx {
            let _ = tx.try_send(state);
        }
        if state == ConnectionState::Failed {
            let _ = error_tx.try_send(WebrpcError::ICEError(format!("Connection failed")));
        }
        Box::pin(async move {})
    }));
}

/// Get the ufrag and pwd from `sock`.
fn get_ice_credential(sock: &SignalSocket) -> WebrpcResult<(String, String)> {
    let remote_cred: ICECredential = serde_json::from_str(&sock.sdp())
        .map_err(|err| WebrpcError::ParseError(err.to_string()))?;
    Ok((remote_cred.ufrag, remote_cred.pwd))
}

async fn make_ice_agent(controlling: bool) -> WebrpcResult<Agent> {
    let mut config = AgentConfig::default();
    config.is_controlling = controlling;
    config.network_types = vec![NetworkType::Udp4];
    config.udp_network = UDPNetwork::Ephemeral(EphemeralUDP::default());
    config.urls = vec![
        Url::parse_url("stun:stun1.l.google.com:19302").unwrap(),
        Url::parse_url("stun:stun2.l.google.com:19302").unwrap(),
        Url::parse_url("stun:stun3.l.google.com:19302").unwrap(),
        Url::parse_url("stun:stun4.l.google.com:19302").unwrap(),
        Url::parse_url("stun:stun.nextcloud.com:443").unwrap(),
        Url::parse_url("stun:stun.mit.de:3478").unwrap(),
    ];
    Ok(Agent::new(config).await?)
}

/// Send ICE credential of `agent` to `sock`.
async fn send_ice_credential(sock: &mut SignalSocket, agent: &Agent) -> WebrpcResult<()> {
    let local_cred = ice_credential(agent).await;
    sock.send_sdp(serde_json::to_string(&local_cred).unwrap())
        .await?;
    Ok(())
}

/// Get local credential of `agent`
async fn ice_credential(agent: &Agent) -> ICECredential {
    let (ufrag, pwd) = agent.get_local_user_credentials().await;
    ICECredential { ufrag, pwd }
}

/// Start gathering candidates and exchange candidates with remote
/// endpoint. If `connected_rx` is closed, don't send any candidates
/// out. Errors are sent to `err_tx`.
fn ice_exchange_candidates(
    agent: Arc<Agent>,
    mut sock: SignalSocket,
    error_tx: mpsc::Sender<WebrpcError>,
    connected_rx: watch::Receiver<()>,
) -> WebrpcResult<()> {
    // Send candidate.
    let candidate_sender = sock.candidate_sender();
    agent.on_candidate(Box::new(move |candidate| {
        if connected_rx.has_changed().is_err() {
            return Box::pin(async {});
        }
        if let Some(candidate) = candidate {
            let candidate = candidate.marshal();
            let candidate_sender = candidate_sender.clone();
            log::debug!("Found candidate: {}", &candidate);
            tokio::spawn(async move {
                let _todo = candidate_sender.send_candidate(candidate).await;
            });
        }
        Box::pin(async {})
    }));

    // Receive candidate.
    let agent_1 = agent.clone();
    tokio::spawn(async move {
        while let Some(candidate) = sock.recv_candidate().await {
            let func = || {
                log::debug!("Received candidate: {}", &candidate);
                let candidate = unmarshal_candidate(&candidate)?;
                let candidate: Arc<dyn Candidate + Send + Sync> = Arc::new(candidate);
                agent_1.add_remote_candidate(&candidate)?;
                Ok::<(), WebrpcError>(())
            };
            if let Err(err) = func() {
                error_tx.send(err).await.unwrap();
            }
        }
    });

    // Gather candidate.
    agent.gather_candidates()?;
    Ok(())
}

mod tests {
    use super::{ice_accept, ice_connect};
    use crate::signaling::{client::Listener, server::run_signaling_server};
    use std::sync::Arc;
    use webrtc_util::Conn;

    async fn test_server(id: String) -> anyhow::Result<()> {
        // Connect to the signaling server.
        let mut listener = Listener::new("ws://127.0.0.1:9000", id).await?;
        listener.bind().await?;
        let sock = listener.accept().await?;

        let conn = ice_accept(sock, None).await?;
        let conn_tx = Arc::clone(&conn);

        // Send and receive message.
        tokio::spawn(async move {
            for i in 0..5 {
                let res = conn_tx.send(format!("server msg {i}").as_bytes()).await;
                if let Err(err) = res {
                    panic!("Error sending message {:?}", err);
                }
            }
        });
        let mut count = 0;
        let mut buf = vec![0u8; 1500];
        while let Ok(len) = conn.recv(&mut buf[..]).await {
            let msg = std::str::from_utf8(&buf[..len]).unwrap();
            println!("Server received msg: {:?}", msg);
            assert!(msg == format!("client msg {}", count));
            count += 1;
            if count == 4 {
                break;
            }
        }
        Ok(())
    }

    async fn test_client(server_id: String) -> anyhow::Result<()> {
        let conn = ice_connect(server_id, "ws://127.0.0.1:9000", None).await?;
        let conn_tx = Arc::clone(&conn);

        // Send and receive message.
        tokio::spawn(async move {
            for i in 0..5 {
                let res = conn_tx.send(format!("client msg {i}").as_bytes()).await;
                if let Err(err) = res {
                    panic!("Error sending message {:?}", err);
                }
            }
        });
        let mut buf = vec![0u8; 1500];
        let mut count = 0;
        while let Ok(len) = conn.recv(&mut buf[..]).await {
            let msg = std::str::from_utf8(&buf[..len]).unwrap();
            println!("Client received msg: {:?}", msg);
            assert!(msg == format!("server msg {}", count));
            count += 1;
            if count == 4 {
                break;
            }
        }
        Ok(())
    }

    #[test]
    fn webrtc_test() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _ = runtime.spawn(run_signaling_server("127.0.0.1:9000"));

        let handle = runtime.spawn(async {
            let res = test_server("1".to_string()).await;
            println!("Server: {:?}", res);
        });
        let _ =
            runtime.block_on(async { tokio::time::sleep(std::time::Duration::from_secs(1)).await });
        let _ = runtime.block_on(async {
            let res = test_client("1".to_string()).await;
            println!("Client: {:?}", res);
        });

        let _ = runtime.block_on(async { handle.await });
    }
}
