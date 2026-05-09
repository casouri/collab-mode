//! Multiplexes a [`WebChannel`] (SCTP/DTLS/ICE via signaling) and an
//! [`SshMsgChannel`] (ssh stdio) behind a single [`MsgChannel`].
//!
//! Tracks per-peer transport in a map populated at [`connect`] time;
//! `send`, `disconnect`, and `broadcast` all dispatch by that map.
//!
//! [`connect`]: MsgChannel::connect

use crate::config_man::ArcKeyCert;
use crate::message::Msg;
use crate::ssh_channel::SshMsgChannel;
use crate::types::ServerId;
use crate::webchannel::{Message, MsgChannel, Transport, WebChannel};
use anyhow::anyhow;
use async_trait::async_trait;
use lsp_server::RequestId;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

#[derive(Copy, Clone, Debug)]
enum TransportKind {
    Sctp,
    Ssh,
}

pub struct PolyMsgChannel {
    my_hostid: ServerId,
    web: Arc<WebChannel>,
    ssh: Arc<SshMsgChannel>,
    loopback_tx: mpsc::UnboundedSender<Message>,
    /// Records which backend handles each peer. Populated in `connect`.
    transports: Arc<Mutex<HashMap<ServerId, TransportKind>>>,
}

impl PolyMsgChannel {
    pub fn new(
        my_hostid: ServerId,
        web: Arc<WebChannel>,
        ssh: Arc<SshMsgChannel>,
        loopback_tx: mpsc::UnboundedSender<Message>,
    ) -> Self {
        Self {
            my_hostid,
            web,
            ssh,
            loopback_tx,
            transports: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl MsgChannel for PolyMsgChannel {
    async fn send(
        &self,
        recipient: &ServerId,
        req_id: Option<RequestId>,
        msg: Msg,
    ) -> anyhow::Result<()> {
        // Loopback short-circuit, mirroring `WebChannel::send`.
        if recipient == &self.my_hostid {
            let message = Message {
                host: self.my_hostid.clone(),
                body: msg,
                req_id,
            };
            self.loopback_tx
                .send(message)
                .map_err(|_| anyhow!("Loopback channel closed"))?;
            return Ok(());
        }

        let kind = self
            .transports
            .lock()
            .unwrap()
            .get(recipient)
            .copied()
            .ok_or_else(|| anyhow!("Not connected to {}", recipient))?;

        match kind {
            TransportKind::Sctp => self.web.send(recipient, req_id, msg).await,
            TransportKind::Ssh => self.ssh.send(recipient, req_id, msg).await,
        }
    }

    async fn connect(
        &self,
        remote_id: ServerId,
        transport: Transport,
        my_key_cert: ArcKeyCert,
    ) -> anyhow::Result<()> {
        let kind = match &transport {
            Transport::Sock(_) => TransportKind::Sctp,
            Transport::Ssh { .. } => TransportKind::Ssh,
            Transport::Dummy => {
                return Err(anyhow!("PolyMsgChannel does not handle Dummy transport"))
            }
        };

        // Record the transport up-front so a concurrent `send` knows
        // where to route. Roll back on connect failure.
        self.transports
            .lock()
            .unwrap()
            .insert(remote_id.clone(), kind);

        let result = match kind {
            TransportKind::Sctp => {
                self.web
                    .connect(remote_id.clone(), transport, my_key_cert)
                    .await
            }
            TransportKind::Ssh => {
                self.ssh
                    .connect(remote_id.clone(), transport, my_key_cert)
                    .await
            }
        };

        if result.is_err() {
            self.transports.lock().unwrap().remove(&remote_id);
        }
        result
    }

    async fn broadcast(&self, req_id: Option<RequestId>, msg: Msg) -> anyhow::Result<()> {
        // Group peers by backend so we can issue per-backend sends.
        let entries: Vec<(ServerId, TransportKind)> = {
            let map = self.transports.lock().unwrap();
            map.iter().map(|(k, v)| (k.clone(), *v)).collect()
        };
        for (peer, kind) in entries {
            let res = match kind {
                TransportKind::Sctp => self.web.send(&peer, req_id.clone(), msg.clone()).await,
                TransportKind::Ssh => self.ssh.send(&peer, req_id.clone(), msg.clone()).await,
            };
            if let Err(err) = res {
                tracing::warn!("Broadcast to {} failed: {}", peer, err);
            }
        }
        Ok(())
    }

    fn disconnect(&self, peer: &ServerId) {
        let kind = self.transports.lock().unwrap().remove(peer);
        match kind {
            Some(TransportKind::Sctp) => self.web.disconnect(peer),
            Some(TransportKind::Ssh) => self.ssh.disconnect(peer),
            None => {}
        }
    }

    async fn shutdown(&self) -> anyhow::Result<()> {
        // The backends’ shutdown are bounded, so we don’t need to use
        // a timeout.
        let (web_res, ssh_res) = tokio::join!(self.web.shutdown(), self.ssh.shutdown());
        if let Err(err) = web_res {
            tracing::warn!("WebChannel shutdown error: {}", err);
        }
        if let Err(err) = ssh_res {
            tracing::warn!("SshMsgChannel shutdown error: {}", err);
        }
        Ok(())
    }
}
