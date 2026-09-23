# Bitcask Evolution Roadmap

A learning-oriented roadmap. Part 1 (Phases 1–6) evolved a single-file store
into the real Bitcask design — immutable data files + keydir + merge +
hints — and is done. Part 2 (Phases 7+) goes beyond the engine, ordered for
Rust fluency first: the remaining PingCAP Talent Plan projects (network,
thread pools, async), then robustness testing and engine optimizations
under real load, then a second engine (LSM), then direct I/O, then
distribution.

# Part 1 — The engine

## Phase 1 — Polish the single-file version

Groundwork that makes rotation and compaction much easier later.

- [x] **Record header: CRC + timestamp.**
  Real Bitcask entries are `crc | tstamp | ksz | value_sz | key | value`.
  Truncation handling covers a torn write at the tail; a CRC also catches
  corruption in the middle of the file and garbage that happens to parse.
  Timestamp becomes useful for merge (and TTL, if ever).
- [x] **Index stores `(offset, len)` instead of just `offset`.**
  `get` becomes a single `read_exact_at` (`std::os::unix::fs::FileExt`) of
  exactly the right size — one syscall, no seek, no mutation of the file
  cursor (matters for concurrency later). This matches Bitcask's keydir shape
  `{file_id, value_pos, value_sz, tstamp}`; `file_id` gets added in Phase 2.
- [x] **Split read and write handles.**
  `get` currently seeks the same handle used for appends. It works only
  because `append` mode ignores the cursor on write, and it breaks the moment
  a write buffer is added.
- [x] **Explicit durability policy** instead of "sync once in `Drop`":
  sync-on-every-put vs. sync-on-interval vs. OS-decides, as a config option.

## Phase 2 — File rotation

- [x] Active file gets a monotonic id (`000001.data`, …). Rotate when it
  exceeds a size threshold: flush + sync + open a new active file.
- [x] Old files become **immutable** — this invariant is what makes
  compaction and lock-free reads possible.
- [x] Keydir entry gains `file_id`. Only the in-memory index changes; the
  on-disk record format stays the same.
- [x] On open: list `*.data`, sort by id, replay in order (later entries win),
  keep a read-only handle per file.

## Phase 3 — Compaction / merge

- [x] Merge over **immutable files only** — the active file keeps taking
  writes; that's why Bitcask can compact without blocking writers.
- [x] Walk old files, keep only entries still current per the keydir, drop
  tombstones whose key doesn't appear in newer files, write survivors into
  fresh data files, atomically update the keydir, delete old files.
- [x] Trigger on tracked garbage: count stale bytes as keys are overwritten /
  removed (`COMPACTION_THRESHOLD` is already waiting for this) — no scanning.
- [x] **Crash-safety of the merge** is the interesting design problem: write
  merged output to temp files; delete originals only after new files are
  synced and the keydir is switched. For every step, ask: what state does a
  crash here leave, and does `open()` recover from it?

## Phase 4 — Hint files

- [x] After a merge, write a `*.hint` file next to each data file containing
  just `key -> (offset, len, tstamp)`.
- [x] On startup, load hints instead of replaying full data files:
  startup goes from O(total bytes) to ~O(keys).

## Phase 5 — Concurrency

Bitcask's model: exactly one writer, many readers.

- [x] Immutable old files + shared `Arc<File>` handles + `read_exact_at` (no
  shared cursor) make readers naturally lock-free except for the keydir
  (pread made per-reader handles unnecessary).
- [x] Start with `RwLock<HashMap>` — done, plus a lock file and a
  concurrent stress test.
- [x] Move merge to a background thread — writes no longer pay for
  compaction (6.6× write throughput in the stress test); see
  [PHASE5.md](PHASE5.md) for the design and measurements.
- [x] Reduce keydir contention: instead of dropping in DashMap/evmap, a
  hand-rolled 16-way sharded index (FNV-1a bucket per key, one `RwLock` per
  bucket — DashMap's design, built to understand it) plus a copy-on-write
  `ArcSwap` for the readers map, making `get`'s file lookup wait-free.
  evmap/left-right stays an optional experiment.

## Phase 6 — Measure

- [x] Criterion benchmarks (`cargo bench`, `benches/bitcask.rs`): sync
  policies, get, startup with vs. without hint files. (Buffered vs.
  unbuffered writes became moot: the write-path `BufWriter` was removed in
  Phase 5 for visibility reasons.) First numbers: set ~1.6 µs (OsDecides and
  SyncOnInterval are indistinguishable), ~4 ms with fsync-per-put, get
  ~0.6 µs. Hints show no win at 32-byte values — hint and data records are
  nearly the same size; re-measure with large values.
- [x] Concurrent get benchmarks (latency and throughput under reader
  threads) — drove the sharded-index + `ArcSwap` refactor.
- [ ] Grow the benchmarks as questions come up (value-size sweep for hints,
  merge throughput, allocation-sensitive paths like `RecordReader`).

# Part 2 — Beyond the engine

Follows the arc of the [PingCAP Talent Plan](https://github.com/pingcap/talent-plan)
where it helps and leaves it where it doesn't. Where we stand: projects 1–2
done (and exceeded — hints, background merge, crash-safe compaction aren't
in the plan), the shared-state half of project 4 done in Phase 5. Missing:
networking (project 3), thread pools (project 4's other half), async
(project 5) — then everything beyond.

## Phase 7 — Network server & client (≈ Talent Plan project 3)

Turn the library into a database. The `Clone + Send + 'static` handle from
Phase 5 is exactly the engine shape a multithreaded server needs — this phase
is why project 4 demands that signature.

- [x] `kvs-server` / `kvs-client` binaries (clap) speaking a custom binary
  protocol over TCP. Hand-roll the wire format first (length-prefixed frames,
  request/response enums, explicit encode/decode) before reaching for serde —
  the framing and partial-read handling *is* the lesson.
- [x] Typed errors across the wire: a server-side `KvsError` arrives at the
  client as a typed error, not a string.
- [ ] `KvsEngine` trait with a second implementation (`sled`) behind it;
  criterion benchmark ours vs. sled at several value sizes and read/write
  mixes. Reading sled's docs to understand *why* it differs is half the value.
- [ ] Stretch with a big payoff: speak a subset of RESP instead of (or beside)
  the custom protocol, so `redis-cli` and `redis-benchmark` work against the
  store for free — instant load-testing tooling.

Rust learned: trait objects vs. generics for the engine, TCP/`BufReader`
framing, binary protocol design, clap.

## Phase 8 — Thread pools from scratch (≈ Talent Plan project 4)

Phase 5 did the shared-state half of project 4; this is the other half.

- [ ] A `ThreadPool` trait with three impls: naive spawn-per-job; a
  shared-queue pool (channel + workers) where a panicking job does **not**
  kill the worker — the subtle bit: `catch_unwind` + `UnwindSafe`, or
  respawn-via-`Drop`-sentinel; and a `rayon`-backed one for comparison.
- [ ] Wire the pools into the server; benchmark throughput/latency across
  pool types × thread counts vs. core count, read-heavy vs. write-heavy.

Rust learned: panic semantics and unwind safety, channels as work queues,
`Drop`-based cleanup patterns, when work-stealing matters.

## Phase 9 — Async (≈ Talent Plan project 5)

- [ ] Port server and client to tokio. The client is the interesting design
  problem: a futures-based API, connection reuse, cancellation.
- [ ] The real question of the phase: what to do with blocking file I/O in an
  async runtime — `spawn_blocking`, a dedicated I/O thread, or just block
  (pread on page-cache-hot data may be cheaper than a handoff). Measure.
- [ ] Benchmark sync-threadpool vs. async under many mostly-idle connections
  — the workload async actually exists for.
- [ ] Stretch: `tokio-uring` for the read path; compare with pread. (The
  full version of this idea is Phase 13.)

Rust learned: async/await, `Pin`/`Future` at least at the "why does the
compiler say that" level, tokio, structured cancellation.

## Phase 10 — Crash-consistency & robustness testing

The most internals-dense phase: how real engines earn trust. Independent of
7–9 — can be done any time — but do it before Phase 11: it's the safety net
for engine surgery.

- [ ] Property tests (proptest): random op sequences checked against a
  `HashMap` oracle, with random reopens (and merges) interleaved.
- [ ] Fuzz the decode path (`cargo-fuzz` on `RecordReader`): no input may
  panic, OOM, or return corrupt data as `Ok`.
- [ ] Fail-point crash injection (`fail` crate): simulate a crash at every
  step of rotation / merge / hint-write, reopen, assert invariants. This
  turns Phase 3's "ask what state a crash leaves" into an executable test.
- [ ] Torn-write simulation: corrupt or truncate files at random offsets;
  `open()` must either recover or fail loudly — never serve bad data.

Rust learned: proptest strategies, fuzzing harnesses, conditional
compilation for failpoints, adversarial API thinking.

## Phase 11 — Engine optimizations, measured under real load

The backlog collected along the way, deliberately deferred until the server
(Phase 7) exists: these are measurement-driven changes, and server-level
concurrent load — plus `redis-benchmark` if the RESP stretch landed — is
what makes the measurements honest. Measure first, then decide; none of
these are worth doing on a hunch.

- [x] Reusable record buffer: `RecordReader::next_record` lends `&[u8]` into
  an internal buffer instead of allocating a `Vec` per record (replay and
  merge touch every record). Lending pattern — can't be a std `Iterator`.
- [x] Fewer fsyncs during merge: sync once per finished output file instead
  of once per source file. (Fell out of the Phase 5 merge redesign: the only
  sync is in `finish_output`, per finished output.)
- [ ] Rebuild `stale_bytes_count` fully on startup: replay counts tombstones
  but not overwrites (and hint-based startup skips counting entirely), so a
  reopened store under-counts garbage and compacts later than it should.
- [ ] Per-file stale counters: compact only garbage-heavy files instead of
  rewriting every sealed file on each merge.
- [ ] Write hint files on rotation too (not only during merges), so every
  sealed file starts up at O(live keys).
- [ ] Value-only reads in `get` (keydir stores value position/length like the
  paper) — halves read I/O for large keys, but gives up the per-read CRC
  check over the whole record. Decide with benchmark numbers in hand.
- [ ] `MAX_RECORD_SIZE` sanity cap, enforced in `set` and checked on read —
  today `get` allocates `vec![0; entry.len]` with a length that ultimately
  comes from a hint file.

## Phase 12 — Second engine: LSM tree (new repo)

Bitcask's honest limitation: every key lives in RAM and there are no range
scans. The fix is a different engine, not a patch — and it's the engine
behind RocksDB/TiKV. Do [mini-lsm](https://skyzh.github.io/mini-lsm/)
(a structured Rust course): memtables, SSTs with bloom filters, leveled
compaction, WAL, MVCC/snapshots, merge iterators.

- [ ] Weeks 1–2 (storage + compaction) minimum; week 3 (MVCC) sets up
  transactions later.
- [ ] Afterwards, benchmark it against this bitcask on the same workloads —
  seeing *where* each wins (point reads vs. scans vs. write amp) is the
  internals lesson no book gives.

Rust learned: serious iterator/trait design (merge iterators are peak Rust),
`Bytes`, block encoding, harder lifetime problems than bitcask poses.

## Phase 13 — Direct I/O & block cache

Replace the page cache with our own — what RocksDB and most serious engines
do. Deliberately after Phase 12: mini-lsm teaches block-encoded SSTs and a
block cache first, so this phase returns to the idea fluent in
block-structured storage and can target either engine. Also wants the
Phase 9 setup: `O_DIRECT` is Linux-only (dev in a VM/container, `cfg`-gated
with a buffered fallback), and the server provides realistic load. The key
insight that keeps it tractable: sealed files are immutable and read-only —
perfect `O_DIRECT` citizens. The active file stays buffered, so the
write-visibility trick from Phase 5 is untouched.

- [ ] Step 1 — block cache over sealed files, still buffered I/O:
  `(file_id, block_no)` → aligned 4 KiB block; sharded LRU; blocks handed
  out as `Arc<Block>` so readers pin them — eviction only removes from the
  map, memory dies with the last reader (the Phase 5 file-handle trick, one
  level down). `get` = keydir → block range → fetch/stitch. Runs on macOS;
  benchmark hit rates and latency vs. today's raw pread.
- [ ] Step 2 — `O_DIRECT` reads for sealed files (`F_NOCACHE` on macOS):
  aligned buffers via `Layout::from_size_align`; cache misses do aligned
  preads. Measure what was invisible before: bounded memory, cold-read
  latency, and merges no longer evicting the hot set (merge bypasses the
  cache or uses a small private pool).
- [ ] Step 3 (hard mode) — `O_DIRECT` active file: records accumulate in an
  aligned tail buffer, full blocks flush, readers check the tail buffer
  first (a mini-memtable); `fallocate` up front so `fdatasync` never
  journals metadata. Group commit falls out naturally.
- [ ] Step 4 — combine with io_uring: registered aligned buffers + batched
  `O_DIRECT` reads is the combination where its numbers get dramatic.
- [ ] Benchmark honestly: cold cache, dataset larger than the cache bound —
  block caching only matters once data outgrows RAM. Pairs with the
  value-size sweep in Phase 6.

Rust learned: aligned allocation (`std::alloc` + `Layout`), sharded caches,
`Arc`-based pinning, `cfg`-gated platform code, careful `unsafe`.

## Phase 14 — Distributed (Talent Plan dss, or Raft over kvs)

The distributed-systems goal, two viable routes:

- [ ] Route A — Talent Plan's "Distributed Systems in Rust" (`courses/dss`):
  Raft from the paper against their labrpc test harness, then a
  fault-tolerant KV service on top, then Percolator-style transactions.
  The repo is archived but the labs and tests still work; the tests are the
  product — they inject partitions, reordering, and crashes for you.
- [ ] Route B — distribute *this* store: replicate the log via `raft-rs`
  (TiKV's Raft) or a from-scratch Raft, snapshot = a merge output + hints
  shipped to followers. More design freedom, less test scaffolding — do A
  first unless you enjoy building harnesses.
- [ ] Either way: deterministic simulation testing with `madsim` is the
  modern way to make these labs debuggable — seeds instead of heisenbugs.

## Feature backlog (engine-level, any time)

- [ ] Range scans: keydir buckets as `BTreeMap` (measure the point-read
  cost) — also what makes the sled/LSM comparisons apples-to-apples.
- [ ] TTL: the record timestamp is already on disk; expiry check in `get`,
  reclamation in merge.
- [ ] Online backup: snapshot = hard-link sealed files + copy hints; restore
  = `open()`. Falls out of immutability almost for free.
- [ ] Metrics endpoint (prometheus) once the server exists: keydir size,
  stale bytes, merge duration, fsync latency histogram.

