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

use assert_cmd::prelude::*;
use kvs::{Bitcask, DurabilityPolicy, KvStore, Result};
use predicates::ord::eq;
use predicates::str::{contains, is_empty, PredicateStrExt};
use std::process::Command;
use tempfile::TempDir;
use walkdir::WalkDir;

// `kvs` with no args should exit with a non-zero code.
#[test]
#[ignore = "requires batch CLI mode"]
fn cli_no_args() {
    Command::cargo_bin("kvs").unwrap().assert().failure();
}

// `kvs -V` should print the version.
#[test]
#[ignore = "requires batch CLI mode"]
fn cli_version() {
    Command::cargo_bin("kvs")
        .unwrap()
        .args(["-V"])
        .assert()
        .stdout(contains(env!("CARGO_PKG_VERSION")));
}

// `kvs get <KEY>` should print "Key not found" for a non-existent key and exit with zero.
#[test]
#[ignore = "requires batch CLI mode"]
fn cli_get_non_existent_key() {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    Command::cargo_bin("kvs")
        .unwrap()
        .args(["get", "key1"])
        .current_dir(&temp_dir)
        .assert()
        .success()
        .stdout(eq("Key not found").trim());
}

// `kvs rm <KEY>` should print "Key not found" for an empty database and exit with non-zero code.
#[test]
#[ignore = "requires batch CLI mode"]
fn cli_rm_non_existent_key() {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    Command::cargo_bin("kvs")
        .unwrap()
        .args(["rm", "key1"])
        .current_dir(&temp_dir)
        .assert()
        .failure()
        .stdout(eq("Key not found").trim());
}

// `kvs set <KEY> <VALUE>` should print nothing and exit with zero.
#[test]
#[ignore = "requires batch CLI mode"]
fn cli_set() {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    Command::cargo_bin("kvs")
        .unwrap()
        .args(["set", "key1", "value1"])
        .current_dir(&temp_dir)
        .assert()
        .success()
        .stdout(is_empty());
}

// `kvs get <KEY>` should print the stored value and exit with zero.
#[test]
#[ignore = "requires batch CLI mode"]
fn cli_get_stored() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");

    let mut store = Bitcask::open(temp_dir.path(), DurabilityPolicy::OsDecides)?;
    store.set("key1", "value1")?;
    store.set("key2", "value2")?;
    drop(store);

    Command::cargo_bin("kvs")
        .unwrap()
        .args(["get", "key1"])
        .current_dir(&temp_dir)
        .assert()
        .success()
        .stdout(eq("value1").trim());

    Command::cargo_bin("kvs")
        .unwrap()
        .args(["get", "key2"])
        .current_dir(&temp_dir)
        .assert()
        .success()
        .stdout(eq("value2").trim());

    Ok(())
}

// `kvs rm <KEY>` should print nothing and exit with zero.
#[test]
#[ignore = "requires batch CLI mode"]
fn cli_rm_stored() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");

    let mut store = Bitcask::open(temp_dir.path(), DurabilityPolicy::OsDecides)?;
    store.set("key1", "value1")?;
    drop(store);

    Command::cargo_bin("kvs")
        .unwrap()
        .args(["rm", "key1"])
        .current_dir(&temp_dir)
        .assert()
        .success()
        .stdout(is_empty());

    Command::cargo_bin("kvs")
        .unwrap()
        .args(["get", "key1"])
        .current_dir(&temp_dir)
        .assert()
        .success()
        .stdout(eq("Key not found").trim());

    Ok(())
}

#[test]
#[ignore = "requires batch CLI mode"]
fn cli_invalid_get() {
    Command::cargo_bin("kvs")
        .unwrap()
        .args(["get"])
        .assert()
        .failure();

    Command::cargo_bin("kvs")
        .unwrap()
        .args(["get", "extra", "field"])
        .assert()
        .failure();
}

#[test]
#[ignore = "requires batch CLI mode"]
fn cli_invalid_set() {
    Command::cargo_bin("kvs")
        .unwrap()
        .args(["set"])
        .assert()
        .failure();

    Command::cargo_bin("kvs")
        .unwrap()
        .args(["set", "missing_field"])
        .assert()
        .failure();

    Command::cargo_bin("kvs")
        .unwrap()
        .args(["set", "extra", "extra", "field"])
        .assert()
        .failure();
}

#[test]
#[ignore = "requires batch CLI mode"]
fn cli_invalid_rm() {
    Command::cargo_bin("kvs")
        .unwrap()
        .args(["rm"])
        .assert()
        .failure();

    Command::cargo_bin("kvs")
        .unwrap()
        .args(["rm", "extra", "field"])
        .assert()
        .failure();
}

#[test]
#[ignore = "requires batch CLI mode"]
fn cli_invalid_subcommand() {
    Command::cargo_bin("kvs")
        .unwrap()
        .args(["unknown", "subcommand"])
        .assert()
        .failure();
}

#[test]
fn set_then_get_with_sync_on_every_put() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let mut store = Bitcask::open(temp_dir.path(), DurabilityPolicy::SyncOnEveryPut)?;

    store.set("key1", "value1")?;
    assert_eq!(store.get("key1")?, Some("value1".to_owned()));

    drop(store);
    let mut store = Bitcask::open(temp_dir.path(), DurabilityPolicy::SyncOnEveryPut)?;
    assert_eq!(store.get("key1")?, Some("value1".to_owned()));

    Ok(())
}

#[test]
fn set_then_get_with_sync_on_interval() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let mut store = Bitcask::open(temp_dir.path(), DurabilityPolicy::SyncOnInterval)?;

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
    let mut store = Bitcask::open(temp_dir.path(), DurabilityPolicy::OsDecides)?;

    store.set("key1", "value1")?;
    store.set("key2", "value2")?;

    assert_eq!(store.get("key1")?, Some("value1".to_owned()));
    assert_eq!(store.get("key2")?, Some("value2".to_owned()));

    // Open from disk again and check persistent data.
    drop(store);
    let mut store = Bitcask::open(temp_dir.path(), DurabilityPolicy::OsDecides)?;
    assert_eq!(store.get("key1")?, Some("value1".to_owned()));
    assert_eq!(store.get("key2")?, Some("value2".to_owned()));

    Ok(())
}

// Should overwrite existent value.
#[test]
fn overwrite_value() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let mut store = Bitcask::open(temp_dir.path(), DurabilityPolicy::OsDecides)?;

    store.set("key1", "value1")?;
    assert_eq!(store.get("key1")?, Some("value1".to_owned()));
    store.set("key1", "value2")?;
    assert_eq!(store.get("key1")?, Some("value2".to_owned()));

    // Open from disk again and check persistent data.
    drop(store);
    let mut store = Bitcask::open(temp_dir.path(), DurabilityPolicy::OsDecides)?;
    assert_eq!(store.get("key1")?, Some("value2".to_owned()));
    store.set("key1", "value3")?;
    assert_eq!(store.get("key1")?, Some("value3".to_owned()));

    Ok(())
}

// Should get `None` when getting a non-existent key.
#[test]
fn get_non_existent_value() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let mut store = Bitcask::open(temp_dir.path(), DurabilityPolicy::OsDecides)?;

    store.set("key1", "value1")?;
    assert_eq!(store.get("key2")?, None);

    // Open from disk again and check persistent data.
    drop(store);
    let mut store = Bitcask::open(temp_dir.path(), DurabilityPolicy::OsDecides)?;
    assert_eq!(store.get("key2")?, None);

    Ok(())
}

#[test]
fn remove_non_existent_key() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let mut store = Bitcask::open(temp_dir.path(), DurabilityPolicy::OsDecides)?;
    assert!(store.remove("key1").is_err());
    Ok(())
}

#[test]
fn remove_key() -> Result<()> {
    let temp_dir = TempDir::new().expect("unable to create temporary working directory");
    let mut store = Bitcask::open(temp_dir.path(), DurabilityPolicy::OsDecides)?;
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
    let mut store = Bitcask::open(temp_dir.path(), DurabilityPolicy::OsDecides)?;

    let value = "v".repeat(1024);
    for i in 0..3000 {
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

    let mut store = Bitcask::open(temp_dir.path(), DurabilityPolicy::OsDecides)?;
    for i in (0..3000).step_by(299) {
        let key = format!("key{}", i);
        assert_eq!(store.get(&key)?, Some(value.clone()), "lost {key} after reopen");
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
    let mut store = Bitcask::open(temp_dir.path(), DurabilityPolicy::OsDecides)?;

    store.set("k1", "old")?;

    // First pass seals k1 into an old file; second pass makes the first pass
    // stale and pushes past the compaction threshold, so k1 (still live, still
    // in the old file) gets merged into a compaction output.
    let filler = "v".repeat(32);
    for i in 0..20000 {
        store.set(&format!("filler{}", i), &filler)?;
    }
    for i in 0..20000 {
        store.set(&format!("filler{}", i), &filler)?;
    }

    store.set("k1", "new")?;
    assert_eq!(store.get("k1")?, Some("new".to_owned()));

    drop(store);
    let mut store = Bitcask::open(temp_dir.path(), DurabilityPolicy::OsDecides)?;
    assert_eq!(store.get("k1")?, Some("new".to_owned()));
    assert_eq!(store.get("filler0")?, Some(filler.clone()));

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

    let mut store = Bitcask::open(temp_dir.path(), DurabilityPolicy::OsDecides)?;
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
    let mut store = Bitcask::open(temp_dir.path(), DurabilityPolicy::OsDecides)?;

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
        let mut store = Bitcask::open(temp_dir.path(), DurabilityPolicy::OsDecides)?;
        for key_id in 0..1000 {
            let key = format!("key{}", key_id);
            assert_eq!(store.get(&key)?, Some(format!("{}", iter)));
        }
        return Ok(());
    }

    panic!("No compaction detected");
}
