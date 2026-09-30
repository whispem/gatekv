//! Differential tests: the assembly routines against the Rust references.

mod reference;

use gatekv::asm::{crc32c, hash};

/// xorshift64*: every run sees the same inputs.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

#[test]
fn crc32c_matches_published_vectors() {
    // RFC 3720 (iSCSI) appendix B.4, plus the usual "123456789" check value.
    let ascending: Vec<u8> = (0..32).collect();
    let descending: Vec<u8> = (0..32).rev().collect();
    assert_eq!(crc32c(0, b""), 0);
    assert_eq!(crc32c(0, b"123456789"), 0xE306_9283);
    assert_eq!(crc32c(0, &[0; 32]), 0x8A91_36AA);
    assert_eq!(crc32c(0, &[0xFF; 32]), 0x62A8_AB43);
    assert_eq!(crc32c(0, &ascending), 0x46DD_794E);
    assert_eq!(crc32c(0, &descending), 0x113F_DB5C);
}

#[test]
fn every_length_up_to_200_at_every_alignment() {
    // Each input is a window into a larger buffer, so a read past its end would change the
    // result and show up as a mismatch.
    let pool: Vec<u8> = (0..512u32).map(|i| (i * 167 + 13) as u8).collect();
    for at in 0..64 {
        for len in 0..=200 {
            let data = &pool[at..at + len];
            let want = reference::crc32c_bitwise(0, data);
            assert_eq!(crc32c(0, data), want, "crc32c, offset {at}, length {len}");
            assert_eq!(reference::crc32c_table(0, data), want);
            assert_eq!(
                hash(data),
                reference::hash(data),
                "hash, offset {at}, length {len}"
            );
        }
    }
}

#[test]
fn random_inputs_and_seeds() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let pool: Vec<u8> = (0..4096).map(|_| rng.next() as u8).collect();
    for _ in 0..50_000 {
        let len = (rng.next() % 201) as usize;
        let at = (rng.next() % 1024) as usize;
        let seed = rng.next() as u32;
        let data = &pool[at..at + len];
        let want = reference::crc32c_bitwise(seed, data);
        assert_eq!(
            crc32c(seed, data),
            want,
            "crc32c, offset {at}, length {len}"
        );
        assert_eq!(reference::crc32c_table(seed, data), want);
        assert_eq!(
            hash(data),
            reference::hash(data),
            "hash, offset {at}, length {len}"
        );
    }
}

#[test]
fn crc32c_chains_across_any_split() {
    let data: Vec<u8> = (0..100).collect();
    let whole = crc32c(0, &data);
    for cut in 0..=data.len() {
        assert_eq!(crc32c(crc32c(0, &data[..cut]), &data[cut..]), whole);
    }
}

#[test]
fn hash_sees_every_byte_and_the_length() {
    let zeros = [0u8; 24];
    let base = hash(&zeros);
    for i in 0..zeros.len() {
        let mut key = zeros;
        key[i] = 1;
        assert_ne!(hash(&key), base, "byte {i} ignored");
        assert_ne!(hash(&zeros[..i]), base, "length {i} collides with 24");
    }
}
