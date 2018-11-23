use toxcore::dht::dht_node::DhtNode;
use toxcore::crypto_core::*;

pub struct Path {
    pub number: u32,
    pub public_keys: [PublicKey; 3],
    pub precomputed_keys: [PrecomputedKey; 3],
    pub nodes: [DhtNode; 3],
}

impl Path {
    pub fn new(keypair: (&PublicKey, &SecretKey), nodes: &[DhtNode]) -> Self {
        let (public_key, secret_key) = keypair;

        let pk1 = public_key.clone();
        let precomp1 = encrypt_precompute(&nodes[0].pk, secret_key);

        let (pk2, random_sk) = gen_keypair();
        let precomp2 = encrypt_precompute(&nodes[1].pk, &random_sk);

        let (pk3, random_sk) = gen_keypair();
        let precomp3 = encrypt_precompute(&nodes[2].pk, &random_sk);
        
        Path {
            number: 0,
            public_keys: [pk1, pk2, pk3],
            precomputed_keys: [precomp1, precomp2, precomp3],
            nodes: [
                nodes[0].clone(),
                nodes[1].clone(),
                nodes[2].clone(),
            ],
        }
    }
}
