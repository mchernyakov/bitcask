use crate::storage::SledStore;
use crate::storage::bitcask::Bitcask;
use crate::{Config, Result};
use clap::ValueEnum;

pub trait KvStore: Sized + Clone + Send + 'static {
    fn open(config: Config) -> crate::Result<Self>
    where
        Self: Sized;
    fn set(&self, key: &str, value: &str) -> crate::Result<()>;
    fn remove(&self, key: &str) -> crate::Result<()>;
    fn get(&self, key: &str) -> crate::Result<Option<String>>;
}

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
#[value(rename_all = "kebab-case")]
pub enum StoreType {
    Bitcask,
    Sled,
}

impl StoreType {
    pub fn as_str(&self) -> &'static str {
        match self {
            StoreType::Bitcask => "bitcask",
            StoreType::Sled => "sled",
        }
    }
}

#[derive(Clone)]
pub enum Store {
    Bitcask(Bitcask),
    Sled(SledStore),
}

impl KvStore for Store {
    fn open(config: Config) -> Result<Self> {
        Ok(match config.store_type {
            StoreType::Bitcask => Store::Bitcask(Bitcask::open(config)?),
            StoreType::Sled => Store::Sled(SledStore::open(config)?),
        })
    }

    fn set(&self, key: &str, value: &str) -> Result<()> {
        match self {
            Store::Bitcask(s) => s.set(key, value),
            Store::Sled(s) => s.set(key, value),
        }
    }

    fn remove(&self, key: &str) -> Result<()> {
        match self {
            Store::Bitcask(s) => s.remove(key),
            Store::Sled(s) => s.remove(key),
        }
    }

    fn get(&self, key: &str) -> Result<Option<String>> {
        match self {
            Store::Bitcask(s) => s.get(key),
            Store::Sled(s) => s.get(key),
        }
    }
}
