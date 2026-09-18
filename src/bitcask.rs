use crate::command::Command;
use crate::config::Config;
use crate::index_value::IndexValue;
use crate::kvstore::KvStore;
use crate::policy::DurabilityPolicy;
use crate::record_reader::{RecordRead, RecordReader};
use crate::{KvsError, Result};
use log::{debug, trace};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn unix_now() -> Result<Duration> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?)
}

pub struct Bitcask {
    shared: Arc<Shared>,
}

// !!! lock order rule: writer → readers → index !!!
struct Shared {
    index: RwLock<HashMap<Vec<u8>, IndexValue>>,
    readers: RwLock<BTreeMap<u64, Arc<File>>>,
    // the single writer
    writer: Mutex<WriterState>,
    next_file_id: AtomicU64,
    merging: AtomicBool,
    config: Config,
}

// fields what only the writer needs to access
struct WriterState {
    current_file_id: u64,
    file: File,
    offset: u64,
    flushed_offset: u64,
    last_ts_flushed: u64,
    stale_bytes_count: u64,
}

impl Shared {
    fn writer_lock(&self) -> Result<MutexGuard<'_, WriterState>> {
        trace!("acquiring lock; struct {}, type {}", "writer", "WRITE");
        self.writer.lock().map_err(|_| KvsError::LockPoisoned)
    }

    fn readers_read(&self) -> Result<std::sync::RwLockReadGuard<'_, BTreeMap<u64, Arc<File>>>> {
        trace!("acquiring lock; struct {}, type {}", "readers", "READ");
        self.readers.read().map_err(|_| KvsError::LockPoisoned)
    }

    fn readers_write(&self) -> Result<std::sync::RwLockWriteGuard<'_, BTreeMap<u64, Arc<File>>>> {
        trace!("acquiring lock; struct {}, type {}", "readers", "WRITE");
        self.readers.write().map_err(|_| KvsError::LockPoisoned)
    }

    fn index_read(&self) -> Result<std::sync::RwLockReadGuard<'_, HashMap<Vec<u8>, IndexValue>>> {
        trace!("acquiring lock; struct {}, type {}", "index", "READ");
        self.index.read().map_err(|_| KvsError::LockPoisoned)
    }

    fn index_write(&self) -> Result<std::sync::RwLockWriteGuard<'_, HashMap<Vec<u8>, IndexValue>>> {
        trace!("acquiring lock; struct {}, type {}", "index", "WRITE");
        self.index.write().map_err(|_| KvsError::LockPoisoned)
    }
}

struct Loader {
    readers: BTreeMap<u64, Arc<File>>,
    index: HashMap<Vec<u8>, IndexValue>,
    writer: WriterState,
}

impl Loader {
    pub fn replay(&mut self, config: &Config) -> Result<()> {
        for (&file_id, reader_file) in &self.readers {
            let hint_file_path = config.dir.join(hint_file_name(file_id));
            if (file_id != self.writer.current_file_id) && fs::exists(&hint_file_path)? {
                let hint_file = OpenOptions::new().read(true).open(&hint_file_path)?;
                let hint_reader = BufReader::new(hint_file);
                let hint_file_len = hint_reader.get_ref().metadata()?.len();
                let mut hint_record_reader = RecordReader::new(hint_reader, hint_file_len);

                let hint_processed = loop {
                    match hint_record_reader.next_record()? {
                        RecordRead::Record { position: _, bytes } => {
                            match IndexValue::deserialize(&bytes, file_id) {
                                Ok((key, value, _)) => {
                                    self.index.insert(key.to_vec(), value);
                                    debug!("replayed hint: {:?}", key);
                                }
                                Err(_) => {
                                    break false;
                                }
                            }
                        }
                        RecordRead::CleanEof => {
                            break true;
                        }
                        RecordRead::TornTail { position: _ } => {
                            break false;
                        }
                    }
                };

                if hint_processed {
                    // handled replay via the hint file
                    continue;
                } else {
                    // the hint file is corrupt, remove it
                    fs::remove_file(hint_file_path)?;
                }
                // else fall through to the data file replay
            }

            let mut buf_reader = BufReader::new(reader_file.as_ref());
            buf_reader.seek(SeekFrom::Start(0))?;

            let data_file_len = reader_file.metadata()?.len();
            self.writer.offset = 0;

            let mut data_file_record_reader = RecordReader::new(buf_reader, data_file_len);

            loop {
                match data_file_record_reader.next_record()? {
                    RecordRead::Record { position, bytes } => {
                        let (deserialized, _) = Command::deserialize(&bytes)?;
                        match deserialized {
                            Command::Set { ts, key, .. } => {
                                let file_id_copy = file_id;
                                self.index.insert(
                                    key.to_vec(),
                                    IndexValue {
                                        file_id: file_id_copy,
                                        ts,
                                        offset: position,
                                        len: bytes.len(),
                                    },
                                );
                                debug!("replayed SET: {:?}", deserialized);
                            }
                            Command::Rm { key, .. } => {
                                self.writer.stale_bytes_count += bytes.len() as u64;
                                self.index.remove(key);
                                debug!("replayed RM: {:?}", deserialized);
                            }
                        }
                        self.writer.offset += bytes.len() as u64;
                    }
                    RecordRead::CleanEof => break,
                    RecordRead::TornTail { position } => {
                        return if file_id == self.writer.current_file_id {
                            self.writer.file.set_len(position)?;
                            self.writer.offset = position;
                            Ok(())
                        } else {
                            Err(KvsError::Corruption)
                        };
                    }
                }
            }
        }

        Ok(())
    }
}

impl Clone for Bitcask {
    fn clone(&self) -> Self {
        Bitcask {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl Drop for Bitcask {
    fn drop(&mut self) {
        let sw = self.shared.writer_lock();
        if let Ok(mut single_writer) = sw {
            let _ = self.sync(&mut single_writer);
        }
    }
}

impl Bitcask {
    fn sync(&self, single_writer: &mut MutexGuard<WriterState>) -> Result<()> {
        single_writer.file.sync_all()?;
        single_writer.flushed_offset = single_writer.offset;
        Ok(())
    }

    fn handle_policy(&self, single_writer: &mut MutexGuard<WriterState>) -> Result<()> {
        match self.shared.config.durability_policy {
            DurabilityPolicy::SyncOnEveryPut => {
                self.sync(single_writer)?;
            }
            DurabilityPolicy::SyncOnInterval => {
                let now = unix_now()?.as_millis() as u64;
                if now - single_writer.last_ts_flushed >= self.shared.config.flush_threshold_millis
                {
                    self.sync(single_writer)?;
                    single_writer.last_ts_flushed = now;
                }
            }
            DurabilityPolicy::OsDecides => {
                // Do nothing, let the OS decide when to flush
            }
        }
        Ok(())
    }

    fn rotate_to(
        &self,
        single_writer: &mut MutexGuard<WriterState>,
        readers: &mut BTreeMap<u64, Arc<File>>,
        next_file_id: u64,
    ) -> Result<()> {
        self.sync(single_writer)?;

        let next_file = self.shared.config.dir.join(data_file_name(next_file_id));

        let next_file_writer = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&next_file)?;

        single_writer.file = next_file_writer;
        single_writer.current_file_id = next_file_id;
        single_writer.flushed_offset = 0;
        single_writer.offset = 0;
        single_writer.last_ts_flushed = unix_now()?.as_millis() as u64;

        let next_file_reader = OpenOptions::new().read(true).open(&next_file)?;
        let reader = Arc::new(next_file_reader);
        readers.insert(next_file_id, reader);
        Ok(())
    }

    fn rotate_if_full(&self, single_writer: &mut MutexGuard<WriterState>) -> Result<()> {
        if single_writer.offset < self.shared.config.file_size_threshold {
            return Ok(());
        }
        let mut readers = self.shared.readers_write()?;
        self.rotate_to(
            single_writer,
            &mut readers,
            single_writer.current_file_id + 1,
        )?;
        Ok(())
    }

    fn post_write_ops(&self, single_writer: &mut MutexGuard<WriterState>) -> Result<()> {
        self.compaction(single_writer)?;
        self.handle_policy(single_writer)?;
        self.rotate_if_full(single_writer)?;
        Ok(())
    }

    fn compaction(&self, single_writer: &mut MutexGuard<WriterState>) -> Result<()> {
        // skip if there is just 1 file
        // or if there are no stale bytes
        // TODO fix the holding the readers lock
        let mut readers_access = self.shared.readers_write()?;
        if readers_access.len() == 1
            || single_writer.stale_bytes_count < self.shared.config.compaction_threshold
        {
            return Ok(());
        }

        debug!(
            "compaction triggered, stale bytes: {}",
            single_writer.stale_bytes_count
        );

        // 1) walk on every sealed file, skip the active one
        // 2) parse entry: if it's SET -> check whether it's still in the index
        // 3) if it's in the index -> append to the compaction file, update the index
        // 4) compaction files get ids above the active file (<id>.data.compact);
        //    if one is full -> sync it and open the next one
        // 5) when all files are processed -> rename every .data.compact to .data
        // 6) rotate the writer to an id above the compacted files, so new writes win on replay
        // 7) remove the old files last: after a crash we either see leftover .compact
        //    files (open() deletes them) or renamed files that win over the old ones
        let mut new_files: HashSet<u64> = HashSet::new();
        let mut ids_to_remove: Vec<u64> = Vec::new();

        let mut output: Option<MergeOutput> = None;
        let mut next_compaction_file_id = single_writer.current_file_id;

        for (&file_id, reader_file) in readers_access.iter() {
            if file_id == single_writer.current_file_id {
                continue;
            }

            debug!("compaction: processing file {}", file_id);

            let out = match &mut output {
                Some(out) => out,
                slot @ None => {
                    next_compaction_file_id += 1;
                    slot.insert(MergeOutput::open(
                        &self.shared.config.dir,
                        next_compaction_file_id,
                    )?)
                }
            };

            let mut buf_reader = BufReader::new(reader_file.as_ref());
            buf_reader.seek(SeekFrom::Start(0))?;

            let file_len = reader_file.metadata()?.len();

            let mut data_file_record_reader = RecordReader::new(buf_reader, file_len);

            let mut index_access_w = self.shared.index_write()?;

            loop {
                match data_file_record_reader.next_record()? {
                    RecordRead::Record { position, bytes } => {
                        let (deserialized, _) = Command::deserialize(&bytes)?;
                        match deserialized {
                            Command::Set { ts, key, .. } => {
                                match index_access_w.get(key) {
                                    // if the key sits in the index, append to the compaction file
                                    Some(index_value)
                                        if index_value.file_id == file_id
                                            && index_value.offset == position =>
                                    {
                                        let new_index_value = out.append(&bytes, key, ts)?;
                                        index_access_w.insert(key.to_vec(), new_index_value);
                                        debug!(
                                            "compaction: key {} appended to file {}",
                                            String::from_utf8_lossy(key),
                                            file_id
                                        );
                                    }
                                    // key was removed or already re-written elsewhere, do nothing
                                    _ => {}
                                }
                            }
                            Command::Rm { .. } => {
                                // do nothing, the index is already updated
                            }
                        }
                    }
                    RecordRead::CleanEof => {
                        // remove the file from the reader's list
                        // we finished processing it
                        ids_to_remove.push(file_id);
                        break;
                    }
                    RecordRead::TornTail { .. } => {
                        return Err(KvsError::Corruption);
                    }
                }
            }

            out.sync()?;
            new_files.insert(out.id);

            if out.is_full(self.shared.config.file_size_threshold) {
                output = None;
            }
        }

        for id in new_files {
            let path = self.shared.config.dir.join(data_file_name_compaction(id));
            let new_path = self.shared.config.dir.join(data_file_name(id));
            fs::rename(&path, &new_path)?;
            let compacted_file = OpenOptions::new().read(true).open(&new_path)?;
            readers_access.insert(id, Arc::new(compacted_file));
        }

        // after compaction, rotate the writer above the merge outputs
        // so new writes win on replay
        self.rotate_to(
            single_writer,
            &mut readers_access,
            next_compaction_file_id + 1,
        )?;

        for id in ids_to_remove {
            if let Some(file) = readers_access.remove(&id) {
                drop(file);
                let path_to_remove = self.shared.config.dir.join(data_file_name(id));
                fs::remove_file(path_to_remove)?;

                // remove associated hint file
                let hint_path = self.shared.config.dir.join(hint_file_name(id));
                if hint_path.exists() {
                    fs::remove_file(hint_path)?;
                }
            }
        }

        single_writer.stale_bytes_count = 0;

        Ok(())
    }

    fn append(&self, command: &Command) -> Result<()> {
        let serialized_command = command.serialize();
        let mut single_writer = self.shared.writer_lock()?;
        let offset = single_writer.offset;

        single_writer.file.write_all(&serialized_command)?;

        match command {
            Command::Set {
                ts: _,
                key: _,
                value: _,
            } => {
                let index_value = IndexValue {
                    file_id: single_writer.current_file_id,
                    ts: command.get_timestamp(),
                    offset,
                    len: serialized_command.len(),
                };

                let mut index_writer = self.shared.index_write()?;
                if let Some(old) = index_writer.insert(command.get_key().to_vec(), index_value) {
                    single_writer.stale_bytes_count += old.len as u64;
                }
            }
            Command::Rm { ts: _, key } => {
                let mut index_writer = self.shared.index_write()?;
                if let Some(old) = index_writer.remove(*key) {
                    single_writer.stale_bytes_count += old.len as u64;
                }
            }
        }

        single_writer.offset += serialized_command.len() as u64;

        self.post_write_ops(&mut single_writer)?;
        Ok(())
    }
}

struct MergeOutput {
    id: u64,
    data: BufWriter<File>,
    hint: BufWriter<File>,
    written: u64,
}

impl MergeOutput {
    fn open(dir: &Path, id: u64) -> Result<Self> {
        let data = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(data_file_name_compaction(id)))?;
        let hint = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(hint_file_name(id)))?;
        Ok(MergeOutput {
            id,
            data: BufWriter::new(data),
            hint: BufWriter::new(hint),
            written: 0,
        })
    }

    fn append(&mut self, record: &[u8], key: &[u8], ts: u64) -> Result<IndexValue> {
        self.data.write_all(record)?;
        let index_value = IndexValue {
            file_id: self.id,
            ts,
            offset: self.written,
            len: record.len(),
        };
        self.hint.write_all(&index_value.serialize(key))?;
        self.written += record.len() as u64;
        Ok(index_value)
    }

    fn sync(&mut self) -> Result<()> {
        self.data.flush()?;
        self.data.get_ref().sync_all()?;
        self.hint.flush()?;
        self.hint.get_ref().sync_all()?;
        Ok(())
    }

    fn is_full(&self, threshold: u64) -> bool {
        self.written >= threshold
    }
}

impl KvStore for Bitcask {
    fn open(config: Config) -> Result<Self> {
        // TODO the lock file

        let dir_path: &Path = config.dir.as_path();

        fs::create_dir_all(dir_path)?;

        let mut read_handlers: BTreeMap<u64, Arc<File>> = BTreeMap::new();
        let mut max_id: Option<u64> = None;

        for entry in fs::read_dir(dir_path)? {
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
                if file_name.ends_with(".data.compact") {
                    // file name is not in the expected format, remove it
                    // like old compacted files
                    fs::remove_file(path)?;
                } else if let Some(id) = file_name
                    .strip_suffix(".hint")
                    .and_then(|stem| stem.parse::<u64>().ok())
                {
                    // remove the hint file if the corresponding data file is gone
                    if !fs::exists(config.dir.join(data_file_name(id)))? {
                        fs::remove_file(path)?;
                    }
                }
                continue;
            };

            let file = OpenOptions::new().read(true).open(&path)?;
            read_handlers.insert(id, Arc::new(file));

            max_id = Some(max_id.map_or(id, |m| m.max(id)));
        }

        let current_path = match max_id {
            None => {
                // no .data files -> create 000001.data
                config.dir.join(data_file_name(1))
            }
            Some(max) => config.dir.join(data_file_name(max)),
        };

        let current_file_id = max_id.unwrap_or(1);

        let writer_state = WriterState {
            current_file_id,
            file: OpenOptions::new()
                .create(true)
                .append(true)
                .open(&current_path)?,
            offset: 0, // the replay func will set this
            flushed_offset: 0,
            last_ts_flushed: unix_now()?.as_millis() as u64,
            stale_bytes_count: 0,
        };

        let r_file = OpenOptions::new().read(true).open(&current_path)?;
        read_handlers.insert(current_file_id, Arc::new(r_file));

        let in_mem_index = HashMap::new();

        let mut loader = Loader {
            readers: read_handlers,
            index: in_mem_index,
            writer: writer_state,
        };

        loader.replay(&config)?;

        let shared = Shared {
            config,
            index: RwLock::new(loader.index),
            readers: RwLock::new(loader.readers),
            writer: Mutex::new(loader.writer),
            next_file_id: AtomicU64::new(current_file_id + 1),
            merging: AtomicBool::new(false),
        };

        Ok(Bitcask {
            shared: Arc::new(shared),
        })
    }

    fn set(&self, key: &str, value: &str) -> Result<()> {
        let ts = unix_now()?.as_secs();
        let command = Command::Set {
            ts,
            key: key.as_bytes(),
            value: value.as_bytes(),
        };

        self.append(&command)?;
        Ok(())
    }

    fn remove(&self, key: &str) -> Result<()> {
        let key_bytes = key.as_bytes();
        if !self.shared.index_read()?.contains_key(key_bytes) {
            return Err(KvsError::KeyNotFound);
        }

        let ts = unix_now()?.as_secs();

        let command = Command::Rm { ts, key: key_bytes };
        self.append(&command)?;
        Ok(())
    }

    fn get(&self, key: &str) -> Result<Option<String>> {
        let key_bytes = key.as_bytes();
        // TODO do the retry loop
        let readers = self.shared.readers_read()?;
        let Some(entry) = self.shared.index_read()?.get(key_bytes).copied() else {
            return Ok(None);
        };

        let file = readers
            .get(&entry.file_id)
            .cloned()
            .ok_or(KvsError::MissingDataFile(entry.file_id))?;
        let mut buf = vec![0u8; entry.len];
        file.read_exact_at(&mut buf, entry.offset)?;

        let (deserialized, _) = Command::deserialize(&buf)?;
        let cmd = deserialized.get_value();
        match cmd {
            Some(value) => Ok(Some(String::from_utf8(value.to_vec())?)),
            None => Ok(None),
        }
    }
}

// name format for the compaction temp files: 000001.data.compact (6 digits)
fn data_file_name_compaction(id: u64) -> String {
    format!("{}.compact", data_file_name(id))
}

// name format for the log data files: 000001.data (6 digits)
fn data_file_name(id: u64) -> String {
    format!("{:06}.data", id)
}

// name format for the log data files: 000001.hint (6 digits)
fn hint_file_name(id: u64) -> String {
    format!("{:06}.hint", id)
}

fn get_file_id(file_name: &str) -> Option<u64> {
    let stem = file_name.strip_suffix(".data")?;
    stem.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test_log::test]
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

        let bitcask = Bitcask::open(Config::new(dir.path()))?;

        assert_eq!(bitcask.get("alpha")?, Some("beta".to_owned()));
        assert_eq!(fs::metadata(&log_path)?.len(), valid.len() as u64);

        Ok(())
    }

    // A hint deliberately mapping a DIFFERENT key ("zeta") to the sealed
    // record makes hint usage observable: if the hint is loaded, "zeta"
    // resolves and "alpha" is unknown; if the data file were replayed
    // instead, it would be the other way around.
    #[test_log::test]
    fn replay_uses_hint_file_for_sealed_files() -> Result<()> {
        let dir = tempdir()?;

        let sealed = Command::Set {
            ts: 1,
            key: b"alpha",
            value: b"beta",
        }
        .serialize();
        fs::write(dir.path().join("000001.data"), &sealed)?;

        let active = Command::Set {
            ts: 2,
            key: b"gamma",
            value: b"delta",
        }
        .serialize();
        fs::write(dir.path().join("000002.data"), &active)?;

        let hint = IndexValue {
            file_id: 1,
            ts: 1,
            offset: 0,
            len: sealed.len(),
        }
        .serialize(b"zeta");
        fs::write(dir.path().join("000001.hint"), &hint)?;

        let bitcask = Bitcask::open(Config::new(dir.path()))?;

        assert_eq!(bitcask.get("zeta")?, Some("beta".to_owned()));
        assert_eq!(bitcask.get("alpha")?, None);
        assert_eq!(bitcask.get("gamma")?, Some("delta".to_owned()));

        Ok(())
    }

    // Hint-loaded files take part in later-id-wins ordering like any other
    // file: a hinted file must override keys replayed from earlier files.
    #[test_log::test]
    fn hint_of_later_file_overrides_earlier_data_replay() -> Result<()> {
        let dir = tempdir()?;

        let old = Command::Set {
            ts: 1,
            key: b"k",
            value: b"old",
        }
        .serialize();
        fs::write(dir.path().join("000001.data"), &old)?;

        let new = Command::Set {
            ts: 2,
            key: b"k",
            value: b"new",
        }
        .serialize();
        fs::write(dir.path().join("000002.data"), &new)?;
        let hint = IndexValue {
            file_id: 2,
            ts: 2,
            offset: 0,
            len: new.len(),
        }
        .serialize(b"k");
        fs::write(dir.path().join("000002.hint"), &hint)?;

        let active = Command::Set {
            ts: 3,
            key: b"other",
            value: b"x",
        }
        .serialize();
        fs::write(dir.path().join("000003.data"), &active)?;

        let bitcask = Bitcask::open(Config::new(dir.path()))?;

        assert_eq!(bitcask.get("k")?, Some("new".to_owned()));
        assert_eq!(bitcask.get("other")?, Some("x".to_owned()));

        Ok(())
    }

    // Crash-recovery shape: a merge output (with hint) is the highest id, so
    // open() elects it as the active file. The hint must be skipped for the
    // active file so replay sets the append offset — writes landing after
    // this reopen must survive the next one.
    #[test_log::test]
    fn writes_after_reopen_onto_compacted_file_survive() -> Result<()> {
        let dir = tempdir()?;

        let sealed = Command::Set {
            ts: 1,
            key: b"a",
            value: b"1",
        }
        .serialize();
        fs::write(dir.path().join("000001.data"), &sealed)?;

        let compacted = Command::Set {
            ts: 2,
            key: b"b",
            value: b"2",
        }
        .serialize();
        fs::write(dir.path().join("000002.data"), &compacted)?;
        let hint = IndexValue {
            file_id: 2,
            ts: 2,
            offset: 0,
            len: compacted.len(),
        }
        .serialize(b"b");
        fs::write(dir.path().join("000002.hint"), &hint)?;

        let bitcask = Bitcask::open(Config::new(dir.path()))?;
        bitcask.set("c", "3")?;
        assert_eq!(bitcask.get("c")?, Some("3".to_owned()));
        drop(bitcask);

        let bitcask = Bitcask::open(Config::new(dir.path()))?;
        assert_eq!(bitcask.get("a")?, Some("1".to_owned()));
        assert_eq!(bitcask.get("b")?, Some("2".to_owned()));
        assert_eq!(bitcask.get("c")?, Some("3".to_owned()));

        Ok(())
    }

    #[test_log::test]
    fn torn_hint_file_falls_back_to_data_replay() -> Result<()> {
        let dir = tempdir()?;

        let sealed = Command::Set {
            ts: 1,
            key: b"alpha",
            value: b"beta",
        }
        .serialize();
        fs::write(dir.path().join("000001.data"), &sealed)?;

        let active = Command::Set {
            ts: 2,
            key: b"gamma",
            value: b"delta",
        }
        .serialize();
        fs::write(dir.path().join("000002.data"), &active)?;

        let mut hint = IndexValue {
            file_id: 1,
            ts: 1,
            offset: 0,
            len: sealed.len(),
        }
        .serialize(b"zeta");
        hint.extend_from_slice(&[0xAA, 0xBB, 0xCC]);
        fs::write(dir.path().join("000001.hint"), &hint)?;

        let bitcask = Bitcask::open(Config::new(dir.path()))?;

        assert_eq!(bitcask.get("alpha")?, Some("beta".to_owned()));
        assert_eq!(bitcask.get("gamma")?, Some("delta".to_owned()));
        assert!(
            !fs::exists(dir.path().join("000001.hint"))?,
            "damaged hint file should be removed during fallback"
        );

        Ok(())
    }
}
