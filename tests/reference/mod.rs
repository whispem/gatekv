//! Plain Rust versions of the assembly routines: the oracle of tests/asm.rs and the baseline
//! of benches/asm.rs.

/// CRC32C (Castagnoli) polynomial, bit-reflected.
const POLY: u32 = 0x82F6_3B78;

const TABLE: [u32; 256] = {
    let mut table = [0; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut bit = 0;
        while bit < 8 {
            c = (c >> 1) ^ (POLY & (c & 1).wrapping_neg());
            bit += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

/// One bit at a time: slow and obviously correct.
pub fn crc32c_bitwise(seed: u32, data: &[u8]) -> u32 {
    let mut crc = !seed;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (POLY & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

/// One byte at a time through a 256-entry table: the usual portable implementation.
pub fn crc32c_table(seed: u32, data: &[u8]) -> u32 {
    let mut crc = !seed;
    for &b in data {
        crc = (crc >> 8) ^ TABLE[((crc ^ u32::from(b)) & 0xFF) as usize];
    }
    !crc
}

/// The key hash that src/asm.rs implements.
pub fn hash(key: &[u8]) -> u64 {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    let round = |h: u64, w: u64| (h.rotate_left(5) ^ w).wrapping_mul(K);
    let mut h = (key.len() as u64).wrapping_mul(K);
    let (words, tail) = key.as_chunks::<8>();
    for w in words {
        h = round(h, u64::from_le_bytes(*w));
    }
    if !tail.is_empty() {
        let mut w = [0; 8];
        w[..tail.len()].copy_from_slice(tail);
        h = round(h, u64::from_le_bytes(w));
    }
    h ^= h >> 32;
    h = h.wrapping_mul(K);
    h ^ (h >> 29)
}
