//! A simple key/value store.

pub use error::{KvsError, Result};
pub use kvstore::KvStore;
pub use bitcask::Bitcask;
pub use config::Config;
pub use policy::DurabilityPolicy;

mod error;
mod kvstore;
mod bitcask;
mod command;
mod config;
mod policy;
mod index_value;
mod bytes_util;
