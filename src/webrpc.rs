//! This module provides an RPC abstraction over webrpc
//! ([webrtc_data::data_channel::DataChannel]), [Endpoint]. You can
//! use [Endpoint] as a client to send requests, and receive one or
//! more response; you can also use [Endpoint] as a server to listen
//! for incoming requests and send back responses.

//! To send a request, use [Endpoint::send_request]. It returns a
//! channel on which you can receive responses to the request. Closing
//! the channel terminates the request for both ends.

//! To send a response, use [Endpoint::send_response]. To send a
//! series of responses, just call [Endpoint::send_response] multiple
//! times until there's an error, which means either the client closes
//! their end, or an error occurs.

//! You need to run [Endpoint::read_messages] before sending requests.
//! This function runs in the background and reads incoming requests
//! and responses and multiplexes them into corresponding channels. It
//! returns a channel from which you can read requests. If you are not
//! interested in incoming requests (eg, you are a client), just drop
//! the channel. Fatal background errors in [Endpoint::read_messages]
//! are sent to the request channel. If the request channel is closed,
//! errors are sent to every response channel.

//! Each request-response session is made of messages flying both
//! ways, a message can be either a request or a response, and they
//! carry an id to tell which request session it belongs to. A message
//! doesn't have a size limit. Each message are chunked into frames,
//! which has a max size, before sending over the data channel.

//! We don't have acks for messages. When a client stops listening for
//! responses of a request, the server eventually learns it, and
//! sending further responses will return a `RequestClosed` error.

use crate::data::{data_accept, data_bind};
use crate::error::{WebrpcError, WebrpcResult};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
use tokio::io::{AsyncBufReadExt, AsyncReadExt};
use tokio::sync::{mpsc, watch};
use webrtc_data::data_channel::{DataChannel, PollDataChannel};

/// Max size of a webrpc frame body in bytes.
const MAX_FRAME_BODY_SIZE: usize = 64 * 1024 * 1024;
/// Max size of a webrpc frame header line in bytes.
const MAX_FRAME_HEADER_LINE_SIZE: usize = 1024;

type RequestId = u32;
type MessageId = u32;

type RawMessage = Vec<u8>;

type ResponseChannelMap = RwLock<HashMap<RequestId, mpsc::UnboundedSender<WebrpcResult<Message>>>>;
type LiveRequestMap = RwLock<HashMap<RequestId, ()>>;

#[derive(Debug)]
pub struct Listener {
    inner: crate::signaling::client::Listener,
}

impl Listener {
    pub async fn bind(id: String, signaling_addr: &str) -> WebrpcResult<Listener> {
        let listener = data_bind(id, signaling_addr).await?;
        Ok(Listener { inner: listener })
    }

    pub async fn accept(&mut self) -> WebrpcResult<Endpoint> {
        let sock = self.inner.accept().await?;
        let name = format!("connection to {}", sock.id());
        let data_channel = data_accept(sock, None).await?;
        Ok(Endpoint::new(name, Arc::new(data_channel)))
    }
}

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
    name: String,
    /// Map of channel senders, used for sending received messages to
    /// their corresponding channel.
    resp_channel_map: Arc<ResponseChannelMap>,
    /// As long as a request is live, it has a value in this map. When
    /// the other stops listening for responses of a request, the
    /// value is removed from this map.
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
    pub id: RequestId,
    pub body: RawMessage,
}

/// A frame. One or more frames make up a message.
struct Frame<'a> {
    message_id: MessageId,
    body: &'a [u8],
    last: bool,
}

impl Endpoint {
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

    /// Send a message as a request over the data channel, returns a
    /// channel for response message(s). Close the channel to end the
    /// request. Use this on a client endpoint.
    pub async fn send_request(
        &self,
        message: RawMessage,
    ) -> WebrpcResult<mpsc::UnboundedReceiver<WebrpcResult<Message>>> {
        let (tx, rx) = mpsc::unbounded_channel();

        let id = self.current_msg_id.fetch_add(1, Ordering::SeqCst) + 1;
        self.resp_channel_map.write().unwrap().insert(id, tx);

        let msg = Message {
            kind: MessageKind::Request,
            body: message,
            id,
        };
        write_message(&self.data_channel, &self.current_msg_id, msg).await?;
        Ok(rx)
    }

    /// Send a response `message` for the request with `id`.
    pub async fn send_response(&self, id: RequestId, message: RawMessage) -> WebrpcResult<()> {
        if self.live_request_map.read().unwrap().get(&id).is_none() {
            return Err(WebrpcError::RequestClosed());
        }
        let msg = Message {
            kind: MessageKind::Response,
            body: message,
            id,
        };
        write_message(&self.data_channel, &self.current_msg_id, msg).await
    }

    fn read_messages(&self) -> WebrpcResult<mpsc::UnboundedReceiver<WebrpcResult<Message>>> {
        let (tx, rx) = mpsc::unbounded_channel();
        let data_channel = self.data_channel.clone();
        let current_msg_id = self.current_msg_id.clone();
        let channel_name = self.name.clone();
        let resp_channel_map = self.resp_channel_map.clone();
        let live_request_map = self.live_request_map.clone();
        let tx_1 = tx.clone();
        tokio::spawn(async move {
            let res = read_messages(
                data_channel,
                &resp_channel_map,
                &live_request_map,
                &current_msg_id,
                &channel_name,
                tx,
            )
            .await;
            if let Err(err) = res {
                // Send to request channel.
                let send_res = tx_1.send(Err(err.clone()));
                // If can't send to request channel, send to all response channel.
                if send_res.is_err() {
                    for (_, tx) in resp_channel_map.read().unwrap().iter() {
                        let _ = tx.send(Err(err.clone()));
                    }
                }
            }
        });
        Ok(rx)
    }
}

/// Read a webrpc frame from `reader`, `header_buf` is for reading
/// header lines and should be [MAX_FRAME_HEADER_LINE_SIZE] large,
/// `body_buf` is for reading frame body, and should be
/// [MAX_FRAME_BODY_SIZE] large. Return the request id and a slice of
/// the `body_buf` that contains the frame body.
async fn read_frame<'a>(
    reader: &mut tokio::io::BufReader<PollDataChannel>,
    header_buf: &mut Vec<u8>,
    body_buf: &'a mut Vec<u8>,
) -> WebrpcResult<Frame<'a>> {
    // Read header.
    let mut body_len = None;
    let mut message_id = None;
    let mut last = false;
    loop {
        header_buf.clear();
        match reader.read_until(b'\n', header_buf).await {
            Ok(len) => {
                if len == 0 {
                    return Err(WebrpcError::DataChannelError(
                        "Unexpected EOF when reading from data channel".to_string(),
                    ));
                } else if len == 1 {
                    break;
                } else {
                    let line = std::str::from_utf8(&header_buf[..header_buf.len() - 1]).map_err(
                        |err| {
                            WebrpcError::ParseError(format!("Can't parse frame header: {:?}", err))
                        },
                    )?;
                    log::debug!("read_frame(): line = {line}");
                    let mut iter = line.split(":");
                    let key = iter.next().ok_or_else(|| {
                        WebrpcError::ParseError(format!("Malformed frame header: {}", line))
                    })?;
                    let value = iter.next().ok_or_else(|| {
                        WebrpcError::ParseError(format!("Malformed frame header: {}", line))
                    })?;
                    match key {
                        "CL" => {
                            let len: usize = value.parse().map_err(|err| {
                                WebrpcError::ParseError(format!(
                                    "Cannot parse the value of CL (content-length) to a number: {:?}",
                                    err
                                ))
                            })?;
                            body_len = Some(len);
                        }
                        "MI" => {
                            let id: u32 = value.parse().map_err(|err| {
                                WebrpcError::ParseError(format!(
                                    "Cannot parse the value of MI (message-id) to a number: {:?}",
                                    err
                                ))
                            })?;
                            message_id = Some(id)
                        }
                        "L" => last = true,
                        _ => log::warn!("Unrecognized RPC frame header: {}", line),
                    }
                }
            }
            Err(err) => return Err(WebrpcError::DataChannelError(err.to_string())),
        }
    }

    // Read body.
    let body_len = body_len.ok_or_else(|| {
        WebrpcError::ParseError(
            "The webrpc frame's header doesn't contain CL (content-length)".to_string(),
        )
    })?;
    let message_id = message_id.ok_or_else(|| {
        WebrpcError::ParseError(
            "The webrpc frame's header doesn't contain MI (message-id)".to_string(),
        )
    })?;
    if body_len > MAX_FRAME_BODY_SIZE {
        return Err(WebrpcError::DataChannelError(format!(
            "The frame we read has a body that's too large: {} bytes, while the max size we allow is {} bytes",
            body_len, MAX_FRAME_BODY_SIZE
        )));
    }
    reader
        .read_exact(&mut body_buf[..body_len])
        .await
        .map_err(|err| {
            WebrpcError::DataChannelError(format!("Error reading RPC frame: {:?}", err))
        })?;

    Ok(Frame {
        message_id,
        body: &body_buf[..body_len],
        last,
    })
}

// Read incoming messages and send them to corresponding receiving
// channels. Use this function on a client endpoint. If an error
// occurs, stop and return the error.
async fn read_messages(
    data_channel: Arc<DataChannel>,
    resp_channel_map: &ResponseChannelMap,
    live_request_map: &LiveRequestMap,
    current_msg_id: &AtomicU32,
    channel_name: &str,
    mut req_tx: mpsc::UnboundedSender<WebrpcResult<Message>>,
) -> WebrpcResult<()> {
    // Read frames from `data_channel` in a loop, assemble them into
    // messages, and send messages to their corresponding channels in
    // `resp_channel_map`.
    let mut reader = tokio::io::BufReader::new(PollDataChannel::new(data_channel.clone()));
    let mut msg_map: HashMap<RequestId, Vec<u8>> = HashMap::new();
    let mut header_buf = vec![0u8; MAX_FRAME_HEADER_LINE_SIZE];
    let mut body_buf = vec![0u8; MAX_FRAME_BODY_SIZE];

    loop {
        let frame = read_frame(&mut reader, &mut header_buf, &mut body_buf).await?;

        // Append this frame to the incomplete message.
        if let Some(incomplete_msg) = msg_map.get_mut(&frame.message_id) {
            incomplete_msg.copy_from_slice(frame.body);
        } else {
            msg_map.insert(frame.message_id, frame.body.to_vec());
        }

        // If this isn't the last frame, keep reading.
        if !frame.last {
            continue;
        }
        // If this is the last frame, remove from `msg_map`
        // and send to the corresponding channel.
        let msg = msg_map.remove(&frame.message_id).ok_or_else(|| {
            WebrpcError::DataChannelError(format!(
                "Cannot find partial message with id {}",
                frame.message_id,
            ))
        })?;
        let msg = bincode::deserialize::<Message>(&msg).map_err(|err| {
            WebrpcError::ParseError(format!(
                "Cannot parse message #{} from data channel [{}]: {:?}",
                frame.message_id, channel_name, err
            ))
        })?;
        handle_new_message(
            msg,
            &data_channel,
            current_msg_id,
            resp_channel_map.clone(),
            live_request_map.clone(),
            channel_name,
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
    channel_name: &str,
    req_tx: &mut mpsc::UnboundedSender<WebrpcResult<Message>>,
) {
    match msg.kind {
        // Server gets closed message from client.
        MessageKind::Close => {
            live_request_map.write().unwrap().remove(&msg.id);
        }
        // Server gets request message from client.
        MessageKind::Request => {
            live_request_map.write().unwrap().insert(msg.id, ());
            let _ = req_tx.send(Ok(msg));
        }
        // Client gets response message from server.
        MessageKind::Response => {
            let tx = resp_channel_map
                .read()
                .unwrap()
                .get(&msg.id)
                .map(|tx| tx.clone());

            let mut channel_unavailable = false;
            let request_id = msg.id;

            if let Some(tx) = tx {
                let res = tx.send(Ok(msg));
                if res.is_err() {
                    channel_unavailable = true;
                    log::warn!(
                        "Cannot send message #{} to its channel in data channel [{}]",
                        request_id,
                        channel_name,
                    );
                    resp_channel_map.write().unwrap().remove(&request_id);
                }
            } else {
                channel_unavailable = true;
                log::warn!(
                    "Cannot find the channel for request #{} in data channel [{}]",
                    request_id,
                    channel_name,
                );
            }

            // If the channel is closed or nonexistent, inform the
            // other end.
            if channel_unavailable {
                let close_msg = Message {
                    kind: MessageKind::Close,
                    id: request_id,
                    body: vec![],
                };
                let res = write_message(data_channel, current_msg_id, close_msg).await;
                if res.is_err() {
                    log::warn!(
                        "Cannot send close message in data channel [{}]",
                        channel_name
                    );
                }
            }
        }
    }
}

/// Chunkify `message` and send it over the data channel.
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
        let last = if chunk_end == total_len {
            format!("L:\n")
        } else {
            format!("\n")
        };
        let header = format!("CL:{body_len}\nMI:{message_id}\n{last}\n")
            .as_bytes()
            .to_vec();
        data_channel.write(&bytes::Bytes::from(header)).await?;

        if total_len == body_len {
            // Sending the whole message in one go.
            data_channel
                .write(&bytes::Bytes::from(message_bytes))
                .await?;
            return Ok(());
        } else {
            // Sending chunks.
            let chunk = message_bytes[start..chunk_end].to_vec();
            data_channel.write(&bytes::Bytes::from(chunk)).await?;
            start += body_len;
        };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Endpoint;
    use crate::data::{data_accept, data_bind, data_connect};
    use crate::signaling::server::run_signaling_server;
    use std::sync::Arc;

    async fn test_server(id: String) -> anyhow::Result<()> {
        let mut listener = data_bind(id, "ws://127.0.0.1:9000").await?;
        let sock = listener.accept().await?;
        let data_channel = data_accept(sock, None).await?;
        let endpoint = Endpoint::new("server endpoint".to_string(), Arc::new(data_channel));

        let mut rx = endpoint.read_messages().unwrap();

        let req1 = rx.recv().await.unwrap().unwrap();
        let msg = std::str::from_utf8(&req1.body[..]).unwrap();
        println!("Server got request: {msg}");
        assert!(msg == "req1");

        let req2 = rx.recv().await.unwrap().unwrap();
        let msg = std::str::from_utf8(&req2.body[..]).unwrap();
        println!("Server got request: {msg}");
        assert!(msg == "req2");

        endpoint.send_response(req1.id, req1.body).await.unwrap();
        endpoint
            .send_response(req2.id, req2.body.clone())
            .await
            .unwrap();
        endpoint
            .send_response(req2.id, req2.body.clone())
            .await
            .unwrap();
        endpoint
            .send_response(req2.id, req2.body.clone())
            .await
            .unwrap();

        // This response should trigger a closed message on client.
        endpoint
            .send_response(req2.id, req2.body.clone())
            .await
            .unwrap();
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        // This response should fail to send out.
        let res = endpoint.send_response(req2.id, req2.body).await;
        assert!(res.is_err());

        Ok(())
    }

    async fn test_client(id: String) -> anyhow::Result<()> {
        let data_channel = data_connect(id, "ws://127.0.0.1:9000", None).await?;
        let endpoint = Endpoint::new("client endpoint".to_string(), Arc::new(data_channel));

        endpoint.read_messages().unwrap();

        let mut rx1 = endpoint.send_request("req1".as_bytes().to_vec()).await?;
        let mut rx2 = endpoint.send_request("req2".as_bytes().to_vec()).await?;

        let resp1 = rx1.recv().await.unwrap().unwrap();
        let msg1 = std::str::from_utf8(&resp1.body[..]).unwrap();
        println!("Client got resp: {msg1}");
        assert!(msg1 == "req1");

        let resp2 = rx2.recv().await.unwrap().unwrap();
        let msg2 = std::str::from_utf8(&resp2.body[..]).unwrap();
        println!("Client got resp: {msg2}");
        assert!(msg2 == "req2");

        let resp2 = rx2.recv().await.unwrap().unwrap();
        let msg2 = std::str::from_utf8(&resp2.body[..]).unwrap();
        println!("Client got resp: {msg2}");
        assert!(msg2 == "req2");

        let resp2 = rx2.recv().await.unwrap().unwrap();
        let msg2 = std::str::from_utf8(&resp2.body[..]).unwrap();
        println!("Client got resp: {msg2}");
        assert!(msg2 == "req2");

        // Now we drop rx, new responses from server will trigger a
        // "closed" message.
        drop(rx2);
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        Ok(())
    }

    #[test]
    fn webrpc_test() {
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
