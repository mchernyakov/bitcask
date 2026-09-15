# bitcask

A [Bitcask](https://riak.com/assets/bitcask-intro.pdf)-style log-structured
key/value store, written from scratch in Rust as a learning project. Loosely
follows the [PingCAP Talent Plan](https://github.com/pingcap/talent-plan)
`kvs` project structure, with a custom binary log format instead of JSON.

**Status: WIP.** The log rotates across multiple data files
(`000001.data`, …) with an append-only active file and immutable sealed
files, and stale data is reclaimed by a crash-safe compaction; hint files
are next — see [ROADMAP.md](ROADMAP.md).

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

```rust
use kvs::{Bitcask, DurabilityPolicy, KvStore};

let mut store = Bitcask::open("./data", DurabilityPolicy::SyncOnInterval)?;
store.set("key", "value")?;
assert_eq!(store.get("key")?, Some("value".to_owned()));
store.remove("key")?;
```

There's also a small interactive REPL: `cargo run --bin kvs`.

## Tests

`cargo test` — CLI tests are ignored until the binary grows a batch mode.
Corruption handling is tested bit-by-bit: every single-bit flip in a record
must be rejected. Compaction is covered end-to-end: the directory must shrink
under overwrite load, and every value must survive a reopen from a compacted,
multi-file state.
