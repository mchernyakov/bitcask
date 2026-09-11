use std::path::PathBuf;
use crate::policy::DurabilityPolicy;

pub trait KvStore {
    fn open(path: impl Into<PathBuf>, durability_policy: DurabilityPolicy) -> crate::Result<Self>
    where
        Self: Sized;
    fn set(&mut self, key: &str, value: &str) -> crate::Result<()>;
    fn remove(&mut self, key: &str) -> crate::Result<()>;
    fn get(&mut self, key: &str) -> crate::Result<Option<String>>;
}