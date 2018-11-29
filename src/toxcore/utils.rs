/*! Common utility functions
*/

use toxcore::crypto_core::*;

/// Generate non-zero ping_id
pub fn gen_ping_id() -> u64 {
    let mut ping_id = 0;
    while ping_id == 0 {
        ping_id = random_u64();
    }
    ping_id
}

/// Get a reference to a random element of the slice
pub fn random_element<T>(slice: &[T]) -> Option<&T> {
    if slice.is_empty() { None }
    else {
        let n = random_usize() % slice.len();
        Some(&slice[n])
    }
}
