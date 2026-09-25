//! Criterion benchmarks (Phase 6). Run with `cargo bench`.
//!
//! To measure a change, benchmark the old code first and save a baseline,
//! then benchmark the new code against it:
//!
//!     cargo bench -- --save-baseline before     # on the old commit
//!     cargo bench -- --baseline before          # with your change applied
//!
//! Criterion runs release builds, warms up, and reports the delta per
//! benchmark with a significance verdict.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use kvs::{Bitcask, Config, DurabilityPolicy, KvStore, StoreType};
use std::fs;
use std::hint::black_box;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Instant;
use tempfile::TempDir;

const VALUE: &str = "vvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvv"; // 32 bytes

// Compaction disabled: these measure the pure write path, without background
// merges stealing time from some samples and not others.
fn write_config(dir: &Path, policy: DurabilityPolicy) -> Config {
    Config {
        durability_policy: policy,
        file_size_threshold: 4 << 20,
        compaction_threshold: u64::MAX,
        ..Config::new(dir, StoreType::Bitcask)
    }
}

fn bench_set(c: &mut Criterion) {
    let mut group = c.benchmark_group("set");
    group.throughput(Throughput::Elements(1));

    for (name, policy) in [
        ("os_decides", DurabilityPolicy::OsDecides),
        ("sync_on_interval", DurabilityPolicy::SyncOnInterval),
    ] {
        group.bench_function(name, |b| {
            let dir = TempDir::new().unwrap();
            let store = Bitcask::open(write_config(dir.path(), policy)).unwrap();
            let mut i = 0u64;
            b.iter(|| {
                i += 1;
                store.set(&format!("key{}", i % 1024), VALUE).unwrap();
            });
        });
    }
    group.finish();

    // an fsync per put is orders of magnitude slower — own group, fewer samples
    let mut group = c.benchmark_group("set_durable");
    group.sample_size(10);
    group.bench_function("sync_on_every_put", |b| {
        let dir = TempDir::new().unwrap();
        let store =
            Bitcask::open(write_config(dir.path(), DurabilityPolicy::SyncOnEveryPut)).unwrap();
        let mut i = 0u64;
        b.iter(|| {
            i += 1;
            store.set(&format!("key{}", i % 1024), VALUE).unwrap();
        });
    });
    group.finish();
}

fn bench_get(c: &mut Criterion) {
    let dir = TempDir::new().unwrap();
    // small files, so the keys spread across many sealed files
    let store = Bitcask::open(Config {
        file_size_threshold: 64 << 10,
        compaction_threshold: u64::MAX,
        ..Config::new(dir.path(), StoreType::Bitcask)
    })
    .unwrap();
    for i in 0..10_000 {
        store.set(&format!("key{i}"), VALUE).unwrap();
    }

    // keys pre-generated: the timed loop must not measure format!/allocation
    let keys: Vec<String> = (0..10_000).map(|i| format!("key{i}")).collect();

    let mut group = c.benchmark_group("get");
    group.throughput(Throughput::Elements(1));
    group.bench_function("across_files", |b| {
        let mut i: usize = 0;
        b.iter(|| {
            i = (i + 7919) % 10_000; // stride co-prime to 10_000: visits every key
            black_box(store.get(&keys[i]).unwrap())
        });
    });
    group.finish();
}

// Build one compacted store (merge outputs + hint files) and a copy of the
// same data files with the hints deleted, so open() is measured on identical
// data both with and without hints.
fn prepare_startup_dirs() -> (TempDir, TempDir) {
    let with_hints = TempDir::new().unwrap();
    {
        let store = Bitcask::open(Config {
            file_size_threshold: 64 << 10,
            compaction_threshold: 128 << 10,
            ..Config::new(with_hints.path(), StoreType::Bitcask)
        })
        .unwrap();
        // two passes: the first pass goes stale, merges fire and write hints
        for _pass in 0..2 {
            for i in 0..5_000 {
                store.set(&format!("key{i}"), VALUE).unwrap();
            }
        }
    } // drop joins the merge thread: any pending merge has finished here

    let hint_count = fs::read_dir(with_hints.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".hint"))
        .count();
    assert!(hint_count > 0, "setup should have produced hint files");

    let without_hints = TempDir::new().unwrap();
    for entry in fs::read_dir(with_hints.path()).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.ends_with(".data") {
            fs::copy(entry.path(), without_hints.path().join(&*name)).unwrap();
        }
    }
    (with_hints, without_hints)
}

fn bench_open(c: &mut Criterion) {
    let (with_hints, without_hints) = prepare_startup_dirs();

    let mut group = c.benchmark_group("open");
    group.sample_size(20);
    group.bench_function("replay_from_hints", |b| {
        b.iter(|| {
            black_box(Bitcask::open(Config::new(with_hints.path(), StoreType::Bitcask)).unwrap())
        })
    });
    group.bench_function("replay_from_data", |b| {
        b.iter(|| {
            black_box(Bitcask::open(Config::new(without_hints.path(), StoreType::Bitcask)).unwrap())
        })
    });
    group.finish();
}

// The concurrency numbers Phase 5 exists to produce. Two views:
//
// - get_latency_under_load: cost of one get on the measured thread while
//   background threads hold the same locks. Uncontended get is the baseline;
//   lock choice (RwLock vs DashMap vs ...) only shows up here and below.
// - get_throughput_scaling: aggregate gets/sec across N threads. Near-linear
//   scaling means RwLock<HashMap> is fine; a plateau or dip is the
//   with-numbers case for a sharded/lock-free keydir.
fn bench_get_concurrent(c: &mut Criterion) {
    let dir = TempDir::new().unwrap();
    let store = Bitcask::open(Config {
        file_size_threshold: 64 << 10,
        compaction_threshold: u64::MAX,
        ..Config::new(dir.path(), StoreType::Bitcask)
    })
    .unwrap();
    for i in 0..10_000 {
        store.set(&format!("key{i}"), VALUE).unwrap();
    }

    // keys pre-generated: the timed loops must not measure format!/allocation
    let keys: Arc<Vec<String>> = Arc::new((0..10_000).map(|i| format!("key{i}")).collect());

    let mut group = c.benchmark_group("get_latency_under_load");
    group.throughput(Throughput::Elements(1));
    // writer variant last: its appends grow the store for later samples
    for (name, background_readers, background_writer) in [
        ("solo", 0usize, false),
        ("3_readers", 3, false),
        ("7_readers", 7, false),
        ("3_readers_1_writer", 3, true),
    ] {
        group.bench_function(name, |b| {
            let stop = Arc::new(AtomicBool::new(false));
            let mut handles = Vec::new();
            for t in 0..background_readers {
                let store = store.clone();
                let stop = Arc::clone(&stop);
                let keys = Arc::clone(&keys);
                handles.push(thread::spawn(move || {
                    let mut i = t * 1_000;
                    while !stop.load(Ordering::Relaxed) {
                        i = (i + 7919) % 10_000;
                        black_box(store.get(&keys[i]).unwrap());
                    }
                }));
            }
            if background_writer {
                let store = store.clone();
                let stop = Arc::clone(&stop);
                let keys = Arc::clone(&keys);
                handles.push(thread::spawn(move || {
                    let mut i = 0usize;
                    while !stop.load(Ordering::Relaxed) {
                        i = (i + 1) % 10_000;
                        store.set(&keys[i], VALUE).unwrap();
                    }
                }));
            }

            let mut i = 0usize;
            b.iter(|| {
                i = (i + 7919) % 10_000;
                black_box(store.get(&keys[i]).unwrap())
            });

            stop.store(true, Ordering::Relaxed);
            for handle in handles {
                handle.join().unwrap();
            }
        });
    }
    group.finish();

    let mut group = c.benchmark_group("get_throughput_scaling");
    group.throughput(Throughput::Elements(1));
    group.sample_size(30);
    for n_threads in [1usize, 2, 4, 8] {
        group.bench_function(format!("{n_threads}_threads"), |b| {
            b.iter_custom(|iters| {
                let per_thread = iters.div_ceil(n_threads as u64);
                let total = per_thread * n_threads as u64;
                let barrier = Arc::new(Barrier::new(n_threads));

                let mut handles = Vec::new();
                for t in 1..n_threads {
                    let store = store.clone();
                    let barrier = Arc::clone(&barrier);
                    let keys = Arc::clone(&keys);
                    handles.push(thread::spawn(move || {
                        let mut i = t * 1_000;
                        barrier.wait();
                        for _ in 0..per_thread {
                            i = (i + 7919) % 10_000;
                            black_box(store.get(&keys[i]).unwrap());
                        }
                    }));
                }

                barrier.wait();
                let start = Instant::now();
                let mut i = 0usize;
                for _ in 0..per_thread {
                    i = (i + 7919) % 10_000;
                    black_box(store.get(&keys[i]).unwrap());
                }
                for handle in handles {
                    handle.join().unwrap();
                }
                // measured wall time covers `total` gets; scale to `iters`
                start.elapsed().mul_f64(iters as f64 / total as f64)
            })
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_set,
    bench_get,
    bench_open,
    bench_get_concurrent
);
criterion_main!(benches);
