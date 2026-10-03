/// Kademlia XOR distance; the output lex-compares as the unsigned 256-bit distance.
#[inline]
pub fn xor32(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    std::array::from_fn(|i| a[i] ^ b[i])
}
