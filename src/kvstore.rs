use crate::config::Config;

pub trait KvStore: Sized + Clone + Send + 'static {
    fn open(config: Config) -> crate::Result<Self>
    where
        Self: Sized;
    fn set(&self, key: &str, value: &str) -> crate::Result<()>;
    fn remove(&self, key: &str) -> crate::Result<()>;
    fn get(&self, key: &str) -> crate::Result<Option<String>>;
}
