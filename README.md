# bitcask

A [Bitcask](https://riak.com/assets/bitcask-intro.pdf)-style log-structured
key/value store, written from scratch in Rust as a learning project. Loosely
follows the [PingCAP Talent Plan](https://github.com/pingcap/talent-plan)
`kvs` project structure.

**Status: WIP.** Working: multi-file log with rotation, crash-safe
compaction on a background thread, hint files for fast startup, and a
concurrent API (one writer, many readers). Next: benchmarks and a lock-free
keydir — see [ROADMAP.md](ROADMAP.md).

## How it works

Writes append records to an active log file that rolls over at a size
threshold; sealed files are immutable. An in-memory index (the "keydir") maps
each key to its latest record, so a read is a single `pread`. Stale data is
reclaimed by merging sealed files, and each merge output gets a `*.hint`
sibling so startup can load the index without replaying full logs. Every
record carries a CRC: torn tails are truncated on startup, any other
corruption fails `open` instead of serving damaged data.

Record format (little-endian):

    [u32 crc][u32 body_len][u64 ts][u8 type][u32 key_len][key][u32 value_len][value]

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

There's also a small interactive REPL: `cargo run --bin kvs`.

## Tests

`cargo test`.
