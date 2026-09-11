use crate::command::{Command, HEADER_LEN};
use crate::kvstore::KvStore;
use crate::policy::DurabilityPolicy;
use crate::{KvsError, Result};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const COMPACTION_THRESHOLD: u64 = 1024 * 1024;
const FLUSH_THRESHOLD_MILLIS: u64 = 1 * 1000; // 1 second

fn unix_now() -> Result<Duration> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?)
}

pub struct IndexValue {
    pub offset: u64,
    pub len: usize,
}

pub struct Bitcask {
    writer: BufWriter<File>,
    reader: File,
    in_mem_index: HashMap<Vec<u8>, IndexValue>,
    offset: u64,
    flushed_offset: u64,
    durability_policy: DurabilityPolicy,
    last_ts_flushed: u64,
}

impl Drop for Bitcask {
    fn drop(&mut self) {
        let _ = self.sync();
    }
}

impl Bitcask {
    fn sync(&mut self) -> Result<()> {
        self.writer.flush()?;
        self.writer.get_ref().sync_all()?;
        Ok(())
    }

    fn replay(&mut self) -> Result<()> {
        let file_len = self.reader.metadata()?.len();
        self.reader.seek(SeekFrom::Start(0))?;
        self.offset = 0;

        let mut buf_reader = BufReader::new(&self.reader);

        loop {
            let start = self.offset;

            let mut header = [0u8; HEADER_LEN];
            match buf_reader.read_exact(&mut header) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e.into()),
            }

            let body_len = header[4..8].try_into();

            let body_len = match body_len {
                Ok(bytes) => u32::from_le_bytes(bytes) as usize,
                Err(_) => return Err(KvsError::Corruption),
            };

            let remaining = file_len - start - HEADER_LEN as u64;
            if body_len as u64 > remaining {
                self.writer.get_ref().set_len(start)?;
                self.offset = start;
                return Ok(());
            }

            let mut buf = vec![0u8; HEADER_LEN + body_len];
            buf[..HEADER_LEN].copy_from_slice(&header);

            match buf_reader.read_exact(&mut buf[HEADER_LEN..]) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    self.writer.get_ref().set_len(start)?;
                    self.writer.seek(SeekFrom::End(0))?;
                    self.offset = start;
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            }

            let (deserialized, _) = Command::deserialize(&buf)?;
            match deserialized {
                Command::Set { key, .. } => {
                    self.in_mem_index.insert(
                        key.to_vec(),
                        IndexValue {
                            offset: start,
                            len: buf.len(),
                        },
                    );
                }
                Command::Rm { key, .. } => {
                    self.in_mem_index.remove(&key.to_vec());
                }
            }

            self.offset += buf.len() as u64;
        }

        Ok(())
    }

    fn handle_policy(&mut self) -> Result<()> {
        match self.durability_policy {
            DurabilityPolicy::SyncOnEveryPut => {
                self.sync()?;
                self.flushed_offset = self.offset;
            }
            DurabilityPolicy::SyncOnInterval => {
                let now = unix_now()?.as_millis() as u64;
                if now - self.last_ts_flushed >= FLUSH_THRESHOLD_MILLIS {
                    self.sync()?;
                    self.last_ts_flushed = now;
                    self.flushed_offset = self.offset;
                }
            }
            DurabilityPolicy::OsDecides => {
                // Do nothing, let the OS decide when to flush
            }
        }
        Ok(())
    }

    fn compaction(&mut self) -> Result<()> {
        todo!("Implement compaction logic to remove stale entries from log file");
    }
}

impl KvStore for Bitcask {
    fn open(dir: impl Into<PathBuf>, durability_policy: DurabilityPolicy) -> Result<Self>
    where
        Self: Sized,
    {
        let path = dir.into();

        std::fs::create_dir_all(&path)?;

        let log_path = path.join("log");
        //let index_path = path.join("index");

        let w_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        let writer = BufWriter::new(w_file);
        let reader = OpenOptions::new().read(true).open(&log_path)?;

        let in_mem_index = HashMap::new();

        let mut bitcask = Bitcask {
            writer,
            reader,
            in_mem_index,
            offset: 0,
            flushed_offset: 0,
            durability_policy,
            last_ts_flushed: unix_now()?.as_millis() as u64,
        };

        bitcask.replay()?;
        bitcask.writer.seek(SeekFrom::End(0))?;

        Ok(bitcask)
    }

    fn set(&mut self, key: &str, value: &str) -> Result<()> {
        let ts = unix_now()?.as_secs();
        let command = Command::Set {
            ts,
            key: key.as_bytes(),
            value: value.as_bytes(),
        };
        let serialized_command = command.serialize();
        let offset = self.offset;

        self.writer.write_all(&serialized_command)?;

        let index_value = IndexValue {
            offset,
            len: serialized_command.len(),
        };

        self.in_mem_index
            .insert(key.as_bytes().to_vec(), index_value);
        self.offset = self.offset + serialized_command.len() as u64;

        self.handle_policy()?;
        Ok(())
    }

    fn remove(&mut self, key: &str) -> Result<()> {
        let key_bytes = key.as_bytes();
        if !self.in_mem_index.contains_key(key_bytes) {
            return Err(KvsError::KeyNotFound);
        }

        let ts = unix_now()?.as_secs();

        let command = Command::Rm { ts, key: key_bytes };
        let serialized_command = command.serialize();

        self.writer.write_all(&serialized_command)?;

        self.in_mem_index.remove(key_bytes);
        self.offset = self.offset + serialized_command.len() as u64;

        self.handle_policy()?;
        Ok(())
    }

    fn get(&mut self, key: &str) -> Result<Option<String>> {
        let key_bytes = key.as_bytes();
        let index_value = match self.in_mem_index.get(key_bytes) {
            Some(index_value) => index_value,
            None => return Ok(None),
        };

        if self.flushed_offset < self.offset {
            self.writer.flush()?;
            self.flushed_offset = self.offset;
        }

        let record_len = index_value.len;
        let mut buf = vec![0u8; record_len];
        self.reader.read_exact_at(&mut buf, index_value.offset)?;

        let (deserialized, _) = Command::deserialize(&buf)?;
        let cmd = deserialized.get_value();
        match cmd {
            Some(value) => {
                let value_str = String::from_utf8(value.to_vec())
                    .map_err(|e| KvsError::Io(io::Error::new(io::ErrorKind::InvalidData, e)))?;
                Ok(Some(value_str))
            }
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn replay_ignores_truncated_tail() -> Result<()> {
        let dir = tempdir()?;
        let log_path = dir.path().join("log");

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&log_path)?;

        let ts = unix_now()?.as_secs();

        let valid = Command::Set {
            ts,
            key: b"alpha",
            value: b"beta",
        }
        .serialize();
        file.write_all(&valid)?;

        let mut truncated = vec![18u8, 0, 0, 0];
        truncated.extend_from_slice(b"incomplete");
        file.write_all(&truncated)?;
        file.flush()?;

        let w_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;

        let mut bitcask = Bitcask {
            writer: BufWriter::new(w_file),
            reader: OpenOptions::new().read(true).open(&log_path)?,
            in_mem_index: HashMap::new(),
            offset: 0,
            flushed_offset: 0,
            durability_policy: DurabilityPolicy::SyncOnEveryPut,
            last_ts_flushed: 0,
        };

        bitcask.replay()?;

        assert_eq!(bitcask.get("alpha")?, Some("beta".to_owned()));
        assert_eq!(bitcask.reader.metadata()?.len(), valid.len() as u64);

        Ok(())
    }
}
