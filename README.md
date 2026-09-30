# gatekv
A key-value store replicated by Raft on three static nodes, in 986 lines of Rust and x86-64
assembly (`wc -l src/*`), std only. Clients speak a RESP subset: `GET`, `SET`, `DEL`, `PING`.

## Build and run
Linux x86-64 only (elsewhere a `compile_error!`); the CPU needs SSE4.2, not AVX. On an Apple
Silicon Mac, use an OrbStack amd64 machine (Rosetta): `orb create --arch amd64 ubuntu kv`,
`orb -m kv`, install rustup with `--default-host x86_64-unknown-linux-gnu`, work under `/mnt/mac`.
To edit on macOS, add that target with rustup and set `rust-analyzer.cargo.target` to it.

    cargo build --release && cargo test   # tests/cluster.rs runs 3 real processes
    ./cluster.sh                          # nodes 0-2 on 127.0.0.1:7001-7003, data in ./data
    redis-cli -p 7001 set k v             # a follower replies NOTLEADER <leader address>

## Architecture and formats
- `raft.rs`: pure state machine, no I/O, clock or threads. `tick()`, `step()`, `propose()` queue
  output that `ready()` hands over: messages, and the term, vote and log suffix to persist first.
- `main.rs`: one thread owns Raft, store and table; an mpsc channel feeds it events and its
  `recv_timeout` paces 50 ms ticks. Each round drains the queue, fsyncs once, sends, applies.
- `store.rs`: log records `crc32c u32 | term u64 | len u64 | command` (CRC over the rest) in an
  append-only file; `meta` holds one record for term and vote, replaced atomically.
- `wire.rs`: frames `len u64 | type u8 | crc32c u32 | payload`, CRC over type and payload. Peers
  open with their id byte, RESP clients (`resp.rs`) with `*`: one port per node.
- `kv.rs`: open addressing, linear probing, backward-shift deletion; applies `G key`, `D key`,
  `S klen:u64 key value`. `asm.rs`: CRC32C and key hash in `global_asm!`. All little-endian.

## Design decisions and limits
- Reads go through the log: a GET commits like a write, so it sees every earlier write with no
  read index or clock-based lease, at the cost of a replicated fsync and a log entry per read.
- Group commit (one fsync per batch); the leader pipelines entries and rewinds on rejection.
- Recovery keeps the longest valid log prefix, which is what a torn write leaves; a corrupt
  record mid-log ends it too (Raft assumes disks do not lie). The commit index is volatile: a
  restarted node replays its log once a leader says how far it may apply.
- No snapshots or compaction: the log (GETs included) grows forever, restarts replay it all.
- Exactly 3 static nodes; no membership changes, pre-vote, check-quorum or client sessions (a
  write cut off by a leader change may still commit). Keys and values up to 64 KiB.

## The assembly, honestly
On a 2.8 GHz Xeon KVM guest, `cargo bench` puts `crc32c` at ~9 GB/s, 22x a table-driven Rust
CRC; rustc's loop over `_mm_crc32_u64` matches it, and inlining makes it faster on short inputs.
The gain is the SSE4.2 instruction, not hand coding. `hash` is integer code that rustc compiles
as well: no gain. Under Rosetta or QEMU a benchmark measures the emulator: measure on native
x86-64 Linux, or run the manual `bench` job of the CI (GitHub's `ubuntu-latest`).

## Status
- [x] Leader election, log replication, majority commit with the current-term rule
- [x] Persistence (fsync before dependent replies), crash recovery with torn-tail truncation
- [x] Linearizable reads through the log; RESP API with leader redirects
- [x] Assembly CRC32C (SSE4.2) and key hash, tested against Rust references
- [x] Deterministic fault-injection simulation; 3-process test with failover and restart
