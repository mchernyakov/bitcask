use crate::command::{Command, HEADER_LEN};
use crate::kvstore::KvStore;
use crate::policy::DurabilityPolicy;
use crate::{KvsError, Result};
use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{fs, io};

const FILE_THRESHOLD: u64 = 1 << 20; // 1 MB
const FLUSH_THRESHOLD_MILLIS: u64 = 1 * 1000; // 1 second

fn unix_now() -> Result<Duration> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?)
}

pub struct IndexValue {
    pub file_id: u64,
    pub offset: u64,
    pub len: usize,
}

pub struct Bitcask {
    // current file
    dir: PathBuf,
    current_file_id: u64,
    writer: BufWriter<File>,
    // readers
    readers: BTreeMap<u64, File>,
    // in-mem fields
    in_mem_index: HashMap<Vec<u8>, IndexValue>,
    offset: u64,
    flushed_offset: u64,
    last_ts_flushed: u64,
    // auxiliary fields
    durability_policy: DurabilityPolicy,
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
        self.flushed_offset = self.offset;
        Ok(())
    }

    fn replay(&mut self) -> Result<()> {
        for (file_id, reader_file) in self.readers.iter() {
            let mut buf_reader = BufReader::new(reader_file);
            buf_reader.seek(SeekFrom::Start(0))?;

            let file_len = reader_file.metadata()?.len();
            self.offset = 0;

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
                    return if *file_id == self.current_file_id {
                        self.writer.get_ref().set_len(start)?;
                        self.offset = start;
                        Ok(())
                    } else {
                        Err(KvsError::Corruption)
                    };
                }

                let mut buf = vec![0u8; HEADER_LEN + body_len];
                buf[..HEADER_LEN].copy_from_slice(&header);

                match buf_reader.read_exact(&mut buf[HEADER_LEN..]) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                        if *file_id == self.current_file_id {
                            self.writer.get_ref().set_len(start)?;
                            self.offset = start;
                            return Ok(());
                        } else {
                            return Err(KvsError::Corruption);
                        }
                    }
                    Err(e) => return Err(e.into()),
                }

                let (deserialized, _) = Command::deserialize(&buf)?;
                match deserialized {
                    Command::Set { key, .. } => {
                        let file_id_copy = *file_id;
                        self.in_mem_index.insert(
                            key.to_vec(),
                            IndexValue {
                                file_id: file_id_copy,
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
        }

        Ok(())
    }

    fn need_to_flush(&mut self, file_id: u64) -> Result<()> {
        if file_id == self.current_file_id {
            if self.flushed_offset < self.offset {
                self.writer.flush()?;
                self.flushed_offset = self.offset;
            }
        }
        Ok(())
    }

    fn handle_policy(&mut self) -> Result<()> {
        match self.durability_policy {
            DurabilityPolicy::SyncOnEveryPut => {
                self.sync()?;
            }
            DurabilityPolicy::SyncOnInterval => {
                let now = unix_now()?.as_millis() as u64;
                if now - self.last_ts_flushed >= FLUSH_THRESHOLD_MILLIS {
                    self.sync()?;
                    self.last_ts_flushed = now;
                }
            }
            DurabilityPolicy::OsDecides => {
                // Do nothing, let the OS decide when to flush
            }
        }
        Ok(())
    }

    fn handle_file_rotation(&mut self) -> Result<()> {
        if self.offset < FILE_THRESHOLD {
            return Ok(());
        }

        self.sync()?;

        let next_file_id = self.current_file_id + 1;

        let next_file = self.dir.join(data_file_name(next_file_id));

        let next_file_writer = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&next_file)?;

        let next_file_reader = OpenOptions::new().read(true).open(&next_file)?;
        self.readers.insert(next_file_id, next_file_reader);

        let next_writer = BufWriter::new(next_file_writer);
        self.writer = next_writer;
        self.current_file_id = next_file_id;
        self.flushed_offset = 0;
        self.offset = 0;
        self.last_ts_flushed = unix_now()?.as_millis() as u64;

        Ok(())
    }

    fn post_write_ops(&mut self) -> Result<()> {
        self.handle_policy()?;
        self.handle_file_rotation()?;
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
        let dir: PathBuf = dir.into();
        let dir_path: &Path = dir.as_path();

        fs::create_dir_all(dir_path)?;

        let mut read_handlers = BTreeMap::new();
        let mut max_id: Option<u64> = None;

        for entry in fs::read_dir(&dir_path)? {
            let entry = entry?;
            let path = entry.path();

            if !path.is_file() {
                continue;
            }

            let Some(file_name_os) = path.file_name() else {
                continue;
            };
            let Some(file_name) = file_name_os.to_str() else {
                continue;
            };

            let Some(id) = get_file_id(file_name) else {
                continue;
            };

            let file = OpenOptions::new().read(true).open(&path)?;
            read_handlers.insert(id, file);

            max_id = Some(max_id.map_or(id, |m| m.max(id)));
        }

        let current_path = match max_id {
            None => {
                // no .data files -> create 000001.data
                dir.join(data_file_name(1))
            }
            Some(max) => dir.join(data_file_name(max)),
        };

        let w_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&current_path)?;

        let r_file = OpenOptions::new().read(true).open(&current_path)?;
        read_handlers.insert(max_id.unwrap_or(1), r_file);

        let writer = BufWriter::new(w_file);

        let in_mem_index = HashMap::new();

        let mut bitcask = Bitcask {
            dir,
            current_file_id: max_id.unwrap_or(1),
            writer,
            readers: read_handlers,
            in_mem_index,
            offset: 0, // the replay func will set this
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
            file_id: self.current_file_id,
            offset,
            len: serialized_command.len(),
        };

        self.in_mem_index
            .insert(key.as_bytes().to_vec(), index_value);
        self.offset = self.offset + serialized_command.len() as u64;

        self.post_write_ops()?;
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

        self.post_write_ops()?;
        Ok(())
    }

    fn get(&mut self, key: &str) -> Result<Option<String>> {
        let key_bytes = key.as_bytes();
        let (file_id, offset, record_len) = match self.in_mem_index.get(key_bytes) {
            Some(index_value) => (index_value.file_id, index_value.offset, index_value.len),
            None => return Ok(None),
        };

        self.need_to_flush(file_id)?;

        let mut buf = vec![0u8; record_len];
        let reader = self.readers.get(&file_id).ok_or_else(|| {
            let msg = format!("File not found, id {}", file_id);
            KvsError::Io(io::Error::new(io::ErrorKind::NotFound, msg))
        })?;

        reader.read_exact_at(&mut buf, offset)?;

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

// name format for the log data files: 000001.data (6 digits)
fn data_file_name(id: u64) -> String {
    format!("{:06}.data", id)
}

fn get_file_id(file_name: &str) -> Option<u64> {
    let stem = file_name.strip_suffix(".data")?;
    stem.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn replay_ignores_truncated_tail() -> Result<()> {
        let dir = tempdir()?;
        let log_path = dir.path().join("000001.data");

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
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
        drop(file);

        let mut bitcask = Bitcask::open(dir.path(), DurabilityPolicy::OsDecides)?;

        assert_eq!(bitcask.get("alpha")?, Some("beta".to_owned()));
        assert_eq!(fs::metadata(&log_path)?.len(), valid.len() as u64);

        Ok(())
    }
}
