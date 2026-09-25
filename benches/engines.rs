//! Phase 7: our Bitcask vs. `sled`, same workload, same harness.
//!
//!     cargo bench --bench engines
//!     cargo bench --bench engines -- set/          # one group
//!
//! Both engines run with `DurabilityPolicy::SyncOnInterval` (fsync every
//! second, sled's `flush_every_ms`), the one policy that means the same thing
//! for both. Everything else is each engine's default: bitcask compacts when
//! it wants to, sled runs its own page cache and GC. The numbers are meant to
//! answer "which one, for this shape of load", not to isolate a code path;
//! `benches/bitcask.rs` does the latter.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use kvs::{Bitcask, Config, DurabilityPolicy, KvStore, SledStore, StoreType};
use std::hint::black_box;
use tempfile::TempDir;

const KEYS: usize = 4096;
const VALUE_SIZES: [usize; 3] = [32, 1 << 10, 16 << 10];

fn open<S: KvStore>(dir: &TempDir, store_type: StoreType) -> S {
    S::open(Config {
        durability_policy: DurabilityPolicy::SyncOnInterval,
        ..Config::new(dir.path(), store_type)
    })
    .unwrap()
}

fn keys() -> Vec<String> {
    (0..KEYS).map(|i| format!("key{i:05}")).collect()
}

// xorshift: deterministic, cheap enough to stay out of the measurement
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

// Overwrites over a fixed key set: the steady state of a live store, where
// most writes replace an existing key.
fn bench_set<S: KvStore>(c: &mut Criterion, engine: &str, store_type: StoreType) {
    let mut group = c.benchmark_group("set");
    let keys = keys();
    for size in VALUE_SIZES {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::new(engine, size), &size, |b, &size| {
            let dir = TempDir::new().unwrap();
            let store: S = open(&dir, store_type);
            let value = "v".repeat(size);
            let mut i = 0usize;
            b.iter(|| {
                i = (i + 1) % KEYS;
                store.set(&keys[i], &value).unwrap();
            });
        });
    }
    group.finish();
}

// Point reads over a preloaded store, keys visited in a fixed scattered order
// so neither engine gets a sequential-access gift.
fn bench_get<S: KvStore>(c: &mut Criterion, engine: &str, store_type: StoreType) {
    let mut group = c.benchmark_group("get");
    let keys = keys();
    for size in VALUE_SIZES {
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::new(engine, size), &size, |b, &size| {
            let dir = TempDir::new().unwrap();
            let store: S = open(&dir, store_type);
            let value = "v".repeat(size);
            for key in &keys {
                store.set(key, &value).unwrap();
            }
            let mut i = 0usize;
            b.iter(|| {
                i = (i + 2531) % KEYS; // co-prime stride: visits every key
                black_box(store.get(&keys[i]).unwrap())
            });
        });
    }
    group.finish();
}

// Read/write mixes at a fixed 1 KiB value: read-heavy (cache-shaped), even,
// and write-heavy (log-shaped). Key choice is uniform random from the seed.
fn bench_mixed<S: KvStore>(c: &mut Criterion, engine: &str, store_type: StoreType) {
    let mut group = c.benchmark_group("mixed");
    group.throughput(Throughput::Elements(1));
    let keys = keys();
    let value = "v".repeat(1 << 10);
    for read_pct in [95u64, 50, 5] {
        group.bench_with_input(
            BenchmarkId::new(engine, format!("{read_pct}r_{}w", 100 - read_pct)),
            &read_pct,
            |b, &read_pct| {
                let dir = TempDir::new().unwrap();
                let store: S = open(&dir, store_type);
                for key in &keys {
                    store.set(key, &value).unwrap();
                }
                let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
                b.iter(|| {
                    let r = rng.next();
                    let key = &keys[(r >> 8) as usize % KEYS];
                    if r % 100 < read_pct {
                        black_box(store.get(key).unwrap());
                    } else {
                        store.set(key, &value).unwrap();
                    }
                });
            },
        );
    }
    group.finish();
}

fn all_engines(c: &mut Criterion) {
    bench_set::<Bitcask>(c, "bitcask", StoreType::Bitcask);
    bench_set::<SledStore>(c, "sled", StoreType::Sled);
    bench_get::<Bitcask>(c, "bitcask", StoreType::Bitcask);
    bench_get::<SledStore>(c, "sled", StoreType::Sled);
    bench_mixed::<Bitcask>(c, "bitcask", StoreType::Bitcask);
    bench_mixed::<SledStore>(c, "sled", StoreType::Sled);
}

criterion_group!(benches, all_engines);
criterion_main!(benches);
