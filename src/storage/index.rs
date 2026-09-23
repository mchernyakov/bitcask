use super::index_value::IndexValue;
use crate::{KvsError, Result};
use log::trace;
use std::collections::HashMap;
use std::sync::RwLock;

pub const INDEX_BUCKETS_NUM: usize = 16;

pub struct Index {
    // for 128 buckets -> use Box
    index: [RwLock<HashMap<Vec<u8>, IndexValue>>; INDEX_BUCKETS_NUM],
}

impl Index {
    pub fn from(arr: [HashMap<Vec<u8>, IndexValue>; INDEX_BUCKETS_NUM]) -> Self {
        Index {
            index: arr.map(RwLock::new),
        }
    }

    pub fn get_bucket(key: &[u8]) -> usize {
        // FNV-1a hashing algorithm
        const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

        let mut hash = FNV_OFFSET;
        for &byte in key {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        ((hash >> 32) ^ hash) as usize % INDEX_BUCKETS_NUM
    }

    pub fn write_access(
        &self,
        key: &[u8],
    ) -> Result<std::sync::RwLockWriteGuard<'_, HashMap<Vec<u8>, IndexValue>>> {
        trace!("acquiring lock; struct {}, type {}", "index", "WRITE");
        let bucket = Self::get_bucket(key);
        self.index[bucket]
            .write()
            .map_err(|_| KvsError::LockPoisoned)
    }

    pub fn read_access(
        &self,
        key: &[u8],
    ) -> Result<std::sync::RwLockReadGuard<'_, HashMap<Vec<u8>, IndexValue>>> {
        trace!("acquiring lock; struct {}, type {}", "index", "READ");
        let bucket = Self::get_bucket(key);
        self.index[bucket]
            .read()
            .map_err(|_| KvsError::LockPoisoned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_bucket_is_deterministic_and_in_range() {
        for i in 0..1_000 {
            let key = format!("key{i}");
            let bucket = Index::get_bucket(key.as_bytes());
            assert!(bucket < INDEX_BUCKETS_NUM);
            assert_eq!(bucket, Index::get_bucket(key.as_bytes()));
        }
        assert!(Index::get_bucket(b"") < INDEX_BUCKETS_NUM);
    }

    #[test]
    fn buckets_are_reasonably_uniform_for_typical_key_patterns() {
        for pattern in ["key", "filler", "user:profile:", ""] {
            let mut counts = [0usize; INDEX_BUCKETS_NUM];
            let total = 10_000;
            for i in 0..total {
                let key = format!("{pattern}{i}");
                counts[Index::get_bucket(key.as_bytes())] += 1;
            }

            let mean = total / INDEX_BUCKETS_NUM;
            for (bucket, &count) in counts.iter().enumerate() {
                assert!(
                    count >= mean / 2 && count <= mean * 2,
                    "pattern {pattern:?}: bucket {bucket} got {count} of {total} keys (mean {mean})"
                );
            }
        }
    }
}
