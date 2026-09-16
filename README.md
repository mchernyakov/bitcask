# bitcask

A [Bitcask](https://riak.com/assets/bitcask-intro.pdf)-style log-structured
key/value store, written from scratch in Rust as a learning project. Loosely
follows the [PingCAP Talent Plan](https://github.com/pingcap/talent-plan)
`kvs` project structure, with a custom binary log format instead of JSON.

**Status: WIP.** The log rotates across multiple data files
(`000001.data`, …) with an append-only active file and immutable sealed
files, stale data is reclaimed by a crash-safe compaction, and hint files
make startup fast; concurrency (one writer, many readers) is next —
see [ROADMAP.md](ROADMAP.md).

## How it works

All writes append a record to the active log file, which rolls over to a new
file at a size threshold; older files are immutable. An in-memory index
(Bitcask's "keydir") maps each key to the file, offset, and length of its
latest record. Reads are a single `pread` at that position — no seeking, no
scanning. On startup the data files are replayed in id order to rebuild the
index.

When enough bytes go stale (overwrites and removes), sealed files are merged:
records still referenced by the index are copied into fresh files with ids
above the active file, the writer rotates above them, and only then are the
old files deleted — so a crash at any point of the merge is recoverable.

Each merge output also gets a sibling hint file (`000004.hint` for
`000004.data`) listing `key -> (ts, offset, len)` for its records. On startup,
a sealed file with a valid hint is loaded from the hint instead of being
replayed; a damaged, torn, or orphaned hint is simply deleted and the data
file is replayed instead — hints are an optimization, never a source of
truth.

Record format (little-endian):

    [u32 crc][u32 body_len][u64 ts][u8 type][u32 key_len][key][u32 value_len][value]

The CRC-32 covers everything after itself. Torn writes at the tail of the log
are detected and truncated on startup; corruption elsewhere fails `open`
loudly rather than serving damaged data.

Writes go through a buffered append-only handle, reads through a separate
read-only handle, with an explicit durability policy chosen at `open`:

- `SyncOnEveryPut` — fsync after every write (durable, slow)
- `SyncOnInterval` — fsync at most once per interval
- `OsDecides` — let the page cache flush when it wants (fast, weakest)

## Usage

The store is opened with a `Config`. `Config::new(dir)` picks sane defaults;
override individual fields with struct-update syntax:

```rust
use kvs::{Bitcask, Config, DurabilityPolicy, KvStore};

// defaults: OsDecides, 1 MB files, 1 MB compaction threshold, 1 s flush interval
let mut store = Bitcask::open(Config::new("./data"))?;

// or tune it:
let mut store = Bitcask::open(Config {
    durability_policy: DurabilityPolicy::SyncOnInterval,
    file_size_threshold: 4 << 20,
    ..Config::new("./data")
})?;

store.set("key", "value")?;
assert_eq!(store.get("key")?, Some("value".to_owned()));
store.remove("key")?;
```

Config fields:

- `dir` — directory holding the data files
- `durability_policy` — when to fsync (see above)
- `file_size_threshold` — the active file rolls over to a new one at this size
- `compaction_threshold` — merge sealed files once this many bytes are stale
- `flush_threshold_millis` — fsync interval for `SyncOnInterval`

There's also a small interactive REPL: `cargo run --bin kvs`.

## Tests

`cargo test` — CLI tests are ignored until the binary grows a batch mode.
Corruption handling is tested bit-by-bit: every single-bit flip in a record
(data or hint) must be rejected. Compaction is covered end-to-end: the
directory must shrink under overwrite load, and every value must survive a
reopen from a compacted, multi-file state. Hint files are proven to be both
used (startup resolves keys through them) and disposable (corrupt, torn, or
orphaned hints fall back to data replay).
