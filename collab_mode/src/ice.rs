use crate::error::{WebrpcError, WebrpcResult};
use crate::signaling::CertDerHash;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::instrument;
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
pub struct ICECredential {
    pub ufrag: String,
    pub pwd: String,
}

/// Connect using an existing Sock from SignalingClient.
///
/// This is the unified connection method that replaces separate
/// ice_connect and ice_accept paths. Both sides use the same process.
/// SDP exchange happens via SignalingMessage::SDP messages through the sock.
/// If `progress_tx` isn't None, report progress to it while establishing connection.
/// The `is_server` parameter determines ICE agent role (must differ between peers).
#[instrument(skip(progress_tx, sock))]
pub async fn ice_connect_with_sock(
    mut sock: crate::signaling::client_new::Sock,
    progress_tx: Option<mpsc::Sender<ConnectionState>>,
    is_server: bool,
) -> WebrpcResult<(
    (Arc<dyn Conn + Send + Sync>, CertDerHash, Arc<Agent>),
    oneshot::Receiver<()>,
)> {
    let (error_tx, mut error_rx) = mpsc::channel(1);
    let (cancel_tx, cancel_rx) = mpsc::channel(1);
    let (connected_tx, connected_rx) = watch::channel(());
    let (conn_broke_tx, conn_broke_rx) = oneshot::channel();

    let agent = Arc::new(make_ice_agent(is_server).await?);

    // Generate and send our SDP to peer
    let (ufrag, pwd) = agent.get_local_user_credentials().await;
    let my_sdp = serde_json::to_string(&ICECredential { ufrag, pwd }).map_err(|e| {
        WebrpcError::ParseError(format!("Failed to serialize ICE credential: {}", e))
    })?;
    sock.send_sdp(my_sdp)
        .await
        .map_err(|e| WebrpcError::SignalingError(e.to_string()))?;

    // Receive peer's SDP
    let peer_sdp = sock
        .recv_sdp()
        .await
        .map_err(|e| WebrpcError::SignalingError(e.to_string()))?;
    let (ufrag, pwd) = get_ice_credential_from_sdp(&peer_sdp)?;
    let their_cert = sock.cert_hash().to_string();

    ice_monitor_progress(agent.clone(), progress_tx, error_tx.clone(), conn_broke_tx);
    let candidate_task =
        ice_exchange_candidates_with_sock(agent.clone(), sock, error_tx.clone(), connected_rx)?;

    // Controlling side dials, controlled side accepts
    let result = if is_server {
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
                let conn: Arc<dyn Conn + Send + Sync> = conn;
                Ok(((conn, their_cert, agent), conn_broke_rx))
            }
        }
    } else {
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
                let conn: Arc<dyn Conn + Send + Sync> = conn;
                Ok(((conn, their_cert, agent), conn_broke_rx))
            }
        }
    };

    // Terminate candidate_task.
    candidate_task.abort();
    let _ = candidate_task.await;

    result
}

/// Helper to parse ICE credentials from SDP string.
fn get_ice_credential_from_sdp(sdp: &str) -> WebrpcResult<(String, String)> {
    let cred: ICECredential = serde_json::from_str(sdp)
        .map_err(|e| WebrpcError::ParseError(format!("Failed to parse ICE credential: {}", e)))?;
    Ok((cred.ufrag, cred.pwd))
}

/// Exchange ICE candidates with peer using Sock.
fn ice_exchange_candidates_with_sock(
    agent: Arc<Agent>,
    mut sock: crate::signaling::client_new::Sock,
    error_tx: mpsc::Sender<WebrpcError>,
    connected_rx: watch::Receiver<()>,
) -> WebrpcResult<JoinHandle<()>> {
    // Send candidates using the on_candidate callback
    let candidate_sender = sock.candidate_sender();
    agent.on_candidate(Box::new(move |candidate| {
        if connected_rx.has_changed().is_err() {
            return Box::pin(async {});
        }
        if let Some(candidate) = candidate {
            let candidate = candidate.marshal();
            let mut candidate_sender = candidate_sender.clone();

            tracing::info!(candidate, "Found ICE candidate");

            tokio::spawn(async move {
                let res = candidate_sender.send_candidate(candidate).await;
                if let Err(err) = res {
                    tracing::error!(?err, "Error sending ICE candidate");
                }
            });
        }
        Box::pin(async {})
    }));

    // Receive candidates
    let agent_1 = agent.clone();
    let candidate_task = tokio::spawn(async move {
        while let Some(candidate) = sock.recv_candidate().await {
            let func = || {
                tracing::info!(?candidate, "Received ICE candidate");

                let candidate = unmarshal_candidate(&candidate)?;
                let candidate: Arc<dyn Candidate + Send + Sync> = Arc::new(candidate);
                agent_1.add_remote_candidate(&candidate)?;
                Ok::<(), WebrpcError>(())
            };
            if let Err(err) = func() {
                // If we can't send error, the connection is likely established
                let _ = error_tx.send(err).await;
                break;
            }
        }
        tracing::info!("Candidate receiver task ending");
    });

    // Gather candidates
    agent.gather_candidates()?;
    Ok(candidate_task)
}

/// Monitor the handshake progress. If connection failed, send an error
/// to `error_tx`. If `progress_tx` is non-none, send ConnectionState
/// to it at each step.
#[instrument(skip_all)]
fn ice_monitor_progress(
    agent: Arc<Agent>,
    progress_tx: Option<mpsc::Sender<ConnectionState>>,
    error_tx: mpsc::Sender<WebrpcError>,
    conn_broke_tx: oneshot::Sender<()>,
) {
    let mut conn_broke_tx = Some(conn_broke_tx);
    agent.on_connection_state_change(Box::new(move |state| {
        tracing::debug!(?state, "ICE state changed");
        if let Some(tx) = &progress_tx {
            let _ = tx.try_send(state);
        }

        // Check for terminal states and close the agent
        if state == ConnectionState::Failed
            // Technically disconnected state is recoverable with
            // ice_restart, but to keep things simple, we just
            // reconnect from scratch.
            || state == ConnectionState::Disconnected
            || state == ConnectionState::Closed
        {
            let description = match state {
                ConnectionState::Failed => "failed",
                ConnectionState::Disconnected => "broke",
                ConnectionState::Closed => "closed",
                _ => "...",
            };

            let _ = error_tx.try_send(WebrpcError::ICEError(format!("Connection {}", description)));

            // Signal connection breakage. Webchannel will close the agent.
            drop(conn_broke_tx.take());

            Box::pin(async move {})
        } else {
            Box::pin(async move {})
        }
    }));
}

/// Create an ICE agent for WebRTC connection.
pub async fn make_ice_agent(controlling: bool) -> WebrpcResult<Agent> {
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
        Url::parse_url("stun:stun.relay.metered.ca:80").unwrap(),
    ];
    // let turn_urls = vec![
    //     Url::parse_url("turn:a.relay.metered.ca:80").unwrap(),
    //     Url::parse_url("turn:a.relay.metered.ca:80?transport=tcp").unwrap(),
    //     Url::parse_url("turn:a.relay.metered.ca:443").unwrap(),
    //     Url::parse_url("turn:a.relay.metered.ca:443?transport=tcp").unwrap(),
    // ]
    // .into_iter()
    // .map(|mut url| {
    //     url.username = "8876b56b638342bdae08c95b".to_string();
    //     url.password = "fzCwuGJBbeSB8bfw".to_string();
    //     url
    // });
    // config.urls.extend(turn_urls);

    Ok(Agent::new(config).await?)
}
