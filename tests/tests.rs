//! Adapted from the PingCAP Talent Plan project-2 test suite
//! (courses/rust/projects/project-2/tests/tests.rs), extended with our own
//! tests for durability policies, rotation, compaction, and hint files.
//!
//! Adaptations to this implementation:
//! - The store is `Bitcask` (implementing the `KvStore` trait), opened with a
//!   `Config`, and the API takes `&str` instead of `String`.
//! - The original CLI tests were dropped: the `kvs` binary is an interactive
//!   REPL, not the batch CLI the upstream suite drives.

use kvs::{Bitcask, Config, DurabilityPolicy, KvStore, Result};
use tempfile::TempDir;
use walkdir::WalkDir;

#[test_log::test]
fn set_then_get_with_sync_on_every_put() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let store = Bitcask::open(Config {
        durability_policy: DurabilityPolicy::SyncOnEveryPut,
        ..Config::new(temp_dir.path())
    })?;

    store.set("key1", "value1")?;
    assert_eq!(store.get("key1")?, Some("value1".to_owned()));

    drop(store);
    let store = Bitcask::open(Config {
        durability_policy: DurabilityPolicy::SyncOnEveryPut,
        ..Config::new(temp_dir.path())
    })?;
    assert_eq!(store.get("key1")?, Some("value1".to_owned()));

    Ok(())
}

#[test_log::test]
fn set_then_get_with_sync_on_interval() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let store = Bitcask::open(Config {
        durability_policy: DurabilityPolicy::SyncOnInterval,
        ..Config::new(temp_dir.path())
    })?;

    store.set("key1", "value1")?;
    assert_eq!(store.get("key1")?, Some("value1".to_owned()));

    // A second put lands within the sync interval, so no flush is due yet;
    // the value must still be readable through the store.
    store.set("key2", "value2")?;
    assert_eq!(store.get("key2")?, Some("value2".to_owned()));

    Ok(())
}

// Should get previously stored value.
#[test_log::test]
fn get_stored_value() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let store = Bitcask::open(Config::new(temp_dir.path()))?;

    store.set("key1", "value1")?;
    store.set("key2", "value2")?;

    assert_eq!(store.get("key1")?, Some("value1".to_owned()));
    assert_eq!(store.get("key2")?, Some("value2".to_owned()));

    // Open from disk again and check persistent data.
    drop(store);
    let store = Bitcask::open(Config::new(temp_dir.path()))?;
    assert_eq!(store.get("key1")?, Some("value1".to_owned()));
    assert_eq!(store.get("key2")?, Some("value2".to_owned()));

    Ok(())
}

// Should overwrite existent value.
#[test_log::test]
fn overwrite_value() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let store = Bitcask::open(Config::new(temp_dir.path()))?;

    store.set("key1", "value1")?;
    assert_eq!(store.get("key1")?, Some("value1".to_owned()));
    store.set("key1", "value2")?;
    assert_eq!(store.get("key1")?, Some("value2".to_owned()));

    // Open from disk again and check persistent data.
    drop(store);
    let store = Bitcask::open(Config::new(temp_dir.path()))?;
    assert_eq!(store.get("key1")?, Some("value2".to_owned()));
    store.set("key1", "value3")?;
    assert_eq!(store.get("key1")?, Some("value3".to_owned()));

    Ok(())
}

// Should get `None` when getting a non-existent key.
#[test_log::test]
fn get_non_existent_value() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let store = Bitcask::open(Config::new(temp_dir.path()))?;

    store.set("key1", "value1")?;
    assert_eq!(store.get("key2")?, None);

    // Open from disk again and check persistent data.
    drop(store);
    let store = Bitcask::open(Config::new(temp_dir.path()))?;
    assert_eq!(store.get("key2")?, None);

    Ok(())
}

#[test_log::test]
fn remove_non_existent_key() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let store = Bitcask::open(Config::new(temp_dir.path()))?;
    assert!(store.remove("key1").is_err());
    Ok(())
}

#[test_log::test]
fn remove_key() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let store = Bitcask::open(Config::new(temp_dir.path()))?;
    store.set("key1", "value1")?;
    assert!(store.remove("key1").is_ok());
    assert_eq!(store.get("key1")?, None);
    Ok(())
}

#[test_log::test]
fn removed_key_stays_removed_after_reopen() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let store = Bitcask::open(Config::new(temp_dir.path()))?;

    store.set("key1", "value1")?;
    store.set("key2", "value2")?;
    store.remove("key1")?;

    drop(store);
    let store = Bitcask::open(Config::new(temp_dir.path()))?;
    assert_eq!(store.get("key1")?, None);
    assert_eq!(store.get("key2")?, Some("value2".to_owned()));

    Ok(())
}

// A removed key whose Set record still sits in a sealed file must not be
// resurrected by the merge dropping its tombstone.
#[test_log::test]
fn remove_then_compaction_does_not_resurrect() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let config = || Config {
        file_size_threshold: 2048,
        compaction_threshold: 2048,
        ..Config::new(temp_dir.path())
    };
    let store = Bitcask::open(config())?;

    store.set("victim", "resurrect-me-not")?;

    let filler = "v".repeat(32);
    for i in 0..100 {
        store.set(&format!("filler{}", i), &filler)?;
    }
    store.remove("victim")?;
    for i in 0..100 {
        store.set(&format!("filler{}", i), &filler)?;
    }

    assert_eq!(store.get("victim")?, None);

    drop(store);
    let store = Bitcask::open(config())?;
    assert_eq!(store.get("victim")?, None);
    assert_eq!(store.get("filler0")?, Some(filler.clone()));

    Ok(())
}

// A single record may legally exceed the rotation threshold.
#[test_log::test]
fn record_larger_than_file_threshold_roundtrips() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let config = || Config {
        file_size_threshold: 1024,
        ..Config::new(temp_dir.path())
    };
    let store = Bitcask::open(config())?;

    let big = "x".repeat(4096);
    store.set("big", &big)?;
    store.set("small", "y")?;
    assert_eq!(store.get("big")?, Some(big.clone()));
    assert_eq!(store.get("small")?, Some("y".to_owned()));

    drop(store);
    let store = Bitcask::open(config())?;
    assert_eq!(store.get("big")?, Some(big));
    assert_eq!(store.get("small")?, Some("y".to_owned()));

    Ok(())
}

#[test_log::test]
fn empty_key_and_empty_value_roundtrip() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let store = Bitcask::open(Config::new(temp_dir.path()))?;

    store.set("", "empty-key")?;
    store.set("k", "")?;
    assert_eq!(store.get("")?, Some("empty-key".to_owned()));
    assert_eq!(store.get("k")?, Some("".to_owned()));

    drop(store);
    let store = Bitcask::open(Config::new(temp_dir.path()))?;
    assert_eq!(store.get("")?, Some("empty-key".to_owned()));
    assert_eq!(store.get("k")?, Some("".to_owned()));
    store.remove("")?;
    assert_eq!(store.get("")?, None);

    Ok(())
}

// Random ops against an in-memory oracle, with thresholds small enough that
// rotation, compaction, and hint files all fire organically, and periodic
// reopens to exercise replay and hint loading. Deterministic seed.
#[test_log::test]
fn random_ops_match_in_memory_model() -> Result<()> {
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

    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let config = || Config {
        file_size_threshold: 2048,
        compaction_threshold: 2048,
        ..Config::new(temp_dir.path())
    };

    let mut store = Bitcask::open(config())?;
    let mut model: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut rng = Rng(0xDEAD_BEEF);

    for step in 0..3000 {
        let key = format!("key{}", rng.next() % 50);
        match rng.next() % 10 {
            0..=6 => {
                let value = format!("value{}", rng.next() % 1000);
                store.set(&key, &value)?;
                model.insert(key, value);
            }
            7..=8 => {
                let expected = model.remove(&key);
                let result = store.remove(&key);
                assert_eq!(
                    result.is_ok(),
                    expected.is_some(),
                    "step {step}: remove({key}) disagreed with the model"
                );
            }
            _ => {
                assert_eq!(
                    store.get(&key)?,
                    model.get(&key).cloned(),
                    "step {step}: get({key}) disagreed with the model"
                );
            }
        }

        if step % 1000 == 999 {
            drop(store);
            store = Bitcask::open(config())?;
            for (k, v) in &model {
                assert_eq!(
                    store.get(k)?.as_deref(),
                    Some(v.as_str()),
                    "after reopen at step {step}: lost {k}"
                );
            }
        }
    }

    Ok(())
}

// Writing more than one file's worth of data must roll over into new data
// files, and every value must survive a reopen from the multi-file state.
#[test_log::test]
fn rotation_splits_log_into_multiple_files() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let store = Bitcask::open(Config {
        file_size_threshold: 4096,
        ..Config::new(temp_dir.path())
    })?;

    let value = "v".repeat(64);
    for i in 0..200 {
        store.set(&format!("key{}", i), &value)?;
    }
    drop(store);

    let data_files = std::fs::read_dir(temp_dir.path())?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .map(|n| n.len() == "000001.data".len() && n.ends_with(".data"))
                .unwrap_or(false)
        })
        .count();
    assert!(
        data_files >= 2,
        "expected rotation to create multiple .data files, found {data_files}"
    );

    let store = Bitcask::open(Config::new(temp_dir.path()))?;
    for i in 0..200 {
        let key = format!("key{}", i);
        assert_eq!(
            store.get(&key)?,
            Some(value.clone()),
            "lost {key} after reopen"
        );
    }

    Ok(())
}

// A key merged into a compaction output (id above the active file) and then
// overwritten in the still-active file must keep the NEW value across reopen:
// replay is later-id-wins, so the active writer has to rotate above the
// compaction outputs before accepting further writes.
#[test_log::test]
fn overwrite_after_compaction_survives_reopen() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let store = Bitcask::open(Config {
        file_size_threshold: 2048,
        compaction_threshold: 2048,
        ..Config::new(temp_dir.path())
    })?;

    store.set("k1", "old")?;

    // First pass seals k1 into an old file; second pass makes the first pass
    // stale and pushes past the compaction threshold, so k1 (still live, still
    // in the old file) gets merged into a compaction output.
    let filler = "v".repeat(32);
    for i in 0..100 {
        store.set(&format!("filler{}", i), &filler)?;
    }
    for i in 0..100 {
        store.set(&format!("filler{}", i), &filler)?;
    }

    store.set("k1", "new")?;
    assert_eq!(store.get("k1")?, Some("new".to_owned()));

    drop(store);
    let store = Bitcask::open(Config::new(temp_dir.path()))?;
    assert_eq!(store.get("k1")?, Some("new".to_owned()));
    assert_eq!(store.get("filler0")?, Some(filler.clone()));

    Ok(())
}

// Every compaction output gets a sibling hint file (000004.data -> 000004.hint)
// describing its records, and no hint file may outlive its data file.
#[test_log::test]
fn merge_writes_hint_files_next_to_outputs() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let store = Bitcask::open(Config {
        file_size_threshold: 2048,
        compaction_threshold: 2048,
        ..Config::new(temp_dir.path())
    })?;

    let filler = "v".repeat(32);
    for i in 0..100 {
        store.set(&format!("filler{}", i), &filler)?;
    }
    for i in 0..100 {
        store.set(&format!("filler{}", i), &filler)?;
    }
    drop(store);

    let mut hint_files = 0;
    let mut non_empty_hint_files = 0;
    for entry in std::fs::read_dir(temp_dir.path())? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(stem) = name.strip_suffix(".hint") {
            hint_files += 1;
            if entry.metadata()?.len() > 0 {
                non_empty_hint_files += 1;
            }
            let data_sibling = temp_dir.path().join(format!("{stem}.data"));
            assert!(
                data_sibling.exists(),
                "hint file {name} has no data file sibling"
            );
        }
    }
    assert!(hint_files >= 1, "merge should write at least one hint file");
    assert!(
        non_empty_hint_files >= 1,
        "at least one hint file should describe live records"
    );

    Ok(())
}

// Hint files are a startup optimization: damaged or orphaned ones must never
// brick open() or corrupt the recovered data.
#[test_log::test]
fn open_survives_corrupt_and_orphan_hint_files() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");

    let config = || Config {
        file_size_threshold: 2048,
        compaction_threshold: 2048,
        ..Config::new(temp_dir.path())
    };

    let store = Bitcask::open(config())?;
    let filler = "v".repeat(32);
    for i in 0..100 {
        store.set(&format!("filler{}", i), &filler)?;
    }
    for i in 0..100 {
        store.set(&format!("filler{}", i), &filler)?;
    }
    drop(store);

    // an orphan hint with no data file (e.g. crash between merge steps)
    std::fs::write(temp_dir.path().join("000099.hint"), b"orphan")?;
    // a foreign file that merely looks hint-like
    std::fs::write(temp_dir.path().join("abc.hint"), b"not ours")?;

    // every real hint file gets its content replaced with garbage
    for entry in std::fs::read_dir(temp_dir.path())? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".hint") && name != "000099.hint" {
            std::fs::write(entry.path(), b"this is not a hint record")?;
        }
    }

    let store = Bitcask::open(config())?;
    for i in 0..100 {
        let key = format!("filler{}", i);
        assert_eq!(store.get(&key)?, Some(filler.clone()), "lost {key}");
    }

    assert!(
        !temp_dir.path().join("000099.hint").exists(),
        "orphan hint with no data file should be removed on open"
    );
    assert!(
        temp_dir.path().join("abc.hint").exists(),
        "foreign .hint files must be kept"
    );

    Ok(())
}

// Loading the keydir from hints and rebuilding it by replay must agree, and
// a successful hint load must not consume the hint files.
#[test_log::test]
fn reopen_from_hints_matches_replay_from_data() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let config = || Config {
        file_size_threshold: 2048,
        compaction_threshold: 2048,
        ..Config::new(temp_dir.path())
    };

    let store = Bitcask::open(config())?;
    let filler = "v".repeat(32);
    for i in 0..100 {
        store.set(&format!("filler{}", i), &filler)?;
    }
    for i in 0..100 {
        store.set(&format!("filler{}", i), &filler)?;
    }
    drop(store);

    let hint_paths: Vec<std::path::PathBuf> = std::fs::read_dir(temp_dir.path())?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "hint"))
        .collect();
    assert!(!hint_paths.is_empty(), "merge should have written hints");

    // reopen #1: keydir comes from the hints
    let store = Bitcask::open(config())?;
    for i in 0..100 {
        let key = format!("filler{}", i);
        assert_eq!(
            store.get(&key)?,
            Some(filler.clone()),
            "hint path lost {key}"
        );
    }
    drop(store);

    for path in &hint_paths {
        assert!(
            path.exists(),
            "a successful hint load must not delete {path:?}"
        );
        std::fs::remove_file(path)?;
    }

    // reopen #2: keydir must come out identical from data replay alone
    let store = Bitcask::open(config())?;
    for i in 0..100 {
        let key = format!("filler{}", i);
        assert_eq!(
            store.get(&key)?,
            Some(filler.clone()),
            "replay path lost {key}"
        );
    }

    Ok(())
}

// Phase 5: one writer, many readers, constant merges. Reader threads hammer
// get() over a small key space while the writer overwrites it with thresholds
// tiny enough that rotation and compaction fire constantly. A reader must only
// ever see a well-formed value for the key it asked for (the per-record CRC
// turns any torn read into an error, so a clean pass means no torn reads
// either); a deadlock shows up as the test hanging. Rough ops/sec numbers are
// printed as a baseline — run with --nocapture to see them; real benchmarks
// are Phase 6 / criterion.
#[test_log::test]
fn concurrent_readers_with_one_writer_under_constant_merges() -> Result<()> {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Instant;

    const READERS: u64 = 4;
    const KEYS: u64 = 16;
    // 5k keeps the default run fast; crank it via STRESS_WRITES for a soak run
    let writes: u64 = std::env::var("STRESS_WRITES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5_000);

    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let store = Bitcask::open(Config {
        file_size_threshold: 2048,
        compaction_threshold: 2048,
        ..Config::new(temp_dir.path())
    })?;

    let stop = Arc::new(AtomicBool::new(false));
    let read_ops = Arc::new(AtomicU64::new(0));
    let started = Instant::now();

    // Values are "key<k>:<generation>" with generations strictly increasing
    // per key, so a reader can check two things without knowing what's
    // current: the value belongs to the key it asked for, and the generation
    // it observes never goes backwards (a backwards jump means a stale index
    // entry resurfaced an old record, e.g. during a merge).
    let readers: Vec<_> = (0..READERS)
        .map(|_| {
            let store = store.clone();
            let stop = Arc::clone(&stop);
            let read_ops = Arc::clone(&read_ops);
            thread::spawn(move || {
                let mut last_gen: HashMap<u64, u64> = HashMap::new();
                let mut k = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    k = (k + 1) % KEYS;
                    let key = format!("key{k}");
                    match store.get(&key) {
                        Ok(Some(value)) => {
                            let generation: u64 = value
                                .strip_prefix(&format!("{key}:"))
                                .and_then(|g| g.parse().ok())
                                .unwrap_or_else(|| {
                                    panic!("get({key}) returned a foreign value: {value}")
                                });
                            let last = last_gen.entry(k).or_insert(generation);
                            assert!(
                                generation >= *last,
                                "get({key}) went backwards: generation {generation} after {last}"
                            );
                            *last = generation;
                        }
                        Ok(None) => {} // not written yet, or removed
                        Err(err) => panic!("get({key}) failed: {err}"),
                    }
                    read_ops.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();

    // The single writer (this thread), with an oracle for the final check.
    let mut live: HashMap<String, String> = HashMap::new();
    for i in 0..writes {
        let key = format!("key{}", i % KEYS);
        if i % 17 == 16 && live.contains_key(&key) {
            store.remove(&key)?;
            live.remove(&key);
        } else {
            let value = format!("{key}:{i}");
            store.set(&key, &value)?;
            live.insert(key, value);
        }
    }

    stop.store(true, Ordering::Relaxed);
    for handle in readers {
        handle.join().expect("reader thread panicked");
    }

    let elapsed = started.elapsed();
    let reads = read_ops.load(Ordering::Relaxed);
    assert!(reads > 0, "readers never got to run");
    eprintln!(
        "stress: {writes} writes, {reads} reads across {READERS} readers in {elapsed:.2?} \
         ({:.0} writes/s, {:.0} reads/s)",
        writes as f64 / elapsed.as_secs_f64(),
        reads as f64 / elapsed.as_secs_f64(),
    );

    // Final state must match the oracle, both live and after a reopen.
    for (key, value) in &live {
        assert_eq!(store.get(key)?.as_deref(), Some(value.as_str()));
    }
    drop(store);
    let store = Bitcask::open(Config::new(temp_dir.path()))?;
    for k in 0..KEYS {
        let key = format!("key{k}");
        assert_eq!(
            store.get(&key)?,
            live.get(&key).cloned(),
            "after reopen: {key}"
        );
    }

    Ok(())
}

// The trait contract (Clone + Send + &self) allows several threads to write
// through their own handles; the writer mutex must serialize them without
// losing a single write, across rotations and merges triggered from any
// thread. Two passes per writer so the first pass goes stale and merges fire.
#[test_log::test]
fn concurrent_writers_do_not_lose_writes() -> Result<()> {
    use std::thread;

    const WRITERS: u64 = 4;
    const KEYS_PER_WRITER: u64 = 300;

    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let store = Bitcask::open(Config {
        file_size_threshold: 2048,
        compaction_threshold: 2048,
        ..Config::new(temp_dir.path())
    })?;

    let handles: Vec<_> = (0..WRITERS)
        .map(|w| {
            let store = store.clone();
            thread::spawn(move || -> Result<()> {
                for pass in 0..2 {
                    for i in 0..KEYS_PER_WRITER {
                        store.set(&format!("w{w}k{i}"), &format!("w{w}v{i}p{pass}"))?;
                    }
                }
                Ok(())
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("writer thread panicked")?;
    }

    let check = |store: &Bitcask| -> Result<()> {
        for w in 0..WRITERS {
            for i in 0..KEYS_PER_WRITER {
                assert_eq!(
                    store.get(&format!("w{w}k{i}"))?,
                    Some(format!("w{w}v{i}p1")),
                    "lost w{w}k{i}"
                );
            }
        }
        Ok(())
    };
    check(&store)?;

    drop(store);
    let store = Bitcask::open(Config::new(temp_dir.path()))?;
    check(&store)?;

    Ok(())
}

// Handles are cheap clones over shared state: writes through one handle are
// visible through another, and dropping a handle (which runs the Drop sync)
// must not disturb the survivors.
#[test_log::test]
fn clone_shares_state_and_dropping_one_handle_is_harmless() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let store = Bitcask::open(Config::new(temp_dir.path()))?;
    let clone = store.clone();

    store.set("k1", "v1")?;
    assert_eq!(clone.get("k1")?, Some("v1".to_owned()));

    drop(store);
    clone.set("k2", "v2")?;
    assert_eq!(clone.get("k1")?, Some("v1".to_owned()));
    assert_eq!(clone.get("k2")?, Some("v2".to_owned()));

    drop(clone);
    let store = Bitcask::open(Config::new(temp_dir.path()))?;
    assert_eq!(store.get("k1")?, Some("v1".to_owned()));
    assert_eq!(store.get("k2")?, Some("v2".to_owned()));

    Ok(())
}

// open() cleans up leftover .data.compact temp files from a crashed merge,
// but must not touch any other file living in the directory.
#[test_log::test]
fn open_removes_compact_leftovers_but_keeps_foreign_files() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let foreign = temp_dir.path().join("notes.txt");
    std::fs::write(&foreign, "do not delete")?;
    let leftover = temp_dir.path().join("000007.data.compact");
    std::fs::write(&leftover, "half-written merge output")?;

    let store = Bitcask::open(Config::new(temp_dir.path()))?;
    store.set("k", "v")?;
    assert_eq!(store.get("k")?, Some("v".to_owned()));
    drop(store);

    assert!(foreign.exists(), "open() must not delete unrelated files");
    assert!(
        !leftover.exists(),
        "open() should clean up .data.compact leftovers"
    );

    Ok(())
}

// Insert data until total size of the directory decreases.
// Test data correctness after compaction.
#[test_log::test]
fn compaction() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let store = Bitcask::open(Config::new(temp_dir.path()))?;

    let dir_size = || {
        let entries = WalkDir::new(temp_dir.path()).into_iter();
        let len: walkdir::Result<u64> = entries
            .map(|res| {
                res.and_then(|entry| entry.metadata())
                    .map(|metadata| metadata.len())
            })
            .sum();
        len.expect("fail to get directory size")
    };

    let mut current_size = dir_size();
    for iter in 0..1000 {
        for key_id in 0..1000 {
            let key = format!("key{}", key_id);
            let value = format!("{}", iter);
            store.set(&key, &value)?;
        }

        let new_size = dir_size();
        if new_size > current_size {
            current_size = new_size;
            continue;
        }
        // Compaction triggered.

        drop(store);
        // reopen and check content.
        let store = Bitcask::open(Config::new(temp_dir.path()))?;
        for key_id in 0..1000 {
            let key = format!("key{}", key_id);
            assert_eq!(store.get(&key)?, Some(format!("{}", iter)));
        }
        return Ok(());
    }

    panic!("No compaction detected");
}
