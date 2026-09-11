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
  See [Buffering notes](#buffering-notes) below.

## Phase 2 — File rotation

- [ ] Active file gets a monotonic id (`000001.data`, …). Rotate when it
  exceeds a size threshold: flush + sync + open a new active file.
- [ ] Old files become **immutable** — this invariant is what makes
  compaction and lock-free reads possible.
- [ ] Keydir entry gains `file_id`. Only the in-memory index changes; the
  on-disk record format stays the same.
- [ ] On open: list `*.data`, sort by id, replay in order (later entries win),
  keep a read-only handle per file.

## Phase 3 — Compaction / merge

- [ ] Merge over **immutable files only** — the active file keeps taking
  writes; that's why Bitcask can compact without blocking writers.
- [ ] Walk old files, keep only entries still current per the keydir, drop
  tombstones whose key doesn't appear in newer files, write survivors into
  fresh data files, atomically update the keydir, delete old files.
- [ ] Trigger on tracked garbage: count stale bytes as keys are overwritten /
  removed (`COMPACTION_THRESHOLD` is already waiting for this) — no scanning.
- [ ] **Crash-safety of the merge** is the interesting design problem: write
  merged output to temp files; delete originals only after new files are
  synced and the keydir is switched. For every step, ask: what state does a
  crash here leave, and does `open()` recover from it?

## Phase 4 — Hint files

- [ ] After a merge, write a `*.hint` file next to each data file containing
  just `key -> (offset, len, tstamp)`.
- [ ] On startup, load hints instead of replaying full data files:
  startup goes from O(total bytes) to ~O(keys).

## Phase 5 — Concurrency

Bitcask's model: exactly one writer, many readers.

- [ ] Immutable old files + per-reader handles + `read_exact_at` (no shared
  cursor) make readers naturally lock-free except for the keydir.
- [ ] Start with `RwLock<HashMap>`; explore lock-free maps afterwards.
- [ ] Move merge to a background thread.

## Phase 6 — Measure

- [ ] Criterion benchmarks: sync policies, buffered vs. unbuffered writes,
  startup with vs. without hint files.
- [ ] Turns the buffering trade-offs into visible numbers.

## Possible continuations

Network server/client, thread pools, async — the later PingCAP Talent Plan
projects are a natural extension after Phase 5.

---

## Buffering notes

Three layers where data sits between `set()` and the disk platter:

1. **Application buffer** (`BufWriter`): currently every `set` is one
   `write()` syscall. A syscall costs ~1µs regardless of size, so for small
   entries syscall overhead dominates. `BufWriter` batches small writes into
   one big syscall — this is where buffering is *needed* on the write path.
2. **Kernel page cache**: when `write()` returns, data is **not on disk** —
   it's in kernel memory, flushed whenever the OS decides. This is buffering
   inherited without asking: writes feel fast, but power loss can eat
   acknowledged writes. Only `sync_all()` (fsync) forces durability. Every
   layer of buffering trades crash-durability for throughput — the point is
   to *choose* the trade, not inherit it.
3. **Read path**: `replay()` does two syscalls per record — brutal over a
   large log. Sequential scans (replay, merge) want `BufReader` / large
   chunks. Random-access `get`s are the opposite: a seek throws the
   `BufReader` buffer away, so the right tool is `read_exact_at` with a known
   length — no buffer at all.

**Trap:** with a `BufWriter` on the active file, a freshly-written value lives
in the process buffer, not the file — a `get` through a separate read handle
would miss it. Flush before reads can see the tail (or serve the tail from the
buffer by checking offsets). This is why split handles + explicit flush policy
(Phase 1) come before adding the buffer.
