/*!
Functionality needed to work as a DHT node.
This module works on top of other modules.
*/

pub mod ping_sender;
pub mod hole_punching;

use futures::{Future, Sink, Stream, future, stream};
use futures::sync::mpsc;
use parking_lot::RwLock;
use tokio::timer::Interval;

use std::io::{ErrorKind, Error};
use std::net::{SocketAddr, IpAddr};
use std::ops::DerefMut;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::mem;

use toxcore::time::*;
use toxcore::crypto_core::*;
use toxcore::dht::packet::*;
use toxcore::dht::packed_node::*;
use toxcore::dht::kbucket::*;
use toxcore::onion::packet::*;
use toxcore::onion::onion_announce::*;
use toxcore::dht::request_queue::*;
use toxcore::io_tokio::*;
use toxcore::dht::dht_friend::*;
use toxcore::dht::server::hole_punching::*;
use toxcore::tcp::packet::OnionRequest;
use toxcore::dht::server::ping_sender::*;
use toxcore::net_crypto::*;
use toxcore::dht::ip_port::IsGlobal;

/// Shorthand for the transmit half of the message channel.
type Tx = mpsc::UnboundedSender<(DhtPacket, SocketAddr)>;

/// Shorthand for the transmit half of the TCP onion channel.
type TcpOnionTx = mpsc::UnboundedSender<(InnerOnionResponse, SocketAddr)>;

/// Number of Nodes Req sending times to find close nodes
pub const MAX_BOOTSTRAP_TIMES: u32 = 5;
/// Interval in seconds of sending NatPingRequest packet
pub const NAT_PING_REQ_INTERVAL: u64 = 3;
/// How often onion key should be refreshed
pub const ONION_REFRESH_KEY_INTERVAL: u64 = 7200;
/// Interval in seconds for random NodesRequest
pub const NODES_REQ_INTERVAL: u64 = 20;
/// Interval in seconds for ping
pub const PING_INTERVAL: u64 = 60;
/// Ping timeout in seconds
pub const PING_TIMEOUT: u64 = 5;

/**
Own DHT node data.

Contains:

- DHT public key
- DHT secret key
- Close List ([`Kbucket`] with nodes close to own DHT public key)

Before a [`PackedNode`] is added to the Close List, it needs to be
checked whether:

- it can be added to [`Kbucket`] \(using [`Kbucket::can_add()`])
- [`PackedNode`] is actually online

Once the first check passes node is added to the temporary list, and
a [`NodesRequest`] request is sent to it in order to check whether it's
online. If the node responds correctly within [`PING_TIMEOUT`], it's
removed from temporary list and added to the Close List.

[`NodesRequest`]: ../dht/struct.NodesRequest.html
[`Kbucket`]: ../dht/struct.Kbucket.html
[`Kbucket::can_add()`]: ../dht/struct.Kbucket.html#method.can_add
[`PackedNode`]: ../dht/struct.PackedNode.html
*/
#[derive(Clone)]
pub struct Server {
    /// secret key
    pub sk: SecretKey,
    /// public key
    pub pk: PublicKey,
    /// tx split of channel to send packet to this peer via udp socket
    pub tx: Tx,
    /// option for hole punching
    pub is_hole_punching_enabled: bool,
    /// store ping object which has sent request packet to peer
    pub request_queue: Arc<RwLock<RequestQueue>>,
    /// Close List (contains nodes close to own DHT PK)
    pub close_nodes: Arc<RwLock<Kbucket>>,
    // symmetric key used for onion return encryption
    onion_symmetric_key: Arc<RwLock<secretbox::Key>>,
    // time when onion key was generated
    onion_symmetric_key_time: Arc<RwLock<Instant>>,
    // onion announce struct to handle onion packets
    onion_announce: Arc<RwLock<OnionAnnounce>>,
    /// friends vector of dht node
    pub friends: Arc<RwLock<Vec<DhtFriend>>>,
    // nodes vector for bootstrap
    bootstrap_nodes: Arc<RwLock<Bucket>>,
    // count for sending NodesRequest to random node which is in close node
    // maximum value is 5, so setting this value to 2 will do sending 3 times
    // setting this value to 0 will do sending 5 times
    bootstrap_times: Arc<RwLock<u32>>,
    last_nodes_req_time: Arc<RwLock<Instant>>,
    ping_sender: Arc<RwLock<PingSender>>,
    // toxcore version used in BootstrapInfo
    tox_core_version: u32,
    // message used in BootstrapInfo
    motd: Vec<u8>,
    // `OnionResponse1` packets that have TCP protocol kind inside onion return
    // should be redirected to TCP sender trough this sink
    // None if there is no TCP relay
    tcp_onion_sink: Option<TcpOnionTx>,
    // Net crypto module that handles `CookieRequest`, `CookieResponse`,
    // `CryptoHandshake` and `CryptoData` packets. It can be `None` in case of
    // pure bootstrap server when we don't have friends and therefore don't
    // have to handle related packets
    net_crypto: Option<NetCrypto>,
    lan_discovery_enabled: bool,
    is_ipv6_mode: bool,
}

impl Server {
    /**
    Create new `Server` instance.
    */
    pub fn new(tx: Tx, pk: PublicKey, sk: SecretKey) -> Server {
        debug!("Created new Server instance");
        Server {
            sk,
            pk,
            tx,
            is_hole_punching_enabled: true,
            request_queue: Arc::new(RwLock::new(RequestQueue::new(Duration::from_secs(PING_TIMEOUT)))),
            close_nodes: Arc::new(RwLock::new(Kbucket::new(&pk))),
            onion_symmetric_key: Arc::new(RwLock::new(secretbox::gen_key())),
            onion_symmetric_key_time: Arc::new(RwLock::new(clock_now())),
            onion_announce: Arc::new(RwLock::new(OnionAnnounce::new(pk))),
            friends: Arc::new(RwLock::new(Vec::new())),
            bootstrap_nodes: Arc::new(RwLock::new(Bucket::new(None))),
            bootstrap_times: Arc::new(RwLock::new(0)),
            last_nodes_req_time: Arc::new(RwLock::new(Instant::now())),
            ping_sender: Arc::new(RwLock::new(PingSender::new())),
            tox_core_version: 0,
            motd: Vec::new(),
            tcp_onion_sink: None,
            net_crypto: None,
            lan_discovery_enabled: true,
            is_ipv6_mode: false,
        }
    }

    /// enable/disacle IPv6 mode of DHT node
    pub fn enable_ipv6_mode(&mut self, enable: bool) {
        self.is_ipv6_mode = enable;
        self.close_nodes.write().is_ipv6_mode = enable;
    }

    /// enable/disable processing LanDiscovery packet received
    pub fn enable_lan_discovery(&mut self, enable: bool) {
        self.lan_discovery_enabled = enable;
    }

    /// add friend
    pub fn add_friend(&self, friend: DhtFriend) {
        let mut friends = self.friends.write();

        friends.push(friend);
    }

    /// main loop of dht server, call this function every second
    fn dht_main_loop(&self) -> IoFuture<()> {
        self.request_queue.write().clear_timed_out();
        self.refresh_onion_key();

        let ping_bootstrap_nodes = self.ping_bootstrap_nodes();
        let ping_and_get_close_nodes = self.ping_and_get_close_nodes();
        let send_nodes_req_random = self.send_nodes_req_random();
        let send_nodes_req_to_friends = self.send_nodes_req_to_friends();

        let ping_sender = self.send_pings();

        let send_nat_ping_req = self.send_nat_ping_req();

        let res = future::join_all(vec![ping_bootstrap_nodes,
                                        ping_and_get_close_nodes,
                                        send_nodes_req_random,
                                        send_nodes_req_to_friends,
                                        ping_sender,
                                        send_nat_ping_req])
            .map(|_| ());

        Box::new(res)
    }

    /// Run DHT main loop periodically. Result future will never be completed
    /// successfully.
    pub fn run(self) -> IoFuture<()> {
        let interval = Duration::from_secs(1);
        let wakeups = Interval::new(Instant::now(), interval);
        let future = wakeups
            .map_err(|e| Error::new(ErrorKind::Other, format!("DHT server timer error: {:?}", e)))
            .for_each(move |_instant| {
                trace!("DHT server wake up");
                self.dht_main_loop()
            });
        Box::new(future)
    }

    // send PingRequest using Ping object
    fn send_pings(&self) -> IoFuture<()> {
        let mut ping_sender = self.ping_sender.write();

        ping_sender.send_pings(&self)
    }

    // send NodesRequest to friends
    fn send_nodes_req_to_friends(&self) -> IoFuture<()> {
        let mut friends = self.friends.write();

        let nodes_sender = friends.iter_mut()
            .map(|friend| {
                friend.send_nodes_req_packets(self)
            });

        let nodes_stream = stream::futures_unordered(nodes_sender).then(|_| Ok(()));
        Box::new(nodes_stream.for_each(|()| Ok(())))
    }

    // send NodesRequest to nodes gotten by NodesResponse
    // this is the checking if the node is alive(ping)
    fn ping_bootstrap_nodes(&self) -> IoFuture<()> {
        let bootstrap_nodes = mem::replace(self.bootstrap_nodes.write().deref_mut(), Bucket::new(None));

        let mut request_queue = self.request_queue.write();

        let bootstrap_nodes = bootstrap_nodes.to_packed();
        let nodes_sender = bootstrap_nodes.iter()
            .map(|node|
                self.send_nodes_req(*node, self.pk, request_queue.new_ping_id(node.pk))
            );

        let nodes_stream = stream::futures_unordered(nodes_sender).then(|_| Ok(()));

        Box::new(nodes_stream.for_each(|()| Ok(())))
    }

    // every 60 seconds DHT node send ping(NodesRequest) to all nodes which is in close list
    fn ping_and_get_close_nodes(&self) -> IoFuture<()> {
        let mut close_nodes = self.close_nodes.write();
        let mut request_queue = self.request_queue.write();

        let nodes_sender = close_nodes.iter_mut()
            .filter(|node|
                node.last_ping_req_time.map_or(true, |time| time.elapsed() >= Duration::from_secs(PING_INTERVAL))
            )
            .map(|node| {
                node.last_ping_req_time = Some(Instant::now());
                self.send_nodes_req(node.clone().into(), self.pk, request_queue.new_ping_id(node.pk))
            });

        let nodes_stream = stream::futures_unordered(nodes_sender).then(|_| Ok(()));

        Box::new(nodes_stream.for_each(|()| Ok(())))
    }

    /// Send `NodesRequest` packet to a random good node every 20 seconds or if
    /// it was sent less than `NODES_REQ_INTERVAL`. This function should be
    /// called every second.
    fn send_nodes_req_random(&self) -> IoFuture<()> {
        if clock_elapsed(*self.last_nodes_req_time.read()) < Duration::from_secs(NODES_REQ_INTERVAL) &&
            *self.bootstrap_times.read() >= MAX_BOOTSTRAP_TIMES {
            return Box::new(future::ok(()));
        }

        let close_nodes = self.close_nodes.read();

        let good_nodes = close_nodes.iter()
            .filter(|&node| !node.is_bad())
            .cloned()
            .map(|node| node.into())
            .collect::<Vec<PackedNode>>();

        if !good_nodes.is_empty() {
            let mut request_queue = self.request_queue.write();

            let mut random_node = random_u32() as usize % good_nodes.len();
            // increase probability of sending packet to a close node (has lower index)
            if random_node != 0 {
                random_node -= random_u32() as usize % (random_node + 1);
            }

            let random_node = good_nodes[random_node];

            let res = self.send_nodes_req(random_node, self.pk, request_queue.new_ping_id(random_node.pk));

            *self.bootstrap_times.write() += 1;
            *self.last_nodes_req_time.write() = Instant::now();

            res
        } else {
            Box::new(future::ok(()))
        }
    }

    /// Send PingRequest to node
    pub fn send_ping_req(&self, node: &PackedNode) -> IoFuture<()> {
        let mut request_queue = self.request_queue.write();

        let payload = PingRequestPayload {
            id: request_queue.new_ping_id(node.pk),
        };
        let ping_req = DhtPacket::PingRequest(PingRequest::new(
            &precompute(&node.pk, &self.sk),
            &self.pk,
            payload
        ));
        self.send_to(node.saddr, ping_req)
    }

    /// Send NodesRequest to peer
    pub fn send_nodes_req(&self, target_peer: PackedNode, search_pk: PublicKey, ping_id: u64) -> IoFuture<()> {
        // Check if packet is going to be sent to ourself.
        if self.pk == target_peer.pk {
            return Box::new(
                future::err(
                    Error::new(ErrorKind::Other, "friend's pk is mine")
                )
            )
        }

        let payload = NodesRequestPayload {
            pk: search_pk,
            id: ping_id,
        };
        let nodes_req = DhtPacket::NodesRequest(NodesRequest::new(
            &precompute(&target_peer.pk, &self.sk),
            &self.pk,
            payload
        ));

        self.send_to(target_peer.saddr, nodes_req)
    }

    // send NatPingRequests to all of my friends and do hole punching.
    fn send_nat_ping_req(&self) -> IoFuture<()> {
        let mut friends = self.friends.write();

        if friends.is_empty() {
            return Box::new(future::ok(()))
        }

        let nats_sender = friends.iter_mut()
            .map(|friend| {
                let addrs_of_clients = friend.get_addrs_of_clients(self.is_ipv6_mode);
                // try hole punching
                friend.hole_punch.try_nat_punch(&self, friend.pk, addrs_of_clients);

                let payload = DhtRequestPayload::NatPingRequest(NatPingRequest {
                    id: friend.hole_punch.ping_id,
                });
                let nat_ping_req_packet = DhtPacket::DhtRequest(DhtRequest::new(
                    &precompute(&friend.pk, &self.sk),
                    &friend.pk,
                    &self.pk,
                    payload
                ));

                if friend.hole_punch.last_send_ping_time.map_or(true, |time| time.elapsed() >= Duration::from_secs(NAT_PING_PUNCHING_INTERVAL)) {
                    friend.hole_punch.last_send_ping_time = Some(Instant::now());
                    self.send_nat_ping_req_inner(friend, nat_ping_req_packet)
                } else {
                    Box::new(future::ok(()))
                }

            });

        let nats_stream = stream::futures_unordered(nats_sender).then(|_| Ok(()));

        Box::new(nats_stream.for_each(|()| Ok(())))
    }

    // actual sending function of NatPingRequest.
    fn send_nat_ping_req_inner(&self, friend: &DhtFriend, nat_ping_req_packet: DhtPacket) -> IoFuture<()> {
        let nats_sender = friend.close_nodes.nodes.iter()
            .map(|node| node.get_socket_addr(self.is_ipv6_mode))
            .filter_map(|addr| addr)
            .map(|addr| {
                self.send_to(addr, nat_ping_req_packet.clone())
            });

        let nats_stream = stream::futures_unordered(nats_sender).then(|_| Ok(()));

        Box::new(nats_stream.for_each(|()| Ok(())))
    }

    /**
    Function to handle incoming packets. If there is a response packet,
    send back it to the peer.
    */
    pub fn handle_packet(&self, packet: DhtPacket, addr: SocketAddr) -> IoFuture<()> {
        match packet {
            DhtPacket::PingRequest(packet) => {
                debug!("Received ping request");
                self.handle_ping_req(packet, addr)
            },
            DhtPacket::PingResponse(packet) => {
                debug!("Received ping response");
                self.handle_ping_resp(packet, addr)
            },
            DhtPacket::NodesRequest(packet) => {
                debug!("Received NodesRequest");
                self.handle_nodes_req(packet, addr)
            },
            DhtPacket::NodesResponse(packet) => {
                debug!("Received NodesResponse");
                self.handle_nodes_resp(packet)
            },
            DhtPacket::CookieRequest(packet) => {
                debug!("Received CookieRequest");
                self.handle_cookie_request(packet, addr)
            },
            DhtPacket::CookieResponse(packet) => {
                debug!("Received CookieResponse");
                self.handle_cookie_response(packet, addr)
            },
            DhtPacket::CryptoHandshake(packet) => {
                debug!("Received CryptoHandshake");
                self.handle_crypto_handshake(packet, addr)
            },
            DhtPacket::DhtRequest(packet) => {
                debug!("Received DhtRequest");
                self.handle_dht_req(packet, addr)
            },
            DhtPacket::LanDiscovery(packet) => {
                debug!("Received LanDiscovery");
                self.handle_lan_discovery(packet, addr)
            },
            DhtPacket::OnionRequest0(packet) => {
                debug!("Received OnionRequest0");
                self.handle_onion_request_0(packet, addr)
            },
            DhtPacket::OnionRequest1(packet) => {
                debug!("Received OnionRequest1");
                self.handle_onion_request_1(packet, addr)
            },
            DhtPacket::OnionRequest2(packet) => {
                debug!("Received OnionRequest2");
                self.handle_onion_request_2(packet, addr)
            },
            DhtPacket::OnionAnnounceRequest(packet) => {
                debug!("Received OnionAnnounceRequest");
                self.handle_onion_announce_request(packet, addr)
            },
            DhtPacket::OnionDataRequest(packet) => {
                debug!("Received OnionDataRequest");
                self.handle_onion_data_request(packet)
            },
            DhtPacket::OnionResponse3(packet) => {
                debug!("Received OnionResponse3");
                self.handle_onion_response_3(packet)
            },
            DhtPacket::OnionResponse2(packet) => {
                debug!("Received OnionResponse2");
                self.handle_onion_response_2(packet)
            },
            DhtPacket::OnionResponse1(packet) => {
                debug!("Received OnionResponse1");
                self.handle_onion_response_1(packet)
            },
            DhtPacket::BootstrapInfo(packet) => {
                debug!("Received BootstrapInfo");
                self.handle_bootstrap_info(packet, addr)
            },
            ref p => {
                Box::new( future::err(
                    Error::new(ErrorKind::Other,
                        format!("DhtPacket is not handled {:?}", p)
                )))
            }
        }
    }

    /// actual send method
    fn send_to(&self, addr: SocketAddr, packet: DhtPacket) -> IoFuture<()> {
        if self.is_ipv6_mode {// DHT node is running in ipv6 mode
            match addr.ip() {
                IpAddr::V4(ip) => {
                    let ip_v6 = ip.to_ipv6_mapped();
                    let addr = SocketAddr::new(IpAddr::V6(ip_v6), addr.port());
                    return send_to(&self.tx, (packet, addr));
                },
                IpAddr::V6(_ip) => {},
            }
        } else { // DHT node is running in ipv4 mode
            if addr.is_ipv6() {
                debug!("DHT node is running in ipv4 mode but target node's socket is ipv6 address");
                return Box::new(future::err(Error::new(
                    ErrorKind::Other,
                    "DHT node is running in ipv4 mode but target node's socket is ipv6 address"
                )))
            }
        }

        send_to(&self.tx, (packet, addr))
    }

    /**
    handle received PingRequest packet, then create PingResponse packet
    and send back it to the peer.
    */
    fn handle_ping_req(&self, packet: PingRequest, addr: SocketAddr) -> IoFuture<()> {
        let payload = packet.get_payload(&self.sk);
        let payload = match payload {
            Err(e) => return Box::new(future::err(e)),
            Ok(payload) => payload,
        };

        let resp_payload = PingResponsePayload {
            id: payload.id,
        };
        let ping_resp = DhtPacket::PingResponse(PingResponse::new(
            &precompute(&packet.pk, &self.sk),
            &self.pk,
            resp_payload
        ));

        // send PingRequest
        let node_to_ping = PackedNode {
            pk: packet.pk,
            saddr: addr,
        };

        // node is added if it's PK is closer than nodes in ping list
        // the result of try_add is ignored, if it is not added, then PingRequest is not sent to the node.
        Box::new(self.ping_sender.write().try_add(&self, &node_to_ping)
            .map(|_| ())
            .join(self.send_to(addr, ping_resp))
            .map(|_| ())
        )
    }
    /**
    handle received PingResponse packet. If ping_id is correct, try_add peer to close_nodes.
    */
    fn handle_ping_resp(&self, packet: PingResponse, addr: SocketAddr) -> IoFuture<()> {
        let payload = packet.get_payload(&self.sk);
        let payload = match payload {
            Err(e) => return Box::new(future::err(e)),
            Ok(payload) => payload,
        };

        if payload.id == 0u64 {
            return Box::new( future::err(
                Error::new(ErrorKind::Other,
                    "PingResponse.ping_id == 0"
            )))
        }

        let mut request_queue = self.request_queue.write();

        if request_queue.check_ping_id(packet.pk, payload.id) {
            let mut close_nodes = self.close_nodes.write();
            if let Some(node) = close_nodes.get_node_mut(&packet.pk) {
                if addr.is_ipv4() {
                    node.last_resp_time_v4 = Instant::now();
                } else {
                    node.last_resp_time_v6 = Instant::now();
                }
                Box::new( future::ok(()) )
            } else {
                Box::new( future::err(
                    Error::new(ErrorKind::Other, "Node from PingResponse does not exist")
                ))
            }
        } else {
            Box::new( future::err(
                Error::new(ErrorKind::Other, "PingResponse.ping_id does not match")
            ))
        }
    }
    /**
    handle received NodesRequest packet, responds with NodesResponse
    */
    fn handle_nodes_req(&self, packet: NodesRequest, addr: SocketAddr) -> IoFuture<()> {
        let payload = packet.get_payload(&self.sk);
        let payload = match payload {
            Err(e) => return Box::new(future::err(e)),
            Ok(payload) => payload,
        };

        let close_nodes = self.close_nodes.read();

        let close_nodes = close_nodes.get_closest(&payload.pk, IsGlobal::is_global(&addr.ip()));

        let mut collected_bucket = Bucket::new(Some(4));

        close_nodes.iter()
            .for_each(|node| {
                collected_bucket.try_add(&payload.pk, node);
            });

        self.friends.read().iter()
            .for_each(|friend| friend.close_nodes.nodes.iter().cloned()
                .for_each(|node| {
                    collected_bucket.try_add(&payload.pk, &node.into());
                })
            );

        let collected_nodes = collected_bucket.nodes.into_iter()
            .map(|node| node.into())
            .collect::<Vec<PackedNode>>();

        let resp_payload = NodesResponsePayload {
            nodes: collected_nodes,
            id: payload.id,
        };

        let nodes_resp = DhtPacket::NodesResponse(NodesResponse::new(
            &precompute(&packet.pk, &self.sk),
            &self.pk,
            resp_payload
        ));

        // send PingRequest
        let node_to_ping = PackedNode {
            pk: packet.pk,
            saddr: addr,
        };

        // node is added if it's PK is closer than nodes in ping list
        // the result of try_add is ignored, if it is not added, then PingRequest is not sent to the node.
        Box::new(self.ping_sender.write().try_add(&self, &node_to_ping)
            .map(|_| ())
            .join(self.send_to(addr, nodes_resp))
            .map(|_| ())
        )
    }
    /**
    handle received NodesResponse from peer.
    */
    fn handle_nodes_resp(&self, packet: NodesResponse) -> IoFuture<()> {
        let payload = packet.get_payload(&self.sk);
        let payload = match payload {
            Err(e) => return Box::new(future::err(e)),
            Ok(payload) => payload,
        };

        let mut request_queue = self.request_queue.write();

        if request_queue.check_ping_id(packet.pk, payload.id) {
            let mut close_nodes = self.close_nodes.write();
            let mut bootstrap_nodes = self.bootstrap_nodes.write();
            let mut friends = self.friends.write();

            for node in &payload.nodes {
                close_nodes.try_add(node);
                bootstrap_nodes.try_add(&self.pk, node);
                friends.iter_mut().for_each(|friend| {
                    friend.add_to_close(node);
                });
            }
            Box::new( future::ok(()) )
        } else {
            Box::new( future::err(
                Error::new(ErrorKind::Other, "NodesResponse.ping_id does not match")
            ))
        }
    }

    /** handle received CookieRequest and pass it to net_crypto module
    */
    fn handle_cookie_request(&self, packet: CookieRequest, addr: SocketAddr) -> IoFuture<()> {
        if let Some(ref net_crypto) = self.net_crypto {
            net_crypto.handle_udp_cookie_request(packet, addr)
        } else {
            Box::new( future::err(
                Error::new(ErrorKind::Other, "Net crypto is not initialised")
            ))
        }
    }

    /** handle received CookieResponse and pass it to net_crypto module
    */
    fn handle_cookie_response(&self, packet: CookieResponse, addr: SocketAddr) -> IoFuture<()> {
        if let Some(ref net_crypto) = self.net_crypto {
            net_crypto.handle_udp_cookie_response(packet, addr)
        } else {
            Box::new( future::err(
                Error::new(ErrorKind::Other, "Net crypto is not initialised")
            ))
        }
    }

    /** handle received CryptoHandshake and pass it to net_crypto module
    */
    fn handle_crypto_handshake(&self, packet: CryptoHandshake, addr: SocketAddr) -> IoFuture<()> {
        if let Some(ref net_crypto) = self.net_crypto {
            net_crypto.handle_udp_crypto_handshake(packet, addr)
        } else {
            Box::new( future::err(
                Error::new(ErrorKind::Other, "Net crypto is not initialised")
            ))
        }
    }

    /**
    handle received DhtRequest, resend if it's sent for someone else, parse and
    handle payload if it's sent for us
    */
    fn handle_dht_req(&self, packet: DhtRequest, addr: SocketAddr) -> IoFuture<()> {
        if packet.rpk == self.pk { // the target peer is me
            let payload = packet.get_payload(&self.sk);
            let payload = match payload {
                Err(e) => return Box::new(future::err(e)),
                Ok(payload) => payload,
            };

            match payload {
                DhtRequestPayload::NatPingRequest(nat_payload) => {
                    debug!("Received nat ping request");
                    self.handle_nat_ping_req(nat_payload, &packet.spk, addr)
                },
                DhtRequestPayload::NatPingResponse(nat_payload) => {
                    debug!("Received nat ping response");
                    let timeout_dur = Duration::from_secs(NAT_PING_PUNCHING_INTERVAL);
                    self.handle_nat_ping_resp(nat_payload, &packet.spk, timeout_dur)
                },
                DhtRequestPayload::DhtPkAnnounce(_dht_pk_payload) => {
                    debug!("Received DHT PublicKey Announce");
                    // TODO: handle this packet in onion client
                    Box::new( future::ok(()) )
                },
            }
        } else {
            let close_nodes = self.close_nodes.read();
            if let Some(node) = close_nodes.get_node(&packet.rpk) { // search close_nodes to find target peer
                if let Some(addr) = node.get_socket_addr(self.is_ipv6_mode) {
                    let packet = DhtPacket::DhtRequest(packet);
                    self.send_to(addr, packet)
                } else {
                    Box::new( future::ok(()) )
                }
            } else {
                Box::new( future::ok(()) )
            }
        }
    }

    /**
    handle received NatPingRequest packet, respond with NatPingResponse
    */
    fn handle_nat_ping_req(&self, payload: NatPingRequest, spk: &PublicKey, addr: SocketAddr) -> IoFuture<()> {
        let resp_payload = DhtRequestPayload::NatPingResponse(NatPingResponse {
            id: payload.id,
        });
        let nat_ping_resp = DhtPacket::DhtRequest(DhtRequest::new(
            &precompute(spk, &self.sk),
            spk,
            &self.pk,
            resp_payload
        ));
        self.send_to(addr, nat_ping_resp)
    }

    /**
    handle received NatPingResponse packet, enable hole-punching
    */
    fn handle_nat_ping_resp(&self, payload: NatPingResponse, spk: &PublicKey, send_nat_ping_interval: Duration) -> IoFuture<()> {
        let mut friends = self.friends.write();
        let friend = friends.iter_mut()
            .find(|friend| friend.pk == *spk);
        let friend = match friend {
            None => return Box::new( future::err(
                Error::new(ErrorKind::Other,
                           "Can't find friend"
                ))),
            Some(friend) => friend,

        };

        if payload.id == 0 {
            return Box::new( future::err(
                Error::new(ErrorKind::Other,
                    "NodesResponse.ping_id == 0"
            )))
        }

        if friend.hole_punch.last_recv_ping_time.elapsed() < send_nat_ping_interval &&
            friend.hole_punch.ping_id == payload.id {
            // enable hole punching
            friend.hole_punch.is_punching_done = false;
            Box::new( future::ok(()) )
        } else {
            Box::new( future::err(
                Error::new(ErrorKind::Other, "NatPingResponse.ping_id does not match or timed out")
            ))
        }
    }
    /**
    handle received LanDiscovery packet, then create NodesRequest packet
    and send back it to the peer.
    */
    fn handle_lan_discovery(&self, packet: LanDiscovery, addr: SocketAddr) -> IoFuture<()> {
        // LanDiscovery is optional
        if !self.lan_discovery_enabled {
            return Box::new(future::ok(()));
        }

        // if Lan Discovery packet has my PK, then it is sent by myself.
        if packet.pk == self.pk {
            return Box::new(future::ok(()));
        }

        let mut request_queue = self.request_queue.write();

        let target_node = PackedNode {
            saddr: addr,
            pk: packet.pk,
        };

        self.send_nodes_req(target_node, self.pk, request_queue.new_ping_id(packet.pk))
    }
    /**
    handle received OnionRequest0 packet, then create OnionRequest1 packet
    and send it to the next peer.
    */
    fn handle_onion_request_0(&self, packet: OnionRequest0, addr: SocketAddr) -> IoFuture<()> {
        let onion_symmetric_key = self.onion_symmetric_key.read();
        let shared_secret = precompute(&packet.temporary_pk, &self.sk);
        let payload = packet.get_payload(&shared_secret);
        let payload = match payload {
            Err(e) => return Box::new(future::err(e)),
            Ok(payload) => payload,
        };

        let onion_return = OnionReturn::new(
            &onion_symmetric_key,
            &IpPort::from_udp_saddr(addr),
            None // no previous onion return
        );
        let next_packet = DhtPacket::OnionRequest1(OnionRequest1 {
            nonce: packet.nonce,
            temporary_pk: payload.temporary_pk,
            payload: payload.inner,
            onion_return
        });
        self.send_to(payload.ip_port.to_saddr(), next_packet)
    }
    /**
    handle received OnionRequest1 packet, then create OnionRequest2 packet
    and send it to the next peer.
    */
    fn handle_onion_request_1(&self, packet: OnionRequest1, addr: SocketAddr) -> IoFuture<()> {
        let onion_symmetric_key = self.onion_symmetric_key.read();
        let shared_secret = precompute(&packet.temporary_pk, &self.sk);
        let payload = packet.get_payload(&shared_secret);
        let payload = match payload {
            Err(e) => return Box::new(future::err(e)),
            Ok(payload) => payload,
        };

        let onion_return = OnionReturn::new(
            &onion_symmetric_key,
            &IpPort::from_udp_saddr(addr),
            Some(&packet.onion_return)
        );
        let next_packet = DhtPacket::OnionRequest2(OnionRequest2 {
            nonce: packet.nonce,
            temporary_pk: payload.temporary_pk,
            payload: payload.inner,
            onion_return
        });
        self.send_to(payload.ip_port.to_saddr(), next_packet)
    }
    /**
    handle received OnionRequest2 packet, then create OnionAnnounceRequest
    or OnionDataRequest packet and send it to the next peer.
    */
    fn handle_onion_request_2(&self, packet: OnionRequest2, addr: SocketAddr) -> IoFuture<()> {
        let onion_symmetric_key = self.onion_symmetric_key.read();
        let shared_secret = precompute(&packet.temporary_pk, &self.sk);
        let payload = packet.get_payload(&shared_secret);
        let payload = match payload {
            Err(e) => return Box::new(future::err(e)),
            Ok(payload) => payload,
        };

        let onion_return = OnionReturn::new(
            &onion_symmetric_key,
            &IpPort::from_udp_saddr(addr),
            Some(&packet.onion_return)
        );
        let next_packet = match payload.inner {
            InnerOnionRequest::InnerOnionAnnounceRequest(inner) => DhtPacket::OnionAnnounceRequest(OnionAnnounceRequest {
                inner,
                onion_return
            }),
            InnerOnionRequest::InnerOnionDataRequest(inner) => DhtPacket::OnionDataRequest(OnionDataRequest {
                inner,
                onion_return
            }),
        };
        self.send_to(payload.ip_port.to_saddr(), next_packet)
    }
    /**
    handle received OnionAnnounceRequest packet and send OnionAnnounceResponse
    packet back if request succeed.
    */
    fn handle_onion_announce_request(&self, packet: OnionAnnounceRequest, addr: SocketAddr) -> IoFuture<()> {
        let mut onion_announce = self.onion_announce.write();
        let close_nodes = self.close_nodes.read();
        let onion_return = packet.onion_return.clone();
        let response = onion_announce.handle_onion_announce_request(packet, &self.sk, &close_nodes, addr);
        match response {
            Ok(response) => self.send_to(addr, DhtPacket::OnionResponse3(OnionResponse3 {
                onion_return,
                payload: InnerOnionResponse::OnionAnnounceResponse(response)
            })),
            Err(e) => Box::new(future::err(e))
        }
    }
    /**
    handle received OnionDataRequest packet and send OnionResponse3 with inner
    OnionDataResponse to destination node through its onion path.
    */
    fn handle_onion_data_request(&self, packet: OnionDataRequest) -> IoFuture<()> {
        let onion_announce = self.onion_announce.read();
        match onion_announce.handle_data_request(packet) {
            Ok((response, addr)) => self.send_to(addr, DhtPacket::OnionResponse3(response)),
            Err(e) => Box::new(future::err(e))
        }
    }
    /**
    handle received OnionResponse3 packet, then create OnionResponse2 packet
    and send it to the next peer which address is stored in encrypted onion return.
    */
    fn handle_onion_response_3(&self, packet: OnionResponse3) -> IoFuture<()> {
        let onion_symmetric_key = self.onion_symmetric_key.read();
        let payload = packet.onion_return.get_payload(&onion_symmetric_key);
        let payload = match payload {
            Err(e) => return Box::new(future::err(e)),
            Ok(payload) => payload,
        };

        if let (ip_port, Some(next_onion_return)) = payload {
            let next_packet = DhtPacket::OnionResponse2(OnionResponse2 {
                onion_return: next_onion_return,
                payload: packet.payload
            });
            self.send_to(ip_port.to_saddr(), next_packet)
        } else {
            Box::new( future::err(
                Error::new(ErrorKind::Other,
                    format!("OnionResponse3 next_onion_return is none")
            )))
        }
    }
    /**
    handle received OnionResponse2 packet, then create OnionResponse1 packet
    and send it to the next peer which address is stored in encrypted onion return.
    */
    fn handle_onion_response_2(&self, packet: OnionResponse2) -> IoFuture<()> {
        let onion_symmetric_key = self.onion_symmetric_key.read();
        let payload = packet.onion_return.get_payload(&onion_symmetric_key);
        let payload = match payload {
            Err(e) => return Box::new(future::err(e)),
            Ok(payload) => payload,
        };

        if let (ip_port, Some(next_onion_return)) = payload {
            let next_packet = DhtPacket::OnionResponse1(OnionResponse1 {
                onion_return: next_onion_return,
                payload: packet.payload
            });
            self.send_to(ip_port.to_saddr(), next_packet)
        } else {
            Box::new( future::err(
                Error::new(ErrorKind::Other,
                    format!("OnionResponse2 next_onion_return is none")
            )))
        }
    }
    /**
    handle received OnionResponse1 packet, then create OnionAnnounceResponse
    or OnionDataResponse packet and send it to the next peer which address
    is stored in encrypted onion return.
    */
    fn handle_onion_response_1(&self, packet: OnionResponse1) -> IoFuture<()> {
        let onion_symmetric_key = self.onion_symmetric_key.read();
        let payload = packet.onion_return.get_payload(&onion_symmetric_key);
        let payload = match payload {
            Err(e) => return Box::new(future::err(e)),
            Ok(payload) => payload,
        };

        if let (ip_port, None) = payload {
            match ip_port.protocol {
                ProtocolType::UDP => {
                    let next_packet = match packet.payload {
                        InnerOnionResponse::OnionAnnounceResponse(inner) => DhtPacket::OnionAnnounceResponse(inner),
                        InnerOnionResponse::OnionDataResponse(inner) => DhtPacket::OnionDataResponse(inner),
                    };
                    self.send_to(ip_port.to_saddr(), next_packet)
                },
                ProtocolType::TCP => {
                    if let Some(ref tcp_onion_sink) = self.tcp_onion_sink {
                        Box::new(tcp_onion_sink.clone() // clone sink for 1 send only
                            .send((packet.payload, ip_port.to_saddr()))
                            .map(|_sink| ()) // ignore sink because it was cloned
                            .map_err(|_| {
                                // This may only happen if sink is gone
                                // So cast SendError<T> to a corresponding std::io::Error
                                Error::from(ErrorKind::UnexpectedEof)
                            })
                        )
                    } else {
                        Box::new( future::err(
                            Error::new(ErrorKind::Other,
                                format!("OnionResponse1 can't be redirected to TCP relay")
                        )))
                    }
                },
            }
        } else {
            Box::new( future::err(
                Error::new(ErrorKind::Other,
                    format!("OnionResponse1 next_onion_return is some")
            )))
        }
    }
    /// refresh onion symmetric key to enforce onion paths expiration
    fn refresh_onion_key(&self) {
        if clock_elapsed(*self.onion_symmetric_key_time.read()) >= Duration::from_secs(ONION_REFRESH_KEY_INTERVAL) {
            *self.onion_symmetric_key_time.write() = clock_now();
            *self.onion_symmetric_key.write() = secretbox::gen_key();
        }
    }
    /// add PackedNode object to close_nodes as a thread-safe manner
    pub fn try_add_to_close_nodes(&self, pn: &PackedNode) -> bool {
        let mut close_nodes = self.close_nodes.write();
        close_nodes.try_add(pn)
    }
    /// handle OnionRequest from TCP relay and send OnionRequest1 packet
    /// to the next node in the onion path
    pub fn handle_tcp_onion_request(&self, packet: OnionRequest, addr: SocketAddr) -> IoFuture<()> {
        let onion_symmetric_key = self.onion_symmetric_key.read();

        let onion_return = OnionReturn::new(
            &onion_symmetric_key,
            &IpPort::from_tcp_saddr(addr),
            None // no previous onion return
        );
        let next_packet = DhtPacket::OnionRequest1(OnionRequest1 {
            nonce: packet.nonce,
            temporary_pk: packet.temporary_pk,
            payload: packet.payload,
            onion_return
        });
        self.send_to(packet.ip_port.to_saddr(), next_packet)
    }
    // handle BootstrapInfo, respond with BootstrapInfo
    fn handle_bootstrap_info(&self, _packet: BootstrapInfo, addr: SocketAddr) -> IoFuture<()> {
        let packet = DhtPacket::BootstrapInfo(BootstrapInfo {
            version: self.tox_core_version,
            motd: self.motd.clone(),
        });
        self.send_to(addr, packet)
    }
    /// set toxcore verson and motd
    pub fn set_bootstrap_info(&mut self, version: u32, motd: Vec<u8>) {
        self.tox_core_version = version;
        self.motd = motd;
    }
    /// set TCP sink for onion packets
    pub fn set_tcp_onion_sink(&mut self, tcp_onion_sink: TcpOnionTx) {
        self.tcp_onion_sink = Some(tcp_onion_sink)
    }
    /// set net crypto module
    pub fn set_net_crypto(&mut self, net_crypto: NetCrypto) {
        self.net_crypto = Some(net_crypto);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use futures::Future;
    use std::net::SocketAddr;
    use tokio_executor;
    use tokio_timer::clock::*;

    use toxcore::time::ConstNow;

    const ONION_RETURN_1_PAYLOAD_SIZE: usize = ONION_RETURN_1_SIZE - secretbox::NONCEBYTES;
    const ONION_RETURN_2_PAYLOAD_SIZE: usize = ONION_RETURN_2_SIZE - secretbox::NONCEBYTES;
    const ONION_RETURN_3_PAYLOAD_SIZE: usize = ONION_RETURN_3_SIZE - secretbox::NONCEBYTES;

    fn create_node() -> (Server, PrecomputedKey, PublicKey, SecretKey,
            mpsc::UnboundedReceiver<(DhtPacket, SocketAddr)>, SocketAddr) {
        crypto_init();

        let (pk, sk) = gen_keypair();
        let (tx, rx) = mpsc::unbounded::<(DhtPacket, SocketAddr)>();
        let alice = Server::new(tx, pk, sk);
        let (bob_pk, bob_sk) = gen_keypair();
        let precomp = precompute(&alice.pk, &bob_sk);

        let addr: SocketAddr = "127.0.0.1:12346".parse().unwrap();
        (alice, precomp, bob_pk, bob_sk, rx, addr)
    }

    #[test]
    fn server_is_clonable() {
        let (alice, _precomp, _bob_pk, _bob_sk, _rx, _addr) = create_node();
        let _ = alice.clone();
    }

    // new()
    #[test]
    fn server_new_test() {
        crypto_init();

        let (pk, sk) = gen_keypair();
        let tx: Tx = mpsc::unbounded().0;
        let _ = Server::new(tx, pk, sk);
    }

    #[test]
    fn add_friend_test() {
        let (alice, _precomp, bob_pk, _bob_sk, _rx, _addr) = create_node();

        let friend = DhtFriend::new(bob_pk, 0);
        alice.add_friend(friend);
    }

    // test handle_packet() with BootstrapInfo packet type
    #[test]
    fn server_handle_packet_with_bootstrap_info_packet_test() {
        let (alice, _precomp, _bob_pk, _bob_sk, _rx, addr) = create_node();
        let packet = DhtPacket::BootstrapInfo(BootstrapInfo {
            version: 00,
            motd: b"Hello".to_owned().to_vec(),
        });
        assert!(alice.handle_packet(packet, addr).wait().is_ok());
    }

    // handle_ping_req()
    #[test]
    fn server_handle_ping_req_test() {
        let (alice, precomp, bob_pk, bob_sk, rx, addr) = create_node();

        // handle ping request, request from bob peer
        let req_payload = PingRequestPayload { id: 42 };
        let ping_req = DhtPacket::PingRequest(PingRequest::new(&precomp, &bob_pk, req_payload));

        assert!(alice.handle_packet(ping_req, addr).wait().is_ok());

        let (received, _rx) = rx.into_future().wait().unwrap();
        let (packet, addr_to_send) = received.unwrap();

        assert_eq!(addr_to_send, addr);

        let ping_resp = unpack!(packet, DhtPacket::PingResponse);
        let ping_resp_payload = ping_resp.get_payload(&bob_sk).unwrap();

        assert_eq!(ping_resp_payload.id, req_payload.id);
    }

    #[test]
    fn server_handle_ping_req_invalid_payload_test() {
        let (alice, precomp, _bob_pk, _bob_sk, _rx, addr) = create_node();

        // error case: can't decrypt
        let req_payload = PingRequestPayload { id: 42 };
        let ping_req = DhtPacket::PingRequest(PingRequest::new(&precomp, &alice.pk, req_payload));

        assert!(alice.handle_packet(ping_req, addr).wait().is_err());
    }

    // handle_ping_resp()
    #[test]
    fn server_handle_ping_resp_test() {
        let (alice, precomp, bob_pk, _bob_sk, _rx, addr) = create_node();

        let packed_node = PackedNode::new(false, addr, &bob_pk);
        assert!(alice.try_add_to_close_nodes(&packed_node));

        let ping_id = alice.request_queue.write().new_ping_id(bob_pk);

        let resp_payload = PingResponsePayload { id: ping_id };
        let ping_resp = DhtPacket::PingResponse(PingResponse::new(&precomp, &bob_pk, resp_payload));

        assert!(alice.handle_packet(ping_resp, addr).wait().is_ok());

        //TODO: check node resp time
    }

    #[test]
    fn server_handle_ping_resp_invalid_payload_test() {
        let (alice, precomp, bob_pk, _bob_sk, _rx, addr) = create_node();

        let packed_node = PackedNode::new(false, addr, &bob_pk);
        assert!(alice.try_add_to_close_nodes(&packed_node));

        let ping_id = alice.request_queue.write().new_ping_id(bob_pk);

        let prs = PingResponsePayload { id: ping_id };
        let ping_resp = DhtPacket::PingResponse(PingResponse::new(&precomp, &alice.pk, prs));

        assert!(alice.handle_packet(ping_resp, addr).wait().is_err());
    }

    #[test]
    fn server_handle_ping_resp_ping_id_is_0_test() {
        let (alice, precomp, bob_pk, _bob_sk, _rx, addr) = create_node();

        let packed_node = PackedNode::new(false, addr, &bob_pk);
        assert!(alice.try_add_to_close_nodes(&packed_node));

        let prs = PingResponsePayload { id: 0 };
        let ping_resp = DhtPacket::PingResponse(PingResponse::new(&precomp, &bob_pk, prs));

        assert!(alice.handle_packet(ping_resp, addr).wait().is_err());
    }

    #[test]
    fn server_handle_ping_resp_invalid_ping_id_test() {
        let (alice, precomp, bob_pk, _bob_sk, _rx, addr) = create_node();

        let packed_node = PackedNode::new(false, addr, &bob_pk);
        assert!(alice.try_add_to_close_nodes(&packed_node));

        let ping_id = alice.request_queue.write().new_ping_id(bob_pk);

        let prs = PingResponsePayload { id: ping_id + 1 };
        let ping_resp = DhtPacket::PingResponse(PingResponse::new(&precomp, &bob_pk, prs));

        assert!(alice.handle_packet(ping_resp, addr).wait().is_err());
    }

    // handle_nodes_req()
    #[test]
    fn server_handle_nodes_req_test() {
        let (alice, precomp, bob_pk, bob_sk, rx, addr) = create_node();

        // success case
        let packed_node = PackedNode::new(false, SocketAddr::V4("127.0.0.1:12345".parse().unwrap()), &bob_pk);

        assert!(alice.try_add_to_close_nodes(&packed_node));

        let req_payload = NodesRequestPayload { pk: bob_pk, id: 42 };
        let nodes_req = DhtPacket::NodesRequest(NodesRequest::new(&precomp, &bob_pk, req_payload));

        assert!(alice.handle_packet(nodes_req, addr).wait().is_ok());

        let (received, _rx) = rx.into_future().wait().unwrap();
        let (packet, addr_to_send) = received.unwrap();

        assert_eq!(addr_to_send, addr);

        let nodes_resp = unpack!(packet, DhtPacket::NodesResponse);

        let nodes_resp_payload = nodes_resp.get_payload(&bob_sk).unwrap();

        assert_eq!(nodes_resp_payload.id, req_payload.id);
    }

    #[test]
    fn server_handle_nodes_req_invalid_payload_test() {
        let (alice, precomp, bob_pk, _bob_sk, _rx, addr) = create_node();

        // error case, can't decrypt
        let req_payload = NodesRequestPayload { pk: bob_pk, id: 42 };
        let nodes_req = DhtPacket::NodesRequest(NodesRequest::new(&precomp, &alice.pk, req_payload));

        assert!(alice.handle_packet(nodes_req, addr).wait().is_err());
    }

    // handle_nodes_resp()
    #[test]
    fn server_handle_nodes_resp_test() {
        let (alice, precomp, bob_pk, _bob_sk, _rx, addr) = create_node();

        let node = vec![PackedNode::new(false, addr, &bob_pk)];

        let ping_id = alice.request_queue.write().new_ping_id(bob_pk);

        let resp_payload = NodesResponsePayload { nodes: node, id: ping_id };
        let nodes_resp = DhtPacket::NodesResponse(NodesResponse::new(&precomp, &bob_pk, resp_payload.clone()));

        assert!(alice.handle_packet(nodes_resp, addr).wait().is_ok());

        let mut close_nodes = Kbucket::new(&alice.pk);
        for pn in &resp_payload.nodes {
            close_nodes.try_add(pn);
        }

        let server_close_nodes = alice.close_nodes.read();

        assert_eq!(server_close_nodes.get_node(&bob_pk).unwrap().pk, close_nodes.get_node(&bob_pk).unwrap().pk);
    }

    #[test]
    fn server_handle_nodes_resp_invalid_payload_test() {
        let (alice, precomp, _bob_pk, _bob_sk, _rx, addr) = create_node();

        // error case, can't decrypt
        let resp_payload = NodesResponsePayload { nodes: vec![
            PackedNode::new(false, SocketAddr::V4("127.0.0.1:12345".parse().unwrap()), &gen_keypair().0)
        ], id: 38 };
        let nodes_resp = DhtPacket::NodesResponse(NodesResponse::new(&precomp, &alice.pk, resp_payload));

        assert!(alice.handle_packet(nodes_resp, addr).wait().is_err());
    }

    #[test]
    fn server_handle_nodes_resp_ping_id_is_0_test() {
        let (alice, precomp, bob_pk, _bob_sk, _rx, addr) = create_node();

        let resp_payload = NodesResponsePayload { nodes: vec![
            PackedNode::new(false, SocketAddr::V4("127.0.0.1:12345".parse().unwrap()), &gen_keypair().0)
        ], id: 0 };
        let nodes_resp = DhtPacket::NodesResponse(NodesResponse::new(&precomp, &bob_pk, resp_payload));

        assert!(alice.handle_packet(nodes_resp, addr).wait().is_err());
    }

    #[test]
    fn server_handle_nodes_resp_invalid_ping_id_test() {
        let (alice, precomp, bob_pk, _bob_sk, _rx, addr) = create_node();

        let ping_id = alice.request_queue.write().new_ping_id(bob_pk);

        let resp_payload = NodesResponsePayload { nodes: vec![
            PackedNode::new(false, SocketAddr::V4("127.0.0.1:12345".parse().unwrap()), &gen_keypair().0)
        ], id: ping_id + 1 };
        let nodes_resp = DhtPacket::NodesResponse(NodesResponse::new(&precomp, &bob_pk, resp_payload));

        assert!(alice.handle_packet(nodes_resp, addr).wait().is_err());
    }

    // handle_cookie_request
    #[test]
    fn handle_cookie_request_test() {
        let (udp_tx, udp_rx) = mpsc::unbounded();
        let (dht_pk, dht_sk) = gen_keypair();
        let mut alice = Server::new(udp_tx.clone(), dht_pk, dht_sk.clone());

        let (dht_pk_tx, _dht_pk_rx) = mpsc::unbounded();
        let (lossless_tx, _lossless_rx) = mpsc::unbounded();
        let (lossy_tx, _lossy_rx) = mpsc::unbounded();
        let (real_pk, _real_sk) = gen_keypair();
        let (bob_pk, bob_sk) = gen_keypair();
        let (bob_real_pk, _bob_real_sk) = gen_keypair();
        let precomp = precompute(&alice.pk, &bob_sk);
        let net_crypto = NetCrypto::new(NetCryptoNewArgs {
            udp_tx,
            dht_pk_tx,
            lossless_tx,
            lossy_tx,
            dht_pk,
            dht_sk,
            real_pk
        });

        alice.set_net_crypto(net_crypto);

        let addr = "127.0.0.1:12346".parse().unwrap();

        let cookie_request_id = 12345;
        let cookie_request_payload = CookieRequestPayload {
            pk: bob_real_pk,
            id: cookie_request_id,
        };
        let cookie_request = DhtPacket::CookieRequest(CookieRequest::new(&precomp, &bob_pk, cookie_request_payload));

        assert!(alice.handle_packet(cookie_request, addr).wait().is_ok());

        let (received, _udp_rx) = udp_rx.into_future().wait().unwrap();
        let (packet, addr_to_send) = received.unwrap();

        assert_eq!(addr_to_send, addr);

        let packet = unpack!(packet, DhtPacket::CookieResponse);
        let payload = packet.get_payload(&precomp).unwrap();

        assert_eq!(payload.id, cookie_request_id);
    }

    #[test]
    fn handle_cookie_request_uninitialized_test() {
        let (alice, precomp, bob_pk, _bob_sk, _rx, addr) = create_node();

        let (bob_real_pk, _bob_real_sk) = gen_keypair();

        let cookie_request_payload = CookieRequestPayload {
            pk: bob_real_pk,
            id: 12345,
        };
        let cookie_request = DhtPacket::CookieRequest(CookieRequest::new(&precomp, &bob_pk, cookie_request_payload));

        assert!(alice.handle_packet(cookie_request, addr).wait().is_err());
    }

    // handle_cookie_response
    #[test]
    fn handle_cookie_response_uninitialized_test() {
        let (alice, precomp, _bob_pk, _bob_sk, _rx, addr) = create_node();

        let cookie = EncryptedCookie {
            nonce: secretbox::gen_nonce(),
            payload: vec![43; 88]
        };
        let cookie_response_payload = CookieResponsePayload {
            cookie: cookie.clone(),
            id: 12345
        };
        let cookie_response = DhtPacket::CookieResponse(CookieResponse::new(&precomp, cookie_response_payload));

        assert!(alice.handle_packet(cookie_response, addr).wait().is_err());
    }

    // handle_crypto_handshake
    #[test]
    fn handle_crypto_handshake_uninitialized_test() {
        let (alice, precomp, _bob_pk, _bob_sk, _rx, addr) = create_node();

        let cookie = EncryptedCookie {
            nonce: secretbox::gen_nonce(),
            payload: vec![43; 88]
        };
        let crypto_handshake_payload = CryptoHandshakePayload {
            base_nonce: gen_nonce(),
            session_pk: gen_keypair().0,
            cookie_hash: cookie.hash(),
            cookie: cookie.clone()
        };
        let crypto_handshake = DhtPacket::CryptoHandshake(CryptoHandshake::new(&precomp, crypto_handshake_payload, cookie));

        assert!(alice.handle_packet(crypto_handshake, addr).wait().is_err());
    }

    // handle_dht_req
    #[test]
    fn server_handle_dht_req_for_unknown_node_test() {
        let (alice, _precomp, bob_pk, bob_sk, _rx, addr) = create_node();

        let (charlie_pk, _charlie_sk) = gen_keypair();
        let precomp = precompute(&charlie_pk, &bob_sk);

        // if receiver' pk != node's pk just returns ok()
        let nat_req = NatPingRequest { id: 42 };
        let nat_payload = DhtRequestPayload::NatPingRequest(nat_req);
        let dht_req = DhtPacket::DhtRequest(DhtRequest::new(&precomp, &charlie_pk, &bob_pk, nat_payload));

        assert!(alice.handle_packet(dht_req, addr).wait().is_ok());
    }

    #[test]
    fn server_handle_dht_req_for_known_node_test() {
        let (alice, _precomp, bob_pk, bob_sk, _rx, addr) = create_node();

        let (charlie_pk, _charlie_sk) = gen_keypair();
        let precomp = precompute(&charlie_pk, &bob_sk);

        // if receiver' pk != node's pk and receiver's pk exists in close_nodes, returns ok()
        let pn = PackedNode::new(false, SocketAddr::V4("127.0.0.1:12345".parse().unwrap()), &charlie_pk);
        alice.try_add_to_close_nodes(&pn);

        let nat_req = NatPingRequest { id: 42 };
        let nat_payload = DhtRequestPayload::NatPingRequest(nat_req);
        let dht_req = DhtPacket::DhtRequest(DhtRequest::new(&precomp, &charlie_pk, &bob_pk, nat_payload));

        assert!(alice.handle_packet(dht_req, addr).wait().is_ok());
    }

    #[test]
    fn server_handle_dht_req_invalid_payload() {
        let (alice, _precomp, bob_pk, _bob_sk, _rx, addr) = create_node();

        let dht_req = DhtPacket::DhtRequest(DhtRequest {
            rpk: alice.pk,
            spk: bob_pk,
            nonce: gen_nonce(),
            payload: vec![42; 123]
        });

        assert!(alice.handle_packet(dht_req, addr).wait().is_err());
    }

    // handle nat ping request
    #[test]
    fn server_handle_nat_ping_req_test() {
        let (alice, precomp, bob_pk, bob_sk, rx, addr) = create_node();

        let nat_req = NatPingRequest { id: 42 };
        let nat_payload = DhtRequestPayload::NatPingRequest(nat_req);
        let dht_req = DhtPacket::DhtRequest(DhtRequest::new(&precomp, &alice.pk, &bob_pk, nat_payload));

        assert!(alice.handle_packet(dht_req, addr).wait().is_ok());

        let (received, _rx) = rx.into_future().wait().unwrap();
        let (packet, addr_to_send) = received.unwrap();

        assert_eq!(addr_to_send, addr);

        let dht_req = unpack!(packet, DhtPacket::DhtRequest);
        let dht_payload = dht_req.get_payload(&bob_sk).unwrap();
        let nat_ping_resp_payload = unpack!(dht_payload, DhtRequestPayload::NatPingResponse);

        assert_eq!(nat_ping_resp_payload.id, nat_req.id);
    }

    // handle nat ping response
    #[test]
    fn server_handle_nat_ping_resp_test() {
        let (alice, precomp, bob_pk, _bob_sk, _rx, addr) = create_node();

        // success case
        let (friend_pk1, _friend_sk1) = gen_keypair();

        let mut friend = DhtFriend::new(bob_pk, 0);
        let pn = PackedNode::new(false, SocketAddr::V4("127.1.1.1:12345".parse().unwrap()), &friend_pk1);
        friend.close_nodes.try_add(&bob_pk, &pn);
        let ping_id = friend.hole_punch.ping_id;
        alice.add_friend(friend);

        let nat_res = NatPingResponse { id: ping_id };
        let nat_payload = DhtRequestPayload::NatPingResponse(nat_res);
        let dht_req = DhtPacket::DhtRequest(DhtRequest::new(&precomp, &alice.pk, &bob_pk, nat_payload));

        assert!(alice.handle_packet(dht_req, addr).wait().is_ok());
    }

    #[test]
    fn server_handle_nat_ping_resp_ping_id_is_0_test() {
        let (alice, precomp, bob_pk, _bob_sk, _rx, addr) = create_node();

        // error case, ping_id = 0
        let nat_res = NatPingResponse { id: 0 };
        let nat_payload = DhtRequestPayload::NatPingResponse(nat_res);
        let dht_req = DhtPacket::DhtRequest(DhtRequest::new(&precomp, &alice.pk, &bob_pk, nat_payload));

        assert!(alice.handle_packet(dht_req, addr).wait().is_err());
    }

    #[test]
    fn server_handle_nat_ping_resp_invalid_ping_id_test() {
        let (alice, precomp, bob_pk, _bob_sk, _rx, addr) = create_node();

        // error case, incorrect ping_id
        let ping_id = alice.request_queue.write().new_ping_id(bob_pk);

        let nat_res = NatPingResponse { id: ping_id + 1 };
        let nat_payload = DhtRequestPayload::NatPingResponse(nat_res);
        let dht_req = DhtPacket::DhtRequest(DhtRequest::new(&precomp, &alice.pk, &bob_pk, nat_payload));

        assert!(alice.handle_packet(dht_req, addr).wait().is_err());
    }

    // handle_onion_request_0
    #[test]
    fn server_handle_onion_request_0_test() {
        let (alice, precomp, bob_pk, _bob_sk, rx, addr) = create_node();

        let temporary_pk = gen_keypair().0;
        let inner = vec![42; 123];
        let ip_port = IpPort {
            protocol: ProtocolType::UDP,
            ip_addr: "5.6.7.8".parse().unwrap(),
            port: 12345
        };
        let payload = OnionRequest0Payload {
            ip_port: ip_port.clone(),
            temporary_pk,
            inner: inner.clone()
        };
        let packet = DhtPacket::OnionRequest0(OnionRequest0::new(&precomp, &bob_pk, payload));

        assert!(alice.handle_packet(packet, addr).wait().is_ok());

        let (received, _rx) = rx.into_future().wait().unwrap();
        let (packet, addr_to_send) = received.unwrap();

        assert_eq!(addr_to_send, ip_port.to_saddr());

        let next_packet = unpack!(packet, DhtPacket::OnionRequest1);

        assert_eq!(next_packet.temporary_pk, temporary_pk);
        assert_eq!(next_packet.payload, inner);

        let onion_symmetric_key = alice.onion_symmetric_key.read();
        let onion_return_payload = next_packet.onion_return.get_payload(&onion_symmetric_key).unwrap();

        assert_eq!(onion_return_payload.0, IpPort::from_udp_saddr(addr));
    }

    #[test]
    fn server_handle_onion_request_0_invalid_payload_test() {
        let (alice, _precomp, _bob_pk, _bob_sk, _rx, addr) = create_node();

        let packet = DhtPacket::OnionRequest0(OnionRequest0 {
            nonce: gen_nonce(),
            temporary_pk: gen_keypair().0,
            payload: vec![42; 123] // not encrypted with dht pk
        });

        assert!(alice.handle_packet(packet, addr).wait().is_err());
    }

    // handle_onion_request_1
    #[test]
    fn server_handle_onion_request_1_test() {
        let (alice, precomp, bob_pk, _bob_sk, rx, addr) = create_node();

        let temporary_pk = gen_keypair().0;
        let inner = vec![42; 123];
        let ip_port = IpPort {
            protocol: ProtocolType::UDP,
            ip_addr: "5.6.7.8".parse().unwrap(),
            port: 12345
        };
        let payload = OnionRequest1Payload {
            ip_port: ip_port.clone(),
            temporary_pk,
            inner: inner.clone()
        };
        let onion_return = OnionReturn {
            nonce: secretbox::gen_nonce(),
            payload: vec![42; ONION_RETURN_1_PAYLOAD_SIZE]
        };
        let packet = DhtPacket::OnionRequest1(OnionRequest1::new(&precomp, &bob_pk, payload, onion_return));

        assert!(alice.handle_packet(packet, addr).wait().is_ok());

        let (received, _rx) = rx.into_future().wait().unwrap();
        let (packet, addr_to_send) = received.unwrap();

        assert_eq!(addr_to_send, ip_port.to_saddr());

        let next_packet = unpack!(packet, DhtPacket::OnionRequest2);

        assert_eq!(next_packet.temporary_pk, temporary_pk);
        assert_eq!(next_packet.payload, inner);

        let onion_symmetric_key = alice.onion_symmetric_key.read();
        let onion_return_payload = next_packet.onion_return.get_payload(&onion_symmetric_key).unwrap();

        assert_eq!(onion_return_payload.0, IpPort::from_udp_saddr(addr));
    }

    #[test]
    fn server_handle_onion_request_1_invalid_payload_test() {
        let (alice, _precomp, _bob_pk, _bob_sk, _rx, addr) = create_node();

        let packet = DhtPacket::OnionRequest1(OnionRequest1 {
            nonce: gen_nonce(),
            temporary_pk: gen_keypair().0,
            payload: vec![42; 123], // not encrypted with dht pk
            onion_return: OnionReturn {
                nonce: secretbox::gen_nonce(),
                payload: vec![42; ONION_RETURN_1_PAYLOAD_SIZE]
            }
        });

        assert!(alice.handle_packet(packet, addr).wait().is_err());
    }

    // handle_onion_request_2
    #[test]
    fn server_handle_onion_request_2_with_onion_announce_request_test() {
        let (alice, precomp, bob_pk, _bob_sk, rx, addr) = create_node();

        let inner = InnerOnionAnnounceRequest {
            nonce: gen_nonce(),
            pk: gen_keypair().0,
            payload: vec![42; 123]
        };
        let ip_port = IpPort {
            protocol: ProtocolType::UDP,
            ip_addr: "5.6.7.8".parse().unwrap(),
            port: 12345
        };
        let payload = OnionRequest2Payload {
            ip_port: ip_port.clone(),
            inner: InnerOnionRequest::InnerOnionAnnounceRequest(inner.clone())
        };
        let onion_return = OnionReturn {
            nonce: secretbox::gen_nonce(),
            payload: vec![42; ONION_RETURN_2_PAYLOAD_SIZE]
        };
        let packet = DhtPacket::OnionRequest2(OnionRequest2::new(&precomp, &bob_pk, payload, onion_return));

        assert!(alice.handle_packet(packet, addr).wait().is_ok());

        let (received, _rx) = rx.into_future().wait().unwrap();
        let (packet, addr_to_send) = received.unwrap();

        assert_eq!(addr_to_send, ip_port.to_saddr());

        let next_packet = unpack!(packet, DhtPacket::OnionAnnounceRequest);

        assert_eq!(next_packet.inner, inner);

        let onion_symmetric_key = alice.onion_symmetric_key.read();
        let onion_return_payload = next_packet.onion_return.get_payload(&onion_symmetric_key).unwrap();

        assert_eq!(onion_return_payload.0, IpPort::from_udp_saddr(addr));
    }

    #[test]
    fn server_handle_onion_request_2_with_onion_data_request_test() {
        let (alice, precomp, bob_pk, _bob_sk, rx, addr) = create_node();

        let inner = InnerOnionDataRequest {
            destination_pk: gen_keypair().0,
            nonce: gen_nonce(),
            temporary_pk: gen_keypair().0,
            payload: vec![42; 123]
        };
        let ip_port = IpPort {
            protocol: ProtocolType::UDP,
            ip_addr: "5.6.7.8".parse().unwrap(),
            port: 12345
        };
        let payload = OnionRequest2Payload {
            ip_port: ip_port.clone(),
            inner: InnerOnionRequest::InnerOnionDataRequest(inner.clone())
        };
        let onion_return = OnionReturn {
            nonce: secretbox::gen_nonce(),
            payload: vec![42; ONION_RETURN_2_PAYLOAD_SIZE]
        };
        let packet = DhtPacket::OnionRequest2(OnionRequest2::new(&precomp, &bob_pk, payload, onion_return));

        assert!(alice.handle_packet(packet, addr).wait().is_ok());

        let (received, _rx) = rx.into_future().wait().unwrap();
        let (packet, addr_to_send) = received.unwrap();

        assert_eq!(addr_to_send, ip_port.to_saddr());

        let next_packet = unpack!(packet, DhtPacket::OnionDataRequest);

        assert_eq!(next_packet.inner, inner);

        let onion_symmetric_key = alice.onion_symmetric_key.read();
        let onion_return_payload = next_packet.onion_return.get_payload(&onion_symmetric_key).unwrap();

        assert_eq!(onion_return_payload.0, IpPort::from_udp_saddr(addr));
    }

    #[test]
    fn server_handle_onion_request_2_invalid_payload_test() {
        let (alice, _precomp, _bob_pk, _bob_sk, _rx, addr) = create_node();

        let packet = DhtPacket::OnionRequest2(OnionRequest2 {
            nonce: gen_nonce(),
            temporary_pk: gen_keypair().0,
            payload: vec![42; 123], // not encrypted with dht pk
            onion_return: OnionReturn {
                nonce: secretbox::gen_nonce(),
                payload: vec![42; ONION_RETURN_2_PAYLOAD_SIZE]
            }
        });

        assert!(alice.handle_packet(packet, addr).wait().is_err());
    }

    // handle_onion_announce_request
    #[test]
    fn server_handle_onion_announce_request_test() {
        let (alice, precomp, bob_pk, _bob_sk, rx, addr) = create_node();

        let sendback_data = 42;
        let payload = OnionAnnounceRequestPayload {
            ping_id: initial_ping_id(),
            search_pk: gen_keypair().0,
            data_pk: gen_keypair().0,
            sendback_data
        };
        let inner = InnerOnionAnnounceRequest::new(&precomp, &bob_pk, payload);
        let onion_return = OnionReturn {
            nonce: secretbox::gen_nonce(),
            payload: vec![42; ONION_RETURN_3_PAYLOAD_SIZE]
        };
        let packet = DhtPacket::OnionAnnounceRequest(OnionAnnounceRequest {
            inner,
            onion_return: onion_return.clone()
        });

        assert!(alice.handle_packet(packet, addr).wait().is_ok());

        let (received, _rx) = rx.into_future().wait().unwrap();
        let (packet, addr_to_send) = received.unwrap();

        assert_eq!(addr_to_send, addr);

        let response = unpack!(packet, DhtPacket::OnionResponse3);

        assert_eq!(response.onion_return, onion_return);

        let response = unpack!(response.payload, InnerOnionResponse::OnionAnnounceResponse);

        assert_eq!(response.sendback_data, sendback_data);

        let payload = response.get_payload(&precomp).unwrap();

        assert_eq!(payload.announce_status, AnnounceStatus::Failed);
    }

    // handle_onion_data_request
    #[test]
    fn server_handle_onion_data_request_test() {
        let (alice, precomp, bob_pk, _bob_sk, rx, addr) = create_node();

        // get ping id

        let payload = OnionAnnounceRequestPayload {
            ping_id: initial_ping_id(),
            search_pk: gen_keypair().0,
            data_pk: gen_keypair().0,
            sendback_data: 42
        };
        let inner = InnerOnionAnnounceRequest::new(&precomp, &bob_pk, payload);
        let onion_return = OnionReturn {
            nonce: secretbox::gen_nonce(),
            payload: vec![42; ONION_RETURN_3_PAYLOAD_SIZE]
        };
        let packet = DhtPacket::OnionAnnounceRequest(OnionAnnounceRequest {
            inner,
            onion_return: onion_return.clone()
        });

        assert!(alice.handle_packet(packet, addr).wait().is_ok());

        let (received, rx) = rx.into_future().wait().unwrap();
        let (packet, _addr_to_send) = received.unwrap();
        let response = unpack!(packet, DhtPacket::OnionResponse3);
        let response = unpack!(response.payload, InnerOnionResponse::OnionAnnounceResponse);
        let payload = response.get_payload(&precomp).unwrap();
        let ping_id = payload.ping_id_or_pk;

        // announce node

        let payload = OnionAnnounceRequestPayload {
            ping_id,
            search_pk: gen_keypair().0,
            data_pk: gen_keypair().0,
            sendback_data: 42
        };
        let inner = InnerOnionAnnounceRequest::new(&precomp, &bob_pk, payload);
        let packet = DhtPacket::OnionAnnounceRequest(OnionAnnounceRequest {
            inner,
            onion_return: onion_return.clone()
        });

        assert!(alice.handle_packet(packet, addr).wait().is_ok());

        // send onion data request

        let nonce = gen_nonce();
        let temporary_pk = gen_keypair().0;
        let payload = vec![42; 123];
        let inner = InnerOnionDataRequest {
            destination_pk: bob_pk,
            nonce,
            temporary_pk,
            payload: payload.clone()
        };
        let packet = DhtPacket::OnionDataRequest(OnionDataRequest {
            inner,
            onion_return: onion_return.clone()
        });

        assert!(alice.handle_packet(packet, addr).wait().is_ok());

        let (received, _rx) = rx.skip(1).into_future().wait().unwrap();
        let (packet, addr_to_send) = received.unwrap();

        assert_eq!(addr_to_send, addr);

        let response = unpack!(packet, DhtPacket::OnionResponse3);

        assert_eq!(response.onion_return, onion_return);

        let response = unpack!(response.payload, InnerOnionResponse::OnionDataResponse);

        assert_eq!(response.nonce, nonce);
        assert_eq!(response.temporary_pk, temporary_pk);
        assert_eq!(response.payload, payload);
    }

    // handle_onion_response_3
    #[test]
    fn server_handle_onion_response_3_test() {
        let (alice, _precomp, _bob_pk, _bob_sk, rx, addr) = create_node();

        let onion_symmetric_key = alice.onion_symmetric_key.read();

        let ip_port = IpPort {
            protocol: ProtocolType::UDP,
            ip_addr: "5.6.7.8".parse().unwrap(),
            port: 12345
        };
        let next_onion_return = OnionReturn {
            nonce: secretbox::gen_nonce(),
            payload: vec![42; ONION_RETURN_2_PAYLOAD_SIZE]
        };
        let onion_return = OnionReturn::new(&onion_symmetric_key, &ip_port, Some(&next_onion_return));
        let payload = InnerOnionResponse::OnionAnnounceResponse(OnionAnnounceResponse {
            sendback_data: 12345,
            nonce: gen_nonce(),
            payload: vec![42; 123]
        });
        let packet = DhtPacket::OnionResponse3(OnionResponse3 {
            onion_return,
            payload: payload.clone()
        });

        assert!(alice.handle_packet(packet, addr).wait().is_ok());

        let (received, _rx) = rx.into_future().wait().unwrap();
        let (packet, addr_to_send) = received.unwrap();

        assert_eq!(addr_to_send, ip_port.to_saddr());

        let next_packet = unpack!(packet, DhtPacket::OnionResponse2);

        assert_eq!(next_packet.payload, payload);
        assert_eq!(next_packet.onion_return, next_onion_return);
    }

    #[test]
    fn server_handle_onion_response_3_invalid_onion_return_test() {
        let (alice, _precomp, _bob_pk, _bob_sk, _rx, addr) = create_node();

        let onion_return = OnionReturn {
            nonce: secretbox::gen_nonce(),
            payload: vec![42; ONION_RETURN_3_PAYLOAD_SIZE] // not encrypted with onion_symmetric_key
        };
        let payload = InnerOnionResponse::OnionAnnounceResponse(OnionAnnounceResponse {
            sendback_data: 12345,
            nonce: gen_nonce(),
            payload: vec![42; 123]
        });
        let packet = DhtPacket::OnionResponse3(OnionResponse3 {
            onion_return,
            payload
        });

        assert!(alice.handle_packet(packet, addr).wait().is_err());
    }

    #[test]
    fn server_handle_onion_response_3_invalid_next_onion_return_test() {
        let (alice, _precomp, _bob_pk, _bob_sk, _rx, addr) = create_node();

        let onion_symmetric_key = alice.onion_symmetric_key.read();

        let ip_port = IpPort {
            protocol: ProtocolType::UDP,
            ip_addr: "5.6.7.8".parse().unwrap(),
            port: 12345
        };
        let onion_return = OnionReturn::new(&onion_symmetric_key, &ip_port, None);
        let inner = OnionDataResponse {
            nonce: gen_nonce(),
            temporary_pk: gen_keypair().0,
            payload: vec![42; 123]
        };
        let packet = DhtPacket::OnionResponse3(OnionResponse3 {
            onion_return,
            payload: InnerOnionResponse::OnionDataResponse(inner.clone())
        });

        assert!(alice.handle_packet(packet, addr).wait().is_err());
    }

    // handle_onion_response_2
    #[test]
    fn server_handle_onion_response_2_test() {
        let (alice, _precomp, _bob_pk, _bob_sk, rx, addr) = create_node();

        let onion_symmetric_key = alice.onion_symmetric_key.read();

        let ip_port = IpPort {
            protocol: ProtocolType::UDP,
            ip_addr: "5.6.7.8".parse().unwrap(),
            port: 12345
        };
        let next_onion_return = OnionReturn {
            nonce: secretbox::gen_nonce(),
            payload: vec![42; ONION_RETURN_1_PAYLOAD_SIZE]
        };
        let onion_return = OnionReturn::new(&onion_symmetric_key, &ip_port, Some(&next_onion_return));
        let payload = InnerOnionResponse::OnionAnnounceResponse(OnionAnnounceResponse {
            sendback_data: 12345,
            nonce: gen_nonce(),
            payload: vec![42; 123]
        });
        let packet = DhtPacket::OnionResponse2(OnionResponse2 {
            onion_return,
            payload: payload.clone()
        });

        assert!(alice.handle_packet(packet, addr).wait().is_ok());

        let (received, _rx) = rx.into_future().wait().unwrap();
        let (packet, addr_to_send) = received.unwrap();

        assert_eq!(addr_to_send, ip_port.to_saddr());

        let next_packet = unpack!(packet, DhtPacket::OnionResponse1);

        assert_eq!(next_packet.payload, payload);
        assert_eq!(next_packet.onion_return, next_onion_return);
    }

    #[test]
    fn server_handle_onion_response_2_invalid_onion_return_test() {
        let (alice, _precomp, _bob_pk, _bob_sk, _rx, addr) = create_node();

        let onion_return = OnionReturn {
            nonce: secretbox::gen_nonce(),
            payload: vec![42; ONION_RETURN_2_PAYLOAD_SIZE] // not encrypted with onion_symmetric_key
        };
        let payload = InnerOnionResponse::OnionAnnounceResponse(OnionAnnounceResponse {
            sendback_data: 12345,
            nonce: gen_nonce(),
            payload: vec![42; 123]
        });
        let packet = DhtPacket::OnionResponse2(OnionResponse2 {
            onion_return,
            payload
        });

        assert!(alice.handle_packet(packet, addr).wait().is_err());
    }

    #[test]
    fn server_handle_onion_response_2_invalid_next_onion_return_test() {
        let (alice, _precomp, _bob_pk, _bob_sk, _rx, addr) = create_node();

        let onion_symmetric_key = alice.onion_symmetric_key.read();

        let ip_port = IpPort {
            protocol: ProtocolType::UDP,
            ip_addr: "5.6.7.8".parse().unwrap(),
            port: 12345
        };
        let onion_return = OnionReturn::new(&onion_symmetric_key, &ip_port, None);
        let inner = OnionDataResponse {
            nonce: gen_nonce(),
            temporary_pk: gen_keypair().0,
            payload: vec![42; 123]
        };
        let packet = DhtPacket::OnionResponse2(OnionResponse2 {
            onion_return,
            payload: InnerOnionResponse::OnionDataResponse(inner.clone())
        });

        assert!(alice.handle_packet(packet, addr).wait().is_err());
    }

    // handle_onion_response_1
    #[test]
    fn server_handle_onion_response_1_with_onion_announce_response_test() {
        let (alice, _precomp, _bob_pk, _bob_sk, rx, addr) = create_node();

        let onion_symmetric_key = alice.onion_symmetric_key.read();

        let ip_port = IpPort {
            protocol: ProtocolType::UDP,
            ip_addr: "5.6.7.8".parse().unwrap(),
            port: 12345
        };
        let onion_return = OnionReturn::new(&onion_symmetric_key, &ip_port, None);
        let inner = OnionAnnounceResponse {
            sendback_data: 12345,
            nonce: gen_nonce(),
            payload: vec![42; 123]
        };
        let packet = DhtPacket::OnionResponse1(OnionResponse1 {
            onion_return,
            payload: InnerOnionResponse::OnionAnnounceResponse(inner.clone())
        });

        assert!(alice.handle_packet(packet, addr).wait().is_ok());

        let (received, _rx) = rx.into_future().wait().unwrap();
        let (packet, addr_to_send) = received.unwrap();

        assert_eq!(addr_to_send, ip_port.to_saddr());

        let next_packet = unpack!(packet, DhtPacket::OnionAnnounceResponse);

        assert_eq!(next_packet, inner);
    }

    #[test]
    fn server_handle_onion_response_1_with_onion_data_response_test() {
        let (alice, _precomp, _bob_pk, _bob_sk, rx, addr) = create_node();

        let onion_symmetric_key = alice.onion_symmetric_key.read();

        let ip_port = IpPort {
            protocol: ProtocolType::UDP,
            ip_addr: "5.6.7.8".parse().unwrap(),
            port: 12345
        };
        let onion_return = OnionReturn::new(&onion_symmetric_key, &ip_port, None);
        let inner = OnionDataResponse {
            nonce: gen_nonce(),
            temporary_pk: gen_keypair().0,
            payload: vec![42; 123]
        };
        let packet = DhtPacket::OnionResponse1(OnionResponse1 {
            onion_return,
            payload: InnerOnionResponse::OnionDataResponse(inner.clone())
        });

        assert!(alice.handle_packet(packet, addr).wait().is_ok());

        let (received, _rx) = rx.into_future().wait().unwrap();
        let (packet, addr_to_send) = received.unwrap();

        assert_eq!(addr_to_send, ip_port.to_saddr());

        let next_packet = unpack!(packet, DhtPacket::OnionDataResponse);

        assert_eq!(next_packet, inner);
    }

    #[test]
    fn server_handle_onion_response_1_redirect_to_tcp_test() {
        let (mut alice, _precomp, _bob_pk, _bob_sk, _rx, _addr) = create_node();
        let (tcp_onion_tx, tcp_onion_rx) = mpsc::unbounded::<(InnerOnionResponse, SocketAddr)>();
        alice.set_tcp_onion_sink(tcp_onion_tx);

        let addr: SocketAddr = "127.0.0.1:12346".parse().unwrap();

        let onion_symmetric_key = alice.onion_symmetric_key.read();

        let ip_port = IpPort {
            protocol: ProtocolType::TCP,
            ip_addr: "5.6.7.8".parse().unwrap(),
            port: 12345
        };
        let onion_return = OnionReturn::new(&onion_symmetric_key, &ip_port, None);
        let inner = InnerOnionResponse::OnionAnnounceResponse(OnionAnnounceResponse {
            sendback_data: 12345,
            nonce: gen_nonce(),
            payload: vec![42; 123]
        });
        let packet = DhtPacket::OnionResponse1(OnionResponse1 {
            onion_return,
            payload: inner.clone()
        });

        assert!(alice.handle_packet(packet, addr).wait().is_ok());

        let (received, _tcp_onion_rx) = tcp_onion_rx.into_future().wait().unwrap();
        let (packet, addr_to_send) = received.unwrap();

        assert_eq!(addr_to_send, ip_port.to_saddr());
        assert_eq!(packet, inner);
    }

    #[test]
    fn server_handle_onion_response_1_can_not_redirect_to_tcp_test() {
        let (alice, _precomp, _bob_pk, _bob_sk, _rx, addr) = create_node();

        let onion_symmetric_key = alice.onion_symmetric_key.read();

        let ip_port = IpPort {
            protocol: ProtocolType::TCP,
            ip_addr: "5.6.7.8".parse().unwrap(),
            port: 12345
        };
        let onion_return = OnionReturn::new(&onion_symmetric_key, &ip_port, None);
        let inner = OnionAnnounceResponse {
            sendback_data: 12345,
            nonce: gen_nonce(),
            payload: vec![42; 123]
        };
        let packet = DhtPacket::OnionResponse1(OnionResponse1 {
            onion_return,
            payload: InnerOnionResponse::OnionAnnounceResponse(inner.clone())
        });

        assert!(alice.handle_packet(packet, addr).wait().is_err());
    }

    #[test]
    fn server_handle_onion_response_1_invalid_onion_return_test() {
        let (alice, _precomp, _bob_pk, _bob_sk, _rx, addr) = create_node();

        let onion_return = OnionReturn {
            nonce: secretbox::gen_nonce(),
            payload: vec![42; ONION_RETURN_1_PAYLOAD_SIZE] // not encrypted with onion_symmetric_key
        };
        let payload = InnerOnionResponse::OnionAnnounceResponse(OnionAnnounceResponse {
            sendback_data: 12345,
            nonce: gen_nonce(),
            payload: vec![42; 123]
        });
        let packet = DhtPacket::OnionResponse1(OnionResponse1 {
            onion_return,
            payload
        });

        assert!(alice.handle_packet(packet, addr).wait().is_err());
    }

    #[test]
    fn server_handle_onion_response_1_invalid_next_onion_return_test() {
        let (alice, _precomp, _bob_pk, _bob_sk, _rx, addr) = create_node();

        let onion_symmetric_key = alice.onion_symmetric_key.read();

        let ip_port = IpPort {
            protocol: ProtocolType::UDP,
            ip_addr: "5.6.7.8".parse().unwrap(),
            port: 12345
        };
        let next_onion_return = OnionReturn {
            nonce: secretbox::gen_nonce(),
            payload: vec![42; ONION_RETURN_1_PAYLOAD_SIZE]
        };
        let onion_return = OnionReturn::new(&onion_symmetric_key, &ip_port, Some(&next_onion_return));
        let inner = OnionDataResponse {
            nonce: gen_nonce(),
            temporary_pk: gen_keypair().0,
            payload: vec![42; 123]
        };
        let packet = DhtPacket::OnionResponse1(OnionResponse1 {
            onion_return,
            payload: InnerOnionResponse::OnionDataResponse(inner.clone())
        });

        assert!(alice.handle_packet(packet, addr).wait().is_err());
    }

     // send_nodes_req()
     #[test]
     fn server_send_nodes_req_test() {
         let (alice, _precomp, bob_pk, _bob_sk, _rx, _addr) = create_node();

         let target_node = PackedNode {
             pk: bob_pk,
             saddr: "127.0.0.1:12345".parse().unwrap(),
         };
         assert!(alice.send_nodes_req(target_node, alice.pk, 42).wait().is_ok());

         let node = PackedNode {
             pk: gen_keypair().0,
             saddr: "127.0.0.1:12347".parse().unwrap(),
         };
         alice.try_add_to_close_nodes(&node);

         assert!(alice.send_nodes_req(target_node, alice.pk, 42).wait().is_ok());
     }

    // send_nat_ping_req()
    #[test]
    fn server_send_nat_ping_req_test() {
        let (alice, _precomp, _bob_pk, _bob_sk, mut rx, _addr) = create_node();

        let (friend_pk1, friend_sk1) = gen_keypair();
        let friend_pk2 = gen_keypair().0;

        let mut friend = DhtFriend::new(friend_pk1, 0);
        let pn = PackedNode::new(false, SocketAddr::V4("127.1.1.1:12345".parse().unwrap()), &friend_pk2);
        friend.close_nodes.try_add(&friend_pk1, &pn);
        alice.add_friend(friend);

        alice.dht_main_loop().wait().unwrap();

        loop {
            let (received, rx1) = rx.into_future().wait().unwrap();
            let (packet, _addr_to_send) = received.unwrap();

            if let DhtPacket::DhtRequest(nat_ping_req) = packet {
                let nat_ping_req_payload = nat_ping_req.get_payload(&friend_sk1).unwrap();
                let nat_ping_req_payload = unpack!(nat_ping_req_payload, DhtRequestPayload::NatPingRequest);

                assert_eq!(alice.friends.read()[0].hole_punch.ping_id, nat_ping_req_payload.id);
                break;
            }
            rx = rx1;
        }
    }

    #[test]
    fn server_handle_lan_discovery_test() {
        let (alice, _precomp, bob_pk, bob_sk, rx, addr) = create_node();

        let lan = DhtPacket::LanDiscovery(LanDiscovery { pk: bob_pk });

        assert!(alice.handle_packet(lan, addr).wait().is_ok());

        let (received, _rx) = rx.into_future().wait().unwrap();
        let (packet, addr_to_send) = received.unwrap();

        assert_eq!(addr_to_send, addr);

        let nodes_req = unpack!(packet, DhtPacket::NodesRequest);
        let _nodes_req_payload = nodes_req.get_payload(&bob_sk).unwrap();

        assert_eq!(nodes_req.pk, alice.pk);
    }

    #[test]
    fn server_handle_lan_discovery_for_ourselves_test() {
        let (alice, _precomp, _bob_pk, _bob_sk, _rx, addr) = create_node();

        let lan = DhtPacket::LanDiscovery(LanDiscovery { pk: alice.pk });

        assert!(alice.handle_packet(lan, addr).wait().is_ok());
    }

    #[test]
    fn refresh_onion_key_test() {
        let (alice, _precomp, _bob_pk, _bob_sk, _rx, _addr) = create_node();

        let onion_symmetric_key = alice.onion_symmetric_key.read().clone();
        let onion_symmetric_key_time = alice.onion_symmetric_key_time.read().clone();

        let mut enter = tokio_executor::enter().unwrap();
        let clock = Clock::new_with_now(ConstNow(
            onion_symmetric_key_time + Duration::from_secs(ONION_REFRESH_KEY_INTERVAL)
        ));

        with_default(&clock, &mut enter, |_| {
            alice.refresh_onion_key();
        });

        assert!(*alice.onion_symmetric_key.read() != onion_symmetric_key)
    }

    #[test]
    fn server_handle_tcp_onion_request_test() {
        let (alice, _precomp, _bob_pk, _bob_sk, rx, addr) = create_node();

        let temporary_pk = gen_keypair().0;
        let payload = vec![42; 123];
        let ip_port = IpPort {
            protocol: ProtocolType::UDP,
            ip_addr: "5.6.7.8".parse().unwrap(),
            port: 12345
        };
        let packet = OnionRequest {
            nonce: gen_nonce(),
            ip_port: ip_port.clone(),
            temporary_pk,
            payload: payload.clone()
        };

        assert!(alice.handle_tcp_onion_request(packet, addr).wait().is_ok());

        let (received, _rx) = rx.into_future().wait().unwrap();
        let (packet, addr_to_send) = received.unwrap();

        assert_eq!(addr_to_send, ip_port.to_saddr());

        let next_packet = unpack!(packet, DhtPacket::OnionRequest1);

        assert_eq!(next_packet.temporary_pk, temporary_pk);
        assert_eq!(next_packet.payload, payload);

        let onion_symmetric_key = alice.onion_symmetric_key.read();
        let onion_return_payload = next_packet.onion_return.get_payload(&onion_symmetric_key).unwrap();

        assert_eq!(onion_return_payload.0, IpPort::from_tcp_saddr(addr));
    }

    #[test]
    fn server_ping_bootstrap_nodes_test() {
        let (alice, _precomp, bob_pk, bob_sk, rx, _addr) = create_node();
        let (ping_pk, ping_sk) = gen_keypair();

        let pn = PackedNode::new(false, SocketAddr::V4("127.1.1.1:12345".parse().unwrap()), &ping_pk);
        assert!(alice.bootstrap_nodes.write().deref_mut().try_add(&alice.pk, &pn));

        let pn = PackedNode::new(false, SocketAddr::V4("127.0.0.1:33445".parse().unwrap()), &bob_pk);
        assert!(alice.bootstrap_nodes.write().deref_mut().try_add(&alice.pk, &pn));

        alice.dht_main_loop().wait().unwrap();

        let mut request_queue = alice.request_queue.write();

        rx.take(2).map(|(packet, addr)| {
            let nodes_req = unpack!(packet, DhtPacket::NodesRequest);
            if addr == SocketAddr::V4("127.0.0.1:33445".parse().unwrap()) {
                let nodes_req_payload = nodes_req.get_payload(&bob_sk).unwrap();
                assert!(request_queue.check_ping_id(bob_pk, nodes_req_payload.id));
            } else {
                let nodes_req_payload = nodes_req.get_payload(&ping_sk).unwrap();
                assert!(request_queue.check_ping_id(ping_pk, nodes_req_payload.id));
            }
        }).collect().wait().unwrap();
    }

    #[test]
    fn server_ping_and_get_close_nodes_test() {
        let (alice, _precomp, bob_pk, bob_sk, rx, _addr) = create_node();
        let (ping_pk, ping_sk) = gen_keypair();

        let pn = PackedNode::new(false, SocketAddr::V4("127.1.1.1:12345".parse().unwrap()), &ping_pk);
        assert!(alice.close_nodes.write().deref_mut().try_add(&pn));

        let pn = PackedNode::new(false, SocketAddr::V4("127.0.0.1:33445".parse().unwrap()), &bob_pk);
        assert!(alice.close_nodes.write().deref_mut().try_add(&pn));

        alice.dht_main_loop().wait().unwrap();

        let mut request_queue = alice.request_queue.write();

        rx.take(2).map(|(packet, addr)| {
            let nodes_req = unpack!(packet, DhtPacket::NodesRequest);
            if addr == SocketAddr::V4("127.0.0.1:33445".parse().unwrap()) {
                let nodes_req_payload = nodes_req.get_payload(&bob_sk).unwrap();
                assert!(request_queue.check_ping_id(bob_pk, nodes_req_payload.id));
            } else {
                let nodes_req_payload = nodes_req.get_payload(&ping_sk).unwrap();
                assert!(request_queue.check_ping_id(ping_pk, nodes_req_payload.id));
            }
        }).collect().wait().unwrap();
    }

    #[test]
    fn server_send_nodes_req_random_test() {
        let (alice, _precomp, bob_pk, bob_sk, rx, _addr) = create_node();
        let (ping_pk, ping_sk) = gen_keypair();

        let pn = PackedNode::new(false, SocketAddr::V4("127.1.1.1:12345".parse().unwrap()), &ping_pk);
        assert!(alice.close_nodes.write().deref_mut().try_add(&pn));

        let pn = PackedNode::new(false, SocketAddr::V4("127.0.0.1:33445".parse().unwrap()), &bob_pk);
        assert!(alice.close_nodes.write().deref_mut().try_add(&pn));

        alice.dht_main_loop().wait().unwrap();

        let mut request_queue = alice.request_queue.write();

        rx.take(2).map(|(packet, addr)| {
            let nodes_req = unpack!(packet, DhtPacket::NodesRequest);
            if addr == SocketAddr::V4("127.0.0.1:33445".parse().unwrap()) {
                let nodes_req_payload = nodes_req.get_payload(&bob_sk).unwrap();
                assert!(request_queue.check_ping_id(bob_pk, nodes_req_payload.id));
            } else {
                let nodes_req_payload = nodes_req.get_payload(&ping_sk).unwrap();
                assert!(request_queue.check_ping_id(ping_pk, nodes_req_payload.id));
            }
        }).collect().wait().unwrap();
    }

    #[test]
    fn server_send_nodes_req_packets_test() {
        let (alice, _precomp, _bob_pk, _bob_sk, _rx, _addr) = create_node();
        let friend_pk1 = gen_keypair().0;
        let friend_pk2 = gen_keypair().0;

        let friend = DhtFriend::new(friend_pk1, 0);
        alice.add_friend(friend);

        let friend = DhtFriend::new(friend_pk2, 0);
        alice.add_friend(friend);

        alice.dht_main_loop().wait().unwrap();
    }

    #[test]
    fn server_set_bootstrap_info_test() {
        let (mut alice, _precomp, _bob_pk, _bob_sk, _rx, _addr) = create_node();

        alice.set_bootstrap_info(42, "test".as_bytes().to_owned());
        assert_eq!(alice.tox_core_version, 42);
        assert_eq!(alice.motd, "test".as_bytes().to_owned());
    }

    #[test]
    fn server_enable_lan_discovery_test() {
        let (mut alice, _precomp, _bob_pk, _bob_sk, _rx, _addr) = create_node();

        alice.enable_lan_discovery(false);
        assert_eq!(alice.lan_discovery_enabled, false);
    }

    #[test]
    fn server_enable_ipv6_mode_test() {
        let (mut alice, _precomp, _bob_pk, _bob_sk, _rx, _addr) = create_node();

        alice.enable_ipv6_mode(true);
        assert_eq!(alice.is_ipv6_mode, true);
    }

    #[test]
    fn server_send_to_test() {
        let (mut alice, _precomp, bob_pk, bob_sk, rx, _addr) = create_node();
        let (ping_pk, ping_sk) = gen_keypair();

        let pn = PackedNode::new(false, SocketAddr::V6("[FF::01]:33445".parse().unwrap()), &bob_pk);
        assert!(alice.close_nodes.write().deref_mut().try_add(&pn));

        let pn = PackedNode::new(false, SocketAddr::V4("127.1.1.1:12345".parse().unwrap()), &ping_pk);
        assert!(alice.close_nodes.write().deref_mut().try_add(&pn));

        // test with ipv6 mode
        alice.enable_ipv6_mode(true);
        alice.dht_main_loop().wait().unwrap();

        let mut request_queue = alice.request_queue.write();

        rx.take(2).map(|(packet, addr)| {
            let nodes_req = unpack!(packet, DhtPacket::NodesRequest);
            if addr == SocketAddr::V6("[FF::01]:33445".parse().unwrap()) {
                let nodes_req_payload = nodes_req.get_payload(&bob_sk).unwrap();
                assert!(request_queue.check_ping_id(bob_pk, nodes_req_payload.id));
            } else {
                let nodes_req_payload = nodes_req.get_payload(&ping_sk).unwrap();
                assert!(request_queue.check_ping_id(ping_pk, nodes_req_payload.id));
            }
        }).collect().wait().unwrap();
    }
}
