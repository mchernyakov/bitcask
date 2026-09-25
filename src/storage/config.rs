use super::policy::DurabilityPolicy;
use crate::StoreType;
use std::path::PathBuf;

const COMPACTION_THRESHOLD: u64 = 1 << 20; // 1 MB
const FILE_SIZE_THRESHOLD: u64 = 1 << 20; // 1 MB
const FLUSH_THRESHOLD_MILLIS: u64 = 1000; // 1 second

pub struct Config {
    pub dir: PathBuf,
    pub durability_policy: DurabilityPolicy,
    pub file_size_threshold: u64,
    pub compaction_threshold: u64,
    pub flush_threshold_millis: u64,
    pub store_type: StoreType,
}

impl Config {
    pub fn new(dir: impl Into<PathBuf>, store_type: StoreType) -> Self {
        Config {
            dir: dir.into(),
            durability_policy: DurabilityPolicy::OsDecides,
            file_size_threshold: FILE_SIZE_THRESHOLD,
            compaction_threshold: COMPACTION_THRESHOLD,
            flush_threshold_millis: FLUSH_THRESHOLD_MILLIS,
            store_type,
        }
    }
}
