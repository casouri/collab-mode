//! This module provides an RPC abstraction over webrpc
//! ([webrtc_data::data_channel::DataChannel]), [Endpoint]. You can
//! use [Endpoint] as a client to send requests, and receive one or
//! more response; you can also use [Endpoint] as a server to listen
//! for incoming requests and send back responses.
//!
//! To create a client, run [Endpoint::connect]. To create a server,
//! first create a [Listener] using [Listener::bind], then accept
//! incoming connections (in the form of [Endpoint]s) with
//! [Listener::accept].
//!
//! For a client, to send a request, use [Endpoint::send_request]. It
//! returns a channel on which you can receive responses to the
//! request. Closing the channel terminates the request for both ends.
//!
//! For a server, you need to run [Endpoint::read_requests] to receive
//! requests after you accepts an endpoint. This function runs in the
//! background and reads incoming requests and responses and
//! multiplexes them into corresponding channels. It returns a channel
//! from which you can read requests.
//!
//! To send a response, use [Endpoint::send_response]. To send a
//! series of responses, just call [Endpoint::send_response] multiple
//! times until there's an error, which means either the client closes
//! their end, or an error occurs.
//!
//! Each request-response session is made of messages flying both
//! ways, a message can be either a request or a response, and they
//! carry an request id to tell which request session it belongs to. A
//! message doesn't have a size limit. Each message are chunked into
//! frames, which has a max size, before sending over the data
//! channel.
//!
//! We don't have synchronous acks for messages. When a client stops
//! listening for responses of a request, the server eventually learns
//! it, and sending further responses will return a `RequestClosed`
//! error.
//!
//! Right now there's no recovery, any error (except
//! [WebrpcError::RequestClosed]) is a fatal error. The user should
//! reconnect and start over the work from a checkpoint.
//!
//! For a server, fatal errors are sent to the request channel; for a client
//! fatal errors are sent to every response channel.
//!
//! Me from 2025: SCTP has channels and messages, so our rpc should've
//! been trivial to implement, sadly webrtc's sctp doesn't implement
//! messages; and we didn't use channels, maybe in the future we can
//! use channels.

use crate::config_man::{hash_der, ArcKeyCert};
use crate::error::{WebrpcError, WebrpcResult};
use crate::ice::{ice_accept, ice_bind, ice_connect};
use crate::signaling::CertDerHash;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;
use tracing::{instrument, Instrument};
use webrtc_dtls::{self, conn::DTLSConn};
use webrtc_sctp::{association, chunk::chunk_payload_data::PayloadProtocolIdentifier, stream};
use webrtc_util::Conn;

pub use crate::signaling::EndpointId;

const MAX_FRAME_BODY_SIZE: usize = 32 * 1024;
const MAX_FRAME_SIZE: usize = 64 * 1024;
const RECEIVE_BUFFER_SIZE: u32 = 1024 * 1024;

type RequestId = u32;
type MessageId = u32;

type ResponseChannelMap = RwLock<HashMap<RequestId, mpsc::UnboundedSender<WebrpcResult<Message>>>>;
type LiveRequestMap = RwLock<HashMap<RequestId, ()>>;

type DataChannel = stream::Stream;

// *** DTLS & SCTP

pub struct Listener {
    inner: crate::signaling::client::Listener,
}

/// Signal Error if `conn` doesn't provide the same certificate as `cert`.
async fn verify_dtls_cert(cert: CertDerHash, conn: &DTLSConn) -> WebrpcResult<()> {
    let certs = conn.connection_state().await.peer_certificates;
    if certs.is_empty() {
        return Err(WebrpcError::CryptoError(format!(
            "Remote host provided empty certificate"
        )));
    }
    let provided_cert_hash = hash_der(&certs[0]);
    if provided_cert_hash != cert {
        return Err(WebrpcError::CryptoError(format!("Certificate hash mismatch, cert from signaling server = {cert}, cert provided by DTLS = {provided_cert_hash}")));
    }
    Ok(())
}

async fn create_dtls_server(
    conn: Arc<dyn Conn + Send + Sync>,
    my_key_cert: ArcKeyCert,
    their_cert: CertDerHash,
) -> WebrpcResult<Arc<DTLSConn>> {
    let config = webrtc_dtls::config::Config {
        certificates: vec![my_key_cert.create_dtls_cert()],
        client_auth: webrtc_dtls::config::ClientAuthType::RequireAnyClientCert,
        // We accept any certificate, and then verifies the provided
        // certificate with the cert we got from signaling server.
        insecure_skip_verify: true,
        ..Default::default()
    };

    let conn = DTLSConn::new(conn, config, false, None).await?;
    verify_dtls_cert(their_cert, &conn).await?;
    Ok(Arc::new(conn))
}

async fn create_dtls_client(
    conn: Arc<dyn Conn + Send + Sync>,
    my_key_cert: ArcKeyCert,
    their_cert: CertDerHash,
) -> WebrpcResult<Arc<DTLSConn>> {
    let config = webrtc_dtls::config::Config {
        certificates: vec![my_key_cert.create_dtls_cert()],
        // We accept any certificate, and then verifies the provided
        // certificate with the cert we got from signaling server.
        insecure_skip_verify: true,
        ..Default::default()
    };

    let conn = DTLSConn::new(conn, config, true, None).await?;
    verify_dtls_cert(their_cert, &conn).await?;
    Ok(Arc::new(conn))
}

async fn create_sctp_server(
    conn: Arc<dyn Conn + Send + Sync>,
) -> WebrpcResult<Arc<association::Association>> {
    let assoc_config = association::Config {
        net_conn: conn,
        max_receive_buffer_size: RECEIVE_BUFFER_SIZE,
        max_message_size: MAX_FRAME_SIZE as u32,
        name: "rpc data channel".to_string(),
    };
    let assoc = association::Association::server(assoc_config).await?;
    Ok(Arc::new(assoc))
}

async fn create_sctp_client(
    conn: Arc<dyn Conn + Send + Sync>,
) -> WebrpcResult<Arc<association::Association>> {
    let assoc_config = association::Config {
        net_conn: conn,
        max_receive_buffer_size: RECEIVE_BUFFER_SIZE,
        max_message_size: MAX_FRAME_SIZE as u32,
        name: "rpc data channel".to_string(),
    };
    let assoc = association::Association::client(assoc_config).await?;
    Ok(Arc::new(assoc))
}

// *** Listener

impl Listener {
    #[instrument(skip(my_key_cert))]
    pub async fn bind(
        id: String,
        my_key_cert: ArcKeyCert,
        signaling_addr: &str,
    ) -> WebrpcResult<Listener> {
        let listener = ice_bind(id, my_key_cert, signaling_addr).await?;
        Ok(Listener { inner: listener })
    }

    #[instrument(skip(self))]
    pub async fn accept(&mut self) -> WebrpcResult<Endpoint> {
        let sock = self.inner.accept().await?;
        let name = format!("connection to {}", sock.id());
        let their_cert = sock.cert_hash();
        let conn = ice_accept(sock, None).await?;
        let dtls_connection =
            create_dtls_server(conn, self.inner.my_key_cert.clone(), their_cert).await?;
        let sctp_connection = create_sctp_server(dtls_connection).await?;
        if let Some(stream) = sctp_connection.accept_stream().await {
            Ok(Endpoint::new(name, stream))
        } else {
            Err(WebrpcError::SCTPError("Can't accept stream".to_string()))
        }
    }
}

// *** Endpoint

/// An RPC endpoint. It can be used as either a client or a server.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// Current message id, used for generating message ids. Message
    /// id is used for combining chunked messages (frames) into one.
    current_msg_id: Arc<AtomicU32>,
    /// Current request id, used for generating request ids. Request
    /// id is used for multiplexing requests.
    current_req_id: Arc<AtomicU32>,
    /// The data channel.
    data_channel: Arc<DataChannel>,
    /// Name of the data channel, used for logging.
    pub name: String,
    /// Map of channel senders, used for sending received messages to
    /// their corresponding channel.
    resp_channel_map: Arc<ResponseChannelMap>,
    /// As long as a request is live, it has a value in this map. When
    /// the other endpoint stops listening for responses of a request,
    /// the value is removed from this map.
    live_request_map: Arc<LiveRequestMap>,
}

/// A message can be either a request, a response, or a close message.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum MessageKind {
    Request,
    Response,
    Close,
}

/// A message. A request exchanges many messages in both direction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    kind: MessageKind,
    pub req_id: RequestId,
    pub body: Vec<u8>,
}

impl Message {
    /// Unpack the message into a `T`.
    pub fn unpack<T: DeserializeOwned>(&self) -> WebrpcResult<T> {
        let res = bincode::deserialize::<T>(&self.body)?;
        Ok(res)
    }
}

/// A frame. One or more frames make up a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Frame<'a> {
    message_id: MessageId,
    body: &'a [u8],
    last: bool,
}

impl Endpoint {
    /// Connect to the endpoint with `id` registered on the signaling server.
    #[instrument(skip(my_key_cert))]
    pub async fn connect(
        id: EndpointId,
        my_key_cert: ArcKeyCert,
        signaling_addr: &str,
    ) -> WebrpcResult<Endpoint> {
        let (conn, their_cert) =
            ice_connect(id.clone(), my_key_cert.clone(), signaling_addr, None).await?;
        let dtls_conn = create_dtls_client(conn, my_key_cert, their_cert).await?;
        let sctp_conn = create_sctp_client(dtls_conn).await?;
        let stream = sctp_conn
            .open_stream(1, PayloadProtocolIdentifier::Binary)
            .await?;
        let endpoint = Endpoint::new(format!("Connection to {id}"), stream);

        let mut rx = endpoint.process_incoming_messages()?;
        rx.close();
        Ok(endpoint)
    }

    pub fn new(name: String, data_channel: Arc<DataChannel>) -> Endpoint {
        let channel_map = Arc::new(RwLock::new(HashMap::new()));
        let live_map = Arc::new(RwLock::new(HashMap::new()));
        Endpoint {
            current_msg_id: Arc::new(AtomicU32::new(0)),
            current_req_id: Arc::new(AtomicU32::new(0)),
            data_channel,
            name,
            resp_channel_map: channel_map,
            live_request_map: live_map,
        }
    }

    /// Like `send_request` but expects only one response.
    pub async fn send_request_oneshot<T: Serialize>(&self, message: &T) -> WebrpcResult<Message> {
        let (mut rx, req_id) = self.send_request(message).await?;
        let timeout = 10u8;
        let rx_recv =
            tokio::time::timeout(std::time::Duration::from_secs(timeout as u64), rx.recv());
        let resp = rx_recv
            .await
            .map_err(|_err| {
                self.resp_channel_map.write().unwrap().remove(&req_id);
                WebrpcError::Timeout(timeout)
            })?
            .unwrap_or_else(|| {
                Err(WebrpcError::DataChannelError(
                    "Unexpected channel close when waiting for response".to_string(),
                ))
            })?;
        Ok(resp)
    }

    /// Send a message as a request over the data channel, returns a
    /// channel for response message(s). Close the channel to end the
    /// request. Use this on a client endpoint.
    #[instrument(skip_all)]
    pub async fn send_request<T: Serialize>(
        &self,
        message: &T,
    ) -> WebrpcResult<(mpsc::UnboundedReceiver<WebrpcResult<Message>>, RequestId)> {
        let (tx, rx) = mpsc::unbounded_channel();
        let tx_1 = tx.clone();

        let req_id = self.current_req_id.fetch_add(1, Ordering::SeqCst) + 1;
        self.resp_channel_map.write().unwrap().insert(req_id, tx);

        // Send close message to the other end when the caller drops
        // the receiver.
        let data_channel = self.data_channel.clone();
        let msg_id = self.current_msg_id.clone();
        tokio::spawn(async move {
            tx_1.closed().await;
            let msg = Message {
                kind: MessageKind::Close,
                body: vec![],
                req_id,
            };
            // Don't need to check if the request is still live.
            // Spurious close message won't hurt. Don't care about
            // error either, since we are closing the connection anyway.
            let _ = write_message(&data_channel, &msg_id, msg).await;
        });

        let msg = Message {
            kind: MessageKind::Request,
            body: bincode::serialize(message).unwrap(),
            req_id,
        };
        write_message(&self.data_channel, &self.current_msg_id, msg).await?;
        Ok((rx, req_id))
    }

    /// Send a response `message` for the request with `id`.
    #[instrument(skip(self, message))]
    pub async fn send_response<T: Serialize>(
        &self,
        req_id: RequestId,
        message: &T,
    ) -> WebrpcResult<()> {
        if self.live_request_map.read().unwrap().get(&req_id).is_none() {
            return Err(WebrpcError::RequestClosed());
        }
        let msg = Message {
            kind: MessageKind::Response,
            body: bincode::serialize(message).unwrap(),
            req_id,
        };
        write_message(&self.data_channel, &self.current_msg_id, msg).await
    }

    // Read incoming remote messages. The messages can be either a
    // request or a response; send requests to the request channel;
    // send responses to the response channel. Request channel is the
    // channel returned by this function. Response channel is the
    // channel returned by [webrpc::Endpoint::send_request].
    pub fn process_incoming_messages(
        &self,
    ) -> WebrpcResult<mpsc::UnboundedReceiver<WebrpcResult<Message>>> {
        let (tx, rx) = mpsc::unbounded_channel();
        let data_channel = self.data_channel.clone();
        let current_msg_id = self.current_msg_id.clone();
        let resp_channel_map = self.resp_channel_map.clone();
        let live_request_map = self.live_request_map.clone();
        let tx_1 = tx.clone();
        let span = tracing::info_span!("process_incoming_message", endpoint_name = self.name);
        tokio::spawn(
            async move {
                let res = read_messages(
                    data_channel,
                    &resp_channel_map,
                    &live_request_map,
                    &current_msg_id,
                    tx,
                )
                .await;
                if let Err(err) = res {
                    // Send to request channel.
                    let _send_res = tx_1.send(Err(err.clone()));
                    // If can't send to request channel, send to all
                    // response channels. Me in 2025: we don't
                    // really need to do this much , caller will receive the error
                    // when we drop the channels.

                    // if send_res.is_err() {
                    //     for (_, tx) in resp_channel_map.read().unwrap().iter() {
                    //         let _ = tx.send(Err(err.clone()));
                    //     }
                    // }
                }
            }
            .instrument(span),
        );
        Ok(rx)
    }
}

// *** Utilities

/// Read a webrpc frame from `reader`, `header_buf` is for reading
/// header lines and should be [MAX_FRAME_HEADER_LINE_SIZE] large,
/// `body_buf` is for reading frame body, and should be
/// [MAX_FRAME_BODY_SIZE] large. Return the request id and a slice of
/// the `body_buf` that contains the frame body.
#[instrument(skip_all)]
async fn read_frame<'a>(
    reader: &DataChannel,
    packet_buf: &'a mut Vec<u8>,
) -> WebrpcResult<Frame<'a>> {
    packet_buf.fill(0);
    match reader.read(packet_buf).await {
        Ok(0) => Err(WebrpcError::SCTPError("Connection closed".to_string())),
        Ok(packet_len) => {
            let frame: Frame = bincode::deserialize(&packet_buf[..packet_len])?;
            tracing::debug!(
                frame.message_id,
                frame.last,
                frame_body_len = frame.body.len()
            );
            Ok(frame)
        }
        Err(err) => Err(err.into()),
    }
}

// Read incoming messages and send them to corresponding receiving
// channels. If an error occurs, stop and return the error.
async fn read_messages(
    data_channel: Arc<DataChannel>,
    resp_channel_map: &ResponseChannelMap,
    live_request_map: &LiveRequestMap,
    current_msg_id: &AtomicU32,
    mut req_tx: mpsc::UnboundedSender<WebrpcResult<Message>>,
) -> WebrpcResult<()> {
    // Read frames from `data_channel` in a loop, assemble them into
    // messages, and send messages to their corresponding channels in
    // `resp_channel_map`.
    let mut msg_map: HashMap<MessageId, Vec<u8>> = HashMap::new();
    let mut body_buf = vec![0u8; MAX_FRAME_SIZE * 2];

    loop {
        let frame = read_frame(&data_channel, &mut body_buf).await?;

        // Append this frame to the incomplete message.
        if let Some(incomplete_msg) = msg_map.get_mut(&frame.message_id) {
            incomplete_msg.extend_from_slice(frame.body);
        } else {
            msg_map.insert(frame.message_id, frame.body.to_vec());
        }

        // If this isn't the last frame, keep reading.
        if !frame.last {
            continue;
        }
        // If this is the last frame, remove from `msg_map`
        // and send to the corresponding channel.
        let msg = msg_map.remove(&frame.message_id).unwrap();
        let msg = bincode::deserialize::<Message>(&msg).map_err(|err| {
            WebrpcError::ParseError(format!(
                "Cannot parse message #{} from data channel: {:?}",
                frame.message_id, err
            ))
        })?;

        tracing::debug!(?msg.kind, msg.req_id, "read_message");

        handle_new_message(
            msg,
            &data_channel,
            current_msg_id,
            resp_channel_map,
            live_request_map,
            &mut req_tx,
        )
        .await;
    }
}

/// Handle a newly arrived message. Send the message to either the
/// request channel or one of the response channels.
async fn handle_new_message(
    msg: Message,
    data_channel: &DataChannel,
    current_msg_id: &AtomicU32,
    resp_channel_map: &ResponseChannelMap,
    live_request_map: &LiveRequestMap,
    req_tx: &mut mpsc::UnboundedSender<WebrpcResult<Message>>,
) {
    match msg.kind {
        // Server gets closed message from client.
        MessageKind::Close => {
            live_request_map.write().unwrap().remove(&msg.req_id);
        }
        // Server gets request message from client.
        MessageKind::Request => {
            live_request_map.write().unwrap().insert(msg.req_id, ());
            // TODO: if req_tx.is_closed(), send back an error message
            // explaining that this endpoint is not processing requests.
            let _ = req_tx.send(Ok(msg));
        }
        // Client gets response message from server.
        MessageKind::Response => {
            let tx = resp_channel_map
                .read()
                .unwrap()
                .get(&msg.req_id)
                .map(|tx| tx.clone());

            let mut channel_unavailable = false;
            let request_id = msg.req_id;

            if let Some(tx) = tx {
                let res = tx.send(Ok(msg));
                if res.is_err() {
                    channel_unavailable = true;
                    tracing::warn!(
                        request_id,
                        "Cannot send message to its channel in data channel",
                    );
                    resp_channel_map.write().unwrap().remove(&request_id);
                }
            } else {
                channel_unavailable = true;
                // It might be because we timed out waiting for this
                // response.
                tracing::warn!(
                    request_id,
                    "Cannot find the channel for the request in data channel"
                );
            }

            // If the channel is closed or nonexistent, inform the
            // other end. This might result in duplicate close
            // messages since we send close messages on closed
            // receiving channels too, but duplicate close message is
            // better than no close message.
            if channel_unavailable {
                let close_msg = Message {
                    kind: MessageKind::Close,
                    req_id: request_id,
                    body: vec![],
                };
                let res = write_message(data_channel, current_msg_id, close_msg).await;
                if let Err(err) = res {
                    tracing::warn!(?err, "Cannot send close message in data channel");
                }
            }
        }
    }
}

/// Chunkify `message` and send it over the data channel.
/// `current_msg_id` will be incremented.
#[instrument(skip_all)]
async fn write_message(
    data_channel: &DataChannel,
    current_msg_id: &AtomicU32,
    message: Message,
) -> WebrpcResult<()> {
    let message_id = current_msg_id.fetch_add(1, Ordering::SeqCst) + 1;
    let message_bytes = bincode::serialize(&message).unwrap();
    let total_len = message_bytes.len();
    let mut start = 0;

    while start < total_len {
        let chunk_end = std::cmp::min(start + MAX_FRAME_BODY_SIZE, total_len);
        let body_len = chunk_end - start;
        let last = chunk_end == total_len;

        if total_len == body_len {
            tracing::debug!(
                message.req_id,
                message_id,
                last,
                message_len = &message_bytes.len()
            );
            tracing::trace!("message body = {:02X?}", &message_bytes);

            let frame = Frame {
                message_id,
                last,
                body: &message_bytes[..],
            };
            let packet = bincode::serialize(&frame).unwrap();
            if packet.len() > MAX_FRAME_SIZE {
                return Err(WebrpcError::SCTPError("Package too large".to_string()));
            }
            // Sending the whole message in one go.
            tracing::debug!("body = {}B", &message_bytes.len());
            tracing::trace!("body = {:02X?}", &message_bytes);
            data_channel.write(&bytes::Bytes::from(packet)).await?;
            return Ok(());
        } else {
            // Sending chunks.
            let chunk = &message_bytes[start..chunk_end];
            let frame = Frame {
                message_id,
                last,
                body: chunk,
            };
            let packet = bincode::serialize(&frame).unwrap();
            if packet.len() > MAX_FRAME_SIZE {
                return Err(WebrpcError::SCTPError("Package too large".to_string()));
            }
            tracing::debug!(message_id, last, message_len = message_bytes.len());
            tracing::trace!("chunk = {:02X?}", &chunk);

            data_channel.write(&bytes::Bytes::from(packet)).await?;
            start += body_len;
        };
    }
    Ok(())
}

// *** Tests

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use crate::ice::{ice_accept, ice_bind, ice_connect};
    use super::Endpoint;
    use super::{create_sctp_client, create_sctp_server};
    use crate::config_man::create_key_cert;
    use crate::signaling::server::run_signaling_server;
    use crate::types::ArcKeyCert;
    use crate::webrpc::{create_dtls_client, create_dtls_server};
    use webrtc_sctp::chunk::chunk_payload_data::PayloadProtocolIdentifier;

    async fn test_server(my_id: String, server_key_cert: ArcKeyCert) -> anyhow::Result<()> {
        let mut listener = ice_bind(my_id, server_key_cert.clone(), "ws://127.0.0.1:9000").await?;
        let sock = listener.accept().await?;
        let client_cert = sock.cert_hash();
        let conn = ice_accept(sock, None).await?;
        let dtls_conn = create_dtls_server(conn, server_key_cert, client_cert).await?;
        let sctp_conn = create_sctp_server(dtls_conn).await?;
        let stream = sctp_conn.accept_stream().await.unwrap();
        let endpoint = Endpoint::new("server endpoint".to_string(), stream);

        let mut rx = endpoint.process_incoming_messages().unwrap();

        let req1 = rx.recv().await.unwrap().unwrap();
        let msg1: String = req1.unpack().unwrap();
        println!("Server got request: {msg1}");
        assert!(msg1 == "req1");

        let req2 = rx.recv().await.unwrap().unwrap();
        let msg2: String = req2.unpack().unwrap();
        println!("Server got request: {msg2}");
        assert!(msg2 == "req2");

        endpoint.send_response(req1.req_id, &msg1).await.unwrap();
        endpoint.send_response(req2.req_id, &msg2).await.unwrap();
        endpoint.send_response(req2.req_id, &msg2).await.unwrap();
        endpoint.send_response(req2.req_id, &msg2).await.unwrap();

        // This response should trigger a closed message on client.
        endpoint
            .send_response(req2.req_id, &String::from_utf8(req2.body.clone()).unwrap())
            .await
            .unwrap();
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        // This response should fail to send out.
        let res = endpoint
            .send_response(req2.req_id, &String::from_utf8(req2.body.clone()).unwrap())
            .await;
        assert!(res.is_err());

        Ok(())
    }

    async fn test_client(server_id: String, client_key_cert: ArcKeyCert) -> anyhow::Result<()> {
        let (conn, server_cert) = ice_connect(
            server_id,
            client_key_cert.clone(),
            "ws://127.0.0.1:9000",
            None,
        )
        .await?;
        let dtls_conn = create_dtls_client(conn, client_key_cert, server_cert).await?;
        let sctp_conn = create_sctp_client(dtls_conn).await?;
        let stream = sctp_conn
            .open_stream(1, PayloadProtocolIdentifier::Binary)
            .await
            .unwrap();
        let endpoint = Endpoint::new("client endpoint".to_string(), stream);

        endpoint.process_incoming_messages().unwrap();

        let (mut rx1, _) = endpoint.send_request(&"req1".to_string()).await?;
        let (mut rx2, _) = endpoint.send_request(&"req2".to_string()).await?;

        let resp1 = rx1.recv().await.unwrap().unwrap();
        let msg1: String = resp1.unpack().unwrap();
        println!("Client got resp: {:?}", msg1);
        assert!(msg1 == "req1");

        let resp2 = rx2.recv().await.unwrap().unwrap();
        let msg2: String = resp2.unpack().unwrap();
        println!("Client got resp: {msg2}");
        assert!(msg2 == "req2");

        let resp2 = rx2.recv().await.unwrap().unwrap();
        let msg2: String = resp2.unpack().unwrap();
        println!("Client got resp: {msg2}");
        assert!(msg2 == "req2");

        let resp2 = rx2.recv().await.unwrap().unwrap();
        let msg2: String = resp2.unpack().unwrap();
        println!("Client got resp: {msg2}");
        assert!(msg2 == "req2");

        drop(rx2);
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        Ok(())
    }

    #[test]
    #[ignore]
    fn webrpc_test() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let db_path = Path::new("/tmp/collab-signal-db.sqlite3");
        let _ = std::fs::remove_file(&db_path);
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
