//! Adapted from the PingCAP Talent Plan project-2 test suite
//! (courses/rust/projects/project-2/tests/tests.rs).
//!
//! Adaptations to this implementation:
//! - The store is `Bitcask` (implementing the `KvStore` trait) instead of a
//!   `KvStore` struct, and the API takes `&str` instead of `String`.
//! - The CLI tests expect a batch CLI (`kvs get <KEY>` etc.), but the current
//!   `kvs` binary is an interactive REPL, so they are `#[ignore]`d until a
//!   batch mode exists. Run them with `cargo test -- --ignored`.
//! - The `compaction` test is expected to fail until compaction is implemented.

use kvs::{Bitcask, Config, DurabilityPolicy, KvStore, Result};
use tempfile::TempDir;
use walkdir::WalkDir;

#[test]
fn set_then_get_with_sync_on_every_put() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let mut store = Bitcask::open(Config {
        durability_policy: DurabilityPolicy::SyncOnEveryPut,
        ..Config::new(temp_dir.path())
    })?;

    store.set("key1", "value1")?;
    assert_eq!(store.get("key1")?, Some("value1".to_owned()));

    drop(store);
    let mut store = Bitcask::open(Config {
        durability_policy: DurabilityPolicy::SyncOnEveryPut,
        ..Config::new(temp_dir.path())
    })?;
    assert_eq!(store.get("key1")?, Some("value1".to_owned()));

    Ok(())
}

#[test]
fn set_then_get_with_sync_on_interval() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let mut store = Bitcask::open(Config {
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
#[test]
fn get_stored_value() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let mut store = Bitcask::open(Config::new(temp_dir.path()))?;

    store.set("key1", "value1")?;
    store.set("key2", "value2")?;

    assert_eq!(store.get("key1")?, Some("value1".to_owned()));
    assert_eq!(store.get("key2")?, Some("value2".to_owned()));

    // Open from disk again and check persistent data.
    drop(store);
    let mut store = Bitcask::open(Config::new(temp_dir.path()))?;
    assert_eq!(store.get("key1")?, Some("value1".to_owned()));
    assert_eq!(store.get("key2")?, Some("value2".to_owned()));

    Ok(())
}

// Should overwrite existent value.
#[test]
fn overwrite_value() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let mut store = Bitcask::open(Config::new(temp_dir.path()))?;

    store.set("key1", "value1")?;
    assert_eq!(store.get("key1")?, Some("value1".to_owned()));
    store.set("key1", "value2")?;
    assert_eq!(store.get("key1")?, Some("value2".to_owned()));

    // Open from disk again and check persistent data.
    drop(store);
    let mut store = Bitcask::open(Config::new(temp_dir.path()))?;
    assert_eq!(store.get("key1")?, Some("value2".to_owned()));
    store.set("key1", "value3")?;
    assert_eq!(store.get("key1")?, Some("value3".to_owned()));

    Ok(())
}

// Should get `None` when getting a non-existent key.
#[test]
fn get_non_existent_value() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let mut store = Bitcask::open(Config::new(temp_dir.path()))?;

    store.set("key1", "value1")?;
    assert_eq!(store.get("key2")?, None);

    // Open from disk again and check persistent data.
    drop(store);
    let mut store = Bitcask::open(Config::new(temp_dir.path()))?;
    assert_eq!(store.get("key2")?, None);

    Ok(())
}

#[test]
fn remove_non_existent_key() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let mut store = Bitcask::open(Config::new(temp_dir.path()))?;
    assert!(store.remove("key1").is_err());
    Ok(())
}

#[test]
fn remove_key() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let mut store = Bitcask::open(Config::new(temp_dir.path()))?;
    store.set("key1", "value1")?;
    assert!(store.remove("key1").is_ok());
    assert_eq!(store.get("key1")?, None);
    Ok(())
}

// Writing more than one file's worth of data must roll over into new data
// files, and every value must survive a reopen from the multi-file state.
#[test]
fn rotation_splits_log_into_multiple_files() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let mut store = Bitcask::open(Config {
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

    let mut store = Bitcask::open(Config::new(temp_dir.path()))?;
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
#[test]
fn overwrite_after_compaction_survives_reopen() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let mut store = Bitcask::open(Config {
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
    let mut store = Bitcask::open(Config::new(temp_dir.path()))?;
    assert_eq!(store.get("k1")?, Some("new".to_owned()));
    assert_eq!(store.get("filler0")?, Some(filler.clone()));

    Ok(())
}

// Every compaction output gets a sibling hint file (000004.data -> 000004.hint)
// describing its records, and no hint file may outlive its data file.
#[test]
fn merge_writes_hint_files_next_to_outputs() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let mut store = Bitcask::open(Config {
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
#[test]
fn open_survives_corrupt_and_orphan_hint_files() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");

    let config = || Config {
        file_size_threshold: 2048,
        compaction_threshold: 2048,
        ..Config::new(temp_dir.path())
    };

    let mut store = Bitcask::open(config())?;
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

    let mut store = Bitcask::open(config())?;
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
#[test]
fn reopen_from_hints_matches_replay_from_data() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let config = || Config {
        file_size_threshold: 2048,
        compaction_threshold: 2048,
        ..Config::new(temp_dir.path())
    };

    let mut store = Bitcask::open(config())?;
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
    let mut store = Bitcask::open(config())?;
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
    let mut store = Bitcask::open(config())?;
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

// open() cleans up leftover .data.compact temp files from a crashed merge,
// but must not touch any other file living in the directory.
#[test]
fn open_removes_compact_leftovers_but_keeps_foreign_files() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let foreign = temp_dir.path().join("notes.txt");
    std::fs::write(&foreign, "do not delete")?;
    let leftover = temp_dir.path().join("000007.data.compact");
    std::fs::write(&leftover, "half-written merge output")?;

    let mut store = Bitcask::open(Config::new(temp_dir.path()))?;
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
#[test]
fn compaction() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let mut store = Bitcask::open(Config::new(temp_dir.path()))?;

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
        let mut store = Bitcask::open(Config::new(temp_dir.path()))?;
        for key_id in 0..1000 {
            let key = format!("key{}", key_id);
            assert_eq!(store.get(&key)?, Some(format!("{}", iter)));
        }
        return Ok(());
    }

    panic!("No compaction detected");
}
