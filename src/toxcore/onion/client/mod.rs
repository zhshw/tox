use toxcore::dht::packed_node::PackedNode;

pub struct Sendback {
    pub friend_num_inc: u32,
    pub node: PackedNode,
    pub path_num: u32
}

// A stub to be replaced by something else
struct PingArray {
}

pub struct Client {
    friends: (),
    announce_ping_array: PingArray,
}
