use super::command::Command;
use super::config::Config;
use super::index::INDEX_BUCKETS_NUM;
use super::index::Index;
use super::index_value::IndexValue;
use super::kvstore::KvStore;
use super::lock_file::LockFile;
use super::policy::DurabilityPolicy;
use super::record_reader::{RecordRead, RecordReader};
use crate::{KvsError, Result, StoreType};
use arc_swap::ArcSwap;
use log::{debug, trace};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Seek, SeekFrom, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{fs, thread};

fn unix_now() -> Result<Duration> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?)
}

pub struct Bitcask {
    handle: Arc<Handle>,
}

struct Handle {
    shared: Arc<Shared>,
    // compaction thread
    compaction_tx: Option<SyncSender<()>>,
    compaction_thread_handle: Option<JoinHandle<()>>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        drop(self.compaction_tx.take());
        if let Some(handle) = self.compaction_thread_handle.take() {
            let _ = handle.join();
        }

        if let Ok(mut single_writer) = self.shared.writer_lock() {
            let _ = self.shared.sync(&mut single_writer);
        }
    }
}

// !!! lock order rule: writer → index(bucket) !!!
struct Shared {
    index: Index,
    readers: ArcSwap<BTreeMap<u64, Arc<File>>>,
    // the single writer
    writer: Mutex<WriterState>,
    config: Config,
    #[allow(dead_code)]
    lock_file: LockFile,
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
    fn sync(&self, single_writer: &mut MutexGuard<WriterState>) -> Result<()> {
        single_writer.file.sync_all()?;
        single_writer.flushed_offset = single_writer.offset;
        Ok(())
    }

    fn readers_update(&self, mutate: impl Fn(&mut BTreeMap<u64, Arc<File>>)) {
        self.readers.rcu(|old| {
            let mut map = BTreeMap::clone(old);
            mutate(&mut map);
            map
        });
    }

    fn writer_lock(&self) -> Result<MutexGuard<'_, WriterState>> {
        trace!("acquiring lock; struct {}, type {}", "writer", "WRITE");
        self.writer.lock().map_err(|_| KvsError::LockPoisoned)
    }

    fn rotate_to(
        &self,
        single_writer: &mut MutexGuard<WriterState>,
        next_file_id: u64,
    ) -> Result<()> {
        self.sync(single_writer)?;

        let next_file = self.config.dir.join(data_file_name(next_file_id));

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
        self.readers_update(|m| {
            m.insert(next_file_id, Arc::clone(&reader));
        });
        Ok(())
    }

    fn rotate_if_full(&self, single_writer: &mut MutexGuard<WriterState>) -> Result<()> {
        if single_writer.offset < self.config.file_size_threshold {
            return Ok(());
        }
        self.rotate_to(single_writer, single_writer.current_file_id + 1)?;
        Ok(())
    }

    fn run_compaction(&self) -> Result<()> {
        // 0) take a snapshot of the readers, stale bytes and next active file id
        let (readers_snapshot, init_stale_bytes, first_compacted_file_id) = {
            let mut writer = self.writer_lock()?;
            if writer.stale_bytes_count < self.config.compaction_threshold {
                return Ok(());
            }

            debug!(
                "compaction triggered, stale bytes: {}",
                writer.stale_bytes_count
            );

            let readers = self.readers.load_full();
            let mut snapshot: Vec<(u64, Arc<File>)> = Vec::new();
            for (&id, file) in &mut readers.iter() {
                if id == writer.current_file_id {
                    continue;
                }
                snapshot.push((id, Arc::clone(file)));
            }
            if snapshot.is_empty() {
                return Ok(());
            }

            let stale = writer.stale_bytes_count;
            let total_bytes: u64 = snapshot
                .iter()
                .map(|(_, f)| f.metadata().map(|m| m.len()).unwrap_or(0))
                .sum();
            let reserve = total_bytes / self.config.file_size_threshold + 2;

            let next_active = writer.current_file_id + reserve + 1;
            let prev_file_id = writer.current_file_id;
            self.rotate_to(&mut writer, next_active)?;
            (snapshot, stale, prev_file_id)
        };

        // 1) walk the snapshot of sealed files, hold no locks during the I/O
        // 2) on entry: if SET -> check index (get(...)): if that points at the exact file and offset ->
        //    append to the compaction file and save the index update in a pending list
        //    (update them later, to avoid locking); otherwise -> do nothing,
        //    the index is already updated (possible in next file)
        // 3) compaction files get ids from the reserved range below the active
        //    file (<id>.data.compact); when one is full -> finish it: sync,
        //    rename to .data, add a read handle, then apply the pending updates,
        //    checking each entry again and skipping keys that were overwritten
        //    during the merge (their copies become garbage for a future merge)
        // 4) finish the last, half-filled output the same way
        // 5) remove the old files last: after a crash we see either leftover
        //    .compact files (open() -> deletes them) or renamed outputs; their ids
        //    are below the active file, so newer writes still win on replay
        // 6) subtract the stale bytes counted at merge start — don't reset to
        //    zero, writes during the merge added new garbage
        let mut new_files: HashSet<u64> = HashSet::new();
        let mut ids_to_remove: Vec<u64> = Vec::new();

        let mut output: Option<MergeOutput> = None;
        let mut next_compaction_file_id = first_compacted_file_id;

        let mut pending_records: Vec<PendingUpdate> = Vec::new();

        for (file_id, reader_file) in readers_snapshot.iter() {
            debug!("compaction: processing file {}", file_id);

            let out = match &mut output {
                Some(out) => out,
                slot @ None => {
                    next_compaction_file_id += 1;
                    slot.insert(MergeOutput::open(
                        &self.config.dir,
                        next_compaction_file_id,
                    )?)
                }
            };

            let mut buf_reader = BufReader::new(reader_file.as_ref());
            buf_reader.seek(SeekFrom::Start(0))?;

            let file_len = reader_file.metadata()?.len();

            let mut data_file_record_reader = RecordReader::new(buf_reader, file_len);

            loop {
                match data_file_record_reader.next_record()? {
                    RecordRead::Record { position, bytes } => {
                        let (cmd_deserialized, _) = Command::deserialize(bytes)?;
                        match cmd_deserialized {
                            Command::Set { ts, key, .. } => {
                                // if the key is in the index, append to the compaction file
                                let sits_in_index =
                                    self.index.read_access(key)?.get(key).is_some_and(|e| {
                                        e.file_id == *file_id && e.offset == position
                                    });

                                if sits_in_index {
                                    let new_index_value = out.append(bytes, key, ts)?;

                                    let prev_index_value = IndexValue {
                                        file_id: *file_id,
                                        ts,
                                        offset: position,
                                        len: bytes.len(),
                                    };

                                    let pending_update = PendingUpdate {
                                        key: key.to_vec(),
                                        previous_value: prev_index_value,
                                        new: new_index_value,
                                    };

                                    pending_records.push(pending_update);
                                    debug!(
                                        "compaction: key {} appended to file {}",
                                        String::from_utf8_lossy(key),
                                        file_id
                                    );
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
                        ids_to_remove.push(*file_id);
                        break;
                    }
                    RecordRead::TornTail { .. } => {
                        return Err(KvsError::Corruption);
                    }
                }
            }

            new_files.insert(out.id);

            if out.is_full(self.config.file_size_threshold) {
                self.finish_output(
                    output
                        .take()
                        .expect("MergeOutput in compaction should be Some"),
                    &mut pending_records,
                )?;

                output = None;
            }
        }

        if let Some(out) = output.take() {
            self.finish_output(out, &mut pending_records)?;
        }

        for id in ids_to_remove {
            let path_to_remove = self.config.dir.join(data_file_name(id));
            fs::remove_file(path_to_remove)?;

            // remove associated hint file
            let hint_path = self.config.dir.join(hint_file_name(id));
            if hint_path.exists() {
                fs::remove_file(hint_path)?;
            }
        }

        self.writer_lock()?.stale_bytes_count -= init_stale_bytes;

        Ok(())
    }

    fn finish_output(&self, mut out: MergeOutput, pending: &mut Vec<PendingUpdate>) -> Result<()> {
        out.sync()?;
        let path = self.config.dir.join(data_file_name_compaction(out.id));
        let new_path = self.config.dir.join(data_file_name(out.id));
        fs::rename(&path, &new_path)?;

        let compacted_file = Arc::new(OpenOptions::new().read(true).open(&new_path)?);
        self.readers_update(|m| {
            m.insert(out.id, Arc::clone(&compacted_file));
        });

        for pending_update in pending.drain(..) {
            let key = pending_update.key;
            let mut index_write = self.index.write_access(&key)?;
            let prev_value = pending_update.previous_value;
            let new_value = pending_update.new;
            let still_prev = index_write
                .get(&key)
                .is_some_and(|e| e.file_id == prev_value.file_id && e.offset == prev_value.offset);

            if still_prev {
                index_write.insert(key, new_value);
            }
        }
        Ok(())
    }
}

struct Loader {
    readers: BTreeMap<u64, Arc<File>>,
    index: [HashMap<Vec<u8>, IndexValue>; INDEX_BUCKETS_NUM],
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
                            match IndexValue::deserialize(bytes, file_id) {
                                Ok((key, value, _)) => {
                                    // hints hold one record per live key, so
                                    // a plain insert (no get_mut-first) wins
                                    let bucket_id = Index::get_bucket(key);
                                    self.index[bucket_id].insert(key.to_vec(), value);
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
                        let (deserialized, _) = Command::deserialize(bytes)?;
                        match deserialized {
                            Command::Set { ts, key, .. } => {
                                let value = IndexValue {
                                    file_id,
                                    ts,
                                    offset: position,
                                    len: bytes.len(),
                                };
                                let bucket_id = Index::get_bucket(key);
                                match self.index[bucket_id].get_mut(key) {
                                    Some(slot) => *slot = value,
                                    None => {
                                        self.index[bucket_id].insert(key.to_vec(), value);
                                    }
                                }
                                debug!("replayed SET: {:?}", deserialized);
                            }
                            Command::Rm { key, .. } => {
                                let bucket_id = Index::get_bucket(key);
                                self.writer.stale_bytes_count += bytes.len() as u64;
                                self.index[bucket_id].remove(key);
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
            handle: Arc::clone(&self.handle),
        }
    }
}

impl Bitcask {
    fn handle_policy(&self, single_writer: &mut MutexGuard<WriterState>) -> Result<()> {
        match self.handle.shared.config.durability_policy {
            DurabilityPolicy::SyncOnEveryPut => {
                self.handle.shared.sync(single_writer)?;
            }
            DurabilityPolicy::SyncOnInterval => {
                let now = unix_now()?.as_millis() as u64;
                if now - single_writer.last_ts_flushed
                    >= self.handle.shared.config.flush_threshold_millis
                {
                    self.handle.shared.sync(single_writer)?;
                    single_writer.last_ts_flushed = now;
                }
            }
            DurabilityPolicy::OsDecides => {
                // Do nothing, let the OS decide when to flush
            }
        }
        Ok(())
    }

    fn post_write_ops(&self, single_writer: &mut MutexGuard<WriterState>) -> Result<()> {
        self.compaction_trigger(single_writer)?;
        self.handle_policy(single_writer)?;
        self.handle.shared.rotate_if_full(single_writer)?;
        Ok(())
    }

    fn compaction_trigger(&self, single_writer: &mut MutexGuard<WriterState>) -> Result<()> {
        if single_writer.stale_bytes_count >= self.handle.shared.config.compaction_threshold {
            // signal to the compaction thread
            if let Some(tx) = &self.handle.compaction_tx {
                let _ = tx.try_send(());
            }
        }
        Ok(())
    }

    fn append(&self, command: &Command) -> Result<()> {
        let serialized_command = command.serialize();
        let mut single_writer = self.handle.shared.writer_lock()?;
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

                let key = command.get_key();
                let mut index_writer = self.handle.shared.index.write_access(key)?;

                match index_writer.get_mut(key) {
                    Some(old) => {
                        single_writer.stale_bytes_count += old.len as u64;
                        *old = index_value;
                    }
                    None => {
                        index_writer.insert(command.get_key().to_vec(), index_value);
                    }
                }
            }
            Command::Rm { ts: _, key } => {
                let mut index_writer = self.handle.shared.index.write_access(key)?;
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

struct PendingUpdate {
    key: Vec<u8>,
    previous_value: IndexValue,
    new: IndexValue,
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
        let dir_path: &Path = config.dir.as_path();

        fs::create_dir_all(dir_path)?;

        let lock_file = LockFile::acquire(dir_path, StoreType::Bitcask)?;

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

        let in_mem_index: [HashMap<Vec<u8>, IndexValue>; INDEX_BUCKETS_NUM] =
            std::array::from_fn(|_| HashMap::new());

        let mut loader = Loader {
            readers: read_handlers,
            index: in_mem_index,
            writer: writer_state,
        };

        // replay the log
        loader.replay(&config)?;

        let (tx, rx) = sync_channel::<()>(1);

        let shared = Arc::new(Shared {
            config,
            index: Index::from(loader.index),
            readers: ArcSwap::from_pointee(loader.readers),
            writer: Mutex::new(loader.writer),
            lock_file,
        });

        let weak = Arc::downgrade(&shared);

        // compaction thread
        let handle = thread::spawn(move || {
            while rx.recv().is_ok() {
                let Some(shared) = weak.upgrade() else { break };
                if let Err(e) = shared.run_compaction() {
                    debug!("compaction failed, will retry on next signal: {e}");
                }
            }
        });

        let internal = Arc::new(Handle {
            shared,
            compaction_tx: Some(tx),
            compaction_thread_handle: Some(handle),
        });

        Ok(Bitcask { handle: internal })
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
        if !self
            .handle
            .shared
            .index
            .read_access(key_bytes)?
            .contains_key(key_bytes)
        {
            return Err(KvsError::KeyNotFound);
        }

        let ts = unix_now()?.as_secs();

        let command = Command::Rm { ts, key: key_bytes };
        self.append(&command)?;
        Ok(())
    }

    fn get(&self, key: &str) -> Result<Option<String>> {
        let key_bytes = key.as_bytes();

        loop {
            let Some(entry) = self
                .handle
                .shared
                .index
                .read_access(key_bytes)?
                .get(key_bytes)
                .copied()
            else {
                return Ok(None);
            };

            // a merge could delete this file when we acquired the index lock,
            // we want to retry until both data structures are in sync.
            // The guard is held across the pread: cloning the Arc<File> out
            // would bump a refcount shared by every reader of that file.
            let readers = self.handle.shared.readers.load();
            let Some(file) = readers.get(&entry.file_id) else {
                continue;
            };

            let mut buf = vec![0u8; entry.len];
            file.read_exact_at(&mut buf, entry.offset)?;

            let (deserialized, _) = Command::deserialize(&buf)?;
            let Some(value_len) = deserialized.get_value().map(<[u8]>::len) else {
                return Ok(None);
            };
            buf.drain(..buf.len() - value_len);
            return Ok(Some(String::from_utf8(buf)?));
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
    use crate::StoreType;
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

        let bitcask = Bitcask::open(Config::new(dir.path(), StoreType::Bitcask))?;

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

        let bitcask = Bitcask::open(Config::new(dir.path(), StoreType::Bitcask))?;

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

        let bitcask = Bitcask::open(Config::new(dir.path(), StoreType::Bitcask))?;

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

        let bitcask = Bitcask::open(Config::new(dir.path(), StoreType::Bitcask))?;
        bitcask.set("c", "3")?;
        assert_eq!(bitcask.get("c")?, Some("3".to_owned()));
        drop(bitcask);

        let bitcask = Bitcask::open(Config::new(dir.path(), StoreType::Bitcask))?;
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

        let bitcask = Bitcask::open(Config::new(dir.path(), StoreType::Bitcask))?;

        assert_eq!(bitcask.get("alpha")?, Some("beta".to_owned()));
        assert_eq!(bitcask.get("gamma")?, Some("delta".to_owned()));
        assert!(
            !fs::exists(dir.path().join("000001.hint"))?,
            "damaged hint file should be removed during fallback"
        );

        Ok(())
    }

    // --- crash-window recovery: each test constructs the exact directory
    // --- state a crash leaves at one step of the background merge and
    // --- asserts open() recovers correct values and a usable store.

    // Crash mid-merge, before any rename: sources intact, a half-written
    // .compact output, its hint (an orphan — no matching .data), and the
    // empty active file the merge created at snapshot time.
    #[test_log::test]
    fn crash_before_merge_rename_recovers_from_sources() -> Result<()> {
        let dir = tempdir()?;

        let src1 = Command::Set {
            ts: 1,
            key: b"k1",
            value: b"a",
        }
        .serialize();
        fs::write(dir.path().join("000001.data"), &src1)?;

        let src2 = Command::Set {
            ts: 2,
            key: b"k2",
            value: b"b",
        }
        .serialize();
        fs::write(dir.path().join("000002.data"), &src2)?;

        fs::write(
            dir.path().join("000005.data.compact"),
            b"half-written merge output",
        )?;
        fs::write(dir.path().join("000005.hint"), b"half-written hint")?;
        fs::write(dir.path().join("000010.data"), b"")?;

        let bitcask = Bitcask::open(Config::new(dir.path(), StoreType::Bitcask))?;

        assert_eq!(bitcask.get("k1")?, Some("a".to_owned()));
        assert_eq!(bitcask.get("k2")?, Some("b".to_owned()));
        assert!(
            !fs::exists(dir.path().join("000005.data.compact"))?,
            ".compact leftover should be removed on open"
        );
        assert!(
            !fs::exists(dir.path().join("000005.hint"))?,
            "orphan hint of the unfinished output should be removed on open"
        );

        // store stays usable across another cycle
        bitcask.set("k3", "c")?;
        drop(bitcask);
        let bitcask = Bitcask::open(Config::new(dir.path(), StoreType::Bitcask))?;
        assert_eq!(bitcask.get("k1")?, Some("a".to_owned()));
        assert_eq!(bitcask.get("k3")?, Some("c".to_owned()));

        Ok(())
    }

    fn merged_output(dir: &Path) -> Result<()> {
        // the live records at merge time: k's latest (copied from file 2)
        // and k2's only version (copied from file 1)
        let rec_k = Command::Set {
            ts: 3,
            key: b"k",
            value: b"new",
        }
        .serialize();
        let rec_k2 = Command::Set {
            ts: 1,
            key: b"k2",
            value: b"x",
        }
        .serialize();

        let mut output = rec_k.clone();
        output.extend_from_slice(&rec_k2);
        fs::write(dir.join("000003.data"), &output)?;

        let mut hint = IndexValue {
            file_id: 3,
            ts: 3,
            offset: 0,
            len: rec_k.len(),
        }
        .serialize(b"k");
        hint.extend_from_slice(
            &IndexValue {
                file_id: 3,
                ts: 1,
                offset: rec_k.len() as u64,
                len: rec_k2.len(),
            }
            .serialize(b"k2"),
        );
        fs::write(dir.join("000003.hint"), &hint)?;

        fs::write(dir.join("000010.data"), b"")?;
        Ok(())
    }

    // Crash after the outputs were renamed to .data but before any source
    // was deleted: sources and outputs coexist; the outputs' higher ids must
    // win over the stale copies in the sources.
    #[test_log::test]
    fn crash_after_rename_before_deletion_prefers_outputs() -> Result<()> {
        let dir = tempdir()?;

        let old_k = Command::Set {
            ts: 1,
            key: b"k",
            value: b"old",
        }
        .serialize();
        let mut src1 = old_k.clone();
        src1.extend_from_slice(
            &Command::Set {
                ts: 1,
                key: b"k2",
                value: b"x",
            }
            .serialize(),
        );
        fs::write(dir.path().join("000001.data"), &src1)?;

        let src2 = Command::Set {
            ts: 3,
            key: b"k",
            value: b"new",
        }
        .serialize();
        fs::write(dir.path().join("000002.data"), &src2)?;

        merged_output(dir.path())?;

        let bitcask = Bitcask::open(Config::new(dir.path(), StoreType::Bitcask))?;

        assert_eq!(
            bitcask.get("k")?,
            Some("new".to_owned()),
            "stale copy in an undeleted source must not win"
        );
        assert_eq!(bitcask.get("k2")?, Some("x".to_owned()));

        Ok(())
    }

    // Crash midway through source deletion: one source already gone, one
    // still present. Same guarantee as above.
    #[test_log::test]
    fn crash_during_source_deletion_prefers_outputs() -> Result<()> {
        let dir = tempdir()?;

        // source 000001.data already deleted; 000002.data survived the crash
        let src2 = Command::Set {
            ts: 3,
            key: b"k",
            value: b"new",
        }
        .serialize();
        fs::write(dir.path().join("000002.data"), &src2)?;

        merged_output(dir.path())?;

        let bitcask = Bitcask::open(Config::new(dir.path(), StoreType::Bitcask))?;

        assert_eq!(bitcask.get("k")?, Some("new".to_owned()));
        assert_eq!(bitcask.get("k2")?, Some("x".to_owned()));

        // and the next merge cycle can still clean up: store stays writable
        bitcask.set("k3", "y")?;
        assert_eq!(bitcask.get("k3")?, Some("y".to_owned()));

        Ok(())
    }
}
