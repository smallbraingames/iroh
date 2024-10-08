//! Implements a socket that can change its communication path while in use, actively searching for the best way to communicate.
//!
//! Based on tailscale/wgengine/magicsock
//!
//! ### `DEV_RELAY_ONLY` env var:
//! When present at *compile time*, this env var will force all packets
//! to be sent over the relay connection, regardless of whether or
//! not we have a direct UDP address for the given node.
//!
//! The intended use is for testing the relay protocol inside the MagicSock
//! to ensure that we can rely on the relay to send packets when two nodes
//! are unable to find direct UDP connections to each other.
//!
//! This also prevent this node from attempting to hole punch and prevents it
//! from responding to any hole punching attempts. This node will still,
//! however, read any packets that come off the UDP sockets.

use std::{
    collections::{BTreeMap, HashMap},
    fmt::Display,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering},
        Arc,
    },
    task::{Context, Poll, Waker},
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context as _, Result};
use bytes::Bytes;
use futures_lite::{FutureExt, Stream, StreamExt};
use futures_util::stream::BoxStream;
use iroh_base::key::NodeId;
use iroh_metrics::{inc, inc_by};
use quinn::AsyncUdpSocket;
use rand::{seq::SliceRandom, Rng, SeedableRng};
use smallvec::{smallvec, SmallVec};
use tokio::{
    sync::{self, mpsc, Mutex},
    task::JoinSet,
    time,
};
use tokio_util::sync::CancellationToken;
use tracing::{
    debug, error, error_span, event, info, info_span, instrument, trace, trace_span, warn,
    Instrument, Level, Span,
};
use url::Url;
use watchable::Watchable;

use crate::{
    defaults::timeouts::NETCHECK_REPORT_TIMEOUT,
    disco::{self, CallMeMaybe, SendAddr},
    discovery::{Discovery, DiscoveryItem},
    dns::DnsResolver,
    endpoint::NodeAddr,
    key::{PublicKey, SecretKey, SharedSecret},
    net::{interfaces, ip::LocalAddresses, netmon},
    netcheck, portmapper,
    relay::{RelayMap, RelayUrl},
    stun, AddrInfo,
};

use self::{
    metrics::Metrics as MagicsockMetrics,
    node_map::{NodeMap, PingAction, PingRole, SendPing},
    relay_actor::{RelayActor, RelayActorMessage, RelayReadResult},
    udp_conn::UdpConn,
};

mod metrics;
mod node_map;
mod relay_actor;
mod timer;
mod udp_conn;

pub use node_map::Source;

pub(super) use self::timer::Timer;

pub use self::metrics::Metrics;
pub use self::node_map::{
    ConnectionType, ConnectionTypeStream, ControlMsg, DirectAddrInfo, RemoteInfo,
};

/// How long we consider a STUN-derived endpoint valid for. UDP NAT mappings typically
/// expire at 30 seconds, so this is a few seconds shy of that.
const ENDPOINTS_FRESH_ENOUGH_DURATION: Duration = Duration::from_secs(27);

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// Contains options for `MagicSock::listen`.
#[derive(derive_more::Debug)]
pub(crate) struct Options {
    /// The IPv4 address to listen on.
    ///
    /// If set to `None` it will choose a random port and listen on `0.0.0.0:0`.
    pub(crate) addr_v4: Option<SocketAddrV4>,
    /// The IPv6 address to listen on.
    ///
    /// If set to `None` it will choose a random port and listen on `[::]:0`.
    pub(crate) addr_v6: Option<SocketAddrV6>,

    /// Secret key for this node.
    pub(crate) secret_key: SecretKey,

    /// The [`RelayMap`] to use, leave empty to not use a relay server.
    pub(crate) relay_map: RelayMap,

    /// An optional [`NodeMap`], to restore information about nodes.
    pub(crate) node_map: Option<Vec<NodeAddr>>,

    /// Optional node discovery mechanism.
    pub(crate) discovery: Option<Box<dyn Discovery>>,

    /// A DNS resolver to use for resolving relay URLs.
    ///
    /// You can use [`crate::dns::default_resolver`] for a resolver that uses the system's DNS
    /// configuration.
    pub(crate) dns_resolver: DnsResolver,

    /// Proxy configuration.
    pub(crate) proxy_url: Option<Url>,

    /// Skip verification of SSL certificates from relay servers
    ///
    /// May only be used in tests.
    #[cfg(any(test, feature = "test-utils"))]
    #[cfg_attr(iroh_docsrs, doc(cfg(any(test, feature = "test-utils"))))]
    pub(crate) insecure_skip_relay_cert_verify: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            addr_v4: None,
            addr_v6: None,
            secret_key: SecretKey::generate(),
            relay_map: RelayMap::empty(),
            node_map: None,
            discovery: None,
            proxy_url: None,
            dns_resolver: crate::dns::default_resolver().clone(),
            #[cfg(any(test, feature = "test-utils"))]
            insecure_skip_relay_cert_verify: false,
        }
    }
}

/// Contents of a relay message. Use a SmallVec to avoid allocations for the very
/// common case of a single packet.
type RelayContents = SmallVec<[Bytes; 1]>;

/// Handle for [`MagicSock`].
///
/// Dereferences to [`MagicSock`], and handles closing.
#[derive(Clone, Debug, derive_more::Deref)]
pub(crate) struct Handle {
    #[deref(forward)]
    msock: Arc<MagicSock>,
    // Empty when closed
    actor_tasks: Arc<Mutex<JoinSet<()>>>,
}

/// Iroh connectivity layer.
///
/// This is responsible for routing packets to nodes based on node IDs, it will initially
/// route packets via a relay and transparently try and establish a node-to-node
/// connection and upgrade to it.  It will also keep looking for better connections as the
/// network details of both nodes change.
///
/// It is usually only necessary to use a single [`MagicSock`] instance in an application, it
/// means any QUIC endpoints on top will be sharing as much information about nodes as
/// possible.
#[derive(derive_more::Debug)]
pub(crate) struct MagicSock {
    actor_sender: mpsc::Sender<ActorMessage>,
    relay_actor_sender: mpsc::Sender<RelayActorMessage>,
    /// String representation of the node_id of this node.
    me: String,
    /// Proxy
    proxy_url: Option<Url>,

    /// Used for receiving relay messages.
    relay_recv_receiver: parking_lot::Mutex<mpsc::Receiver<RelayRecvResult>>,
    /// Stores wakers, to be called when relay_recv_ch receives new data.
    network_recv_wakers: parking_lot::Mutex<Option<Waker>>,
    network_send_wakers: Arc<parking_lot::Mutex<Option<Waker>>>,

    /// The DNS resolver to be used in this magicsock.
    dns_resolver: DnsResolver,

    /// Key for this node.
    secret_key: SecretKey,

    /// Cached version of the Ipv4 and Ipv6 addrs of the current connection.
    local_addrs: std::sync::RwLock<(SocketAddr, Option<SocketAddr>)>,

    /// Preferred port from `Options::port`; 0 means auto.
    port: AtomicU16,

    /// Close is in progress (or done)
    closing: AtomicBool,
    /// Close was called.
    closed: AtomicBool,
    /// If the last netcheck report, reports IPv6 to be available.
    ipv6_reported: Arc<AtomicBool>,

    /// None (or zero nodes) means relay is disabled.
    relay_map: RelayMap,
    /// Nearest relay node ID; 0 means none/unknown.
    my_relay: Watchable<Option<RelayUrl>>,
    /// Tracks the networkmap node entity for each node discovery key.
    node_map: NodeMap,
    /// UDP IPv4 socket
    pconn4: UdpConn,
    /// UDP IPv6 socket
    pconn6: Option<UdpConn>,
    /// Netcheck client
    net_checker: netcheck::Addr,
    /// The state for an active DiscoKey.
    disco_secrets: DiscoSecrets,

    /// UDP disco (ping) queue
    udp_disco_sender: mpsc::Sender<(SocketAddr, PublicKey, disco::Message)>,

    /// Optional discovery service
    discovery: Option<Box<dyn Discovery>>,

    /// Our discovered direct addresses.
    direct_addrs: Watchable<DiscoveredDirectAddrs>,

    /// List of CallMeMaybe disco messages that should be sent out after the next endpoint update
    /// completes
    pending_call_me_maybes: parking_lot::Mutex<HashMap<PublicKey, RelayUrl>>,

    /// Indicates the direct addr update state.
    direct_addr_update_state: DirectAddrUpdateState,

    /// Skip verification of SSL certificates from relay servers
    ///
    /// May only be used in tests.
    #[cfg(any(test, feature = "test-utils"))]
    #[cfg_attr(iroh_docsrs, doc(cfg(any(test, feature = "test-utils"))))]
    insecure_skip_relay_cert_verify: bool,
}

impl MagicSock {
    /// Creates a magic [`MagicSock`] listening on [`Options::addr_v4`] and [`Options::addr_v6`].
    pub(crate) async fn spawn(opts: Options) -> Result<Handle> {
        Handle::new(opts).await
    }

    /// Returns the relay node we are connected to, that has the best latency.
    ///
    /// If `None`, then we are not connected to any relay nodes.
    pub(crate) fn my_relay(&self) -> Option<RelayUrl> {
        self.my_relay.get()
    }

    /// Get the current proxy configuration.
    pub(crate) fn proxy_url(&self) -> Option<&Url> {
        self.proxy_url.as_ref()
    }

    /// Sets the relay node with the best latency.
    ///
    /// If we are not connected to any relay nodes, set this to `None`.
    fn set_my_relay(&self, my_relay: Option<RelayUrl>) -> Option<RelayUrl> {
        self.my_relay.replace(my_relay)
    }

    fn is_closing(&self) -> bool {
        self.closing.load(Ordering::Relaxed)
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn public_key(&self) -> PublicKey {
        self.secret_key.public()
    }

    /// Get the cached version of the Ipv4 and Ipv6 addrs of the current connection.
    pub(crate) fn local_addr(&self) -> (SocketAddr, Option<SocketAddr>) {
        *self.local_addrs.read().expect("not poisoned")
    }

    /// Returns `true` if we have at least one candidate address where we can send packets to.
    pub(crate) fn has_send_address(&self, node_key: PublicKey) -> bool {
        self.remote_info(node_key)
            .map(|info| info.has_send_address())
            .unwrap_or(false)
    }

    /// Return the [`RemoteInfo`]s of all nodes in the node map.
    pub(crate) fn list_remote_infos(&self) -> Vec<RemoteInfo> {
        self.node_map.list_remote_infos(Instant::now())
    }

    /// Return the [`RemoteInfo`] for a single node in the node map.
    pub(crate) fn remote_info(&self, node_id: NodeId) -> Option<RemoteInfo> {
        self.node_map.remote_info(node_id)
    }

    /// Returns the direct addresses as a stream.
    ///
    /// The [`MagicSock`] continuously monitors the direct addresses, the network addresses
    /// it might be able to be contacted on, for changes.  Whenever changes are detected
    /// this stream will yield a new list of addresses.
    ///
    /// Upon the first creation on the [`MagicSock`] it may not yet have completed a first
    /// direct addresses discovery, in this case the first item of the stream will not be
    /// immediately available.  Once this first set of direct addresses are discovered the
    /// stream will always return the first set of addresses immediately, which are the most
    /// recently discovered addresses.
    ///
    /// To get the current direct addresses, drop the stream after the first item was
    /// received.
    pub(crate) fn direct_addresses(&self) -> DirectAddrsStream {
        DirectAddrsStream {
            initial: Some(self.direct_addrs.get()),
            inner: self.direct_addrs.watch().into_stream(),
        }
    }

    /// Watch for changes to the home relay.
    ///
    /// Note that this can be used to wait for the initial home relay to be known. If the home
    /// relay is known at this point, it will be the first item in the stream.
    pub(crate) fn watch_home_relay(&self) -> impl Stream<Item = RelayUrl> {
        let current = futures_lite::stream::iter(self.my_relay());
        let changes = self
            .my_relay
            .watch()
            .into_stream()
            .filter_map(|maybe_relay| maybe_relay);
        current.chain(changes)
    }

    /// Returns a stream that reports the [`ConnectionType`] we have to the
    /// given `node_id`.
    ///
    /// The `NodeMap` continuously monitors the `node_id`'s endpoint for
    /// [`ConnectionType`] changes, and sends the latest [`ConnectionType`]
    /// on the stream.
    ///
    /// The current [`ConnectionType`] will the the initial entry on the stream.
    ///
    /// # Errors
    ///
    /// Will return an error if there is no address information known about the
    /// given `node_id`.
    pub(crate) fn conn_type_stream(&self, node_id: NodeId) -> Result<ConnectionTypeStream> {
        self.node_map.conn_type_stream(node_id)
    }

    /// Returns the socket address which can be used by the QUIC layer to dial this node.
    pub(crate) fn get_mapping_addr(&self, node_id: NodeId) -> Option<QuicMappedAddr> {
        self.node_map.get_quic_mapped_addr_for_node_key(node_id)
    }

    /// Add addresses for a node to the magic socket's addresbook.
    #[instrument(skip_all, fields(me = %self.me))]
    pub fn add_node_addr(&self, mut addr: NodeAddr, source: node_map::Source) -> Result<()> {
        let my_addresses = self.direct_addrs.get().addrs;
        let mut pruned = 0;
        for my_addr in my_addresses.into_iter().map(|ep| ep.addr) {
            if addr.info.direct_addresses.remove(&my_addr) {
                warn!(node_id=addr.node_id.fmt_short(), %my_addr, %source, "not adding our addr for node");
                pruned += 1;
            }
        }
        if !addr.info.is_empty() {
            self.node_map.add_node_addr(addr, source);
            Ok(())
        } else if pruned != 0 {
            Err(anyhow::anyhow!(
                "empty addressing info, {pruned} direct addresses have been pruned"
            ))
        } else {
            Err(anyhow::anyhow!("empty addressing info"))
        }
    }

    /// Updates our direct addresses.
    ///
    /// On a successful update, our address is published to discovery.
    pub(super) fn update_direct_addresses(&self, eps: Vec<DirectAddr>) {
        let updated = self
            .direct_addrs
            .update(DiscoveredDirectAddrs::new(eps))
            .is_ok();
        if updated {
            let direct_addrs = self.direct_addrs.read();
            direct_addrs.log_direct_addrs_change();
            self.node_map
                .on_direct_addr_discovered(direct_addrs.iter().map(|addr| addr.addr));
            self.publish_my_addr();
        }
    }

    /// Get a reference to the DNS resolver used in this [`MagicSock`].
    pub(crate) fn dns_resolver(&self) -> &DnsResolver {
        &self.dns_resolver
    }

    /// Reference to optional discovery service
    pub(crate) fn discovery(&self) -> Option<&dyn Discovery> {
        self.discovery.as_ref().map(Box::as_ref)
    }

    /// Call to notify the system of potential network changes.
    pub(crate) async fn network_change(&self) {
        self.actor_sender
            .send(ActorMessage::NetworkChange)
            .await
            .ok();
    }

    #[cfg(test)]
    async fn force_network_change(&self, is_major: bool) {
        self.actor_sender
            .send(ActorMessage::ForceNetworkChange(is_major))
            .await
            .ok();
    }

    #[cfg_attr(windows, allow(dead_code))]
    fn normalized_local_addr(&self) -> io::Result<SocketAddr> {
        let (v4, v6) = self.local_addr();
        let addr = if let Some(v6) = v6 { v6 } else { v4 };
        Ok(addr)
    }

    fn create_io_poller(&self) -> Pin<Box<dyn quinn::UdpPoller>> {
        // To do this properly the MagicSock would need a registry of pollers.  For each
        // node we would look up the poller or create one.  Then on each try_send we can
        // look up the correct poller and configure it to poll the paths it needs.
        //
        // Note however that the current quinn impl calls UdpPoller::poll_writable()
        // **before** it calls try_send(), as opposed to how it is documented.  That is a
        // problem as we would not yet know the path that needs to be polled.  To avoid such
        // ambiguity the API could be changed to a .poll_send(&self, cx: &mut Context,
        // io_poller: Pin<&mut dyn UdpPoller>, transmit: &Transmit) -> Poll<io::Result<()>>
        // instead of the existing .try_send() because then we would have control over this.
        //
        // Right now however we have one single poller behaving the same for each
        // connection.  It checks all paths and returns Poll::Ready as soon as any path is
        // ready.
        let ipv4_poller = Arc::new(self.pconn4.clone()).create_io_poller();
        let ipv6_poller = self
            .pconn6
            .as_ref()
            .map(|sock| Arc::new(sock.clone()).create_io_poller());
        let relay_sender = self.relay_actor_sender.clone();
        Box::pin(IoPoller {
            ipv4_poller,
            ipv6_poller,
            relay_sender,
            relay_send_waker: self.network_send_wakers.clone(),
        })
    }

    /// Implementation for AsyncUdpSocket::try_send
    #[instrument(skip_all)]
    fn try_send(&self, transmit: &quinn_udp::Transmit) -> io::Result<()> {
        inc_by!(MagicsockMetrics, send_data, transmit.contents.len() as _);

        if self.is_closed() {
            inc_by!(
                MagicsockMetrics,
                send_data_network_down,
                transmit.contents.len() as _
            );
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "connection closed",
            ));
        }

        let dest = QuicMappedAddr(transmit.destination);
        trace!(
            dst = %dest,
            src = ?transmit.src_ip,
            len = %transmit.contents.len(),
            "sending",
        );
        let mut transmit = transmit.clone();
        match self
            .node_map
            .get_send_addrs(dest, self.ipv6_reported.load(Ordering::Relaxed))
        {
            Some((node_id, udp_addr, relay_url, msgs)) => {
                let mut pings_sent = false;
                // If we have pings to send, we *have* to send them out first.
                if !msgs.is_empty() {
                    if let Err(err) = self.try_send_ping_actions(msgs) {
                        warn!(
                            node = %node_id.fmt_short(),
                            "failed to handle ping actions: {err:#}",
                        );
                    }
                    pings_sent = true;
                }

                let mut udp_sent = false;
                let mut udp_error = None;
                let mut relay_sent = false;
                let mut relay_error = None;

                // send udp
                if let Some(addr) = udp_addr {
                    // rewrite target address
                    transmit.destination = addr;
                    match self.try_send_udp(addr, &transmit) {
                        Ok(()) => {
                            trace!(node = %node_id.fmt_short(), dst = %addr,
                                   "sent transmit over UDP");
                            udp_sent = true;
                        }
                        Err(err) => {
                            error!(node = %node_id.fmt_short(), dst = %addr,
                                   "failed to send udp: {err:#}");
                            udp_error = Some(err);
                        }
                    }
                }

                // send relay
                if let Some(ref relay_url) = relay_url {
                    match self.try_send_relay(relay_url, node_id, split_packets(&transmit)) {
                        Ok(()) => {
                            relay_sent = true;
                        }
                        Err(err) => {
                            relay_error = Some(err);
                        }
                    }
                }

                let udp_pending = udp_error
                    .as_ref()
                    .map(|err| err.kind() == io::ErrorKind::WouldBlock)
                    .unwrap_or_default();
                let relay_pending = relay_error
                    .as_ref()
                    .map(|err| err.kind() == io::ErrorKind::WouldBlock)
                    .unwrap_or_default();
                if udp_pending && relay_pending {
                    // Handle backpressure.
                    Err(io::Error::new(io::ErrorKind::WouldBlock, "pending"))
                } else {
                    if relay_sent || udp_sent {
                        trace!(
                            node = %node_id.fmt_short(),
                            send_udp = ?udp_addr,
                            send_relay = ?relay_url,
                            "sent transmit",
                        );
                    } else if !pings_sent {
                        // Returning Ok here means we let QUIC handle a timeout for a lost
                        // packet, same would happen if we returned any errors.  The
                        // philosophy of quinn-udp is that a UDP connection could come back
                        // at any time so these errors should be treated as transient and
                        // are just timeouts.  Hence we opt for returning Ok.  See
                        // test_try_send_no_udp_addr_or_relay_url to explore this further.
                        error!(
                            node = %node_id.fmt_short(),
                            "no UDP or relay paths available for node",
                        );
                    }
                    Ok(())
                }
            }
            None => {
                error!(%dest, "no NodeState for mapped address");
                // Returning Ok here means we let QUIC timeout.  Returning WouldBlock
                // triggers a hot loop.  Returning an error would immediately fail a
                // connection.  The philosophy of quinn-udp is that a UDP connection could
                // come back at any time or missing should be transient so chooses to let
                // these kind of errors time out.  See test_try_send_no_send_addr to try
                // this out.
                Ok(())
            }
        }
    }

    fn try_send_relay(
        &self,
        url: &RelayUrl,
        node: NodeId,
        contents: RelayContents,
    ) -> io::Result<()> {
        trace!(
            node = %node.fmt_short(),
            relay_url = %url,
            count = contents.len(),
            len = contents.iter().map(|c| c.len()).sum::<usize>(),
            "send relay",
        );
        let msg = RelayActorMessage::Send {
            url: url.clone(),
            contents,
            remote_node: node,
        };
        match self.relay_actor_sender.try_send(msg) {
            Ok(_) => {
                trace!(node = %node.fmt_short(), relay_url = %url,
                       "send relay: message queued");
                Ok(())
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                warn!(node = %node.fmt_short(), relay_url = %url,
                      "send relay: message dropped, channel to actor is closed");
                Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "channel to actor is closed",
                ))
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                warn!(node = %node.fmt_short(), relay_url = %url,
                      "send relay: message dropped, channel to actor is full");
                Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "channel to actor is full",
                ))
            }
        }
    }

    fn try_send_udp(&self, addr: SocketAddr, transmit: &quinn_udp::Transmit) -> io::Result<()> {
        let conn = self.conn_for_addr(addr)?;
        conn.try_send(transmit)?;
        let total_bytes: u64 = transmit.contents.len() as u64;
        if addr.is_ipv6() {
            inc_by!(MagicsockMetrics, send_ipv6, total_bytes);
        } else {
            inc_by!(MagicsockMetrics, send_ipv4, total_bytes);
        }
        Ok(())
    }

    fn conn_for_addr(&self, addr: SocketAddr) -> io::Result<&UdpConn> {
        let sock = match addr {
            SocketAddr::V4(_) => &self.pconn4,
            SocketAddr::V6(_) => self
                .pconn6
                .as_ref()
                .ok_or(io::Error::new(io::ErrorKind::Other, "no IPv6 connection"))?,
        };
        Ok(sock)
    }

    /// NOTE: Receiving on a [`Self::closed`] socket will return [`Poll::Pending`] indefinitely.
    #[instrument(skip_all)]
    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [io::IoSliceMut<'_>],
        metas: &mut [quinn_udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        // FIXME: currently ipv4 load results in ipv6 traffic being ignored
        debug_assert_eq!(bufs.len(), metas.len(), "non matching bufs & metas");
        if self.is_closed() {
            return Poll::Pending;
        }

        // order of polling is: UDPv4, UDPv6, relay
        let (msgs, from_ipv4) = match self.pconn4.poll_recv(cx, bufs, metas)? {
            Poll::Pending | Poll::Ready(0) => match &self.pconn6 {
                Some(conn) => match conn.poll_recv(cx, bufs, metas)? {
                    Poll::Pending | Poll::Ready(0) => {
                        return self.poll_recv_relay(cx, bufs, metas);
                    }
                    Poll::Ready(n) => (n, false),
                },
                None => {
                    return self.poll_recv_relay(cx, bufs, metas);
                }
            },
            Poll::Ready(n) => (n, true),
        };

        // Adding the IP address we received something on results in Quinn using this
        // address on the send path to send from.  However we let Quinn use a
        // QuicMappedAddress, not a real address.  So we used to substitute our bind address
        // here so that Quinn would send on the right address.  But that would sometimes
        // result in the wrong address family and Windows trips up on that.
        //
        // What should be done is that this dst_ip from the RecvMeta is stored in the
        // NodeState/PathState.  Then on the send path it should be retrieved from the
        // NodeState/PathSate together with the send address and substituted at send time.
        // This is relevant for IPv6 link-local addresses where the OS otherwise does not
        // know which intervace to send from.
        #[cfg(not(windows))]
        let dst_ip = self.normalized_local_addr().ok().map(|addr| addr.ip());
        // Reasoning for this here:
        // https://github.com/n0-computer/iroh/pull/2595#issuecomment-2290947319
        #[cfg(windows)]
        let dst_ip = None;

        let mut quic_packets_total = 0;

        for (meta, buf) in metas.iter_mut().zip(bufs.iter_mut()).take(msgs) {
            let mut is_quic = false;
            let mut quic_packets_count = 0;
            if meta.len > meta.stride {
                trace!(%meta.len, %meta.stride, "GRO datagram received");
                inc!(MagicsockMetrics, recv_gro_datagrams);
            }

            // find disco and stun packets and forward them to the actor
            for packet in buf[..meta.len].chunks_mut(meta.stride) {
                if packet.len() < meta.stride {
                    trace!(
                        len = %packet.len(),
                        %meta.stride,
                        "Last GRO datagram smaller than stride",
                    );
                }

                let packet_is_quic = if stun::is(packet) {
                    trace!(src = %meta.addr, len = %meta.stride, "UDP recv: stun packet");
                    let packet2 = Bytes::copy_from_slice(packet);
                    self.net_checker.receive_stun_packet(packet2, meta.addr);
                    false
                } else if let Some((sender, sealed_box)) = disco::source_and_box(packet) {
                    // Disco?
                    trace!(src = %meta.addr, len = %meta.stride, "UDP recv: disco packet");
                    self.handle_disco_message(
                        sender,
                        sealed_box,
                        DiscoMessageSource::Udp(meta.addr),
                    );
                    false
                } else {
                    trace!(src = %meta.addr, len = %meta.stride, "UDP recv: quic packet");
                    if from_ipv4 {
                        inc_by!(MagicsockMetrics, recv_data_ipv4, packet.len() as _);
                    } else {
                        inc_by!(MagicsockMetrics, recv_data_ipv6, packet.len() as _);
                    }
                    true
                };

                if packet_is_quic {
                    quic_packets_count += 1;
                    is_quic = true;
                } else {
                    // overwrite the first byte of the packets with zero.
                    // this makes quinn reliably and quickly ignore the packet as long as
                    // [`quinn::EndpointConfig::grease_quic_bit`] is set to `false`
                    // (which we always do in Endpoint::bind).
                    packet[0] = 0u8;
                }
            }

            if is_quic {
                // remap addr
                match self.node_map.receive_udp(meta.addr) {
                    None => {
                        warn!(src = ?meta.addr, count = %quic_packets_count, len = meta.len, "UDP recv quic packets: no node state found, skipping");
                        // if we have no node state for the from addr, set len to 0 to make quinn skip the buf completely.
                        meta.len = 0;
                    }
                    Some((node_id, quic_mapped_addr)) => {
                        trace!(src = ?meta.addr, node = %node_id.fmt_short(), count = %quic_packets_count, len = meta.len, "UDP recv quic packets");
                        quic_packets_total += quic_packets_count;
                        meta.addr = quic_mapped_addr.0;
                    }
                }
            } else {
                // if there is no non-stun,non-disco packet in the chunk, set len to zero to make
                // quinn skip the buf completely.
                meta.len = 0;
            }
            // Normalize local_ip
            meta.dst_ip = dst_ip;
        }

        if quic_packets_total > 0 {
            inc_by!(MagicsockMetrics, recv_datagrams, quic_packets_total as _);
            trace!("UDP recv: {} packets", quic_packets_total);
        }

        Poll::Ready(Ok(msgs))
    }

    #[instrument(skip_all)]
    fn poll_recv_relay(
        &self,
        cx: &mut Context,
        bufs: &mut [io::IoSliceMut<'_>],
        metas: &mut [quinn_udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        let mut num_msgs = 0;
        for (buf_out, meta_out) in bufs.iter_mut().zip(metas.iter_mut()) {
            if self.is_closed() {
                break;
            }
            let mut relay_recv_receiver = self.relay_recv_receiver.lock();
            match relay_recv_receiver.try_recv() {
                Err(mpsc::error::TryRecvError::Empty) => {
                    self.network_recv_wakers.lock().replace(cx.waker().clone());
                    break;
                }
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "connection closed",
                    )));
                }
                Ok(Err(err)) => return Poll::Ready(Err(err)),
                Ok(Ok((node_id, meta, bytes))) => {
                    inc_by!(MagicsockMetrics, recv_data_relay, bytes.len() as _);
                    trace!(src = %meta.addr, node = %node_id.fmt_short(), count = meta.len / meta.stride, len = meta.len, "recv quic packets from relay");
                    buf_out[..bytes.len()].copy_from_slice(&bytes);
                    *meta_out = meta;
                    num_msgs += 1;
                }
            }
        }

        // If we have any msgs to report, they are in the first `num_msgs_total` slots
        if num_msgs > 0 {
            inc_by!(MagicsockMetrics, recv_datagrams, num_msgs as _);
            Poll::Ready(Ok(num_msgs))
        } else {
            Poll::Pending
        }
    }

    /// Handles a discovery message.
    #[instrument("disco_in", skip_all, fields(node = %sender.fmt_short(), %src))]
    fn handle_disco_message(&self, sender: PublicKey, sealed_box: &[u8], src: DiscoMessageSource) {
        trace!("handle_disco_message start");
        if self.is_closed() {
            return;
        }

        // We're now reasonably sure we're expecting communication from
        // this node, do the heavy crypto lifting to see what they want.
        let dm = match self.disco_secrets.unseal_and_decode(
            &self.secret_key,
            sender,
            sealed_box.to_vec(),
        ) {
            Ok(dm) => dm,
            Err(DiscoBoxError::Open(err)) => {
                warn!(?err, "failed to open disco box");
                inc!(MagicsockMetrics, recv_disco_bad_key);
                return;
            }
            Err(DiscoBoxError::Parse(err)) => {
                // Couldn't parse it, but it was inside a correctly
                // signed box, so just ignore it, assuming it's from a
                // newer version of Tailscale that we don't
                // understand. Not even worth logging about, lest it
                // be too spammy for old clients.

                inc!(MagicsockMetrics, recv_disco_bad_parse);
                debug!(?err, "failed to parse disco message");
                return;
            }
        };

        if src.is_relay() {
            inc!(MagicsockMetrics, recv_disco_relay);
        } else {
            inc!(MagicsockMetrics, recv_disco_udp);
        }

        let span = trace_span!("handle_disco", ?dm);
        let _guard = span.enter();
        trace!("receive disco message");
        match dm {
            disco::Message::Ping(ping) => {
                inc!(MagicsockMetrics, recv_disco_ping);
                self.handle_ping(ping, sender, src);
            }
            disco::Message::Pong(pong) => {
                inc!(MagicsockMetrics, recv_disco_pong);
                self.node_map.handle_pong(sender, &src, pong);
            }
            disco::Message::CallMeMaybe(cm) => {
                inc!(MagicsockMetrics, recv_disco_call_me_maybe);
                match src {
                    DiscoMessageSource::Relay { url, .. } => {
                        event!(
                            target: "events.net.call-me-maybe.recv",
                            Level::DEBUG,
                            remote_node = sender.fmt_short(),
                            via = ?url,
                            their_addrs = ?cm.my_numbers,
                        );
                    }
                    _ => {
                        warn!("call-me-maybe packets should only come via relay");
                        return;
                    }
                }
                let ping_actions = self.node_map.handle_call_me_maybe(sender, cm);
                for action in ping_actions {
                    match action {
                        PingAction::SendCallMeMaybe { .. } => {
                            warn!("Unexpected CallMeMaybe as response of handling a CallMeMaybe");
                        }
                        PingAction::SendPing(ping) => {
                            self.send_ping_queued(ping);
                        }
                    }
                }
            }
        }
        trace!("disco message handled");
    }

    /// Handle a ping message.
    fn handle_ping(&self, dm: disco::Ping, sender: NodeId, src: DiscoMessageSource) {
        // Insert the ping into the node map, and return whether a ping with this tx_id was already
        // received.
        let addr: SendAddr = src.clone().into();
        let handled = self.node_map.handle_ping(sender, addr.clone(), dm.tx_id);
        match handled.role {
            PingRole::Duplicate => {
                debug!(%src, tx = %hex::encode(dm.tx_id), "received ping: path already confirmed, skip");
                return;
            }
            PingRole::LikelyHeartbeat => {}
            PingRole::NewPath => {
                debug!(%src, tx = %hex::encode(dm.tx_id), "received ping: new path");
            }
            PingRole::Activate => {
                debug!(%src, tx = %hex::encode(dm.tx_id), "received ping: path active");
            }
        }

        // Send a pong.
        debug!(tx = %hex::encode(dm.tx_id), %addr, dstkey = %sender.fmt_short(),
               "sending pong");
        let pong = disco::Message::Pong(disco::Pong {
            tx_id: dm.tx_id,
            ping_observed_addr: addr.clone(),
        });
        event!(
            target: "events.net.pong.sent",
            Level::DEBUG,
            remote_node = %sender.fmt_short(),
            dst = ?addr,
            txn = ?dm.tx_id,
        );

        if !self.send_disco_message_queued(addr.clone(), sender, pong) {
            warn!(%addr, "failed to queue pong");
        }

        if let Some(ping) = handled.needs_ping_back {
            debug!(
                %addr,
                dstkey = %sender.fmt_short(),
                "sending direct ping back",
            );
            self.send_ping_queued(ping);
        }
    }

    fn encode_disco_message(&self, dst_key: PublicKey, msg: &disco::Message) -> Bytes {
        self.disco_secrets
            .encode_and_seal(&self.secret_key, dst_key, msg)
    }

    fn send_ping_queued(&self, ping: SendPing) {
        let SendPing {
            id,
            dst,
            dst_node,
            tx_id,
            purpose,
        } = ping;
        let msg = disco::Message::Ping(disco::Ping {
            tx_id,
            node_key: self.public_key(),
        });
        let sent = match dst {
            SendAddr::Udp(addr) => self
                .udp_disco_sender
                .try_send((addr, dst_node, msg))
                .is_ok(),
            SendAddr::Relay(ref url) => self.send_disco_message_relay(url, dst_node, msg),
        };
        if sent {
            let msg_sender = self.actor_sender.clone();
            trace!(%dst, tx = %hex::encode(tx_id), ?purpose, "ping sent (queued)");
            self.node_map
                .notify_ping_sent(id, dst, tx_id, purpose, msg_sender);
        } else {
            warn!(dst = ?dst, tx = %hex::encode(tx_id), ?purpose, "failed to send ping: queues full");
        }
    }

    /// Tries to send the ping actions.
    ///
    /// Note that on failure the (remaining) ping actions are simply dropped.  That's bad!
    /// The Endpoint will think a full ping was done and not request a new full-ping for a
    /// while.  We should probably be buffering the pings.
    fn try_send_ping_actions(&self, msgs: Vec<PingAction>) -> io::Result<()> {
        for msg in msgs {
            // Abort sending as soon as we know we are shutting down.
            if self.is_closing() || self.is_closed() {
                return Ok(());
            }
            match msg {
                PingAction::SendCallMeMaybe {
                    ref relay_url,
                    dst_node,
                } => {
                    self.send_or_queue_call_me_maybe(relay_url, dst_node);
                }
                PingAction::SendPing(ping) => {
                    self.try_send_ping(ping)?;
                }
            }
        }
        Ok(())
    }

    /// Send a disco message. UDP messages will be queued.
    ///
    /// If `dst` is [`SendAddr::Relay`], the message will be pushed into the relay client channel.
    /// If `dst` is [`SendAddr::Udp`], the message will be pushed into the udp disco send channel.
    ///
    /// Returns true if the channel had capacity for the message, and false if the message was
    /// dropped.
    fn send_disco_message_queued(
        &self,
        dst: SendAddr,
        dst_key: PublicKey,
        msg: disco::Message,
    ) -> bool {
        match dst {
            SendAddr::Udp(addr) => self.udp_disco_sender.try_send((addr, dst_key, msg)).is_ok(),
            SendAddr::Relay(ref url) => self.send_disco_message_relay(url, dst_key, msg),
        }
    }

    /// Send a disco message. UDP messages will be polled to send directly on the UDP socket.
    fn try_send_disco_message(
        &self,
        dst: SendAddr,
        dst_key: PublicKey,
        msg: disco::Message,
    ) -> io::Result<()> {
        match dst {
            SendAddr::Udp(addr) => {
                self.try_send_disco_message_udp(addr, dst_key, &msg)?;
            }
            SendAddr::Relay(ref url) => {
                self.send_disco_message_relay(url, dst_key, msg);
            }
        }
        Ok(())
    }

    fn send_disco_message_relay(&self, url: &RelayUrl, dst: NodeId, msg: disco::Message) -> bool {
        debug!(node = %dst.fmt_short(), %url, %msg, "send disco message (relay)");
        let pkt = self.encode_disco_message(dst, &msg);
        inc!(MagicsockMetrics, send_disco_relay);
        match self.try_send_relay(url, dst, smallvec![pkt]) {
            Ok(()) => {
                if let disco::Message::CallMeMaybe(CallMeMaybe { ref my_numbers }) = msg {
                    event!(
                        target: "events.net.call-me-maybe.sent",
                        Level::DEBUG,
                        remote_node = %dst.fmt_short(),
                        via = ?url,
                        addrs = ?my_numbers,
                    );
                }
                inc!(MagicsockMetrics, sent_disco_relay);
                disco_message_sent(&msg);
                true
            }
            Err(_) => false,
        }
    }

    async fn send_disco_message_udp(
        &self,
        dst: SocketAddr,
        dst_node: NodeId,
        msg: &disco::Message,
    ) -> io::Result<()> {
        futures_lite::future::poll_fn(move |cx| {
            loop {
                match self.try_send_disco_message_udp(dst, dst_node, msg) {
                    Ok(()) => return Poll::Ready(Ok(())),
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                        // This is the socket .try_send_disco_message_udp used.
                        let sock = self.conn_for_addr(dst)?;
                        let sock = Arc::new(sock.clone());
                        let mut poller = sock.create_io_poller();
                        match poller.as_mut().poll_writable(cx)? {
                            Poll::Ready(()) => continue,
                            Poll::Pending => return Poll::Pending,
                        }
                    }
                    Err(err) => return Poll::Ready(Err(err)),
                }
            }
        })
        .await
    }

    fn try_send_disco_message_udp(
        &self,
        dst: SocketAddr,
        dst_node: NodeId,
        msg: &disco::Message,
    ) -> std::io::Result<()> {
        trace!(%dst, %msg, "send disco message (UDP)");
        if self.is_closed() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "connection closed",
            ));
        }
        let pkt = self.encode_disco_message(dst_node, msg);
        // TODO: These metrics will be wrong with the poll impl
        // Also - do we need it? I'd say the `sent_disco_udp` below is enough.
        inc!(MagicsockMetrics, send_disco_udp);
        let transmit = quinn_udp::Transmit {
            destination: dst,
            contents: &pkt,
            ecn: None,
            segment_size: None,
            src_ip: None, // TODO
        };
        let sent = self.try_send_udp(dst, &transmit);
        match sent {
            Ok(()) => {
                trace!(%dst, node = %dst_node.fmt_short(), %msg, "sent disco message");
                inc!(MagicsockMetrics, sent_disco_udp);
                disco_message_sent(msg);
                Ok(())
            }
            Err(err) => {
                warn!(%dst, node = %dst_node.fmt_short(), ?msg, ?err,
                      "failed to send disco message");
                Err(err)
            }
        }
    }

    #[instrument(skip_all)]
    async fn handle_ping_actions(&mut self, msgs: Vec<PingAction>) {
        // TODO: This used to make sure that all ping actions are sent.  Though on the
        // poll_send/try_send path we also do fire-and-forget.  try_send_ping_actions()
        // really should store any unsent pings on the Inner and send them at the next
        // possible time.
        if let Err(err) = self.try_send_ping_actions(msgs) {
            warn!("Not all ping actions were sent: {err:#}");
        }
    }

    fn try_send_ping(&self, ping: SendPing) -> io::Result<()> {
        let SendPing {
            id,
            dst,
            dst_node,
            tx_id,
            purpose,
        } = ping;
        let msg = disco::Message::Ping(disco::Ping {
            tx_id,
            node_key: self.public_key(),
        });
        self.try_send_disco_message(dst.clone(), dst_node, msg)?;
        debug!(%dst, tx = %hex::encode(tx_id), ?purpose, "ping sent (polled)");
        let msg_sender = self.actor_sender.clone();
        self.node_map
            .notify_ping_sent(id, dst.clone(), tx_id, purpose, msg_sender);
        Ok(())
    }

    fn poll_send_relay(
        &self,
        url: &RelayUrl,
        node: PublicKey,
        contents: RelayContents,
    ) -> Poll<bool> {
        trace!(node = %node.fmt_short(), relay_url = %url, count = contents.len(), len = contents.iter().map(|c| c.len()).sum::<usize>(), "send relay");
        let msg = RelayActorMessage::Send {
            url: url.clone(),
            contents,
            remote_node: node,
        };
        match self.relay_actor_sender.try_send(msg) {
            Ok(_) => {
                trace!(node = %node.fmt_short(), relay_url = %url, "send relay: message queued");
                Poll::Ready(true)
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                warn!(node = %node.fmt_short(), relay_url = %url, "send relay: message dropped, channel to actor is closed");
                Poll::Ready(false)
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                warn!(node = %node.fmt_short(), relay_url = %url, "send relay: message dropped, channel to actor is full");
                Poll::Pending
            }
        }
    }

    fn send_queued_call_me_maybes(&self) {
        let msg = self.direct_addrs.read().to_call_me_maybe_message();
        let msg = disco::Message::CallMeMaybe(msg);
        for (public_key, url) in self.pending_call_me_maybes.lock().drain() {
            if !self.send_disco_message_relay(&url, public_key, msg.clone()) {
                warn!(node = %public_key.fmt_short(), "relay channel full, dropping call-me-maybe");
            }
        }
    }

    fn send_or_queue_call_me_maybe(&self, url: &RelayUrl, dst_node: NodeId) {
        let direct_addrs = self.direct_addrs.read();
        if direct_addrs.fresh_enough() {
            let msg = direct_addrs.to_call_me_maybe_message();
            let msg = disco::Message::CallMeMaybe(msg);
            if !self.send_disco_message_relay(url, dst_node, msg) {
                warn!(dstkey = %dst_node.fmt_short(), relayurl = ?url,
                      "relay channel full, dropping call-me-maybe");
            } else {
                debug!(dstkey = %dst_node.fmt_short(), relayurl = ?url, "call-me-maybe sent");
            }
        } else {
            self.pending_call_me_maybes
                .lock()
                .insert(dst_node, url.clone());
            debug!(
                last_refresh_ago = ?direct_addrs.updated_at.map(|x| x.elapsed()),
                "want call-me-maybe but direct addrs stale; queuing after restun",
            );
            self.re_stun("refresh-for-peering");
        }
    }

    /// Triggers an address discovery. The provided why string is for debug logging only.
    #[instrument(skip_all)]
    fn re_stun(&self, why: &'static str) {
        debug!("re_stun: {}", why);
        inc!(MagicsockMetrics, re_stun_calls);
        self.direct_addr_update_state.schedule_run(why);
    }

    /// Publishes our address to a discovery service, if configured.
    ///
    /// Called whenever our addresses or home relay node changes.
    fn publish_my_addr(&self) {
        if let Some(ref discovery) = self.discovery {
            let addrs = self.direct_addrs.read();
            let relay_url = self.my_relay();
            let direct_addresses = addrs.iter().map(|da| da.addr).collect();
            let info = AddrInfo {
                relay_url,
                direct_addresses,
            };
            discovery.publish(&info);
        }
    }
}

#[derive(Clone, Debug)]
enum DiscoMessageSource {
    Udp(SocketAddr),
    Relay { url: RelayUrl, key: PublicKey },
}

impl Display for DiscoMessageSource {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::Udp(addr) => write!(f, "Udp({addr})"),
            Self::Relay { ref url, key } => write!(f, "Relay({url}, {})", key.fmt_short()),
        }
    }
}

impl From<DiscoMessageSource> for SendAddr {
    fn from(value: DiscoMessageSource) -> Self {
        match value {
            DiscoMessageSource::Udp(addr) => SendAddr::Udp(addr),
            DiscoMessageSource::Relay { url, .. } => SendAddr::Relay(url),
        }
    }
}

impl From<&DiscoMessageSource> for SendAddr {
    fn from(value: &DiscoMessageSource) -> Self {
        match value {
            DiscoMessageSource::Udp(addr) => SendAddr::Udp(*addr),
            DiscoMessageSource::Relay { url, .. } => SendAddr::Relay(url.clone()),
        }
    }
}

impl DiscoMessageSource {
    fn is_relay(&self) -> bool {
        matches!(self, DiscoMessageSource::Relay { .. })
    }
}

/// Manages currently running direct addr discovery, aka netcheck runs.
///
/// Invariants:
/// - only one direct addr update must be running at a time
/// - if an update is scheduled while another one is running, remember that
///   and start a new one when the current one has finished
#[derive(Debug)]
struct DirectAddrUpdateState {
    /// If running, set to the reason for the currently the update.
    running: sync::watch::Sender<Option<&'static str>>,
    /// If set, start a new update as soon as the current one is finished.
    want_update: parking_lot::Mutex<Option<&'static str>>,
}

impl DirectAddrUpdateState {
    fn new() -> Self {
        let (running, _) = sync::watch::channel(None);
        DirectAddrUpdateState {
            running,
            want_update: Default::default(),
        }
    }

    /// Schedules a new run, either starting it immediately if none is running or
    /// scheduling it for later.
    fn schedule_run(&self, why: &'static str) {
        if self.is_running() {
            let _ = self.want_update.lock().insert(why);
        } else {
            self.run(why);
        }
    }

    /// Returns `true` if an update is currently in progress.
    fn is_running(&self) -> bool {
        self.running.borrow().is_some()
    }

    /// Trigger a new run.
    fn run(&self, why: &'static str) {
        self.running.send(Some(why)).ok();
    }

    /// Clears the current running state.
    fn finish_run(&self) {
        self.running.send(None).ok();
    }

    /// Returns the next update, if one is set.
    fn next_update(&self) -> Option<&'static str> {
        self.want_update.lock().take()
    }
}

impl Handle {
    /// Creates a magic [`MagicSock`] listening on [`Options::addr_v4`] and [`Options::addr_v6`].
    async fn new(opts: Options) -> Result<Self> {
        let me = opts.secret_key.public().fmt_short();
        if crate::util::relay_only_mode() {
            warn!(
                "creating a MagicSock that will only send packets over a relay relay connection."
            );
        }

        Self::with_name(me, opts)
            .instrument(error_span!("magicsock"))
            .await
    }

    async fn with_name(me: String, opts: Options) -> Result<Self> {
        let port_mapper = portmapper::Client::default();

        let Options {
            addr_v4,
            addr_v6,
            secret_key,
            relay_map,
            node_map,
            discovery,
            dns_resolver,
            proxy_url,
            #[cfg(any(test, feature = "test-utils"))]
            insecure_skip_relay_cert_verify,
        } = opts;

        let (relay_recv_sender, relay_recv_receiver) = mpsc::channel(128);

        let (pconn4, pconn6) = bind(addr_v4, addr_v6)?;
        let port = pconn4.port();

        // NOTE: we can end up with a zero port if `std::net::UdpSocket::socket_addr` fails
        match port.try_into() {
            Ok(non_zero_port) => {
                port_mapper.update_local_port(non_zero_port);
            }
            Err(_zero_port) => debug!("Skipping port mapping with zero local port"),
        }
        let ipv4_addr = pconn4.local_addr()?;
        let ipv6_addr = pconn6.as_ref().and_then(|c| c.local_addr().ok());

        let net_checker = netcheck::Client::new(Some(port_mapper.clone()), dns_resolver.clone())?;

        let (actor_sender, actor_receiver) = mpsc::channel(256);
        let (relay_actor_sender, relay_actor_receiver) = mpsc::channel(256);
        let (udp_disco_sender, mut udp_disco_receiver) = mpsc::channel(256);

        // load the node data
        let node_map = node_map.unwrap_or_default();
        let node_map = NodeMap::load_from_vec(node_map);

        let inner = Arc::new(MagicSock {
            me,
            port: AtomicU16::new(port),
            secret_key,
            proxy_url,
            local_addrs: std::sync::RwLock::new((ipv4_addr, ipv6_addr)),
            closing: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            relay_recv_receiver: parking_lot::Mutex::new(relay_recv_receiver),
            network_recv_wakers: parking_lot::Mutex::new(None),
            network_send_wakers: Arc::new(parking_lot::Mutex::new(None)),
            actor_sender: actor_sender.clone(),
            ipv6_reported: Arc::new(AtomicBool::new(false)),
            relay_map,
            my_relay: Default::default(),
            pconn4: pconn4.clone(),
            pconn6: pconn6.clone(),
            net_checker: net_checker.addr(),
            disco_secrets: DiscoSecrets::default(),
            node_map,
            relay_actor_sender: relay_actor_sender.clone(),
            udp_disco_sender,
            discovery,
            direct_addrs: Watchable::new(Default::default()),
            pending_call_me_maybes: Default::default(),
            direct_addr_update_state: DirectAddrUpdateState::new(),
            dns_resolver,
            #[cfg(any(test, feature = "test-utils"))]
            insecure_skip_relay_cert_verify,
        });

        let mut actor_tasks = JoinSet::default();

        let relay_actor = RelayActor::new(inner.clone(), actor_sender.clone());
        let relay_actor_cancel_token = relay_actor.cancel_token();
        actor_tasks.spawn(
            async move {
                relay_actor.run(relay_actor_receiver).await;
            }
            .instrument(info_span!("relay-actor")),
        );

        let inner2 = inner.clone();
        actor_tasks.spawn(async move {
            while let Some((dst, dst_key, msg)) = udp_disco_receiver.recv().await {
                if let Err(err) = inner2.send_disco_message_udp(dst, dst_key, &msg).await {
                    warn!(%dst, node = %dst_key.fmt_short(), ?err, "failed to send disco message (UDP)");
                }
            }
        });

        let inner2 = inner.clone();
        let network_monitor = netmon::Monitor::new().await?;
        actor_tasks.spawn(
            async move {
                let actor = Actor {
                    msg_receiver: actor_receiver,
                    msg_sender: actor_sender,
                    relay_actor_sender,
                    relay_actor_cancel_token,
                    msock: inner2,
                    relay_recv_sender,
                    periodic_re_stun_timer: new_re_stun_timer(false),
                    net_info_last: None,
                    port_mapper,
                    pconn4,
                    pconn6,
                    no_v4_send: false,
                    net_checker,
                    network_monitor,
                };

                if let Err(err) = actor.run().await {
                    warn!("relay handler errored: {:?}", err);
                }
            }
            .instrument(info_span!("actor")),
        );

        let c = Handle {
            msock: inner,
            actor_tasks: Arc::new(Mutex::new(actor_tasks)),
        };

        Ok(c)
    }

    /// Closes the connection.
    ///
    /// Only the first close does anything. Any later closes return nil.
    /// Polling the socket ([`AsyncUdpSocket::poll_recv`]) will return [`Poll::Pending`]
    /// indefinitely after this call.
    #[instrument(skip_all, fields(me = %self.msock.me))]
    pub(crate) async fn close(&self) -> Result<()> {
        if self.msock.is_closed() {
            return Ok(());
        }
        self.msock.closing.store(true, Ordering::Relaxed);
        self.msock.actor_sender.send(ActorMessage::Shutdown).await?;
        self.msock.closed.store(true, Ordering::SeqCst);
        self.msock.direct_addrs.shutdown();

        let mut tasks = self.actor_tasks.lock().await;

        // give the tasks a moment to shutdown cleanly
        let tasks_ref = &mut tasks;
        let shutdown_done = time::timeout(Duration::from_millis(100), async move {
            while let Some(task) = tasks_ref.join_next().await {
                if let Err(err) = task {
                    warn!("unexpected error in task shutdown: {:?}", err);
                }
            }
        })
        .await;
        if shutdown_done.is_ok() {
            debug!("tasks shutdown complete");
        } else {
            // shutdown all tasks
            debug!("aborting remaining {}/3 tasks", tasks.len());
            tasks.shutdown().await;
        }

        Ok(())
    }
}

/// Stream returning local endpoints as they change.
#[derive(Debug)]
pub struct DirectAddrsStream {
    initial: Option<DiscoveredDirectAddrs>,
    inner: watchable::WatcherStream<DiscoveredDirectAddrs>,
}

impl Stream for DirectAddrsStream {
    type Item = Vec<DirectAddr>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        if let Some(initial_endpoints) = this.initial.take() {
            if !initial_endpoints.is_empty() {
                return Poll::Ready(Some(initial_endpoints.into_iter().collect()));
            }
        }
        loop {
            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Pending => break Poll::Pending,
                Poll::Ready(Some(discovered)) => {
                    if discovered.is_empty() {
                        // When we start up we might initially have empty local endpoints as
                        // the magic socket has not yet figured this out.  Later on this set
                        // should never be empty.  However even if it was the magicsock
                        // would be in a state not very usable so skipping those events is
                        // probably fine.
                        // To make sure we install the right waker we loop rather than
                        // returning Poll::Pending immediately here.
                        continue;
                    } else {
                        break Poll::Ready(Some(discovered.into_iter().collect()));
                    }
                }
                Poll::Ready(None) => break Poll::Ready(None),
            }
        }
    }
}

#[derive(Debug, Default)]
struct DiscoSecrets(parking_lot::Mutex<HashMap<PublicKey, SharedSecret>>);

impl DiscoSecrets {
    fn get(
        &self,
        secret: &SecretKey,
        node_id: PublicKey,
    ) -> parking_lot::MappedMutexGuard<SharedSecret> {
        parking_lot::MutexGuard::map(self.0.lock(), |inner| {
            inner
                .entry(node_id)
                .or_insert_with(|| secret.shared(&node_id))
        })
    }

    pub fn encode_and_seal(
        &self,
        secret_key: &SecretKey,
        node_id: PublicKey,
        msg: &disco::Message,
    ) -> Bytes {
        let mut seal = msg.as_bytes();
        self.get(secret_key, node_id).seal(&mut seal);
        disco::encode_message(&secret_key.public(), seal).into()
    }

    pub fn unseal_and_decode(
        &self,
        secret: &SecretKey,
        node_id: PublicKey,
        mut sealed_box: Vec<u8>,
    ) -> Result<disco::Message, DiscoBoxError> {
        self.get(secret, node_id)
            .open(&mut sealed_box)
            .map_err(DiscoBoxError::Open)?;
        disco::Message::from_bytes(&sealed_box).map_err(DiscoBoxError::Parse)
    }
}

#[derive(Debug, thiserror::Error)]
enum DiscoBoxError {
    #[error("Failed to open crypto box")]
    Open(anyhow::Error),
    #[error("Failed to parse disco message")]
    Parse(anyhow::Error),
}

type RelayRecvResult = Result<(PublicKey, quinn_udp::RecvMeta, Bytes), io::Error>;

/// Reports whether x and y represent the same set of endpoints. The order doesn't matter.
fn endpoint_sets_equal(xs: &[DirectAddr], ys: &[DirectAddr]) -> bool {
    if xs.is_empty() && ys.is_empty() {
        return true;
    }
    if xs.len() == ys.len() {
        let mut order_matches = true;
        for (i, x) in xs.iter().enumerate() {
            if x != &ys[i] {
                order_matches = false;
                break;
            }
        }
        if order_matches {
            return true;
        }
    }
    let mut m: HashMap<&DirectAddr, usize> = HashMap::new();
    for x in xs {
        *m.entry(x).or_default() |= 1;
    }
    for y in ys {
        *m.entry(y).or_default() |= 2;
    }

    m.values().all(|v| *v == 3)
}

impl AsyncUdpSocket for Handle {
    fn create_io_poller(self: Arc<Self>) -> Pin<Box<dyn quinn::UdpPoller>> {
        self.msock.create_io_poller()
    }

    fn try_send(&self, transmit: &quinn_udp::Transmit) -> io::Result<()> {
        self.msock.try_send(transmit)
    }

    /// NOTE: Receiving on a [`Self::close`]d socket will return [`Poll::Pending`] indefinitely.
    fn poll_recv(
        &self,
        cx: &mut Context,
        bufs: &mut [io::IoSliceMut<'_>],
        metas: &mut [quinn_udp::RecvMeta],
    ) -> Poll<io::Result<usize>> {
        self.msock.poll_recv(cx, bufs, metas)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        match &*self.msock.local_addrs.read().expect("not poisoned") {
            (ipv4, None) => {
                // Pretend to be IPv6, because our QuinnMappedAddrs
                // need to be IPv6.
                let ip: IpAddr = match ipv4.ip() {
                    IpAddr::V4(ip) => ip.to_ipv6_mapped().into(),
                    IpAddr::V6(ip) => ip.into(),
                };
                Ok(SocketAddr::new(ip, ipv4.port()))
            }
            (_, Some(ipv6)) => Ok(*ipv6),
        }
    }

    fn max_transmit_segments(&self) -> usize {
        if let Some(pconn6) = self.pconn6.as_ref() {
            std::cmp::min(
                pconn6.max_transmit_segments(),
                self.pconn4.max_transmit_segments(),
            )
        } else {
            self.pconn4.max_transmit_segments()
        }
    }

    fn max_receive_segments(&self) -> usize {
        if let Some(pconn6) = self.pconn6.as_ref() {
            // `max_receive_segments` controls the size of the `RecvMeta` buffer
            // that quinn creates. Having buffers slightly bigger than necessary
            // isn't terrible, and makes sure a single socket can read the maximum
            // amount with a single poll. We considered adding these numbers instead,
            // but we never get data from both sockets at the same time in `poll_recv`
            // and it's impossible and unnecessary to be refactored that way.
            std::cmp::max(
                pconn6.max_receive_segments(),
                self.pconn4.max_receive_segments(),
            )
        } else {
            self.pconn4.max_receive_segments()
        }
    }

    fn may_fragment(&self) -> bool {
        if let Some(pconn6) = self.pconn6.as_ref() {
            pconn6.may_fragment() || self.pconn4.may_fragment()
        } else {
            self.pconn4.may_fragment()
        }
    }
}

#[derive(Debug)]
struct IoPoller {
    ipv4_poller: Pin<Box<dyn quinn::UdpPoller>>,
    ipv6_poller: Option<Pin<Box<dyn quinn::UdpPoller>>>,
    relay_sender: mpsc::Sender<RelayActorMessage>,
    relay_send_waker: Arc<parking_lot::Mutex<Option<Waker>>>,
}

impl quinn::UdpPoller for IoPoller {
    fn poll_writable(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        // This version returns Ready as soon as any of them are ready.
        let this = &mut *self;
        match this.ipv4_poller.as_mut().poll_writable(cx) {
            Poll::Ready(_) => return Poll::Ready(Ok(())),
            Poll::Pending => (),
        }
        if let Some(ref mut ipv6_poller) = this.ipv6_poller {
            match ipv6_poller.as_mut().poll_writable(cx) {
                Poll::Ready(_) => return Poll::Ready(Ok(())),
                Poll::Pending => (),
            }
        }
        match this.relay_sender.capacity() {
            0 => {
                self.relay_send_waker.lock().replace(cx.waker().clone());
                Poll::Pending
            }
            _ => Poll::Ready(Ok(())),
        }
    }
}

#[derive(Debug)]
enum ActorMessage {
    Shutdown,
    ReceiveRelay(RelayReadResult),
    EndpointPingExpired(usize, stun::TransactionId),
    NetcheckReport(Result<Option<Arc<netcheck::Report>>>, &'static str),
    NetworkChange,
    #[cfg(test)]
    ForceNetworkChange(bool),
}

struct Actor {
    msock: Arc<MagicSock>,
    msg_receiver: mpsc::Receiver<ActorMessage>,
    msg_sender: mpsc::Sender<ActorMessage>,
    relay_actor_sender: mpsc::Sender<RelayActorMessage>,
    relay_actor_cancel_token: CancellationToken,
    /// Channel to send received relay messages on, for processing.
    relay_recv_sender: mpsc::Sender<RelayRecvResult>,
    /// When set, is an AfterFunc timer that will call MagicSock::do_periodic_stun.
    periodic_re_stun_timer: time::Interval,
    /// The `NetInfo` provided in the last call to `net_info_func`. It's used to deduplicate calls to netInfoFunc.
    net_info_last: Option<NetInfo>,

    // The underlying UDP sockets used to send/rcv packets.
    pconn4: UdpConn,
    pconn6: Option<UdpConn>,

    /// The NAT-PMP/PCP/UPnP prober/client, for requesting port mappings from NAT devices.
    port_mapper: portmapper::Client,

    /// Whether IPv4 UDP is known to be unable to transmit
    /// at all. This could happen if the socket is in an invalid state
    /// (as can happen on darwin after a network link status change).
    no_v4_send: bool,

    /// The prober that discovers local network conditions, including the closest relay relay and NAT mappings.
    net_checker: netcheck::Client,

    network_monitor: netmon::Monitor,
}

impl Actor {
    async fn run(mut self) -> Result<()> {
        // Setup network monitoring
        let (link_change_s, mut link_change_r) = mpsc::channel(8);
        let _token = self
            .network_monitor
            .subscribe(move |is_major| {
                let link_change_s = link_change_s.clone();
                async move {
                    link_change_s.send(is_major).await.ok();
                }
                .boxed()
            })
            .await?;

        // Let the the heartbeat only start a couple seconds later
        let mut direct_addr_heartbeat_timer = time::interval_at(
            time::Instant::now() + HEARTBEAT_INTERVAL,
            HEARTBEAT_INTERVAL,
        );
        let mut direct_addr_update_receiver =
            self.msock.direct_addr_update_state.running.subscribe();
        let mut portmap_watcher = self.port_mapper.watch_external_address();

        let mut discovery_events: BoxStream<DiscoveryItem> =
            Box::pin(futures_lite::stream::empty());
        if let Some(d) = self.msock.discovery() {
            if let Some(events) = d.subscribe() {
                discovery_events = events;
            }
        }
        loop {
            inc!(Metrics, actor_tick_main);
            tokio::select! {
                Some(msg) = self.msg_receiver.recv() => {
                    trace!(?msg, "tick: msg");
                    inc!(Metrics, actor_tick_msg);
                    if self.handle_actor_message(msg).await {
                        return Ok(());
                    }
                }
                tick = self.periodic_re_stun_timer.tick() => {
                    trace!("tick: re_stun {:?}", tick);
                    inc!(Metrics, actor_tick_re_stun);
                    self.msock.re_stun("periodic");
                }
                Ok(()) = portmap_watcher.changed() => {
                    trace!("tick: portmap changed");
                    inc!(Metrics, actor_tick_portmap_changed);
                    let new_external_address = *portmap_watcher.borrow();
                    debug!("external address updated: {new_external_address:?}");
                    self.msock.re_stun("portmap_updated");
                },
                _ = direct_addr_heartbeat_timer.tick() => {
                    trace!(
                        "tick: direct addr heartbeat {} direct addrs",
                        self.msock.node_map.node_count(),
                    );
                    inc!(Metrics, actor_tick_direct_addr_heartbeat);
                    // TODO: this might trigger too many packets at once, pace this

                    self.msock.node_map.prune_inactive();
                    let msgs = self.msock.node_map.nodes_stayin_alive();
                    self.handle_ping_actions(msgs).await;
                }
                _ = direct_addr_update_receiver.changed() => {
                    let reason = *direct_addr_update_receiver.borrow();
                    trace!("tick: direct addr update receiver {:?}", reason);
                    inc!(Metrics, actor_tick_direct_addr_update_receiver);
                    if let Some(reason) = reason {
                        self.update_direct_addrs(reason).await;
                    }
                }
                Some(is_major) = link_change_r.recv() => {
                    trace!("tick: link change {}", is_major);
                    inc!(Metrics, actor_link_change);
                    self.handle_network_change(is_major).await;
                }
                Some(discovery_item) = discovery_events.next() => {
                    trace!("tick: discovery event, address discovered: {discovery_item:?}");
                    let node_addr = NodeAddr {node_id: discovery_item.node_id, info: discovery_item.addr_info};
                    if let Err(e) = self.msock.add_node_addr(node_addr.clone(), Source::Discovery { name: discovery_item.provenance.into() }) {
                        warn!(?node_addr, "unable to add discovered node address to the node map: {e:?}");
                    }
                }
                else => {
                    trace!("tick: other");
                    inc!(Metrics, actor_tick_other);
                }
            }
        }
    }

    async fn handle_network_change(&mut self, is_major: bool) {
        debug!("link change detected: major? {}", is_major);

        if is_major {
            self.msock.dns_resolver.clear_cache();
            self.msock.re_stun("link-change-major");
            self.close_stale_relay_connections().await;
            self.reset_endpoint_states();
        } else {
            self.msock.re_stun("link-change-minor");
        }
    }

    #[instrument(skip_all)]
    async fn handle_ping_actions(&mut self, msgs: Vec<PingAction>) {
        // TODO: This used to make sure that all ping actions are sent.  Though on the
        // poll_send/try_send path we also do fire-and-forget.  try_send_ping_actions()
        // really should store any unsent pings on the Inner and send them at the next
        // possible time.
        if let Err(err) = self.msock.try_send_ping_actions(msgs) {
            warn!("Not all ping actions were sent: {err:#}");
        }
    }

    /// Processes an incoming actor message.
    ///
    /// Returns `true` if it was a shutdown.
    async fn handle_actor_message(&mut self, msg: ActorMessage) -> bool {
        match msg {
            ActorMessage::Shutdown => {
                debug!("shutting down");

                self.msock.node_map.notify_shutdown();
                self.port_mapper.deactivate();
                self.relay_actor_cancel_token.cancel();

                // Ignore errors from pconnN
                // They will frequently have been closed already by a call to connBind.Close.
                debug!("stopping connections");
                if let Some(ref conn) = self.pconn6 {
                    conn.close().await.ok();
                }
                self.pconn4.close().await.ok();

                debug!("shutdown complete");
                return true;
            }
            ActorMessage::ReceiveRelay(read_result) => {
                let passthroughs = self.process_relay_read_result(read_result);
                for passthrough in passthroughs {
                    self.relay_recv_sender
                        .send(passthrough)
                        .await
                        .expect("missing recv sender");
                    let mut wakers = self.msock.network_recv_wakers.lock();
                    if let Some(waker) = wakers.take() {
                        waker.wake();
                    }
                }
            }
            ActorMessage::EndpointPingExpired(id, txid) => {
                self.msock.node_map.notify_ping_timeout(id, txid);
            }
            ActorMessage::NetcheckReport(report, why) => {
                match report {
                    Ok(report) => {
                        self.handle_netcheck_report(report).await;
                    }
                    Err(err) => {
                        warn!("failed to generate netcheck report for: {}: {:?}", why, err);
                    }
                }
                self.finalize_direct_addrs_update(why);
            }
            ActorMessage::NetworkChange => {
                self.network_monitor.network_change().await.ok();
            }
            #[cfg(test)]
            ActorMessage::ForceNetworkChange(is_major) => {
                self.handle_network_change(is_major).await;
            }
        }

        false
    }

    #[cfg_attr(windows, allow(dead_code))]
    fn normalized_local_addr(&self) -> io::Result<SocketAddr> {
        self.msock.normalized_local_addr()
    }

    fn process_relay_read_result(&mut self, dm: RelayReadResult) -> Vec<RelayRecvResult> {
        trace!("process_relay_read {} bytes", dm.buf.len());
        if dm.buf.is_empty() {
            warn!("received empty relay packet");
            return Vec::new();
        }
        let url = &dm.url;

        let quic_mapped_addr = self.msock.node_map.receive_relay(url, dm.src);

        // the relay packet is made up of multiple udp packets, prefixed by a u16 be length prefix
        //
        // split the packet into these parts
        let parts = PacketSplitIter::new(dm.buf);
        // Normalize local_ip
        #[cfg(not(windows))]
        let dst_ip = self.normalized_local_addr().ok().map(|addr| addr.ip());
        // Reasoning for this here: https://github.com/n0-computer/iroh/pull/2595#issuecomment-2290947319
        #[cfg(windows)]
        let dst_ip = None;

        let mut out = Vec::new();
        for part in parts {
            match part {
                Ok(part) => {
                    if self.handle_relay_disco_message(&part, url, dm.src) {
                        // Message was internal, do not bubble up.
                        continue;
                    }

                    let meta = quinn_udp::RecvMeta {
                        len: part.len(),
                        stride: part.len(),
                        addr: quic_mapped_addr.0,
                        dst_ip,
                        ecn: None,
                    };
                    out.push(Ok((dm.src, meta, part)));
                }
                Err(e) => {
                    out.push(Err(e));
                }
            }
        }

        out
    }

    /// Refreshes knowledge about our direct addresses.
    ///
    /// In other words, this triggers a netcheck run.
    ///
    /// Note that invoking this is managed by the [`DirectAddrUpdateState`] and this should
    /// never be invoked directly.  Some day this will be refactored to not allow this easy
    /// mistake to be made.
    #[instrument(level = "debug", skip_all)]
    async fn update_direct_addrs(&mut self, why: &'static str) {
        inc!(MagicsockMetrics, update_direct_addrs);

        debug!("starting direct addr update ({})", why);
        self.port_mapper.procure_mapping();
        self.update_net_info(why).await;
    }

    /// Stores the results of a successful direct addr update.
    async fn store_direct_addr_update(&mut self, nr: Option<Arc<netcheck::Report>>) {
        let portmap_watcher = self.port_mapper.watch_external_address();

        // direct_addr -> how it was found
        let mut already = HashMap::new();
        // unique direct addrs
        let mut addrs = Vec::new();

        macro_rules! add_addr {
            ($already:expr, $addrs:expr, $ipp:expr, $typ:expr) => {
                #[allow(clippy::map_entry)]
                if !$already.contains_key(&$ipp) {
                    $already.insert($ipp, $typ);
                    $addrs.push(DirectAddr {
                        addr: $ipp,
                        typ: $typ,
                    });
                }
            };
        }

        let maybe_port_mapped = *portmap_watcher.borrow();

        if let Some(portmap_ext) = maybe_port_mapped.map(SocketAddr::V4) {
            add_addr!(already, addrs, portmap_ext, DirectAddrType::Portmapped);
            self.set_net_info_have_port_map().await;
        }

        if let Some(nr) = nr {
            if let Some(global_v4) = nr.global_v4 {
                add_addr!(already, addrs, global_v4.into(), DirectAddrType::Stun);

                // If they're behind a hard NAT and are using a fixed
                // port locally, assume they might've added a static
                // port mapping on their router to the same explicit
                // port that we are running with. Worst case it's an invalid candidate mapping.
                let port = self.msock.port.load(Ordering::Relaxed);
                if nr.mapping_varies_by_dest_ip.unwrap_or_default() && port != 0 {
                    let mut addr = global_v4;
                    addr.set_port(port);
                    add_addr!(already, addrs, addr.into(), DirectAddrType::Stun4LocalPort);
                }
            }
            if let Some(global_v6) = nr.global_v6 {
                add_addr!(already, addrs, global_v6.into(), DirectAddrType::Stun);
            }
        }
        let local_addr_v4 = self.pconn4.local_addr().ok();
        let local_addr_v6 = self.pconn6.as_ref().and_then(|c| c.local_addr().ok());

        let is_unspecified_v4 = local_addr_v4
            .map(|a| a.ip().is_unspecified())
            .unwrap_or(false);
        let is_unspecified_v6 = local_addr_v6
            .map(|a| a.ip().is_unspecified())
            .unwrap_or(false);

        let msock = self.msock.clone();

        tokio::spawn(
            async move {
                // Depending on the OS and network interfaces attached and their state enumerating
                // the local interfaces can take a long time.  Especially Windows is very slow.
                let LocalAddresses {
                    regular: mut ips,
                    loopback,
                } = tokio::task::spawn_blocking(LocalAddresses::new)
                    .await
                    .unwrap();

                if is_unspecified_v4 || is_unspecified_v6 {
                    if ips.is_empty() && addrs.is_empty() {
                        // Only include loopback addresses if we have no
                        // interfaces at all to use as direct addrs and don't
                        // have a public IPv4 or IPv6 address. This allows
                        // for localhost testing when you're on a plane and
                        // offline, for example.
                        ips = loopback;
                    }
                    let v4_port = local_addr_v4.and_then(|addr| {
                        if addr.ip().is_unspecified() {
                            Some(addr.port())
                        } else {
                            None
                        }
                    });

                    let v6_port = local_addr_v6.and_then(|addr| {
                        if addr.ip().is_unspecified() {
                            Some(addr.port())
                        } else {
                            None
                        }
                    });

                    for ip in ips {
                        match ip {
                            IpAddr::V4(_) => {
                                if let Some(port) = v4_port {
                                    add_addr!(
                                        already,
                                        addrs,
                                        SocketAddr::new(ip, port),
                                        DirectAddrType::Local
                                    );
                                }
                            }
                            IpAddr::V6(_) => {
                                if let Some(port) = v6_port {
                                    add_addr!(
                                        already,
                                        addrs,
                                        SocketAddr::new(ip, port),
                                        DirectAddrType::Local
                                    );
                                }
                            }
                        }
                    }
                }

                if !is_unspecified_v4 {
                    if let Some(addr) = local_addr_v4 {
                        // Our local socket is bound to a particular address.
                        // Do not offer addresses on other local interfaces.
                        add_addr!(already, addrs, addr, DirectAddrType::Local);
                    }
                }

                if !is_unspecified_v6 {
                    if let Some(addr) = local_addr_v6 {
                        // Our local socket is bound to a particular address.
                        // Do not offer addresses on other local interfaces.
                        add_addr!(already, addrs, addr, DirectAddrType::Local);
                    }
                }

                // Note: the direct addrs are intentionally returned in priority order,
                // from "farthest but most reliable" to "closest but least
                // reliable." Addresses returned from STUN should be globally
                // addressable, but might go farther on the network than necessary.
                // Local interface addresses might have lower latency, but not be
                // globally addressable.
                //
                // The STUN address(es) are always first.
                // Despite this sorting, clients are not relying on this sorting for decisions;

                msock.update_direct_addresses(addrs);

                // Regardless of whether our direct addrs changed, we now want to send any
                // queued call-me-maybe messages.
                msock.send_queued_call_me_maybes();
            }
            .instrument(Span::current()),
        );
    }

    /// Called when a direct addr update is done, no matter if it was successful or not.
    fn finalize_direct_addrs_update(&mut self, why: &'static str) {
        let new_why = self.msock.direct_addr_update_state.next_update();
        if !self.msock.is_closed() {
            if let Some(new_why) = new_why {
                self.msock.direct_addr_update_state.run(new_why);
                return;
            }
            self.periodic_re_stun_timer = new_re_stun_timer(true);
        }

        self.msock.direct_addr_update_state.finish_run();
        debug!("direct addr update done ({})", why);
    }

    /// Updates `NetInfo.HavePortMap` to true.
    #[instrument(level = "debug", skip_all)]
    async fn set_net_info_have_port_map(&mut self) {
        if let Some(ref mut net_info_last) = self.net_info_last {
            if net_info_last.have_port_map {
                // No change.
                return;
            }
            net_info_last.have_port_map = true;
            self.net_info_last = Some(net_info_last.clone());
        }
    }

    #[instrument(level = "debug", skip_all)]
    async fn call_net_info_callback(&mut self, ni: NetInfo) {
        if let Some(ref net_info_last) = self.net_info_last {
            if ni.basically_equal(net_info_last) {
                return;
            }
        }

        self.net_info_last = Some(ni);
    }

    /// Calls netcheck.
    ///
    /// Note that invoking this is managed by [`DirectAddrUpdateState`] via
    /// [`Actor::update_direct_addrs`] and this should never be invoked directly.  Some day
    /// this will be refactored to not allow this easy mistake to be made.
    #[instrument(level = "debug", skip_all)]
    async fn update_net_info(&mut self, why: &'static str) {
        if self.msock.relay_map.is_empty() {
            debug!("skipping netcheck, empty RelayMap");
            self.msg_sender
                .send(ActorMessage::NetcheckReport(Ok(None), why))
                .await
                .ok();
            return;
        }

        let relay_map = self.msock.relay_map.clone();
        let pconn4 = Some(self.pconn4.as_socket());
        let pconn6 = self.pconn6.as_ref().map(|p| p.as_socket());

        debug!("requesting netcheck report");
        match self
            .net_checker
            .get_report_channel(relay_map, pconn4, pconn6)
            .await
        {
            Ok(rx) => {
                let msg_sender = self.msg_sender.clone();
                tokio::task::spawn(async move {
                    let report = time::timeout(NETCHECK_REPORT_TIMEOUT, rx).await;
                    let report: anyhow::Result<_> = match report {
                        Ok(Ok(Ok(report))) => Ok(Some(report)),
                        Ok(Ok(Err(err))) => Err(err),
                        Ok(Err(_)) => Err(anyhow!("netcheck report not received")),
                        Err(err) => Err(anyhow!("netcheck report timeout: {:?}", err)),
                    };
                    msg_sender
                        .send(ActorMessage::NetcheckReport(report, why))
                        .await
                        .ok();
                    // The receiver of the NetcheckReport message will call
                    // .finalize_endpoints_update().
                });
            }
            Err(err) => {
                warn!("unable to start netcheck generation: {:?}", err);
                self.finalize_direct_addrs_update(why);
            }
        }
    }

    async fn handle_netcheck_report(&mut self, report: Option<Arc<netcheck::Report>>) {
        if let Some(ref report) = report {
            self.msock
                .ipv6_reported
                .store(report.ipv6, Ordering::Relaxed);
            let r = &report;
            trace!(
                "setting no_v4_send {} -> {}",
                self.no_v4_send,
                !r.ipv4_can_send
            );
            self.no_v4_send = !r.ipv4_can_send;

            let have_port_map = self.port_mapper.watch_external_address().borrow().is_some();
            let mut ni = NetInfo {
                relay_latency: Default::default(),
                mapping_varies_by_dest_ip: r.mapping_varies_by_dest_ip,
                hair_pinning: r.hair_pinning,
                portmap_probe: r.portmap_probe.clone(),
                have_port_map,
                working_ipv6: Some(r.ipv6),
                os_has_ipv6: Some(r.os_has_ipv6),
                working_udp: Some(r.udp),
                working_icmp_v4: r.icmpv4,
                working_icmp_v6: r.icmpv6,
                preferred_relay: r.preferred_relay.clone(),
            };
            for (rid, d) in r.relay_v4_latency.iter() {
                ni.relay_latency
                    .insert(format!("{rid}-v4"), d.as_secs_f64());
            }
            for (rid, d) in r.relay_v6_latency.iter() {
                ni.relay_latency
                    .insert(format!("{rid}-v6"), d.as_secs_f64());
            }

            if ni.preferred_relay.is_none() {
                // Perhaps UDP is blocked. Pick a deterministic but arbitrary one.
                ni.preferred_relay = self.pick_relay_fallback();
            }

            if !self.set_nearest_relay(ni.preferred_relay.clone()) {
                ni.preferred_relay = None;
            }

            // TODO: set link type
            self.call_net_info_callback(ni).await;
        }
        self.store_direct_addr_update(report).await;
    }

    fn set_nearest_relay(&mut self, relay_url: Option<RelayUrl>) -> bool {
        let my_relay = self.msock.my_relay();
        if relay_url == my_relay {
            // No change.
            return true;
        }
        let old_relay = self.msock.set_my_relay(relay_url.clone());

        if let Some(ref relay_url) = relay_url {
            inc!(MagicsockMetrics, relay_home_change);

            // On change, notify all currently connected relay servers and
            // start connecting to our home relay if we are not already.
            info!("home is now relay {}, was {:?}", relay_url, old_relay);
            self.msock.publish_my_addr();

            self.send_relay_actor(RelayActorMessage::SetHome {
                url: relay_url.clone(),
            });
        }

        true
    }

    /// Returns a deterministic relay node to connect to. This is only used if netcheck
    /// couldn't find the nearest one, for instance, if UDP is blocked and thus STUN
    /// latency checks aren't working.
    ///
    /// If no the [`RelayMap`] is empty, returns `0`.
    fn pick_relay_fallback(&self) -> Option<RelayUrl> {
        // TODO: figure out which relay node most of our nodes are using,
        // and use that region as our fallback.
        //
        // If we already had selected something in the past and it has any
        // nodes, we want to stay on it. If there are no nodes at all,
        // stay on whatever relay we previously picked. If we need to pick
        // one and have no node info, pick a node randomly.
        //
        // We used to do the above for legacy clients, but never updated it for disco.

        let my_relay = self.msock.my_relay();
        if my_relay.is_some() {
            return my_relay;
        }

        let ids = self.msock.relay_map.urls().collect::<Vec<_>>();
        let mut rng = rand::rngs::StdRng::seed_from_u64(0);
        ids.choose(&mut rng).map(|c| (*c).clone())
    }

    /// Resets the preferred address for all nodes.
    /// This is called when connectivity changes enough that we no longer trust the old routes.
    #[instrument(skip_all, fields(me = %self.msock.me))]
    fn reset_endpoint_states(&mut self) {
        self.msock.node_map.reset_node_states()
    }

    /// Tells the relay actor to close stale relay connections.
    ///
    /// The relay connections who's local endpoints no longer exist after a network change
    /// will error out soon enough.  Closing them eagerly speeds this up however and allows
    /// re-establishing a relay connection faster.
    async fn close_stale_relay_connections(&self) {
        let ifs = interfaces::State::new().await;
        let local_ips = ifs
            .interfaces
            .values()
            .flat_map(|netif| netif.addrs())
            .map(|ipnet| ipnet.addr())
            .collect();
        self.send_relay_actor(RelayActorMessage::MaybeCloseRelaysOnRebind(local_ips));
    }

    fn send_relay_actor(&self, msg: RelayActorMessage) {
        match self.relay_actor_sender.try_send(msg) {
            Ok(_) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => {
                warn!("unable to send to relay actor, already closed");
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                warn!("dropping message for relay actor, channel is full");
            }
        }
    }

    fn handle_relay_disco_message(
        &mut self,
        msg: &[u8],
        url: &RelayUrl,
        relay_node_src: PublicKey,
    ) -> bool {
        match disco::source_and_box(msg) {
            Some((source, sealed_box)) => {
                if relay_node_src != source {
                    // TODO: return here?
                    warn!("Received relay disco message from connection for {}, but with message from {}", relay_node_src.fmt_short(), source.fmt_short());
                }
                self.msock.handle_disco_message(
                    source,
                    sealed_box,
                    DiscoMessageSource::Relay {
                        url: url.clone(),
                        key: relay_node_src,
                    },
                );
                true
            }
            None => false,
        }
    }
}

fn new_re_stun_timer(initial_delay: bool) -> time::Interval {
    // Pick a random duration between 20 and 26 seconds (just under 30s,
    // a common UDP NAT timeout on Linux,etc)
    let mut rng = rand::thread_rng();
    let d: Duration = rng.gen_range(Duration::from_secs(20)..=Duration::from_secs(26));
    if initial_delay {
        debug!("scheduling periodic_stun to run in {}s", d.as_secs());
        time::interval_at(time::Instant::now() + d, d)
    } else {
        debug!(
            "scheduling periodic_stun to run immediately and in {}s",
            d.as_secs()
        );
        time::interval(d)
    }
}

/// Initial connection setup.
fn bind(
    addr_v4: Option<SocketAddrV4>,
    addr_v6: Option<SocketAddrV6>,
) -> Result<(UdpConn, Option<UdpConn>)> {
    let addr_v4 = addr_v4.unwrap_or_else(|| SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0));
    let pconn4 = UdpConn::bind(SocketAddr::V4(addr_v4)).context("bind IPv4 failed")?;

    let ip4_port = pconn4.local_addr()?.port();
    let ip6_port = ip4_port.checked_add(1).unwrap_or(ip4_port - 1);
    let addr_v6 =
        addr_v6.unwrap_or_else(|| SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, ip6_port, 0, 0));
    let pconn6 = match UdpConn::bind(SocketAddr::V6(addr_v6)) {
        Ok(conn) => Some(conn),
        Err(err) => {
            info!("bind ignoring IPv6 bind failure: {:?}", err);
            None
        }
    };

    Ok((pconn4, pconn6))
}

/// The discovered direct addresses of this [`MagicSock`].
///
/// These are all the [`DirectAddr`]s that this [`MagicSock`] is aware of for itself.
/// They include all locally bound ones as well as those discovered by other mechanisms like
/// STUN.
#[derive(derive_more::Debug, Default, Clone)]
struct DiscoveredDirectAddrs {
    /// The last set of discovered direct addresses.
    addrs: Vec<DirectAddr>,

    /// The last time the direct addresses were updated, even if there was no change.
    updated_at: Option<Instant>,
}

impl PartialEq for DiscoveredDirectAddrs {
    fn eq(&self, other: &Self) -> bool {
        endpoint_sets_equal(&self.addrs, &other.addrs)
    }
}

impl DiscoveredDirectAddrs {
    fn new(endpoints: Vec<DirectAddr>) -> Self {
        Self {
            addrs: endpoints,
            updated_at: Some(Instant::now()),
        }
    }

    fn into_iter(self) -> impl Iterator<Item = DirectAddr> {
        self.addrs.into_iter()
    }

    fn iter(&self) -> impl Iterator<Item = &DirectAddr> + '_ {
        self.addrs.iter()
    }

    fn is_empty(&self) -> bool {
        self.addrs.is_empty()
    }

    fn fresh_enough(&self) -> bool {
        match self.updated_at.as_ref() {
            None => false,
            Some(time) => time.elapsed() <= ENDPOINTS_FRESH_ENOUGH_DURATION,
        }
    }

    fn to_call_me_maybe_message(&self) -> disco::CallMeMaybe {
        let my_numbers = self.addrs.iter().map(|ep| ep.addr).collect();
        disco::CallMeMaybe { my_numbers }
    }

    fn log_direct_addrs_change(&self) {
        event!(
            target: "events.net.direct_addrs",
            Level::DEBUG,
            addrs = ?self.addrs,
        );
    }
}

/// Split a transmit containing a GSO payload into individual packets.
///
/// This allocates the data.
///
/// If the transmit has a segment size it contains multiple GSO packets.  It will be split
/// into multiple packets according to that segment size.  If it does not have a segment
/// size, the contents will be sent as a single packet.
// TODO: If quinn stayed on bytes this would probably be much cheaper, probably.  Need to
// figure out where they allocate the Vec.
fn split_packets(transmit: &quinn_udp::Transmit) -> RelayContents {
    let mut res = SmallVec::with_capacity(1);
    let contents = transmit.contents;
    if let Some(segment_size) = transmit.segment_size {
        for chunk in contents.chunks(segment_size) {
            res.push(Bytes::from(chunk.to_vec()));
        }
    } else {
        res.push(Bytes::from(contents.to_vec()));
    }
    res
}

/// Splits a packet into its component items.
#[derive(Debug)]
struct PacketSplitIter {
    bytes: Bytes,
}

impl PacketSplitIter {
    /// Create a new PacketSplitIter from a packet.
    ///
    /// Returns an error if the packet is too big.
    fn new(bytes: Bytes) -> Self {
        Self { bytes }
    }

    fn fail(&mut self) -> Option<std::io::Result<Bytes>> {
        self.bytes.clear();
        Some(Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "",
        )))
    }
}

impl Iterator for PacketSplitIter {
    type Item = std::io::Result<Bytes>;

    fn next(&mut self) -> Option<Self::Item> {
        use bytes::Buf;
        if self.bytes.has_remaining() {
            if self.bytes.remaining() < 2 {
                return self.fail();
            }
            let len = self.bytes.get_u16_le() as usize;
            if self.bytes.remaining() < len {
                return self.fail();
            }
            let item = self.bytes.split_to(len);
            Some(Ok(item))
        } else {
            None
        }
    }
}

/// The fake address used by the QUIC layer to address a node.
///
/// You can consider this as nothing more than a lookup key for a node the [`MagicSock`] knows
/// about.
///
/// [`MagicSock`] can reach a node by several real socket addresses, or maybe even via the relay
/// node.  The QUIC layer however needs to address a node by a stable [`SocketAddr`] so
/// that normal socket APIs can function.  Thus when a new node is introduced to a [`MagicSock`]
/// it is given a new fake address.  This is the type of that address.
///
/// It is but a newtype.  And in our QUIC-facing socket APIs like [`AsyncUdpSocket`] it
/// comes in as the inner [`SocketAddr`], in those interfaces we have to be careful to do
/// the conversion to this type.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub(crate) struct QuicMappedAddr(pub(crate) SocketAddr);

/// Counter to always generate unique addresses for [`QuicMappedAddr`].
static ADDR_COUNTER: AtomicU64 = AtomicU64::new(1);

impl QuicMappedAddr {
    /// The Prefix/L of our Unique Local Addresses.
    const ADDR_PREFIXL: u8 = 0xfd;
    /// The Global ID used in our Unique Local Addresses.
    const ADDR_GLOBAL_ID: [u8; 5] = [21, 7, 10, 81, 11];
    /// The Subnet ID used in our Unique Local Addresses.
    const ADDR_SUBNET: [u8; 2] = [0; 2];

    /// Generates a globally unique fake UDP address.
    ///
    /// This generates and IPv6 Unique Local Address according to RFC 4193.
    pub(crate) fn generate() -> Self {
        let mut addr = [0u8; 16];
        addr[0] = Self::ADDR_PREFIXL;
        addr[1..6].copy_from_slice(&Self::ADDR_GLOBAL_ID);
        addr[6..8].copy_from_slice(&Self::ADDR_SUBNET);

        let counter = ADDR_COUNTER.fetch_add(1, Ordering::Relaxed);
        addr[8..16].copy_from_slice(&counter.to_be_bytes());

        Self(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(addr)), 12345))
    }
}

impl std::fmt::Display for QuicMappedAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "QuicMappedAddr({})", self.0)
    }
}
fn disco_message_sent(msg: &disco::Message) {
    match msg {
        disco::Message::Ping(_) => {
            inc!(MagicsockMetrics, sent_disco_ping);
        }
        disco::Message::Pong(_) => {
            inc!(MagicsockMetrics, sent_disco_pong);
        }
        disco::Message::CallMeMaybe(_) => {
            inc!(MagicsockMetrics, sent_disco_call_me_maybe);
        }
    }
}

/// A *direct address* on which an iroh-node might be contactable.
///
/// Direct addresses are UDP socket addresses on which an iroh-net node could potentially be
/// contacted.  These can come from various sources depending on the network topology of the
/// iroh-net node, see [`DirectAddrType`] for the several kinds of sources.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DirectAddr {
    /// The address.
    pub addr: SocketAddr,
    /// The origin of this direct address.
    pub typ: DirectAddrType,
}

/// The type of direct address.
///
/// These are the various sources or origins from which an iroh-net node might have found a
/// possible [`DirectAddr`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DirectAddrType {
    /// Not yet determined..
    Unknown,
    /// A locally bound socket address.
    Local,
    /// Public internet address discovered via STUN.
    ///
    /// When possible an iroh-net node will perform STUN to discover which is the address
    /// from which it sends data on the public internet.  This can be different from locally
    /// bound addresses when the node is on a local network which performs NAT or similar.
    Stun,
    /// An address assigned by the router using port mapping.
    ///
    /// When possible an iroh-net node will request a port mapping from the local router to
    /// get a publicly routable direct address.
    Portmapped,
    /// Hard NAT: STUN'ed IPv4 address + local fixed port.
    ///
    /// It is possible to configure iroh-net to bound to a specific port and independently
    /// configure the router to forward this port to the iroh-net node.  This indicates a
    /// situation like this, which still uses STUN to discover the public address.
    Stun4LocalPort,
}

impl Display for DirectAddrType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DirectAddrType::Unknown => write!(f, "?"),
            DirectAddrType::Local => write!(f, "local"),
            DirectAddrType::Stun => write!(f, "stun"),
            DirectAddrType::Portmapped => write!(f, "portmap"),
            DirectAddrType::Stun4LocalPort => write!(f, "stun4localport"),
        }
    }
}

/// Contains information about the host's network state.
#[derive(Debug, Clone, PartialEq)]
struct NetInfo {
    /// Says whether the host's NAT mappings vary based on the destination IP.
    mapping_varies_by_dest_ip: Option<bool>,

    /// If their router does hairpinning. It reports true even if there's no NAT involved.
    hair_pinning: Option<bool>,

    /// Whether the host has IPv6 internet connectivity.
    working_ipv6: Option<bool>,

    /// Whether the OS supports IPv6 at all, regardless of whether IPv6 internet connectivity is available.
    os_has_ipv6: Option<bool>,

    /// Whether the host has UDP internet connectivity.
    working_udp: Option<bool>,

    /// Whether ICMPv4 works, `None` means not checked.
    working_icmp_v4: Option<bool>,

    /// Whether ICMPv6 works, `None` means not checked.
    working_icmp_v6: Option<bool>,

    /// Whether we have an existing portmap open (UPnP, PMP, or PCP).
    have_port_map: bool,

    /// Probe indicating the presence of port mapping protocols on the LAN.
    portmap_probe: Option<portmapper::ProbeOutput>,

    /// This node's preferred relay server for incoming traffic.
    ///
    /// The node might be be temporarily connected to multiple relay servers (to send to
    /// other nodes) but this is the relay on which you can always contact this node.  Also
    /// known as home relay.
    preferred_relay: Option<RelayUrl>,

    /// The fastest recent time to reach various relay STUN servers, in seconds.
    ///
    /// This should only be updated rarely, or when there's a
    /// material change, as any change here also gets uploaded to the control plane.
    relay_latency: BTreeMap<String, f64>,
}

impl NetInfo {
    /// Checks if this is probably still the same network as *other*.
    ///
    /// This tries to compare the network situation, without taking into account things
    /// expected to change a little like e.g. latency to the relay server.
    fn basically_equal(&self, other: &Self) -> bool {
        let eq_icmp_v4 = match (self.working_icmp_v4, other.working_icmp_v4) {
            (Some(slf), Some(other)) => slf == other,
            _ => true, // ignore for comparison if only one report had this info
        };
        let eq_icmp_v6 = match (self.working_icmp_v6, other.working_icmp_v6) {
            (Some(slf), Some(other)) => slf == other,
            _ => true, // ignore for comparison if only one report had this info
        };
        self.mapping_varies_by_dest_ip == other.mapping_varies_by_dest_ip
            && self.hair_pinning == other.hair_pinning
            && self.working_ipv6 == other.working_ipv6
            && self.os_has_ipv6 == other.os_has_ipv6
            && self.working_udp == other.working_udp
            && eq_icmp_v4
            && eq_icmp_v6
            && self.have_port_map == other.have_port_map
            && self.portmap_probe == other.portmap_probe
            && self.preferred_relay == other.preferred_relay
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Context;
    use iroh_test::CallOnDrop;
    use rand::RngCore;
    use tokio_util::task::AbortOnDropHandle;

    use crate::defaults::staging::EU_RELAY_HOSTNAME;
    use crate::relay::RelayMode;
    use crate::tls;
    use crate::Endpoint;

    use super::*;

    const ALPN: &[u8] = b"n0/test/1";

    impl MagicSock {
        #[track_caller]
        pub fn add_test_addr(&self, node_addr: NodeAddr) {
            self.add_node_addr(
                node_addr,
                Source::NamedApp {
                    name: "test".into(),
                },
            )
            .unwrap()
        }
    }

    /// Magicsock plus wrappers for sending packets
    #[derive(Clone)]
    struct MagicStack {
        secret_key: SecretKey,
        endpoint: Endpoint,
    }

    impl MagicStack {
        async fn new(relay_mode: RelayMode) -> Result<Self> {
            let secret_key = SecretKey::generate();

            let mut transport_config = quinn::TransportConfig::default();
            transport_config.max_idle_timeout(Some(Duration::from_secs(10).try_into().unwrap()));

            let endpoint = Endpoint::builder()
                .secret_key(secret_key.clone())
                .transport_config(transport_config)
                .relay_mode(relay_mode)
                .alpns(vec![ALPN.to_vec()])
                .bind()
                .await?;

            Ok(Self {
                secret_key,
                endpoint,
            })
        }

        fn tracked_endpoints(&self) -> Vec<PublicKey> {
            self.endpoint
                .magic_sock()
                .list_remote_infos()
                .into_iter()
                .map(|ep| ep.node_id)
                .collect()
        }

        fn public(&self) -> PublicKey {
            self.secret_key.public()
        }
    }

    /// Monitors endpoint changes and plumbs things together.
    ///
    /// This is a way of connecting endpoints without a relay server.  Whenever the local
    /// endpoints of a magic endpoint change this address is added to the other magic
    /// sockets.  This function will await until the endpoints are connected the first time
    /// before returning.
    ///
    /// When the returned drop guard is dropped, the tasks doing this updating are stopped.
    #[instrument(skip_all)]
    async fn mesh_stacks(stacks: Vec<MagicStack>) -> Result<CallOnDrop> {
        /// Registers endpoint addresses of a node to all other nodes.
        fn update_direct_addrs(stacks: &[MagicStack], my_idx: usize, new_addrs: Vec<DirectAddr>) {
            let me = &stacks[my_idx];
            for (i, m) in stacks.iter().enumerate() {
                if i == my_idx {
                    continue;
                }

                let addr = NodeAddr {
                    node_id: me.public(),
                    info: crate::AddrInfo {
                        relay_url: None,
                        direct_addresses: new_addrs.iter().map(|ep| ep.addr).collect(),
                    },
                };
                m.endpoint.magic_sock().add_test_addr(addr);
            }
        }

        // For each node, start a task which monitors its local endpoints and registers them
        // with the other nodes as local endpoints become known.
        let mut tasks = JoinSet::new();
        for (my_idx, m) in stacks.iter().enumerate() {
            let m = m.clone();
            let stacks = stacks.clone();
            tasks.spawn(async move {
                let me = m.endpoint.node_id().fmt_short();
                let mut stream = m.endpoint.direct_addresses();
                while let Some(new_eps) = stream.next().await {
                    info!(%me, "conn{} endpoints update: {:?}", my_idx + 1, new_eps);
                    update_direct_addrs(&stacks, my_idx, new_eps);
                }
            });
        }
        let guard = CallOnDrop::new(move || {
            tasks.abort_all();
        });

        // Wait for all nodes to be registered with each other.
        time::timeout(Duration::from_secs(10), async move {
            let all_node_ids: Vec<_> = stacks.iter().map(|ms| ms.endpoint.node_id()).collect();
            loop {
                let mut ready = Vec::with_capacity(stacks.len());
                for ms in stacks.iter() {
                    let endpoints = ms.tracked_endpoints();
                    let my_node_id = ms.endpoint.node_id();
                    let all_nodes_meshed = all_node_ids
                        .iter()
                        .filter(|node_id| **node_id != my_node_id)
                        .all(|node_id| endpoints.contains(node_id));
                    ready.push(all_nodes_meshed);
                }
                if ready.iter().all(|meshed| *meshed) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .context("failed to connect nodes")?;
        info!("all nodes meshed");
        Ok(guard)
    }

    #[instrument(skip_all, fields(me = %ep.endpoint.node_id().fmt_short()))]
    async fn echo_receiver(ep: MagicStack) -> Result<()> {
        info!("accepting conn");
        let conn = ep.endpoint.accept().await.expect("no conn");

        info!("connecting");
        let conn = conn.await.context("[receiver] connecting")?;
        info!("accepting bi");
        let (mut send_bi, mut recv_bi) =
            conn.accept_bi().await.context("[receiver] accepting bi")?;

        info!("reading");
        let val = recv_bi
            .read_to_end(usize::MAX)
            .await
            .context("[receiver] reading to end")?;

        info!("replying");
        for chunk in val.chunks(12) {
            send_bi
                .write_all(chunk)
                .await
                .context("[receiver] sending chunk")?;
        }

        info!("finishing");
        send_bi.finish().context("[receiver] finishing")?;
        send_bi.stopped().await.context("[receiver] stopped")?;

        let stats = conn.stats();
        info!("stats: {:#?}", stats);
        // TODO: ensure panics in this function are reported ok
        assert!(
            stats.path.lost_packets < 10,
            "[receiver] should not loose many packets",
        );

        info!("close");
        conn.close(0u32.into(), b"done");
        info!("wait idle");
        ep.endpoint.endpoint().wait_idle().await;

        Ok(())
    }

    #[instrument(skip_all, fields(me = %ep.endpoint.node_id().fmt_short()))]
    async fn echo_sender(ep: MagicStack, dest_id: PublicKey, msg: &[u8]) -> Result<()> {
        info!("connecting to {}", dest_id.fmt_short());
        let dest = NodeAddr::new(dest_id);
        let conn = ep
            .endpoint
            .connect(dest, ALPN)
            .await
            .context("[sender] connect")?;

        info!("opening bi");
        let (mut send_bi, mut recv_bi) = conn.open_bi().await.context("[sender] open bi")?;

        info!("writing message");
        send_bi.write_all(msg).await.context("[sender] write all")?;

        info!("finishing");
        send_bi.finish().context("[sender] finish")?;
        send_bi.stopped().await.context("[sender] stopped")?;

        info!("reading_to_end");
        let val = recv_bi.read_to_end(usize::MAX).await.context("[sender]")?;
        assert_eq!(
            val,
            msg,
            "[sender] expected {}, got {}",
            hex::encode(msg),
            hex::encode(&val)
        );

        let stats = conn.stats();
        info!("stats: {:#?}", stats);
        assert!(
            stats.path.lost_packets < 10,
            "[sender] should not loose many packets",
        );

        info!("close");
        conn.close(0u32.into(), b"done");
        info!("wait idle");
        ep.endpoint.endpoint().wait_idle().await;
        Ok(())
    }

    /// Runs a roundtrip between the [`echo_sender`] and [`echo_receiver`].
    async fn run_roundtrip(sender: MagicStack, receiver: MagicStack, payload: &[u8]) {
        let send_node_id = sender.endpoint.node_id();
        let recv_node_id = receiver.endpoint.node_id();
        info!("\nroundtrip: {send_node_id:#} -> {recv_node_id:#}");

        let receiver_task = tokio::spawn(echo_receiver(receiver));
        let sender_res = echo_sender(sender, recv_node_id, payload).await;
        let sender_is_err = match sender_res {
            Ok(()) => false,
            Err(err) => {
                eprintln!("[sender] Error:\n{err:#?}");
                true
            }
        };
        let receiver_is_err = match receiver_task.await {
            Ok(Ok(())) => false,
            Ok(Err(err)) => {
                eprintln!("[receiver] Error:\n{err:#?}");
                true
            }
            Err(joinerr) => {
                if joinerr.is_panic() {
                    std::panic::resume_unwind(joinerr.into_panic());
                } else {
                    eprintln!("[receiver] Error:\n{joinerr:#?}");
                }
                true
            }
        };
        if sender_is_err || receiver_is_err {
            panic!("Sender or receiver errored");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_two_devices_roundtrip_quinn_magic() -> Result<()> {
        iroh_test::logging::setup_multithreaded();

        let m1 = MagicStack::new(RelayMode::Disabled).await?;
        let m2 = MagicStack::new(RelayMode::Disabled).await?;

        let _guard = mesh_stacks(vec![m1.clone(), m2.clone()]).await?;

        for i in 0..5 {
            info!("\n-- round {i}");
            run_roundtrip(m1.clone(), m2.clone(), b"hello m1").await;
            run_roundtrip(m2.clone(), m1.clone(), b"hello m2").await;

            info!("\n-- larger data");
            let mut data = vec![0u8; 10 * 1024];
            rand::thread_rng().fill_bytes(&mut data);
            run_roundtrip(m1.clone(), m2.clone(), &data).await;
            run_roundtrip(m2.clone(), m1.clone(), &data).await;
        }

        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_two_devices_roundtrip_network_change() -> Result<()> {
        time::timeout(
            Duration::from_secs(90),
            test_two_devices_roundtrip_network_change_impl(),
        )
        .await?
    }

    /// Same structure as `test_two_devices_roundtrip_quinn_magic`, but interrupts regularly
    /// with (simulated) network changes.
    async fn test_two_devices_roundtrip_network_change_impl() -> Result<()> {
        iroh_test::logging::setup_multithreaded();

        let m1 = MagicStack::new(RelayMode::Disabled).await?;
        let m2 = MagicStack::new(RelayMode::Disabled).await?;

        let _guard = mesh_stacks(vec![m1.clone(), m2.clone()]).await?;

        let offset = || {
            let delay = rand::thread_rng().gen_range(10..=500);
            Duration::from_millis(delay)
        };
        let rounds = 5;

        // Regular network changes to m1 only.
        let m1_network_change_guard = {
            let m1 = m1.clone();
            let task = tokio::spawn(async move {
                loop {
                    println!("[m1] network change");
                    m1.endpoint.magic_sock().force_network_change(true).await;
                    time::sleep(offset()).await;
                }
            });
            CallOnDrop::new(move || {
                task.abort();
            })
        };

        for i in 0..rounds {
            println!("-- [m1 changes] round {}", i + 1);
            run_roundtrip(m1.clone(), m2.clone(), b"hello m1").await;
            run_roundtrip(m2.clone(), m1.clone(), b"hello m2").await;

            println!("-- [m1 changes] larger data");
            let mut data = vec![0u8; 10 * 1024];
            rand::thread_rng().fill_bytes(&mut data);
            run_roundtrip(m1.clone(), m2.clone(), &data).await;
            run_roundtrip(m2.clone(), m1.clone(), &data).await;
        }

        std::mem::drop(m1_network_change_guard);

        // Regular network changes to m2 only.
        let m2_network_change_guard = {
            let m2 = m2.clone();
            let task = tokio::spawn(async move {
                loop {
                    println!("[m2] network change");
                    m2.endpoint.magic_sock().force_network_change(true).await;
                    time::sleep(offset()).await;
                }
            });
            CallOnDrop::new(move || {
                task.abort();
            })
        };

        for i in 0..rounds {
            println!("-- [m2 changes] round {}", i + 1);
            run_roundtrip(m1.clone(), m2.clone(), b"hello m1").await;
            run_roundtrip(m2.clone(), m1.clone(), b"hello m2").await;

            println!("-- [m2 changes] larger data");
            let mut data = vec![0u8; 10 * 1024];
            rand::thread_rng().fill_bytes(&mut data);
            run_roundtrip(m1.clone(), m2.clone(), &data).await;
            run_roundtrip(m2.clone(), m1.clone(), &data).await;
        }

        std::mem::drop(m2_network_change_guard);

        // Regular network changes to both m1 and m2 only.
        let m1_m2_network_change_guard = {
            let m1 = m1.clone();
            let m2 = m2.clone();
            let task = tokio::spawn(async move {
                println!("-- [m1] network change");
                m1.endpoint.magic_sock().force_network_change(true).await;
                println!("-- [m2] network change");
                m2.endpoint.magic_sock().force_network_change(true).await;
                time::sleep(offset()).await;
            });
            CallOnDrop::new(move || {
                task.abort();
            })
        };

        for i in 0..rounds {
            println!("-- [m1 & m2 changes] round {}", i + 1);
            run_roundtrip(m1.clone(), m2.clone(), b"hello m1").await;
            run_roundtrip(m2.clone(), m1.clone(), b"hello m2").await;

            println!("-- [m1 & m2 changes] larger data");
            let mut data = vec![0u8; 10 * 1024];
            rand::thread_rng().fill_bytes(&mut data);
            run_roundtrip(m1.clone(), m2.clone(), &data).await;
            run_roundtrip(m2.clone(), m1.clone(), &data).await;
        }

        std::mem::drop(m1_m2_network_change_guard);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_two_devices_setup_teardown() -> Result<()> {
        iroh_test::logging::setup_multithreaded();
        for i in 0..10 {
            println!("-- round {i}");
            println!("setting up magic stack");
            let m1 = MagicStack::new(RelayMode::Disabled).await?;
            let m2 = MagicStack::new(RelayMode::Disabled).await?;

            let _guard = mesh_stacks(vec![m1.clone(), m2.clone()]).await?;

            println!("closing endpoints");
            let msock1 = m1.endpoint.magic_sock();
            let msock2 = m2.endpoint.magic_sock();
            m1.endpoint.close(0u32.into(), b"done").await?;
            m2.endpoint.close(0u32.into(), b"done").await?;

            assert!(msock1.msock.is_closed());
            assert!(msock2.msock.is_closed());
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_two_devices_roundtrip_quinn_raw() -> Result<()> {
        let _guard = iroh_test::logging::setup();

        let make_conn = |addr: SocketAddr| -> anyhow::Result<quinn::Endpoint> {
            let key = SecretKey::generate();
            let conn = std::net::UdpSocket::bind(addr)?;

            let quic_server_config = tls::make_server_config(&key, vec![ALPN.to_vec()], false)?;
            let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(quic_server_config));
            let mut transport_config = quinn::TransportConfig::default();
            transport_config.keep_alive_interval(Some(Duration::from_secs(5)));
            transport_config.max_idle_timeout(Some(Duration::from_secs(10).try_into().unwrap()));
            server_config.transport_config(Arc::new(transport_config));
            let mut quic_ep = quinn::Endpoint::new(
                quinn::EndpointConfig::default(),
                Some(server_config),
                conn,
                Arc::new(quinn::TokioRuntime),
            )?;

            let quic_client_config =
                tls::make_client_config(&key, None, vec![ALPN.to_vec()], false)?;
            let mut client_config = quinn::ClientConfig::new(Arc::new(quic_client_config));
            let mut transport_config = quinn::TransportConfig::default();
            transport_config.max_idle_timeout(Some(Duration::from_secs(10).try_into().unwrap()));
            client_config.transport_config(Arc::new(transport_config));
            quic_ep.set_default_client_config(client_config);

            Ok(quic_ep)
        };

        let m1 = make_conn("127.0.0.1:0".parse().unwrap())?;
        let m2 = make_conn("127.0.0.1:0".parse().unwrap())?;

        // msg from  a -> b
        macro_rules! roundtrip {
            ($a:expr, $b:expr, $msg:expr) => {
                let a = $a.clone();
                let b = $b.clone();
                let a_name = stringify!($a);
                let b_name = stringify!($b);
                println!("{} -> {} ({} bytes)", a_name, b_name, $msg.len());

                let a_addr = a.local_addr()?;
                let b_addr = b.local_addr()?;

                println!("{}: {}, {}: {}", a_name, a_addr, b_name, b_addr);

                let b_task = tokio::task::spawn(async move {
                    println!("[{b_name}] accepting conn");
                    let conn = b.accept().await.expect("no conn");
                    println!("[{}] connecting", b_name);
                    let conn = conn
                        .await
                        .with_context(|| format!("[{b_name}] connecting"))?;
                    println!("[{}] accepting bi", b_name);
                    let (mut send_bi, mut recv_bi) = conn
                        .accept_bi()
                        .await
                        .with_context(|| format!("[{b_name}] accepting bi"))?;

                    println!("[{b_name}] reading");
                    let val = recv_bi
                        .read_to_end(usize::MAX)
                        .await
                        .with_context(|| format!("[{b_name}] reading to end"))?;
                    println!("[{b_name}] finishing");
                    send_bi
                        .finish()
                        .with_context(|| format!("[{b_name}] finishing"))?;
                    send_bi
                        .stopped()
                        .await
                        .with_context(|| format!("[b_name] stopped"))?;

                    println!("[{b_name}] close");
                    conn.close(0u32.into(), b"done");
                    println!("[{b_name}] closed");

                    Ok::<_, anyhow::Error>(val)
                });

                println!("[{a_name}] connecting to {b_addr}");
                let conn = a
                    .connect(b_addr, "localhost")?
                    .await
                    .with_context(|| format!("[{a_name}] connect"))?;

                println!("[{a_name}] opening bi");
                let (mut send_bi, mut recv_bi) = conn
                    .open_bi()
                    .await
                    .with_context(|| format!("[{a_name}] open bi"))?;
                println!("[{a_name}] writing message");
                send_bi
                    .write_all(&$msg[..])
                    .await
                    .with_context(|| format!("[{a_name}] write all"))?;

                println!("[{a_name}] finishing");
                send_bi
                    .finish()
                    .with_context(|| format!("[{a_name}] finish"))?;
                send_bi
                    .stopped()
                    .await
                    .with_context(|| format!("[{a_name}] stopped"))?;

                println!("[{a_name}] reading_to_end");
                let _ = recv_bi
                    .read_to_end(usize::MAX)
                    .await
                    .with_context(|| format!("[{a_name}] reading_to_end"))?;
                println!("[{a_name}] close");
                conn.close(0u32.into(), b"done");
                println!("[{a_name}] wait idle");
                a.wait_idle().await;

                drop(send_bi);

                // make sure the right values arrived
                println!("[{a_name}] waiting for channel");
                let val = b_task.await??;
                anyhow::ensure!(
                    val == $msg,
                    "expected {}, got {}",
                    hex::encode($msg),
                    hex::encode(val)
                );
            };
        }

        for i in 0..10 {
            println!("-- round {}", i + 1);
            roundtrip!(m1, m2, b"hello m1");
            roundtrip!(m2, m1, b"hello m2");

            println!("-- larger data");

            let mut data = vec![0u8; 10 * 1024];
            rand::thread_rng().fill_bytes(&mut data);
            roundtrip!(m1, m2, data);
            roundtrip!(m2, m1, data);
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_two_devices_roundtrip_quinn_rebinding_conn() -> Result<()> {
        let _guard = iroh_test::logging::setup();

        fn make_conn(addr: SocketAddr) -> anyhow::Result<quinn::Endpoint> {
            let key = SecretKey::generate();
            let conn = UdpConn::bind(addr)?;

            let quic_server_config = tls::make_server_config(&key, vec![ALPN.to_vec()], false)?;
            let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(quic_server_config));
            let mut transport_config = quinn::TransportConfig::default();
            transport_config.keep_alive_interval(Some(Duration::from_secs(5)));
            transport_config.max_idle_timeout(Some(Duration::from_secs(10).try_into().unwrap()));
            server_config.transport_config(Arc::new(transport_config));
            let mut quic_ep = quinn::Endpoint::new_with_abstract_socket(
                quinn::EndpointConfig::default(),
                Some(server_config),
                Arc::new(conn),
                Arc::new(quinn::TokioRuntime),
            )?;

            let quic_client_config =
                tls::make_client_config(&key, None, vec![ALPN.to_vec()], false)?;
            let mut client_config = quinn::ClientConfig::new(Arc::new(quic_client_config));
            let mut transport_config = quinn::TransportConfig::default();
            transport_config.max_idle_timeout(Some(Duration::from_secs(10).try_into().unwrap()));
            client_config.transport_config(Arc::new(transport_config));
            quic_ep.set_default_client_config(client_config);

            Ok(quic_ep)
        }

        let m1 = make_conn("127.0.0.1:7770".parse().unwrap())?;
        let m2 = make_conn("127.0.0.1:7771".parse().unwrap())?;

        // msg from  a -> b
        macro_rules! roundtrip {
            ($a:expr, $b:expr, $msg:expr) => {
                let a = $a.clone();
                let b = $b.clone();
                let a_name = stringify!($a);
                let b_name = stringify!($b);
                println!("{} -> {} ({} bytes)", a_name, b_name, $msg.len());

                let a_addr: SocketAddr = format!("127.0.0.1:{}", a.local_addr()?.port())
                    .parse()
                    .unwrap();
                let b_addr: SocketAddr = format!("127.0.0.1:{}", b.local_addr()?.port())
                    .parse()
                    .unwrap();

                println!("{}: {}, {}: {}", a_name, a_addr, b_name, b_addr);

                let b_task = tokio::task::spawn(async move {
                    println!("[{}] accepting conn", b_name);
                    let conn = b.accept().await.expect("no conn");
                    println!("[{}] connecting", b_name);
                    let conn = conn
                        .await
                        .with_context(|| format!("[{}] connecting", b_name))?;
                    println!("[{}] accepting bi", b_name);
                    let (mut send_bi, mut recv_bi) = conn
                        .accept_bi()
                        .await
                        .with_context(|| format!("[{}] accepting bi", b_name))?;

                    println!("[{}] reading", b_name);
                    let val = recv_bi
                        .read_to_end(usize::MAX)
                        .await
                        .with_context(|| format!("[{}] reading to end", b_name))?;
                    println!("[{}] finishing", b_name);
                    send_bi
                        .finish()
                        .with_context(|| format!("[{}] finishing", b_name))?;
                    send_bi
                        .stopped()
                        .await
                        .with_context(|| format!("[{b_name}] stopped"))?;

                    println!("[{}] close", b_name);
                    conn.close(0u32.into(), b"done");
                    println!("[{}] closed", b_name);

                    Ok::<_, anyhow::Error>(val)
                });

                println!("[{}] connecting to {}", a_name, b_addr);
                let conn = a
                    .connect(b_addr, "localhost")?
                    .await
                    .with_context(|| format!("[{}] connect", a_name))?;

                println!("[{}] opening bi", a_name);
                let (mut send_bi, mut recv_bi) = conn
                    .open_bi()
                    .await
                    .with_context(|| format!("[{}] open bi", a_name))?;
                println!("[{}] writing message", a_name);
                send_bi
                    .write_all(&$msg[..])
                    .await
                    .with_context(|| format!("[{}] write all", a_name))?;

                println!("[{}] finishing", a_name);
                send_bi
                    .finish()
                    .with_context(|| format!("[{}] finish", a_name))?;
                send_bi
                    .stopped()
                    .await
                    .with_context(|| format!("[{a_name}] stopped"))?;

                println!("[{}] reading_to_end", a_name);
                let _ = recv_bi
                    .read_to_end(usize::MAX)
                    .await
                    .with_context(|| format!("[{}]", a_name))?;
                println!("[{}] close", a_name);
                conn.close(0u32.into(), b"done");
                println!("[{}] wait idle", a_name);
                a.wait_idle().await;

                drop(send_bi);

                // make sure the right values arrived
                println!("[{}] waiting for channel", a_name);
                let val = b_task.await??;
                anyhow::ensure!(
                    val == $msg,
                    "expected {}, got {}",
                    hex::encode($msg),
                    hex::encode(val)
                );
            };
        }

        for i in 0..10 {
            println!("-- round {}", i + 1);
            roundtrip!(m1, m2, b"hello m1");
            roundtrip!(m2, m1, b"hello m2");

            println!("-- larger data");

            let mut data = vec![0u8; 10 * 1024];
            rand::thread_rng().fill_bytes(&mut data);
            roundtrip!(m1, m2, data);
            roundtrip!(m2, m1, data);
        }

        Ok(())
    }

    #[test]
    fn test_split_packets() {
        fn mk_transmit(contents: &[u8], segment_size: Option<usize>) -> quinn_udp::Transmit<'_> {
            let destination = "127.0.0.1:0".parse().unwrap();
            quinn_udp::Transmit {
                destination,
                ecn: None,
                contents,
                segment_size,
                src_ip: None,
            }
        }
        fn mk_expected(parts: impl IntoIterator<Item = &'static str>) -> RelayContents {
            parts
                .into_iter()
                .map(|p| p.as_bytes().to_vec().into())
                .collect()
        }
        // no split
        assert_eq!(
            split_packets(&mk_transmit(b"hello", None)),
            mk_expected(["hello"])
        );
        // split without rest
        assert_eq!(
            split_packets(&mk_transmit(b"helloworld", Some(5))),
            mk_expected(["hello", "world"])
        );
        // split with rest and second transmit
        assert_eq!(
            split_packets(&mk_transmit(b"hello world", Some(5))),
            mk_expected(["hello", " worl", "d"]) // spellchecker:disable-line
        );
        // split that results in 1 packet
        assert_eq!(
            split_packets(&mk_transmit(b"hello world", Some(1000))),
            mk_expected(["hello world"])
        );
    }

    #[tokio::test]
    async fn test_local_endpoints() {
        let _guard = iroh_test::logging::setup();
        let ms = Handle::new(Default::default()).await.unwrap();

        // See if we can get endpoints.
        let mut eps0 = ms.direct_addresses().next().await.unwrap();
        eps0.sort();
        println!("{eps0:?}");
        assert!(!eps0.is_empty());

        // Getting the endpoints again immediately should give the same results.
        let mut eps1 = ms.direct_addresses().next().await.unwrap();
        eps1.sort();
        println!("{eps1:?}");
        assert_eq!(eps0, eps1);
    }

    #[tokio::test]
    async fn test_watch_home_relay() {
        // use an empty relay map to get full control of the changes during the test
        let ops = Options {
            relay_map: RelayMap::empty(),
            ..Default::default()
        };
        let msock = MagicSock::spawn(ops).await.unwrap();
        let mut relay_stream = msock.watch_home_relay();

        // no relay, nothing to report
        assert_eq!(
            futures_lite::future::poll_once(relay_stream.next()).await,
            None
        );

        let url: RelayUrl = format!("https://{}", EU_RELAY_HOSTNAME).parse().unwrap();
        msock.set_my_relay(Some(url.clone()));

        assert_eq!(relay_stream.next().await, Some(url.clone()));

        // drop the stream and query it again, the result should be immediately available

        let mut relay_stream = msock.watch_home_relay();
        assert_eq!(
            futures_lite::future::poll_once(relay_stream.next()).await,
            Some(Some(url))
        );
    }

    /// Creates a new [`quinn::Endpoint`] hooked up to a [`MagicSock`].
    ///
    /// This is without involving [`crate::endpoint::Endpoint`].  The socket will accept
    /// connections using [`ALPN`].
    ///
    /// Use [`magicsock_connect`] to establish connections.
    #[instrument(name = "ep", skip_all, fields(me = secret_key.public().fmt_short()))]
    async fn magicsock_ep(secret_key: SecretKey) -> anyhow::Result<(quinn::Endpoint, Handle)> {
        let opts = Options {
            addr_v4: None,
            addr_v6: None,
            secret_key: secret_key.clone(),
            relay_map: RelayMap::empty(),
            node_map: None,
            discovery: None,
            dns_resolver: crate::dns::default_resolver().clone(),
            proxy_url: None,
            insecure_skip_relay_cert_verify: true,
        };
        let msock = MagicSock::spawn(opts).await?;
        let server_config = crate::endpoint::make_server_config(
            &secret_key,
            vec![ALPN.to_vec()],
            Arc::new(quinn::TransportConfig::default()),
            true,
        )?;
        let mut endpoint_config = quinn::EndpointConfig::default();
        endpoint_config.grease_quic_bit(false);
        let endpoint = quinn::Endpoint::new_with_abstract_socket(
            endpoint_config,
            Some(server_config),
            Arc::new(msock.clone()),
            Arc::new(quinn::TokioRuntime),
        )?;
        Ok((endpoint, msock))
    }

    /// Connects from `ep` returned by [`magicsock_ep`] to the `node_id`.
    ///
    /// Uses [`ALPN`], `node_id`, must match `addr`.
    #[instrument(name = "connect", skip_all, fields(me = ep_secret_key.public().fmt_short()))]
    async fn magicsock_connect(
        ep: &quinn::Endpoint,
        ep_secret_key: SecretKey,
        addr: QuicMappedAddr,
        node_id: NodeId,
    ) -> Result<quinn::Connection> {
        // Endpoint::connect sets this, do the same to have similar behaviour.
        let mut transport_config = quinn::TransportConfig::default();
        transport_config.keep_alive_interval(Some(Duration::from_secs(1)));

        magicsock_connet_with_transport_config(
            ep,
            ep_secret_key,
            addr,
            node_id,
            Arc::new(transport_config),
        )
        .await
    }

    /// Connects from `ep` returned by [`magicsock_ep`] to the `node_id`.
    ///
    /// This version allows customising the transport config.
    ///
    /// Uses [`ALPN`], `node_id`, must match `addr`.
    #[instrument(name = "connect", skip_all, fields(me = ep_secret_key.public().fmt_short()))]
    async fn magicsock_connet_with_transport_config(
        ep: &quinn::Endpoint,
        ep_secret_key: SecretKey,
        addr: QuicMappedAddr,
        node_id: NodeId,
        transport_config: Arc<quinn::TransportConfig>,
    ) -> Result<quinn::Connection> {
        let alpns = vec![ALPN.to_vec()];
        let quic_client_config =
            tls::make_client_config(&ep_secret_key, Some(node_id), alpns, true)?;
        let mut client_config = quinn::ClientConfig::new(Arc::new(quic_client_config));
        client_config.transport_config(transport_config);
        let connect = ep.connect_with(client_config, addr.0, "localhost")?;
        let connection = connect.await?;
        Ok(connection)
    }

    #[tokio::test]
    async fn test_try_send_no_send_addr() {
        // Regression test: if there is no send_addr we should keep being able to use the
        // Endpoint.
        let _guard = iroh_test::logging::setup();

        let secret_key_1 = SecretKey::from_bytes(&[1u8; 32]);
        let secret_key_2 = SecretKey::from_bytes(&[2u8; 32]);
        let node_id_2 = secret_key_2.public();
        let secret_key_missing_node = SecretKey::from_bytes(&[255u8; 32]);
        let node_id_missing_node = secret_key_missing_node.public();

        let (ep_1, msock_1) = magicsock_ep(secret_key_1.clone()).await.unwrap();

        // Generate an address not present in the NodeMap.
        let bad_addr = QuicMappedAddr::generate();

        // 500ms is rather fast here.  Running this locally it should always be the correct
        // timeout.  If this is too slow however the test will not become flaky as we are
        // expecting the timeout, we might just get the timeout for the wrong reason.  But
        // this speeds up the test.
        let res = tokio::time::timeout(
            Duration::from_millis(500),
            magicsock_connect(&ep_1, secret_key_1.clone(), bad_addr, node_id_missing_node),
        )
        .await;
        assert!(res.is_err(), "expecting timeout");

        // Now check we can still create another connection with this endpoint.
        let (ep_2, msock_2) = magicsock_ep(secret_key_2.clone()).await.unwrap();

        // This needs an accept task
        let accept_task = tokio::spawn({
            async fn accept(ep: quinn::Endpoint) -> Result<()> {
                let incoming = ep.accept().await.ok_or(anyhow!("no incoming"))?;
                let _conn = incoming.accept()?.await?;

                // Keep this connection alive for a while
                tokio::time::sleep(Duration::from_secs(10)).await;
                info!("accept finished");
                Ok(())
            }
            let ep_2 = ep_2.clone();
            async move {
                if let Err(err) = accept(ep_2).await {
                    error!("{err:#}");
                }
            }
            .instrument(info_span!("ep2.accept, me = node_id_2.fmt_short()"))
        });
        let _accept_task = AbortOnDropHandle::new(accept_task);

        let node_addr_2 = NodeAddr {
            node_id: node_id_2,
            info: AddrInfo {
                relay_url: None,
                direct_addresses: msock_2
                    .direct_addresses()
                    .next()
                    .await
                    .expect("no direct addrs")
                    .into_iter()
                    .map(|x| x.addr)
                    .collect(),
            },
        };
        msock_1
            .add_node_addr(
                node_addr_2,
                Source::NamedApp {
                    name: "test".into(),
                },
            )
            .unwrap();
        let addr = msock_1.get_mapping_addr(node_id_2).unwrap();
        let res = tokio::time::timeout(
            Duration::from_secs(10),
            magicsock_connect(&ep_1, secret_key_1.clone(), addr, node_id_2),
        )
        .await
        .expect("timeout while connecting");

        // aka assert!(res.is_ok()) but with nicer error reporting.
        res.unwrap();

        // TODO: Now check if we can connect to a repaired ep_3, but we can't modify that
        // much internal state for now.
    }

    #[tokio::test]
    async fn test_try_send_no_udp_addr_or_relay_url() {
        // This specifically tests the `if udp_addr.is_none() && relay_url.is_none()`
        // behaviour of MagicSock::try_send.
        let _logging_guard = iroh_test::logging::setup();

        let secret_key_1 = SecretKey::from_bytes(&[1u8; 32]);
        let secret_key_2 = SecretKey::from_bytes(&[2u8; 32]);
        let node_id_2 = secret_key_2.public();

        let (ep_1, msock_1) = magicsock_ep(secret_key_1.clone()).await.unwrap();
        let (ep_2, msock_2) = magicsock_ep(secret_key_2.clone()).await.unwrap();

        // We need a task to accept the connection.
        let accept_task = tokio::spawn({
            async fn accept(ep: quinn::Endpoint) -> Result<()> {
                let incoming = ep.accept().await.ok_or(anyhow!("no incoming"))?;
                let conn = incoming.accept()?.await?;
                let mut stream = conn.accept_uni().await?;
                stream.read_to_end(1 << 16).await?;
                info!("accept finished");
                Ok(())
            }
            let ep_2 = ep_2.clone();
            async move {
                if let Err(err) = accept(ep_2).await {
                    error!("{err:#}");
                }
            }
            .instrument(info_span!("ep2.accept", me = node_id_2.fmt_short()))
        });
        let _accept_task = AbortOnDropHandle::new(accept_task);

        // Add an empty entry in the NodeMap of ep_1
        msock_1.node_map.add_node_addr(
            NodeAddr {
                node_id: node_id_2,
                info: AddrInfo::default(),
            },
            Source::NamedApp {
                name: "test".into(),
            },
        );
        let addr_2 = msock_1.get_mapping_addr(node_id_2).unwrap();

        // Set a low max_idle_timeout so quinn gives up on this quickly and our test does
        // not take forever.  You need to check the log output to verify this is really
        // triggering the correct error.
        // In test_try_send_no_send_addr() above you may have noticed we used
        // tokio::time::timeout() on the connection attempt instead.  Here however we want
        // Quinn itself to have fully given up on the connection attempt because we will
        // later connect to **the same** node.  If Quinn did not give up on the connection
        // we'd close it on drop, and the retransmits of the close packets would interfere
        // with the next handshake, closing it during the handshake.  This makes the test a
        // little slower though.
        let mut transport_config = quinn::TransportConfig::default();
        transport_config.max_idle_timeout(Some(Duration::from_millis(200).try_into().unwrap()));
        let res = magicsock_connet_with_transport_config(
            &ep_1,
            secret_key_1.clone(),
            addr_2,
            node_id_2,
            Arc::new(transport_config),
        )
        .await;
        assert!(res.is_err(), "expected timeout");
        info!("first connect timed out as expected");

        // Provide correct addressing information
        msock_1.node_map.add_node_addr(
            NodeAddr {
                node_id: node_id_2,
                info: AddrInfo {
                    relay_url: None,
                    direct_addresses: msock_2
                        .direct_addresses()
                        .next()
                        .await
                        .expect("no direct addrs")
                        .into_iter()
                        .map(|x| x.addr)
                        .collect(),
                },
            },
            Source::NamedApp {
                name: "test".into(),
            },
        );

        // We can now connect
        tokio::time::timeout(Duration::from_secs(10), async move {
            info!("establishing new connection");
            let conn = magicsock_connect(&ep_1, secret_key_1.clone(), addr_2, node_id_2)
                .await
                .unwrap();
            info!("have connection");
            let mut stream = conn.open_uni().await.unwrap();
            stream.write_all(b"hello").await.unwrap();
            stream.finish().unwrap();
            stream.stopped().await.unwrap();
            info!("finished stream");
        })
        .await
        .expect("connection timed out");

        // TODO: could remove the addresses again, send, add it back and see it recover.
        // But we don't have that much private access to the NodeMap.  This will do for now.
    }
}
