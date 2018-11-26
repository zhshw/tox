use toxcore::crypto_core::random_usize;
use std::time::Instant;
use toxcore::dht::dht_node::DhtNode;
use toxcore::dht::packed_node::PackedNode;

use super::path::Path;

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

pub struct Sendback {
    pub friend_num_inc: u32,
    pub node: PackedNode,
    pub path_num: u32
}

// A stub to be replaced by something else
struct PingArray {
}

pub struct Node {}

impl Node {
    fn is_timeout(&self, time: Instant) -> bool {
        unimplemented!()
    }
    fn is_stable(&self, time: Instant) -> bool {
        unimplemented!()
    }
}

pub struct ClientPath {
    path: Path,
    last_success: Instant,
    last_used: Instant,
    creation_time: Instant,
    failures: usize,
}

impl ClientPath {
    fn is_stable(&self, time: Instant) -> bool {
        unimplemented!()
    }
}

pub struct Client {
    friends: (),
    
    announce_list: Vec<Node>,
    last_announce: Instant,

    self_paths: Vec<ClientPath>,

    path_nodes: Vec<PackedNode>,

    announce_ping_array: PingArray,
}

impl Client {
    //
    fn add_to_list(&mut self, ) {}
    fn announce_self(&mut self, time: Instant) {
        self.announce_list.retain(|n| !n.is_timeout(time));

        for node in &self.announce_list {
            //
        }

        let len = self.announce_list.len();
        let margin = random_usize() % MAX_ONION_CLIENTS_ANNOUNCE;
        if len <= margin && !self.path_nodes.is_empty() {
            for i in 0 .. MAX_ONION_CLIENTS_ANNOUNCE / 2 {
                let num = random_usize() % len;
                // send_announce_request
            }
        }
    }
}
