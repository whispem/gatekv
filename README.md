# gatekv

A key-value store replicated by Raft on three static nodes.
986 lines of Rust and x86-64 assembly, std only.

It is a small, readable implementation of the core of Raft, not a production database.
Read the [limits](#limits) before relying on it for anything.

| | |
| --- | --- |
| Consensus | Raft, 3 static nodes |
| Client protocol | RESP subset: `GET`, `SET`, `DEL`, `PING` |
| Dependencies | none (std only) |
| Size | 986 lines (`wc -l src/*`) |
| Platform | Linux x86-64, SSE4.2 (no AVX needed) |

## Quick start

```
cargo build --release && cargo test   # tests/cluster.rs runs 3 real processes
./cluster.sh                          # nodes 0-2 on 127.0.0.1:7001-7003, data in ./data
redis-cli -p 7001 set k v             # a follower replies NOTLEADER <leader address>
```

Any `redis-cli` works as a client. On other platforms the build stops with a `compile_error!`.

### Apple Silicon

gatekv is developed on a MacBook Pro with an Apple M5 Max, inside an OrbStack amd64 machine
(Rosetta):

```
orb create --arch amd64 ubuntu kv
orb -m kv
```

Install rustup with `--default-host x86_64-unknown-linux-gnu` and work under `/mnt/mac`.
To edit on macOS, add that target with rustup and set `rust-analyzer.cargo.target` to it.

## Architecture

| Module | Role |
| --- | --- |
| `raft.rs` | Pure state machine: no I/O, clock or threads. `tick()`, `step()` and `propose()` queue output that `ready()` hands over: messages, and the term, vote and log suffix to persist first. |
| `main.rs` | One thread owns Raft, store and table. An mpsc channel feeds it events, and its `recv_timeout` paces 50 ms ticks. Each round drains the queue, fsyncs once, sends, applies. |
| `store.rs` | Append-only log file, plus a `meta` file for term and vote. |
| `wire.rs` | Framing between peers. |
| `resp.rs` | RESP clients. Peers open a connection with their id byte, clients with `*`: one port per node. |
| `kv.rs` | Hash table: open addressing, linear probing, backward-shift deletion. |
| `asm.rs` | CRC32C and key hash in `global_asm!`. |

### Formats

All integers are little-endian.

```
log record   crc32c u32 | term u64 | len u64 | command       CRC over the rest
meta         one record for term and vote, replaced atomically
peer frame   len u64 | type u8 | crc32c u32 | payload        CRC over type and payload
command      G key | D key | S klen:u64 key value
```

## Design decisions

- **Reads go through the log.** A GET commits like a write, so it sees every earlier write with no
  read index or clock-based lease. The cost is a replicated fsync and a log entry per read.
- **Group commit.** One fsync per batch. The leader pipelines entries and rewinds on rejection.
- **Recovery keeps the longest valid log prefix**, which is what a torn write leaves. A corrupt
  record mid-log ends it too: Raft assumes disks do not lie.
- **The commit index is volatile.** A restarted node replays its log once a leader says how far
  it may apply.

## Limits

- No snapshots or compaction: the log (GETs included) grows forever, and restarts replay all of it.
- Exactly 3 static nodes. No membership changes, pre-vote or check-quorum.
- No client sessions: a write cut off by a leader change may still commit.
- Keys and values are limited to 64 KiB.

## The assembly, honestly

The assembly is here because this project was meant to include some, not because it wins.
The numbers below say where it helps and where it does not.

### Setup

- Command: `cargo bench` (`benches/asm.rs`), via the manual `bench` job of the CI
- Machine: GitHub `ubuntu-latest`, AMD EPYC 7763, 2 vCPUs, rustc 1.98.1
- One run on shared virtualized hardware, no confidence intervals: read differences of a few
  percent as noise
- Under Rosetta or QEMU a benchmark measures the emulator, so measure on native x86-64 Linux

### CRC32C throughput

GB/s, higher is better.

| Input | asm | Rust intrinsics | Rust table | Rust bitwise |
| --- | --- | --- | --- | --- |
| 8 B | 2.57 | 3.22 | 1.11 | 0.21 |
| 64 B | 11.87 | 17.30 | 0.50 | 0.20 |
| 256 B | 8.91 | 11.14 | 0.42 | 0.20 |
| 4 KiB | 8.75 | 8.86 | 0.40 | 0.20 |
| 64 KiB | 8.58 | 8.59 | 0.40 | 0.20 |

### Key hash throughput

GB/s, higher is better.

| Input | asm | Rust |
| --- | --- | --- |
| 8 B | 2.94 | 3.21 |
| 64 B | 10.19 | 10.59 |
| 4 KiB | 5.31 | 5.31 |
| 64 KiB | 5.17 | 5.16 |

### Takeaways

- **The gain is the instruction, not the hand coding.** On large inputs the assembly runs at about
  8.6 GB/s, roughly 21x the table-driven Rust CRC and 43x the bitwise one. A Rust loop over
  `_mm_crc32_u64` matches it to the second digit.
- **Short inputs favor the intrinsics** (about 1.5x at 64 B): rustc inlines them, and an `extern`
  assembly call cannot be inlined.
- **The hash assembly gains nothing.** It is plain integer code that rustc compiles just as well,
  and it is slightly behind on small inputs.
- **The 8.6 GB/s ceiling** is what a single dependent chain of `crc32` instructions gives. Going
  faster means several independent streams combined with a carry-less multiply; neither version
  does that.

The assembly stays, tested against the Rust references, as the part of the project that exercises
`global_asm!` and the calling convention. A production build would use the intrinsics.

## Status

- [x] Leader election, log replication, majority commit with the current-term rule
- [x] Persistence (fsync before dependent replies), crash recovery with torn-tail truncation
- [x] Linearizable reads through the log, RESP API with leader redirects
- [x] Assembly CRC32C (SSE4.2) and key hash, tested against Rust references
- [x] Deterministic fault-injection simulation, 3-process test with failover and restart

## License

Dual-licensed under [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option.