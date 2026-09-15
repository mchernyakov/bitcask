use crate::config::Config;

pub trait KvStore {
    fn open(config: Config) -> crate::Result<Self>
    where
        Self: Sized;
    fn set(&mut self, key: &str, value: &str) -> crate::Result<()>;
    fn remove(&mut self, key: &str) -> crate::Result<()>;
    fn get(&mut self, key: &str) -> crate::Result<Option<String>>;
}
