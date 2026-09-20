use crate::{KvsError, Result};
use std::fs::{OpenOptions, TryLockError};
use std::path::Path;

pub struct LockFile {
    file: std::fs::File,
}

impl LockFile {
    pub fn acquire(path: impl AsRef<Path>) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)?;

        match file.try_lock() {
            Ok(()) => Ok(Self { file }),
            Err(TryLockError::WouldBlock) => Err(KvsError::StoreLocked),
            Err(TryLockError::Error(err)) => Err(err.into()),
        }
    }
}
