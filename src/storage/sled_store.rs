use crate::storage::lock_file::LockFile;
use crate::{Config, DurabilityPolicy, KvStore, KvsError, Result, StoreType};
use std::sync::Arc;

#[derive(Clone)]
pub struct SledStore {
    db: sled::Db,
    sync_every_put: bool,
    lock_file: Arc<LockFile>,
}

impl From<sled::Error> for KvsError {
    fn from(e: sled::Error) -> Self {
        match e {
            sled::Error::Io(io) => KvsError::Io(io),
            sled::Error::Corruption { .. } => KvsError::Corruption,
            other => KvsError::InvalidData(other.to_string()),
        }
    }
}

impl KvStore for SledStore {
    fn open(config: Config) -> Result<Self> {
        std::fs::create_dir_all(&config.dir)?;
        let lock = LockFile::acquire(&config.dir, StoreType::Sled)?;

        let flush = match config.durability_policy {
            DurabilityPolicy::SyncOnInterval => Some(config.flush_threshold_millis),
            _ => None,
        };
        let db = sled::Config::new()
            .path(&config.dir)
            .flush_every_ms(flush)
            .mode(sled::Mode::HighThroughput)
            .open()?;
        Ok(Self {
            db,
            sync_every_put: matches!(config.durability_policy, DurabilityPolicy::SyncOnEveryPut),
            lock_file: Arc::new(lock),
        })
    }

    fn set(&self, key: &str, value: &str) -> Result<()> {
        self.db.insert(key, value)?;
        if self.sync_every_put {
            self.db.flush()?;
        }
        Ok(())
    }

    fn remove(&self, key: &str) -> Result<()> {
        match self.db.remove(key)? {
            Some(_) => {
                if self.sync_every_put {
                    self.db.flush()?;
                }
                Ok(())
            }
            None => Err(KvsError::KeyNotFound),
        }
    }

    fn get(&self, key: &str) -> Result<Option<String>> {
        self.db
            .get(key)?
            .map(|v| String::from_utf8(v.to_vec()).map_err(Into::into))
            .transpose()
    }
}
