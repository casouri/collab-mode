use crate::error::{WebrpcError, WebrpcResult};
use crate::signaling::CertDerHash;
use crate::signaling::{
    client::{Listener, Socket as SignalSocket},
    EndpointId,
};
use crate::types::ArcKeyCert;
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
struct ICECredential {
    ufrag: String,
    pwd: String,
}

/// Bind to `id` with `key` on the signaling server at
/// `signaling_addr`. `signaling_addr` should be a valid websocket
/// url.
pub async fn ice_bind(
    id: EndpointId,
    my_key_cert: ArcKeyCert,
    signaling_addr: &str,
) -> WebrpcResult<Listener> {
    let mut listener = Listener::new(signaling_addr, id, my_key_cert).await?;
    listener.bind().await?;
    Ok(listener)
}

/// Accept connections from `sock`. If `progress_tx` isn’t None,
/// report progress to it while establishing connection.
#[instrument(skip_all)]
pub async fn ice_accept(
    mut sock: SignalSocket,
    progress_tx: Option<mpsc::Sender<ConnectionState>>,
) -> WebrpcResult<(Arc<impl Conn + Send + Sync>, oneshot::Receiver<()>)> {
    let (error_tx, mut error_rx) = mpsc::channel(1);
    let (cancel_tx, cancel_rx) = mpsc::channel(1);
    let (connected_tx, connected_rx) = watch::channel(());
    let (conn_broke_tx, conn_broke_rx) = oneshot::channel();

    let (ufrag, pwd) = get_ice_credential(&sock)?;
    let agent = Arc::new(make_ice_agent(false).await?);
    send_ice_credential(&mut sock, &agent).await?;

    ice_monitor_progress(agent.clone(), progress_tx, error_tx.clone(), conn_broke_tx);
    let candidate_task = ice_exchange_candidates(agent.clone(), sock, error_tx, connected_rx)?;

    let result = tokio::select! {
        err = error_rx.recv() => {
            drop(connected_tx);
            drop(cancel_tx);
            // We hold error_tx, this should never panic.
            Err(err.unwrap().into())
        }
        conn = agent.accept(cancel_rx, ufrag, pwd) => {
            drop(connected_tx);
            let conn = conn?;
            Ok((conn, conn_broke_rx))
        }
    };

    // Terminate candidate_task.
    candidate_task.abort();
    let _ = candidate_task.await;

    result
}

/// Connect to the endpoint with `id` and `key` on the signaling
/// server at signaling_addr`. `signaling_addr` should be a valid
/// websocket url. If `progress_tx` isn’t None, report progress to it
/// while establishing connection.
#[instrument(skip(my_key_cert, progress_tx))]
pub async fn ice_connect(
    id: EndpointId,
    my_key_cert: ArcKeyCert,
    signaling_addr: &str,
    progress_tx: Option<mpsc::Sender<ConnectionState>>,
    my_id: Option<String>,
) -> WebrpcResult<(
    (Arc<impl Conn + Send + Sync>, CertDerHash),
    oneshot::Receiver<()>,
)> {
    let (error_tx, mut error_rx) = mpsc::channel(1);
    let (cancel_tx, cancel_rx) = mpsc::channel(1);
    let (connected_tx, connected_rx) = watch::channel(());
    let (conn_broke_tx, conn_broke_rx) = oneshot::channel();

    let my_id = my_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let mut listener = Listener::new(signaling_addr, my_id, my_key_cert).await?;
    let agent = Arc::new(make_ice_agent(true).await?);
    let cred = ice_credential(&agent).await;
    let sock = listener
        .connect(id, serde_json::to_string(&cred).unwrap())
        .await?;
    let (ufrag, pwd) = get_ice_credential(&sock)?;
    let their_cert = sock.cert_hash();

    ice_monitor_progress(agent.clone(), progress_tx, error_tx.clone(), conn_broke_tx);
    let candidate_task =
        ice_exchange_candidates(agent.clone(), sock, error_tx.clone(), connected_rx)?;

    let result = tokio::select! {
        err = error_rx.recv() => {
            drop(connected_tx);
            drop(cancel_tx);
            // We hold error_tx, this should never panic.
            Err(err.unwrap().into())
        }
        conn = agent.dial(cancel_rx, ufrag, pwd) => {
            drop(connected_tx);
            let conn = conn?;
            Ok(((conn, their_cert), conn_broke_rx))
        }
    };

    // Terminate candidate_task.
    candidate_task.abort();
    let _ = candidate_task.await;

    result
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
    let agent_clone = agent.clone();
    let mut conn_broke_tx = Some(conn_broke_tx);
    agent.on_connection_state_change(Box::new(move |state| {
        tracing::debug!(?state, "ICE state changed");
        if let Some(tx) = &progress_tx {
            let _ = tx.try_send(state);
        }

        // Check for terminal states and close the agent
        if state == ConnectionState::Failed
            || state == ConnectionState::Disconnected
            || state == ConnectionState::Closed
        {
            let agent_to_close = agent_clone.clone();
            let description = match state {
                ConnectionState::Failed => "failed",
                ConnectionState::Disconnected => "broke",
                ConnectionState::Closed => "closed",
                _ => "...",
            };

            let _ = error_tx.try_send(WebrpcError::ICEError(format!("Connection {}", description)));

            // Drop conn_broke_tx to signal connection termination
            // drop(conn_broke_tx.take());

            Box::pin(async move {
                tracing::debug!("Closing ICE agent due to terminal state: {:?}", state);
                if let Err(err) = agent_to_close.close().await {
                    tracing::error!("Error closing ICE agent: {:?}", err);
                }
            })
        } else {
            Box::pin(async move {})
        }
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

/// Send ICE credential of `agent` to `sock`.
async fn send_ice_credential(sock: &mut SignalSocket, agent: &Agent) -> WebrpcResult<()> {
    let local_cred = ice_credential(agent).await;
    tracing::info!(?local_cred, "Sending SDP");
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
/// out. Errors are sent to `err_tx`. Returns a JoinHandle for the
/// candidate receiver task that should be aborted after connection.
#[instrument(skip_all)]
fn ice_exchange_candidates(
    agent: Arc<Agent>,
    mut sock: SignalSocket,
    error_tx: mpsc::Sender<WebrpcError>,
    connected_rx: watch::Receiver<()>,
) -> WebrpcResult<JoinHandle<()>> {
    // Send candidate.
    let candidate_sender = sock.candidate_sender();
    agent.on_candidate(Box::new(move |candidate| {
        if connected_rx.has_changed().is_err() {
            return Box::pin(async {});
        }
        if let Some(candidate) = candidate {
            let candidate = candidate.marshal();
            let candidate_sender = candidate_sender.clone();

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

    // Receive candidate - store the JoinHandle so we can abort it later.
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

    // Gather candidate.
    agent.gather_candidates()?;
    Ok(candidate_task)
}

#[cfg(test)]
mod tests {
    use super::{ice_accept, ice_connect};
    use crate::{
        config_man::create_key_cert,
        signaling::{client::Listener, server::run_signaling_server},
        types::ArcKeyCert,
    };
    use std::{path::Path, sync::Arc};
    use webrtc_util::Conn;

    async fn test_server(id: String, server_key_cert: ArcKeyCert) -> anyhow::Result<()> {
        // Connect to the signaling server.
        let mut listener = Listener::new("ws://127.0.0.1:9000", id, server_key_cert).await?;
        listener.bind().await?;
        let sock = listener.accept().await?;

        let (conn, _conn_broke_rx) = ice_accept(sock, None).await?;
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

    async fn test_client(server_id: String, client_key_cert: ArcKeyCert) -> anyhow::Result<()> {
        // We don't verify server_cert until DTLS connection is
        // established. So no use for the server_cert at this layer.
        let ((conn, _server_cert), _conn_broke_rx) = ice_connect(
            server_id,
            client_key_cert,
            "ws://127.0.0.1:9000",
            None,
            None,
        )
        .await?;
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
    #[ignore]
    fn webrtc_test() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let db_path = Path::new("/tmp/collab-signal-db.sqlite3");
        std::fs::remove_file(&db_path).unwrap();
        let _ = runtime.spawn(async move {
            let res = run_signaling_server("127.0.0.1:9000", &db_path).await;
            println!("Signaling server: {:?}", res);
            res.unwrap();
        });

        let server_id = "server#1".to_string();
        let client_id = "client#1".to_string();
        let server_key_cert = Arc::new(create_key_cert(&server_id));
        let client_key_cert = Arc::new(create_key_cert(&client_id));

        let server_id_1 = server_id.clone();
        let handle = runtime.spawn(async {
            let res = test_server(server_id_1, server_key_cert).await;
            println!("Server: {:?}", res);
            res.unwrap();
        });
        let _ =
            runtime.block_on(async { tokio::time::sleep(std::time::Duration::from_secs(1)).await });
        let _ = runtime.block_on(async {
            let res = test_client(server_id, client_key_cert).await;
            println!("Client: {:?}", res);
            res.unwrap();
        });

        let _ = runtime.block_on(async { handle.await });
    }
}
