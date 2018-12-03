use std::net::SocketAddr;
use toxcore::utils::random_element;
use sodiumoxide::crypto::hash::sha256::Digest;
use toxcore::crypto_core::*;
use std::time::Instant;
use toxcore::dht::packed_node::PackedNode;

use super::path::Path;
use super::packet::*;

const MAX_ONION_CLIENTS: usize = 8;
const MAX_ONION_CLIENTS_ANNOUNCE: usize = 12;
const ONION_NODE_PING_INTERVAL: usize = 15;
const ONION_NODE_TIMEOUT: usize = ONION_NODE_PING_INTERVAL;

/* The interval in seconds at which to tell our friends where we are */
const ONION_DHTPK_SEND_INTERVAL: usize = 30;
const DHT_DHTPK_SEND_INTERVAL: usize = 20;

const NUMBER_ONION_PATHS: usize = 6;

/* The timeout the first time the path is added and
   then for all the next consecutive times */
const ONION_PATH_FIRST_TIMEOUT: usize = 4;
const ONION_PATH_TIMEOUT: usize = 10;
const ONION_PATH_MAX_LIFETIME: usize = 1200;
const ONION_PATH_MAX_NO_RESPONSE_USES: usize = 4;

const MAX_STORED_PINGED_NODES: usize = 9;
const MIN_NODE_PING_TIME: usize = 10;

const ONION_NODE_MAX_PINGS: usize = 3;

const MAX_PATH_NODES: usize = 32;

enum Packet {
    AnnounceRequest(InnerOnionAnnounceRequest),
}

impl From<InnerOnionAnnounceRequest> for Packet {
    fn from(req: InnerOnionAnnounceRequest) -> Self {
        Packet::AnnounceRequest(req)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sendback {
    pub friend_num: Option<u32>,
    pub node: PackedNode,
    pub path_num: u32
}

// A stub to be replaced by something else
struct PingArray {
}


pub struct Node {
    node: PackedNode,
    pingid: (),
    is_stored: bool,
}

impl Node {
    fn is_timeout(&self, time: Instant) -> bool {
        unimplemented!()
    }
    fn is_stable(&self, time: Instant) -> bool {
        unimplemented!()
    }
}

struct Friend {
}

pub struct ClientPath {
    inner: Path,
    last_success: Instant,
    last_used: Instant,
    creation_time: Instant,
    usages_credit: usize,
}

impl ClientPath {
    fn is_stable(&self, time: Instant) -> bool {
        unimplemented!()
    }
}

pub struct Client {
    pk: PublicKey,
    sk: SecretKey,
    pck: PrecomputedKey,

    real_pk: PublicKey,
    real_sk: SecretKey,

    friends: Friend,
    
    // TODO: fixed size vec with update by PartialOrd
    announce_list: Vec<Node>,
    last_announce: Instant,

    self_paths: Vec<ClientPath>,

    temp_pk: PublicKey,

    // TODO: replace with a vec of fixed size where nodes are updated circularly
    path_nodes: Vec<PackedNode>,

    announce_ping_array: PingArray,
}

impl Client {
    fn add_sendback(
        &mut self, friend_num: Option<u32>, node: PackedNode, path_num: u32
    ) -> u64 {
        unimplemented!()
    }

    fn get_sendback(&self, sendback: u64) -> Option<&Sendback> {
        unimplemented!()
    }

    fn random_path_nodes(&self) -> Option<Vec<PackedNode>> {
        if self.path_nodes.len() < NUMBER_ONION_PATHS {
            return None
        };

        let mut nodes = vec![];
        while nodes.len() < NUMBER_ONION_PATHS {
            let node = random_element(&self.path_nodes).unwrap();

            if !self.path_nodes.contains(node) {
                nodes.push(node.clone())
            } else {
                continue
            }
        }

        Some(nodes)
    }

    fn random_path(&mut self) -> Option<ClientPath> {
        unimplemented!()
    }

    fn get_path(&mut self, path_num: usize) -> Option<ClientPath> {
        unimplemented!()
    }

    fn add_to_list(
        &mut self,
        _fnum: Option<u32>,
        node: &PackedNode,
        status: AnnounceStatus,
        pingid_or_pk: sha256::Digest,
        path_used: u32
    ) {
        use toxcore::dht::kbucket::Distance;

        // TODO: support friends
        let status = 
            if status == AnnounceStatus::Found && pingid_or_pk.0 != self.temp_pk.0 {
                AnnounceStatus::Failed
            } else {
                status
            };
        let ref_key = self.real_pk.clone();

        // self.announce_list.sort_by(|l, r| ref_key.distance(&l.node.pk, &r.node.pk));
        // insert node to the list

        unimplemented!()
    }

    fn send_self_announce_request(
        &mut self, path: &ClientPath, dest: &PackedNode, ping_id: Option<Digest>
    ) {
        // TODO: support friends
        let sendback = self.add_sendback(None, dest.clone(), path.inner.number);
        let payload = OnionAnnounceRequestPayload::new(
            self.real_pk.clone(), self.temp_pk.clone(), ping_id, sendback
        );
        let pck = precompute(&dest.pk, &self.real_sk);
        let request = InnerOnionAnnounceRequest::new(&pck, &self.real_pk, &payload);
        let packet = request.into();

        self.send_onion_packet(path, packet)
    }

    fn send_onion_packet(&mut self, path: &ClientPath, packet: Packet) {
        unimplemented!()
    }

    fn announce_self(&mut self, time: Instant) -> Result<(), ()> {
        self.announce_list.retain(|n| !n.is_timeout(time));

        for node in &self.announce_list {
            //if stored && path_exists {}

            //if timeout(last_pinged) || (timeout(last_announce) && random_magic)
            {
                //get_path
                //send_ann_request(Some(ping_id))
            }
        }

        let len = self.announce_list.len();
        let margin = random_usize() % MAX_ONION_CLIENTS_ANNOUNCE;
        if len <= margin && !self.path_nodes.is_empty() {
            for i in 0 .. MAX_ONION_CLIENTS_ANNOUNCE / 2 {
                let num = random_usize() % len;
                let path = self.random_path().unwrap(); // FIXME
                //self.send_self_announce_request(&path, &self.path_nodes[num], None)
                // send_announce_request
            }
        }

        Ok(())
    }

    fn handle_announce_responce(
        &mut self,
        source: SocketAddr,
        announce: OnionAnnounceResponse
    ) -> Result<(), ()> {
        // TODO: support friends
        let sb = self.get_sendback(announce.sendback_data).cloned().ok_or(())?;
        let key = precompute(&sb.node.pk, &self.real_sk);
        // FIXME: this can panic
        let payload = announce.get_payload(&key).unwrap();

        self.set_path_timeouts(sb.friend_num, sb.path_num);
        //add_to_list

        if !payload.nodes.is_empty() {
            //self.ping_nodes(&payload.nodes, )
        }

        unimplemented!()
    }

    fn set_path_timeouts(&mut self, fnum: Option<u32>, path_num: u32) -> Option<u32> {
        unimplemented!()
    }

    fn ping_nodes(&mut self) {
        // TODO: friends support
        // Nodes that are closer to us than nodes in announce_list we choose.
        // Ensure they are different from the nodes in announce_list
        // If they are good to ping, AnnounceRequest (ping_id = 0) is send
        unimplemented!()
    }
}
