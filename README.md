# bitcask

A [Bitcask](https://riak.com/assets/bitcask-intro.pdf)-style log-structured
key/value store, written from scratch in Rust as a learning project. Loosely
follows the [PingCAP Talent Plan](https://github.com/pingcap/talent-plan)
`kvs` project structure.

**Status:** the engine is done — multi-file log with rotation, crash-safe
background compaction, hint files, a concurrent API (one writer, many
readers, sharded keydir), criterion benchmarks. Next: a network layer.

## Design

Writes append records to an active log file that rolls over at a size
threshold; sealed files are immutable. An in-memory index (the "keydir")
maps each key to `(file_id, offset, len, ts)` of its latest record, so a
read is a single `pread`. A background merge reclaims stale data from sealed
files. Each merge output gets a `*.hint` sibling, so startup loads the index
at ~O(live keys) instead of replaying full logs.

Record format (little-endian):

    [u32 crc][u32 body_len][u64 ts][u8 type][u32 key_len][key][u32 value_len][value]

Notes on the tricky parts:

**Concurrency — one writer, many readers.**
- `Bitcask` is a cheap-clone `Arc` handle with `&self` methods. A
  `Mutex<WriterState>` keeps writes single-threaded; a lock file (`flock`)
  keeps the directory single-process.
- A read takes a short keydir read lock, then `pread`s a shared `Arc<File>`
  — no lock held during I/O. If a merge deletes the file mid-read, the open
  handle still works (unlink semantics); if the keydir entry went stale, the
  read retries.
- Read-your-writes without fsync: the active file has no write buffer —
  `write(2)` first, keydir insert second. A published entry is always
  readable via the page cache. Visibility and durability are separate.

**Crash safety — every record has a CRC.**
- A torn tail on the active file is truncated on open. Corruption anywhere
  else fails `open` instead of serving bad data. A corrupt hint file is
  deleted and its data file replayed.
- Merge writes to `*.data.compact`, then per output: fsync → rename →
  publish. Originals are deleted last. A crash at any step leaves a state
  `open()` recovers from: leftover `.compact` files are removed, and output
  ids sit below the active file, so "later file id wins" replay stays
  correct.
- A merge running while the writer rotates would break that id order, so
  the merge reserves its output ids up front and the writer rotates above
  them before copying starts.

**Durability is a policy**: fsync on every put, on an interval, or let the
OS decide. Only the writer syncs.

Numbers so far: set ~1.6 µs / get ~0.6 µs (`OsDecides`, criterion); fsync
per put ~4 ms; background merge gave 6.6× write throughput under constant
merges.

**Limitations** (mostly Bitcask's own trade-offs): all keys live in RAM; no
range scans; one process per directory; Unix-only (`pread`, `flock`).

## Usage

```rust
use kvs::{Bitcask, Config, DurabilityPolicy, KvStore};

let store = Bitcask::open(Config::new("./data"))?;

store.set("key", "value")?;
assert_eq!(store.get("key")?, Some("value".to_owned()));
store.remove("key")?;

// handles are cheap clones sharing one store
let handle = store.clone();
std::thread::spawn(move || handle.get("key"));

// tuning via Config (struct-update over defaults):
let store = Bitcask::open(Config {
    durability_policy: DurabilityPolicy::SyncOnEveryPut,
    file_size_threshold: 4 << 20,
    ..Config::new("./data")
})?;
```

Config fields: `dir`, `durability_policy` (when to fsync: every put /
on an interval / OS decides), `file_size_threshold` (rotation size),
`compaction_threshold` (stale bytes that trigger a merge), and
`flush_threshold_millis` (the interval for `SyncOnInterval`).

## Binaries

The crate builds three binaries; `cargo build --bins` builds all of them
into `target/debug/`.

| Binary | What it is | Run |
|---|---|---|
| `kvs` | Interactive REPL over a local store (default `kvs/`) | `cargo run` |
| `kvs-server` | TCP server exposing a store | `cargo run --bin kvs-server` |
| `kvs-client` | Interactive REPL that talks to a server | `cargo run --bin kvs-client` |

`kvs` is the default run target. The REPL commands are the same locally and
over the network: `set <KEY> <VALUE>`, `get <KEY>`, `rm <KEY>`, `help`,
`quit` (or Ctrl-D).

Server options:

```
kvs-server [--addr <ADDR>] [--dir <DIR>]
  --addr   address to listen on   (default 127.0.0.1:4000)
  --dir    store directory        (default kvs/)
```

Client options:

```
kvs-client [--addr <ADDR>]
  --addr   server address         (default 127.0.0.1:4000)
```

Pass options after `--` when using `cargo run`, e.g.
`cargo run --bin kvs-server -- --addr 0.0.0.0:5000 --dir ./data`. Logging
goes to stderr and is controlled with `RUST_LOG` (default `kvs=info`).

## Tests

`cargo test`. Benchmarks: `cargo bench` (criterion, see `benches/bitcask.rs`).
