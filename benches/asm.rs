//! `cargo bench`: the assembly routines against Rust, best of five rounds per case. Only
//! native x86-64 numbers mean anything; under Rosetta or QEMU this measures the emulator.

#[path = "../tests/reference/mod.rs"]
mod reference;

use std::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u64};
use std::hint::black_box;
use std::time::Instant;

/// What rustc emits for CRC32C when handed the SSE4.2 intrinsics: the fair rival of the asm.
#[target_feature(enable = "sse4.2")]
fn crc32c_intrinsics(seed: u32, data: &[u8]) -> u32 {
    let (words, tail) = data.as_chunks::<8>();
    let mut crc = u64::from(!seed);
    for w in words {
        crc = _mm_crc32_u64(crc, u64::from_le_bytes(*w));
    }
    let mut crc = crc as u32;
    for &b in tail {
        crc = _mm_crc32_u8(crc, b);
    }
    !crc
}

fn bench(name: &str, len: usize, f: impl Fn(&[u8]) -> u64) {
    let buf: Vec<u8> = (0..len).map(|i| (i * 131 + 7) as u8).collect();
    let iters = (8 << 20) / len;
    let mut best = f64::INFINITY;
    for _ in 0..5 {
        let start = Instant::now();
        for _ in 0..iters {
            black_box(f(black_box(&buf)));
        }
        best = best.min(start.elapsed().as_nanos() as f64 / iters as f64);
    }
    let gbps = len as f64 / best;
    println!("{name:<18} {len:>6} B {best:>10.2} ns/call {gbps:>7.2} GB/s");
}

fn main() {
    assert!(is_x86_feature_detected!("sse4.2"), "SSE4.2 is required");
    for len in [8, 16, 32, 64, 256, 4096, 65536] {
        let data: Vec<u8> = (0..len).map(|i| i as u8).collect();
        // SAFETY (both unsafe blocks): SSE4.2 support was asserted above.
        let fair = unsafe { crc32c_intrinsics(7, &data) };
        assert_eq!(
            fair,
            gatekv::asm::crc32c(7, &data),
            "intrinsics baseline is wrong"
        );
        bench("crc32c asm", len, |b| gatekv::asm::crc32c(0, b).into());
        bench("crc32c intrinsics", len, |b| {
            unsafe { crc32c_intrinsics(0, b) }.into()
        });
        bench("crc32c table", len, |b| {
            reference::crc32c_table(0, b).into()
        });
        bench("crc32c bitwise", len, |b| {
            reference::crc32c_bitwise(0, b).into()
        });
        bench("hash asm", len, gatekv::asm::hash);
        bench("hash rust", len, reference::hash);
        println!();
    }
}
