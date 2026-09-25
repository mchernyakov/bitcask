use crate::{KvsError, Result, StoreType};
use std::fs::{OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::path::Path;

pub const LOCK_FILE_NAME: &str = "kvs.lock";

pub struct LockFile {
    #[allow(dead_code)]
    file: std::fs::File,
}

impl LockFile {
    pub fn acquire(path: impl AsRef<Path>, engine: StoreType) -> Result<Self> {
        let lock_file = path.as_ref().join(LOCK_FILE_NAME);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_file)?;

        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(KvsError::StoreLocked),
            Err(TryLockError::Error(err)) => return Err(err.into()),
        }

        let mut marker = String::new();
        file.read_to_string(&mut marker)?;
        let marker = marker.trim();
        if marker.is_empty() {
            file.write_all(engine.as_str().as_bytes())?;
            file.sync_all()?;
        } else if marker != engine.as_str() {
            return Err(KvsError::EngineMismatch {
                found: marker.to_owned(),
                requested: engine.as_str().to_owned(),
            });
        }

        Ok(Self { file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn fresh_lock_file_is_stamped_and_reopens_for_same_engine() -> Result<()> {
        let dir = tempdir()?;
        let lock = LockFile::acquire(dir.path(), StoreType::Sled)?;
        assert_eq!(
            std::fs::read_to_string(dir.path().join(LOCK_FILE_NAME))?,
            "sled"
        );
        drop(lock);
        LockFile::acquire(dir.path(), StoreType::Sled)?;
        Ok(())
    }

    #[test]
    fn other_engine_is_refused() -> Result<()> {
        let dir = tempdir()?;
        drop(LockFile::acquire(dir.path(), StoreType::Bitcask)?);
        assert!(matches!(
            LockFile::acquire(dir.path(), StoreType::Sled),
            Err(KvsError::EngineMismatch { found, requested })
                if found == "bitcask" && requested == "sled"
        ));
        Ok(())
    }

    #[test]
    fn garbage_marker_is_refused_not_overwritten() -> Result<()> {
        let dir = tempdir()?;
        std::fs::write(dir.path().join(LOCK_FILE_NAME), "rocksdb\n")?;
        assert!(matches!(
            LockFile::acquire(dir.path(), StoreType::Bitcask),
            Err(KvsError::EngineMismatch { found, .. }) if found == "rocksdb"
        ));
        assert_eq!(
            std::fs::read_to_string(dir.path().join(LOCK_FILE_NAME))?,
            "rocksdb\n"
        );
        Ok(())
    }

    #[test]
    fn held_lock_blocks_second_acquire() -> Result<()> {
        let dir = tempdir()?;
        let _held = LockFile::acquire(dir.path(), StoreType::Bitcask)?;
        assert!(matches!(
            LockFile::acquire(dir.path(), StoreType::Bitcask),
            Err(KvsError::StoreLocked)
        ));
        Ok(())
    }
}
