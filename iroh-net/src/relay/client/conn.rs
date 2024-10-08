//! Manages client-side connections to the relay server.
//!
//! based on tailscale/derp/derp_client.go

use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{anyhow, bail, ensure, Context as _, Result};
use bytes::Bytes;
use futures_lite::Stream;
use futures_sink::Sink;
use futures_util::stream::{SplitSink, SplitStream, StreamExt};
use futures_util::SinkExt;
use tokio::sync::mpsc;
use tokio_tungstenite_wasm::WebSocketStream;
use tokio_util::codec::{FramedRead, FramedWrite};
use tokio_util::task::AbortOnDropHandle;
use tracing::{debug, info_span, trace, Instrument};

use crate::defaults::timeouts::relay::CLIENT_RECV_TIMEOUT;
use crate::key::{PublicKey, SecretKey};
use crate::relay::client::streams::{MaybeTlsStreamReader, MaybeTlsStreamWriter};
use crate::relay::codec::{
    write_frame, DerpCodec, Frame, MAX_PACKET_SIZE, PER_CLIENT_SEND_QUEUE_DEPTH, PROTOCOL_VERSION,
};
use crate::relay::codec::{ClientInfo, PER_CLIENT_READ_QUEUE_DEPTH};

impl PartialEq for Conn {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Eq for Conn {}

/// A connection to a relay server.
///
/// Cheaply clonable.
/// Call `close` to shut down the write loop and read functionality.
#[derive(Debug, Clone)]
pub struct Conn {
    inner: Arc<ConnTasks>,
}

/// The channel on which a relay connection sends received messages.
///
/// The [`Conn`] to a relay is easily clonable but can only send DISCO messages to a relay
/// server.  This is the counterpart which receives DISCO messages from the relay server for
/// a connection.  It is not clonable.
#[derive(Debug)]
pub struct ConnReceiver {
    /// The reader channel, receiving incoming messages.
    reader_channel: mpsc::Receiver<Result<ReceivedMessage>>,
}

impl ConnReceiver {
    /// Reads a messages from a relay server.
    ///
    /// Once it returns an error, the [`Conn`] is dead forever.
    pub async fn recv(&mut self) -> Result<ReceivedMessage> {
        let msg = self
            .reader_channel
            .recv()
            .await
            .ok_or(anyhow!("shut down"))??;
        Ok(msg)
    }
}

#[derive(derive_more::Debug)]
pub struct ConnTasks {
    /// Our local address, if known.
    ///
    /// Is `None` in tests or when using websockets (because we don't control connection establishment in browsers).
    local_addr: Option<SocketAddr>,
    /// Channel on which to communicate to the server. The associated [`mpsc::Receiver`] will close
    /// if there is ever an error writing to the server.
    writer_channel: mpsc::Sender<ConnWriterMessage>,
    /// JoinHandle for the [`ConnWriter`] task
    writer_task: AbortOnDropHandle<Result<()>>,
    reader_task: AbortOnDropHandle<()>,
}

impl Conn {
    /// Sends a packet to the node identified by `dstkey`
    ///
    /// Errors if the packet is larger than [`MAX_PACKET_SIZE`]
    pub async fn send(&self, dstkey: PublicKey, packet: Bytes) -> Result<()> {
        trace!(%dstkey, len = packet.len(), "[RELAY] send");

        self.inner
            .writer_channel
            .send(ConnWriterMessage::Packet((dstkey, packet)))
            .await?;
        Ok(())
    }

    /// Send a ping with 8 bytes of random data.
    pub async fn send_ping(&self, data: [u8; 8]) -> Result<()> {
        self.inner
            .writer_channel
            .send(ConnWriterMessage::Ping(data))
            .await?;
        Ok(())
    }

    /// Respond to a ping request. The `data` field should be filled
    /// by the 8 bytes of random data send by the ping.
    pub async fn send_pong(&self, data: [u8; 8]) -> Result<()> {
        self.inner
            .writer_channel
            .send(ConnWriterMessage::Pong(data))
            .await?;
        Ok(())
    }

    /// Sends a packet that tells the server whether this
    /// connection is to the user's preferred server. This is only
    /// used in the server for stats.
    pub async fn note_preferred(&self, preferred: bool) -> Result<()> {
        self.inner
            .writer_channel
            .send(ConnWriterMessage::NotePreferred(preferred))
            .await?;
        Ok(())
    }

    /// The local address that the [`Conn`] is listening on.
    ///
    /// `None`, when run in a testing environment or when using websockets.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.inner.local_addr
    }

    /// Whether or not this [`Conn`] is closed.
    ///
    /// The [`Conn`] is considered closed if the write side of the connection is no longer running.
    pub fn is_closed(&self) -> bool {
        self.inner.writer_task.is_finished()
    }

    /// Close the connection
    ///
    /// Shuts down the write loop directly and marks the connection as closed. The [`Conn`] will
    /// check if the it is closed before attempting to read from it.
    pub async fn close(&self) {
        if self.inner.writer_task.is_finished() && self.inner.reader_task.is_finished() {
            return;
        }

        self.inner
            .writer_channel
            .send(ConnWriterMessage::Shutdown)
            .await
            .ok();
        self.inner.reader_task.abort();
    }
}

fn process_incoming_frame(frame: Frame) -> Result<ReceivedMessage> {
    match frame {
        Frame::KeepAlive => {
            // A one-way keep-alive message that doesn't require an ack.
            // This predated FrameType::Ping/FrameType::Pong.
            Ok(ReceivedMessage::KeepAlive)
        }
        Frame::PeerGone { peer } => Ok(ReceivedMessage::PeerGone(peer)),
        Frame::RecvPacket { src_key, content } => {
            let packet = ReceivedMessage::ReceivedPacket {
                source: src_key,
                data: content,
            };
            Ok(packet)
        }
        Frame::Ping { data } => Ok(ReceivedMessage::Ping(data)),
        Frame::Pong { data } => Ok(ReceivedMessage::Pong(data)),
        Frame::Health { problem } => {
            let problem = std::str::from_utf8(&problem)?.to_owned();
            let problem = Some(problem);
            Ok(ReceivedMessage::Health { problem })
        }
        Frame::Restarting {
            reconnect_in,
            try_for,
        } => {
            let reconnect_in = Duration::from_millis(reconnect_in as u64);
            let try_for = Duration::from_millis(try_for as u64);
            Ok(ReceivedMessage::ServerRestarting {
                reconnect_in,
                try_for,
            })
        }
        _ => bail!("unexpected packet: {:?}", frame.typ()),
    }
}

/// The kinds of messages we can send to the [`Server`](crate::relay::server::Server)
#[derive(Debug)]
enum ConnWriterMessage {
    /// Send a packet (addressed to the [`PublicKey`]) to the server
    Packet((PublicKey, Bytes)),
    /// Send a pong to the server
    Pong([u8; 8]),
    /// Send a ping to the server
    Ping([u8; 8]),
    /// Tell the server whether or not this client is the user's preferred client
    NotePreferred(bool),
    /// Shutdown the writer
    Shutdown,
}

/// Call [`ConnWriterTasks::run`] to listen for messages to send to the connection.
/// Should be used by the [`Conn`]
///
/// Shutsdown when you send a [`ConnWriterMessage::Shutdown`], or if there is an error writing to
/// the server.
struct ConnWriterTasks {
    recv_msgs: mpsc::Receiver<ConnWriterMessage>,
    writer: ConnWriter,
    rate_limiter: Option<RateLimiter>,
}

impl ConnWriterTasks {
    async fn run(mut self) -> Result<()> {
        while let Some(msg) = self.recv_msgs.recv().await {
            match msg {
                ConnWriterMessage::Packet((key, bytes)) => {
                    send_packet(&mut self.writer, &self.rate_limiter, key, bytes).await?;
                }
                ConnWriterMessage::Pong(data) => {
                    write_frame(&mut self.writer, Frame::Pong { data }, None).await?;
                    self.writer.flush().await?;
                }
                ConnWriterMessage::Ping(data) => {
                    write_frame(&mut self.writer, Frame::Ping { data }, None).await?;
                    self.writer.flush().await?;
                }
                ConnWriterMessage::NotePreferred(preferred) => {
                    write_frame(&mut self.writer, Frame::NotePreferred { preferred }, None).await?;
                    self.writer.flush().await?;
                }
                ConnWriterMessage::Shutdown => {
                    return Ok(());
                }
            }
        }

        bail!("channel unexpectedly closed");
    }
}

/// The Builder returns a [`Conn`] and a [`ConnReceiver`] and
/// runs a [`ConnWriterTasks`] in the background.
pub struct ConnBuilder {
    secret_key: SecretKey,
    reader: ConnReader,
    writer: ConnWriter,
    local_addr: Option<SocketAddr>,
}

pub(crate) enum ConnReader {
    Derp(FramedRead<MaybeTlsStreamReader, DerpCodec>),
    Ws(SplitStream<WebSocketStream>),
}

pub(crate) enum ConnWriter {
    Derp(FramedWrite<MaybeTlsStreamWriter, DerpCodec>),
    Ws(SplitSink<WebSocketStream, tokio_tungstenite_wasm::Message>),
}

fn tung_wasm_to_io_err(e: tokio_tungstenite_wasm::Error) -> std::io::Error {
    match e {
        tokio_tungstenite_wasm::Error::Io(io_err) => io_err,
        _ => std::io::Error::new(std::io::ErrorKind::Other, e.to_string()),
    }
}

impl Stream for ConnReader {
    type Item = Result<Frame>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match *self {
            Self::Derp(ref mut ws) => Pin::new(ws).poll_next(cx),
            Self::Ws(ref mut ws) => match Pin::new(ws).poll_next(cx) {
                Poll::Ready(Some(Ok(tokio_tungstenite_wasm::Message::Binary(vec)))) => {
                    Poll::Ready(Some(Frame::decode_from_ws_msg(vec)))
                }
                Poll::Ready(Some(Ok(msg))) => {
                    tracing::warn!(?msg, "Got websocket message of unsupported type, skipping.");
                    Poll::Pending
                }
                Poll::Ready(Some(Err(e))) => Poll::Ready(Some(Err(e.into()))),
                Poll::Ready(None) => Poll::Ready(None),
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

impl Sink<Frame> for ConnWriter {
    type Error = std::io::Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match *self {
            Self::Derp(ref mut ws) => Pin::new(ws).poll_ready(cx),
            Self::Ws(ref mut ws) => Pin::new(ws).poll_ready(cx).map_err(tung_wasm_to_io_err),
        }
    }

    fn start_send(mut self: Pin<&mut Self>, item: Frame) -> Result<(), Self::Error> {
        match *self {
            Self::Derp(ref mut ws) => Pin::new(ws).start_send(item),
            Self::Ws(ref mut ws) => Pin::new(ws)
                .start_send(tokio_tungstenite_wasm::Message::binary(
                    item.encode_for_ws_msg(),
                ))
                .map_err(tung_wasm_to_io_err),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match *self {
            Self::Derp(ref mut ws) => Pin::new(ws).poll_flush(cx),
            Self::Ws(ref mut ws) => Pin::new(ws).poll_flush(cx).map_err(tung_wasm_to_io_err),
        }
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match *self {
            Self::Derp(ref mut ws) => Pin::new(ws).poll_close(cx),
            Self::Ws(ref mut ws) => Pin::new(ws).poll_close(cx).map_err(tung_wasm_to_io_err),
        }
    }
}

impl ConnBuilder {
    pub fn new(
        secret_key: SecretKey,
        local_addr: Option<SocketAddr>,
        reader: ConnReader,
        writer: ConnWriter,
    ) -> Self {
        Self {
            secret_key,
            reader,
            writer,
            local_addr,
        }
    }

    async fn server_handshake(&mut self) -> Result<Option<RateLimiter>> {
        debug!("server_handshake: started");
        let client_info = ClientInfo {
            version: PROTOCOL_VERSION,
        };
        debug!("server_handshake: sending client_key: {:?}", &client_info);
        crate::relay::codec::send_client_key(&mut self.writer, &self.secret_key, &client_info)
            .await?;

        // TODO: add some actual configuration
        let rate_limiter = RateLimiter::new(0, 0)?;

        debug!("server_handshake: done");
        Ok(rate_limiter)
    }

    pub async fn build(mut self) -> Result<(Conn, ConnReceiver)> {
        // exchange information with the server
        let rate_limiter = self.server_handshake().await?;

        // create task to handle writing to the server
        let (writer_sender, writer_recv) = mpsc::channel(PER_CLIENT_SEND_QUEUE_DEPTH);
        let writer_task = tokio::task::spawn(
            ConnWriterTasks {
                rate_limiter,
                writer: self.writer,
                recv_msgs: writer_recv,
            }
            .run()
            .instrument(info_span!("conn.writer")),
        );

        let (reader_sender, reader_recv) = mpsc::channel(PER_CLIENT_READ_QUEUE_DEPTH);
        let reader_task = tokio::task::spawn({
            let writer_sender = writer_sender.clone();
            async move {
                loop {
                    let frame = tokio::time::timeout(CLIENT_RECV_TIMEOUT, self.reader.next()).await;
                    let res = match frame {
                        Ok(Some(Ok(frame))) => process_incoming_frame(frame),
                        Ok(Some(Err(err))) => {
                            // Error processing incoming messages
                            Err(err)
                        }
                        Ok(None) => {
                            // EOF
                            Err(anyhow::anyhow!("EOF: reader stream ended"))
                        }
                        Err(err) => {
                            // Timeout
                            Err(err.into())
                        }
                    };
                    if res.is_err() {
                        // shutdown
                        writer_sender.send(ConnWriterMessage::Shutdown).await.ok();
                        break;
                    }
                    if reader_sender.send(res).await.is_err() {
                        // shutdown, as the reader is gone
                        writer_sender.send(ConnWriterMessage::Shutdown).await.ok();
                        break;
                    }
                }
            }
            .instrument(info_span!("conn.reader"))
        });

        let conn = Conn {
            inner: Arc::new(ConnTasks {
                local_addr: self.local_addr,
                writer_channel: writer_sender,
                writer_task: AbortOnDropHandle::new(writer_task),
                reader_task: AbortOnDropHandle::new(reader_task),
            }),
        };

        let conn_receiver = ConnReceiver {
            reader_channel: reader_recv,
        };

        Ok((conn, conn_receiver))
    }
}

#[derive(derive_more::Debug, Clone)]
/// The type of message received by the [`Conn`] from a relay server.
pub enum ReceivedMessage {
    /// Represents an incoming packet.
    ReceivedPacket {
        /// The [`PublicKey`] of the packet sender.
        source: PublicKey,
        /// The received packet bytes.
        #[debug(skip)]
        data: Bytes, // TODO: ref
    },
    /// Indicates that the client identified by the underlying public key had previously sent you a
    /// packet but has now disconnected from the server.
    PeerGone(PublicKey),
    /// Request from a client or server to reply to the
    /// other side with a [`ReceivedMessage::Pong`] with the given payload.
    Ping([u8; 8]),
    /// Reply to a [`ReceivedMessage::Ping`] from a client or server
    /// with the payload sent previously in the ping.
    Pong([u8; 8]),
    /// A one-way empty message from server to client, just to
    /// keep the connection alive. It's like a [`ReceivedMessage::Ping`], but doesn't solicit
    /// a reply from the client.
    KeepAlive,
    /// A one-way message from server to client, declaring the connection health state.
    Health {
        /// If set, is a description of why the connection is unhealthy.
        ///
        /// If `None` means the connection is healthy again.
        ///
        /// The default condition is healthy, so the server doesn't broadcast a [`ReceivedMessage::Health`]
        /// until a problem exists.
        problem: Option<String>,
    },
    /// A one-way message from server to client, advertising that the server is restarting.
    ServerRestarting {
        /// An advisory duration that the client should wait before attempting to reconnect.
        /// It might be zero. It exists for the server to smear out the reconnects.
        reconnect_in: Duration,
        /// An advisory duration for how long the client should attempt to reconnect
        /// before giving up and proceeding with its normal connection failure logic. The interval
        /// between retries is undefined for now. A server should not send a TryFor duration more
        /// than a few seconds.
        try_for: Duration,
    },
}

pub(crate) async fn send_packet<S: Sink<Frame, Error = std::io::Error> + Unpin>(
    mut writer: S,
    rate_limiter: &Option<RateLimiter>,
    dst_key: PublicKey,
    packet: Bytes,
) -> Result<()> {
    ensure!(
        packet.len() <= MAX_PACKET_SIZE,
        "packet too big: {}",
        packet.len()
    );

    let frame = Frame::SendPacket { dst_key, packet };
    if let Some(rate_limiter) = rate_limiter {
        if rate_limiter.check_n(frame.len()).is_err() {
            tracing::warn!("dropping send: rate limit reached");
            return Ok(());
        }
    }
    writer.send(frame).await?;
    writer.flush().await?;

    Ok(())
}

pub(crate) struct RateLimiter {
    inner: governor::RateLimiter<
        governor::state::direct::NotKeyed,
        governor::state::InMemoryState,
        governor::clock::DefaultClock,
        governor::middleware::NoOpMiddleware,
    >,
}

impl RateLimiter {
    pub(crate) fn new(bytes_per_second: usize, bytes_burst: usize) -> Result<Option<Self>> {
        if bytes_per_second == 0 || bytes_burst == 0 {
            return Ok(None);
        }
        let bytes_per_second = NonZeroU32::new(u32::try_from(bytes_per_second)?)
            .context("bytes_per_second not non-zero")?;
        let bytes_burst =
            NonZeroU32::new(u32::try_from(bytes_burst)?).context("bytes_burst not non-zero")?;
        Ok(Some(Self {
            inner: governor::RateLimiter::direct(
                governor::Quota::per_second(bytes_per_second).allow_burst(bytes_burst),
            ),
        }))
    }

    pub(crate) fn check_n(&self, n: usize) -> Result<()> {
        let n = NonZeroU32::new(u32::try_from(n)?).context("n not non-zero")?;
        match self.inner.check_n(n) {
            Ok(_) => Ok(()),
            Err(_) => bail!("batch cannot go through"),
        }
    }
}
