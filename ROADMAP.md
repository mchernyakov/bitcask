# Bitcask Evolution Roadmap

A learning-oriented roadmap for evolving the current single-file implementation
toward the real Bitcask design (immutable data files + keydir + merge + hints).

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
- [ ] Explore lock-free maps for the keydir (DashMap / evmap) afterwards.

## Phase 6 — Measure

- [x] Criterion benchmarks (`cargo bench`, `benches/bitcask.rs`): sync
  policies, get, startup with vs. without hint files. (Buffered vs.
  unbuffered writes became moot: the write-path `BufWriter` was removed in
  Phase 5 for visibility reasons.) First numbers: set ~1.6 µs (OsDecides and
  SyncOnInterval are indistinguishable), ~4 ms with fsync-per-put, get
  ~0.6 µs. Hints show no win at 32-byte values — hint and data records are
  nearly the same size; re-measure with large values.
- [ ] Grow the benchmarks as questions come up (value-size sweep for hints,
  merge throughput, allocation-sensitive paths like `RecordReader`).

## Future optimizations (backlog)

Ideas noted along the way. Measure first (Phase 6), then decide — none of
these are worth doing on a hunch.

- [x] Reusable record buffer: `RecordReader::next_record` lends `&[u8]` into
  an internal buffer instead of allocating a `Vec` per record (replay and
  merge touch every record). Lending pattern — can't be a std `Iterator`.
- [ ] Rebuild `stale_bytes_count` fully on startup: replay counts tombstones
  but not overwrites, so a reopened store under-counts garbage and compacts
  later than it should.
- [ ] Per-file stale counters: compact only garbage-heavy files instead of
  rewriting every sealed file on each merge.
- [ ] Write hint files on rotation too (not only during merges), so every
  sealed file starts up at O(live keys).
- [x] Fewer fsyncs during merge: sync once per finished output file instead
  of once per source file. (Fell out of the Phase 5 merge redesign: the only
  sync is in `finish_output`, per finished output.)
- [ ] Value-only reads in `get` (keydir stores value position/length like the
  paper) — halves read I/O for large keys, but gives up the per-read CRC
  check over the whole record. Decide with benchmark numbers in hand.
- [ ] `MAX_RECORD_SIZE` sanity cap, enforced in `set` and checked on read
  (defense-in-depth leftover from Phase 1).
