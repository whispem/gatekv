//! Frame codec: round trips, and rejection of damaged or hostile frames without panicking.

use gatekv::asm::crc32c;
use gatekv::raft::{Body, Entry, Msg};
use gatekv::wire::{self, MAX_FRAME};

fn samples() -> Vec<Msg> {
    let entries = vec![
        Entry {
            term: 3,
            cmd: b"Skey".to_vec(),
        },
        Entry {
            term: 3,
            cmd: Vec::new(),
        },
    ];
    [
        (1, Body::Vote(3, 1)),
        (2, Body::VoteResp(true)),
        (3, Body::Append(4, 2, entries, 4)),
        (0, Body::Append(0, 0, Vec::new(), 0)),
        (u64::MAX, Body::AppendResp(false, 7)),
    ]
    .into_iter()
    .map(|(term, body)| Msg { term, body })
    .collect()
}

/// A frame with a valid header and checksum around an arbitrary payload.
fn frame(ty: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = (payload.len() as u64).to_le_bytes().to_vec();
    f.push(ty);
    f.extend(crc32c(crc32c(0, &[ty]), payload).to_le_bytes());
    f.extend_from_slice(payload);
    f
}

#[test]
fn round_trips() {
    for m in samples() {
        let f = wire::encode(&m);
        assert_eq!(wire::read(&mut &f[..]).unwrap(), m);
    }
}

#[test]
fn rejects_every_truncation() {
    for m in samples() {
        let f = wire::encode(&m);
        for cut in 0..f.len() {
            assert!(wire::read(&mut &f[..cut]).is_err(), "{m:?} cut at {cut}");
        }
    }
}

#[test]
fn rejects_every_corrupt_byte() {
    for m in samples() {
        let f = wire::encode(&m);
        for at in 0..f.len() {
            for flip in [0x01, 0x80, 0xFF] {
                let mut bad = f.clone();
                bad[at] ^= flip;
                assert!(
                    wire::read(&mut &bad[..]).is_err(),
                    "{m:?} byte {at} ^ {flip:#x}"
                );
            }
        }
    }
}

#[test]
fn rejects_oversized_frames_before_reading_them() {
    let mut f = frame(3, &[]);
    f[..8].copy_from_slice(&(MAX_FRAME as u64 + 1).to_le_bytes());
    assert!(wire::read(&mut &f[..]).is_err());
    f[..8].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(wire::read(&mut &f[..]).is_err());
}

#[test]
fn checksummed_garbage_is_rejected_or_decoded_without_panic() {
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    // Unknown types, huge counts and lengths, trailing bytes: all must fail cleanly.
    assert!(wire::read(&mut &frame(9, &[0; 8])[..]).is_err());
    assert!(wire::read(&mut &frame(2, &[0; 10])[..]).is_err());
    let mut huge = vec![0; 32];
    huge.extend(u64::MAX.to_le_bytes());
    assert!(wire::read(&mut &frame(3, &huge)[..]).is_err());
    for _ in 0..100_000 {
        let len = (next() % 96) as usize;
        let payload: Vec<u8> = (0..len)
            .map(|_| if next() % 3 == 0 { 0xFF } else { next() as u8 })
            .collect();
        let ty = (next() % 6) as u8;
        _ = wire::read(&mut &frame(ty, &payload)[..]);
    }
}
