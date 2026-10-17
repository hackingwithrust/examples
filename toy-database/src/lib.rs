use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// How often the sparse index records a key: every fourth record in the
/// table starts a block, and a lookup reads only the one block it needs.
const BLOCK_RECORDS: usize = 4;

/// A persistent key-value store in two files: an append-only log that
/// absorbs writes, and a sorted table on disk that answers reads — plus
/// the in-memory table of what the log holds that the disk table does not
/// yet know about.
pub struct TinyLsm {
    /// Newest writes. `Some(value)` is a live key; `None` is a tombstone
    /// that shadows an older copy of the key in the on-disk table.
    mem: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    /// Path of the append-only log, the store's write path.
    path: PathBuf,
    /// The sorted table on disk, present once the first compaction has run.
    table: Option<Sstable>,
}

impl TinyLsm {
    /// Open the store at `path`: load the sorted table's sparse index, if a
    /// table exists, then replay the log on top of it. The log is newer
    /// than the table by construction — compaction empties the log the
    /// moment it folds its records into the table — so replaying it after
    /// loading the table restores the true state. A torn tail in the log is
    /// cut; a whole record that fails its checksum stops the open with an
    /// error instead of being dropped.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let table = Sstable::load(&table_path(&path))?;
        let mut log = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)?;
        let mut bytes = Vec::new();
        log.read_to_end(&mut bytes)?;
        let mut mem = BTreeMap::new();
        let valid = replay(&bytes, &mut mem)?;
        log.set_len(valid as u64)?;
        Ok(TinyLsm { mem, path, table })
    }

    /// Insert or overwrite `key`. The write hits the log before the map.
    pub fn put(&mut self, key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> io::Result<()> {
        let key = key.into();
        let value = value.into();
        let payload = encode(b'P', &key, Some(&value));
        append(&self.path, &payload)?;
        self.mem.insert(key, Some(value));
        Ok(())
    }

    /// Return the newest value for `key`. The in-memory table wins over the
    /// on-disk one, and a tombstone in memory shadows the table's older
    /// copy. Reading the table touches the disk and can fail, so the answer
    /// comes back in a `Result` — and owned, because it may have been read
    /// out of a file rather than borrowed from memory.
    pub fn get(&self, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
        match self.mem.get(key) {
            Some(Some(value)) => return Ok(Some(value.clone())),
            Some(None) => return Ok(None),
            None => {}
        }
        match &self.table {
            Some(table) => table.lookup(key),
            None => Ok(None),
        }
    }

    /// Remove `key`, appending a tombstone when the key exists — in memory
    /// or in the on-disk table. Returns whether it did. The tombstone is
    /// what keeps the table's older copy shadowed until the next compaction
    /// drops it, so a delete of a table-only key still writes to the log.
    pub fn delete(&mut self, key: &[u8]) -> io::Result<bool> {
        let live = match self.mem.get(key) {
            Some(live) => live.is_some(),
            None => match &self.table {
                Some(table) => table.lookup(key)?.is_some(),
                None => false,
            },
        };
        if !live {
            return Ok(false);
        }
        let payload = encode(b'D', key, None);
        append(&self.path, &payload)?;
        self.mem.insert(key.to_vec(), None);
        Ok(true)
    }

    /// Fold the log and the old table into a new table — one put record per
    /// live key, in key order, tombstones gone — then empty the log. The new
    /// table lands through a temporary file and a rename, so a crash part
    /// way through leaves the old table and the log exactly as they were.
    pub fn compact(&mut self) -> io::Result<()> {
        let mut merged: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
        if let Some(table) = &self.table {
            // Compaction is the one reader allowed to walk the whole table:
            // it is about to rewrite all of it anyway.
            let bytes = std::fs::read(&table.path)?;
            for (_, payload) in walk(&bytes)? {
                if let Some((b'P', key, value)) = parse(payload) {
                    merged.insert(key.to_vec(), value.to_vec());
                }
            }
        }
        for (key, value) in &self.mem {
            match value {
                Some(value) => {
                    merged.insert(key.clone(), value.clone());
                }
                None => {
                    // a tombstone erases the table's older copy
                    merged.remove(key);
                }
            }
        }
        self.table = Some(Sstable::build(&table_path(&self.path), &merged)?);
        File::create(&self.path)?; // the table has absorbed the log: empty it
        self.mem.clear();
        Ok(())
    }
}

/// The sorted table that belongs to a log at `log`: `store.log` keeps its
/// records in `store.sst`.
fn table_path(log: &Path) -> PathBuf {
    log.with_extension("sst")
}

/// A sorted table on disk: the framed records from steps 3 and 4, written
/// in key order, with a sparse index in memory — every fourth record's key
/// and offset — so a lookup reads one block instead of the whole file.
struct Sstable {
    path: PathBuf,
    index: Vec<(Vec<u8>, u64)>,
}

impl Sstable {
    /// Load the sparse index for the table at `path`, or `None` when no
    /// table exists yet. The index is rebuilt by one sequential scan that
    /// verifies every checksum on the way; a real store writes the index
    /// into a footer at the end of the file to skip exactly this scan.
    fn load(path: &Path) -> io::Result<Option<Sstable>> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let records = walk(&bytes)?;
        if valid_end(&records) != bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "table is corrupt: records stop short of the end of the file",
            ));
        }
        let mut index = Vec::new();
        for (i, (pos, payload)) in records.iter().enumerate() {
            if i % BLOCK_RECORDS == 0 {
                let Some((_, key, _)) = parse(payload) else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "table is corrupt: unreadable record",
                    ));
                };
                index.push((key.to_vec(), *pos as u64));
            }
        }
        Ok(Some(Sstable {
            path: path.to_path_buf(),
            index,
        }))
    }

    /// Value for `key`, through the sparse index: binary-search the index
    /// for the block that could hold the key, read that block, and scan it.
    /// The scan stops at the first key past `key` — the table is sorted, so
    /// a larger key means the answer is not in the table.
    fn lookup(&self, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
        let block = self.index.partition_point(|(k, _)| k.as_slice() <= key);
        if block == 0 {
            return Ok(None); // sorts before the first indexed key
        }
        let (_, start) = self.index[block - 1];
        let end = self.index.get(block).map_or(u64::MAX, |&(_, off)| off);
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        file.take(end - start).read_to_end(&mut bytes)?;
        for (_, payload) in walk(&bytes)? {
            let Some((_, found, value)) = parse(payload) else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "table is corrupt: unreadable record",
                ));
            };
            match found.cmp(key) {
                Ordering::Equal => return Ok(Some(value.to_vec())),
                Ordering::Greater => return Ok(None),
                Ordering::Less => {}
            }
        }
        Ok(None)
    }

    /// Write `entries` as a new table at `path`, atomically: the records go
    /// to a temporary file that is fsynced and renamed over `path` once the
    /// last one lands. The map is written in its own iteration order, which
    /// is key order — sorting is the table's whole point, and it costs
    /// nothing coming out of a `BTreeMap`. Returns the table with its index.
    fn build(path: &Path, entries: &BTreeMap<Vec<u8>, Vec<u8>>) -> io::Result<Sstable> {
        let tmp = path.with_extension("sst.tmp");
        let mut index = Vec::new();
        {
            let mut out = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp)?;
            let mut offset = 0u64;
            for (i, (key, value)) in entries.iter().enumerate() {
                if i % BLOCK_RECORDS == 0 {
                    index.push((key.clone(), offset));
                }
                let record = frame(&encode(b'P', key, Some(value)));
                out.write_all(&record)?;
                offset += record.len() as u64;
            }
            out.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(Sstable {
            path: path.to_path_buf(),
            index,
        })
    }
}

/// Frame one payload as a record and append it to the log at `path`. The
/// handle is opened per call: one extra `open` per write is noise next to
/// the flush, and it keeps no handle alive that a rename could orphan.
fn append(path: &Path, payload: &[u8]) -> io::Result<()> {
    let mut log = OpenOptions::new().append(true).open(path)?;
    log.write_all(&frame(payload))?;
    log.flush()
}

/// FNV-1a, 32-bit: three lines, deterministic, and enough to catch a torn
/// or corrupted record on the way back in.
fn checksum(payload: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for &byte in payload {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// One log payload: op byte, then the key and value, each length-prefixed.
/// A delete carries no value — its payload ends after an empty length.
fn encode(op: u8, key: &[u8], value: Option<&[u8]>) -> Vec<u8> {
    let value = value.unwrap_or(&[]);
    let mut payload = Vec::with_capacity(9 + key.len() + value.len());
    payload.push(op);
    payload.extend_from_slice(&(key.len() as u32).to_le_bytes());
    payload.extend_from_slice(key);
    payload.extend_from_slice(&(value.len() as u32).to_le_bytes());
    payload.extend_from_slice(value);
    payload
}

/// Frame one payload as a record: payload length, checksum, payload.
fn frame(payload: &[u8]) -> Vec<u8> {
    let mut record = Vec::with_capacity(8 + payload.len());
    record.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    record.extend_from_slice(&checksum(payload).to_le_bytes());
    record.extend_from_slice(payload);
    record
}

/// Walk the framed records in `bytes`, returning each one's offset and
/// payload. The walk stops at a torn tail — a record a crash cut short —
/// and a whole record whose checksum fails is an error, not a record to
/// drop. Both the log and the table are read through this.
fn walk(bytes: &[u8]) -> io::Result<Vec<(usize, &[u8])>> {
    let mut records = Vec::new();
    let mut rest = bytes;
    let mut pos = 0;
    while rest.len() >= 8 {
        let len = u32::from_le_bytes(rest[..4].try_into().unwrap()) as usize;
        let sum = u32::from_le_bytes(rest[4..8].try_into().unwrap());
        if len > rest.len() - 8 {
            break; // the payload never finished landing: torn tail
        }
        let (payload, tail) = rest[8..].split_at(len);
        if checksum(payload) != sum {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "corrupt record: checksum mismatch",
            ));
        }
        records.push((pos, payload));
        rest = tail;
        pos += 8 + len;
    }
    Ok(records)
}

/// Offset just past the last walked record: the end of the valid prefix.
fn valid_end(records: &[(usize, &[u8])]) -> usize {
    records
        .last()
        .map_or(0, |(pos, payload)| pos + 8 + payload.len())
}

/// Replay the log's records into `mem` and return the offset where the
/// valid prefix ends — everything after that, the caller cuts.
fn replay(bytes: &[u8], mem: &mut BTreeMap<Vec<u8>, Option<Vec<u8>>>) -> io::Result<usize> {
    let records = walk(bytes)?;
    let end = valid_end(&records);
    for (_, payload) in &records {
        apply(payload, mem)?;
    }
    Ok(end)
}

/// Apply one record payload to `mem`: a put stores its value, a delete
/// stores a tombstone. `Err` when the payload is not shaped like a record —
/// with the checksum already verified, that is a bug in the writer, not a
/// damaged file.
fn apply(payload: &[u8], mem: &mut BTreeMap<Vec<u8>, Option<Vec<u8>>>) -> io::Result<()> {
    let Some((op, key, value)) = parse(payload) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "corrupt record: malformed payload",
        ));
    };
    match op {
        b'P' => {
            mem.insert(key.to_vec(), Some(value.to_vec()));
            Ok(())
        }
        b'D' => {
            mem.insert(key.to_vec(), None);
            Ok(())
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "corrupt record: unknown operation",
        )),
    }
}

/// Parse one payload into `(op, key, value)`: an op byte, then the key and
/// value, each length-prefixed. `None` when the payload is not shaped like
/// a record.
fn parse(payload: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&op, rest) = payload.split_first()?;
    let (klen, rest) = read_u32(rest)?;
    let (key, rest) = take(klen as usize, rest)?;
    let (vlen, rest) = read_u32(rest)?;
    let (value, rest) = take(vlen as usize, rest)?;
    if !rest.is_empty() {
        return None;
    }
    Some((op, key, value))
}

/// Pop a little-endian `u32` off the front of `bytes`.
fn read_u32(bytes: &[u8]) -> Option<(u32, &[u8])> {
    let head = bytes.get(..4)?;
    Some((u32::from_le_bytes(head.try_into().ok()?), &bytes[4..]))
}

/// Split `n` bytes off the front of `bytes`, or `None` if it is shorter.
fn take(n: usize, bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    if bytes.len() < n {
        None
    } else {
        Some(bytes.split_at(n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A unique scratch path per test: the tests in this module run in
    /// parallel, and a shared path would have them overwriting each other.
    fn temp_log(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("tinylsm-{}-{}.log", name, std::process::id()));
        path
    }

    /// A store now lives in up to three files: the log, the sorted table
    /// beside it, and the table's temporary file mid-compaction.
    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("sst"));
        let _ = std::fs::remove_file(path.with_extension("sst.tmp"));
    }

    #[test]
    fn put_then_get() {
        let path = temp_log("put-get");
        let mut store = TinyLsm::open(&path).unwrap();
        store.put(b"alpha", b"one").unwrap();
        assert_eq!(store.get(b"alpha").unwrap(), Some(b"one".to_vec()));
        cleanup(&path);
    }

    #[test]
    fn get_missing_is_none() {
        let path = temp_log("missing");
        let store = TinyLsm::open(&path).unwrap();
        assert_eq!(store.get(b"nope").unwrap(), None);
        cleanup(&path);
    }

    #[test]
    fn put_overwrites() {
        let path = temp_log("overwrite");
        let mut store = TinyLsm::open(&path).unwrap();
        store.put(b"alpha", b"one").unwrap();
        store.put(b"alpha", b"two").unwrap();
        assert_eq!(store.get(b"alpha").unwrap(), Some(b"two".to_vec()));
        cleanup(&path);
    }

    #[test]
    fn delete_removes_and_reports() {
        let path = temp_log("delete");
        let mut store = TinyLsm::open(&path).unwrap();
        store.put(b"alpha", b"one").unwrap();
        assert!(store.delete(b"alpha").unwrap());
        assert_eq!(store.get(b"alpha").unwrap(), None);
        assert!(!store.delete(b"alpha").unwrap());
        cleanup(&path);
    }

    #[test]
    fn keys_are_independent() {
        let path = temp_log("independent");
        let mut store = TinyLsm::open(&path).unwrap();
        store.put(b"alpha", b"one").unwrap();
        store.put(b"beta", b"two").unwrap();
        assert_eq!(store.get(b"beta").unwrap(), Some(b"two".to_vec()));
        store.delete(b"alpha").unwrap();
        assert_eq!(store.get(b"beta").unwrap(), Some(b"two".to_vec()));
        cleanup(&path);
    }

    #[test]
    fn put_appends_one_record() {
        let path = temp_log("append");
        let mut store = TinyLsm::open(&path).unwrap();
        store.put(b"alpha", b"one").unwrap();
        drop(store);

        let bytes = std::fs::read(&path).unwrap();
        // header (4 + 4) + payload (op 1, klen 4, key 5, vlen 4, value 3)
        assert_eq!(bytes.len(), 8 + 17);
        assert_eq!(bytes[8], b'P');
        assert_eq!(&bytes[13..18], b"alpha");
        assert_eq!(&bytes[22..25], b"one");
        cleanup(&path);
    }

    #[test]
    fn delete_appends_a_tombstone() {
        let path = temp_log("tombstone");
        let mut store = TinyLsm::open(&path).unwrap();
        store.put(b"alpha", b"one").unwrap();
        store.delete(b"alpha").unwrap();
        drop(store);

        let bytes = std::fs::read(&path).unwrap();
        // the put record (25 bytes), then a delete record with no value:
        // header 8 + payload (op 1, klen 4, key 5, vlen 4) = 22
        assert_eq!(bytes.len(), 25 + 22);
        assert_eq!(bytes[25 + 8], b'D');
        cleanup(&path);
    }

    #[test]
    fn deleting_a_missing_key_writes_nothing() {
        let path = temp_log("no-tombstone");
        let mut store = TinyLsm::open(&path).unwrap();
        assert!(!store.delete(b"ghost").unwrap());
        assert_eq!(std::fs::read(&path).unwrap().len(), 0);
        cleanup(&path);
    }

    #[test]
    fn open_replays_writes() {
        let path = temp_log("replay");
        {
            let mut store = TinyLsm::open(&path).unwrap();
            store.put(b"alpha", b"one").unwrap();
            store.put(b"beta", b"two").unwrap();
        }
        let store = TinyLsm::open(&path).unwrap();
        assert_eq!(store.get(b"alpha").unwrap(), Some(b"one".to_vec()));
        assert_eq!(store.get(b"beta").unwrap(), Some(b"two".to_vec()));
        cleanup(&path);
    }

    #[test]
    fn open_replays_the_last_write() {
        let path = temp_log("replay-last");
        {
            let mut store = TinyLsm::open(&path).unwrap();
            store.put(b"alpha", b"one").unwrap();
            store.put(b"alpha", b"two").unwrap();
        }
        let store = TinyLsm::open(&path).unwrap();
        assert_eq!(store.get(b"alpha").unwrap(), Some(b"two".to_vec()));
        cleanup(&path);
    }

    #[test]
    fn open_replays_tombstones() {
        let path = temp_log("replay-tombstone");
        {
            let mut store = TinyLsm::open(&path).unwrap();
            store.put(b"alpha", b"one").unwrap();
            store.put(b"beta", b"two").unwrap();
            store.delete(b"alpha").unwrap();
        }
        let store = TinyLsm::open(&path).unwrap();
        assert_eq!(store.get(b"alpha").unwrap(), None);
        assert_eq!(store.get(b"beta").unwrap(), Some(b"two".to_vec()));
        cleanup(&path);
    }

    #[test]
    fn open_a_missing_file_starts_empty() {
        let path = temp_log("fresh");
        let store = TinyLsm::open(&path).unwrap();
        assert_eq!(store.get(b"alpha").unwrap(), None);
        assert_eq!(std::fs::read(&path).unwrap().len(), 0);
        cleanup(&path);
    }

    /// The first 10 bytes of a well-formed put record for `beta`, simulating
    /// a crash that cut the record short after its header and 2 payload
    /// bytes landed.
    fn torn_tail(path: &Path) {
        let payload = encode(b'P', b"beta", Some(b"two"));
        let record = frame(&payload);
        let mut log = OpenOptions::new().append(true).open(path).unwrap();
        log.write_all(&record[..10]).unwrap();
    }

    #[test]
    fn open_cuts_a_torn_tail() {
        let path = temp_log("torn");
        {
            let mut store = TinyLsm::open(&path).unwrap();
            store.put(b"alpha", b"one").unwrap();
        }
        torn_tail(&path);

        let store = TinyLsm::open(&path).unwrap();
        assert_eq!(store.get(b"alpha").unwrap(), Some(b"one".to_vec()));
        assert_eq!(store.get(b"beta").unwrap(), None);
        drop(store);
        // the tail is gone, and the file ends after the last whole record
        assert_eq!(std::fs::read(&path).unwrap().len(), 25);
        cleanup(&path);
    }

    #[test]
    fn a_reopened_store_keeps_appending_cleanly() {
        let path = temp_log("append-after-torn");
        {
            let mut store = TinyLsm::open(&path).unwrap();
            store.put(b"alpha", b"one").unwrap();
        }
        torn_tail(&path);

        let mut store = TinyLsm::open(&path).unwrap();
        store.put(b"beta", b"two").unwrap();
        drop(store);

        let bytes = std::fs::read(&path).unwrap();
        // the cut put (25) plus a whole put record for beta (24): no hole,
        // no half record between them
        assert_eq!(bytes.len(), 25 + 24);
        let store = TinyLsm::open(&path).unwrap();
        assert_eq!(store.get(b"beta").unwrap(), Some(b"two".to_vec()));
        cleanup(&path);
    }

    #[test]
    fn open_refuses_a_corrupt_record() {
        let path = temp_log("corrupt");
        {
            let mut store = TinyLsm::open(&path).unwrap();
            store.put(b"alpha", b"one").unwrap();
        }
        // flip one value byte: the record is whole, but not what was written
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[22] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();

        let err = TinyLsm::open(&path).err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        // the file is left alone — nothing is silently discarded
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        cleanup(&path);
    }

    #[test]
    fn compact_empties_the_log_and_fills_the_table() {
        let path = temp_log("compact-drains");
        let mut store = TinyLsm::open(&path).unwrap();
        store.put(b"alpha", b"one").unwrap();
        store.put(b"beta", b"two").unwrap();
        store.compact().unwrap();

        // the log is drained; the table beside it holds both records,
        // 25 + 24 bytes in the step 3 format
        assert_eq!(std::fs::read(&path).unwrap().len(), 0);
        let table = std::fs::read(path.with_extension("sst")).unwrap();
        assert_eq!(table.len(), 25 + 24);

        let store = TinyLsm::open(&path).unwrap();
        assert_eq!(store.get(b"alpha").unwrap(), Some(b"one".to_vec()));
        assert_eq!(store.get(b"beta").unwrap(), Some(b"two".to_vec()));
        cleanup(&path);
    }

    #[test]
    fn compact_drops_tombstones_and_superseded_values() {
        let path = temp_log("compact-shrinks");
        let mut store = TinyLsm::open(&path).unwrap();
        store.put(b"alpha", b"one").unwrap();
        store.put(b"alpha", b"two").unwrap();
        store.put(b"beta", b"two").unwrap();
        store.delete(b"alpha").unwrap();
        store.compact().unwrap();

        // one record survives: beta's put, 8-byte header + 16-byte payload
        let table = std::fs::read(path.with_extension("sst")).unwrap();
        assert_eq!(table.len(), 24);
        let store = TinyLsm::open(&path).unwrap();
        assert_eq!(store.get(b"alpha").unwrap(), None);
        assert_eq!(store.get(b"beta").unwrap(), Some(b"two".to_vec()));
        cleanup(&path);
    }

    #[test]
    fn compact_writes_keys_in_sorted_order() {
        let path = temp_log("compact-sorted");
        let mut store = TinyLsm::open(&path).unwrap();
        store.put(b"zeta", b"one").unwrap();
        store.put(b"alpha", b"two").unwrap();
        store.put(b"mid", b"three").unwrap();
        store.compact().unwrap();

        // the first record in the table carries the first key in the map's
        // order, which is sorted
        let table = std::fs::read(path.with_extension("sst")).unwrap();
        let len = u32::from_le_bytes(table[..4].try_into().unwrap()) as usize;
        let payload = &table[8..8 + len];
        assert_eq!(payload[0], b'P');
        let klen = u32::from_le_bytes(payload[1..5].try_into().unwrap()) as usize;
        assert_eq!(&payload[5..5 + klen], b"alpha");
        cleanup(&path);
    }

    #[test]
    fn a_tombstone_in_memory_shadows_the_table() {
        let path = temp_log("shadow");
        let mut store = TinyLsm::open(&path).unwrap();
        store.put(b"alpha", b"one").unwrap();
        store.compact().unwrap(); // alpha now lives only in the table

        assert!(store.delete(b"alpha").unwrap());
        // the tombstone in memory keeps the table's older copy hidden
        assert_eq!(store.get(b"alpha").unwrap(), None);
        cleanup(&path);
    }

    #[test]
    fn a_tombstone_survives_a_reopen() {
        let path = temp_log("shadow-reopen");
        let mut store = TinyLsm::open(&path).unwrap();
        store.put(b"alpha", b"one").unwrap();
        store.compact().unwrap();
        store.delete(b"alpha").unwrap();
        drop(store);

        // the log replays the tombstone over the table's older copy
        let store = TinyLsm::open(&path).unwrap();
        assert_eq!(store.get(b"alpha").unwrap(), None);
        cleanup(&path);
    }

    #[test]
    fn compaction_erases_what_a_tombstone_points_at() {
        let path = temp_log("compact-erase");
        let mut store = TinyLsm::open(&path).unwrap();
        store.put(b"alpha", b"one").unwrap();
        store.put(b"beta", b"two").unwrap();
        store.compact().unwrap();
        store.delete(b"alpha").unwrap();
        store.compact().unwrap();

        // the tombstone removes alpha from the merged table: beta's put,
        // 24 bytes, and nothing else
        let table = std::fs::read(path.with_extension("sst")).unwrap();
        assert_eq!(table.len(), 24);
        let store = TinyLsm::open(&path).unwrap();
        assert_eq!(store.get(b"alpha").unwrap(), None);
        assert_eq!(store.get(b"beta").unwrap(), Some(b"two".to_vec()));
        cleanup(&path);
    }

    #[test]
    fn a_store_keeps_working_after_compaction() {
        let path = temp_log("compact-continue");
        let mut store = TinyLsm::open(&path).unwrap();
        store.put(b"alpha", b"one").unwrap();
        store.delete(b"alpha").unwrap();
        store.compact().unwrap();
        // writes after the rewrite land in the log and shadow the table
        store.put(b"beta", b"two").unwrap();
        store.put(b"alpha", b"three").unwrap();
        drop(store);

        let store = TinyLsm::open(&path).unwrap();
        assert_eq!(store.get(b"alpha").unwrap(), Some(b"three".to_vec()));
        assert_eq!(store.get(b"beta").unwrap(), Some(b"two".to_vec()));
        cleanup(&path);
    }

    #[test]
    fn compacting_an_empty_store_writes_an_empty_table() {
        let path = temp_log("compact-empty");
        let mut store = TinyLsm::open(&path).unwrap();
        store.compact().unwrap();
        assert_eq!(std::fs::read(&path).unwrap().len(), 0);
        assert_eq!(std::fs::read(path.with_extension("sst")).unwrap().len(), 0);
        cleanup(&path);
    }

    #[test]
    fn compact_leaves_no_temporary_file_behind() {
        let path = temp_log("compact-tmp");
        let mut store = TinyLsm::open(&path).unwrap();
        store.put(b"alpha", b"one").unwrap();
        store.compact().unwrap();

        let tmp = path.with_extension("sst.tmp");
        assert!(!tmp.exists());
        cleanup(&path);
    }

    #[test]
    fn a_temporary_file_left_by_a_crash_is_replaced() {
        let path = temp_log("compact-stale-tmp");
        let mut store = TinyLsm::open(&path).unwrap();
        store.put(b"alpha", b"one").unwrap();
        // a crash mid-compaction leaves a half-written temp file behind;
        // the next compaction truncates it and carries on
        let tmp = path.with_extension("sst.tmp");
        std::fs::write(&tmp, b"garbage bytes").unwrap();

        store.compact().unwrap();
        assert!(!tmp.exists());
        let store = TinyLsm::open(&path).unwrap();
        assert_eq!(store.get(b"alpha").unwrap(), Some(b"one".to_vec()));
        cleanup(&path);
    }

    #[test]
    fn compaction_round_trips_through_a_reopen() {
        let path = temp_log("compact-roundtrip");
        {
            let mut store = TinyLsm::open(&path).unwrap();
            store.put(b"alpha", b"one").unwrap();
            store.put(b"beta", b"two").unwrap();
            store.put(b"alpha", b"three").unwrap();
            store.delete(b"beta").unwrap();
            store.compact().unwrap();
        }
        let store = TinyLsm::open(&path).unwrap();
        assert_eq!(store.get(b"alpha").unwrap(), Some(b"three".to_vec()));
        assert_eq!(store.get(b"beta").unwrap(), None);
        cleanup(&path);
    }

    #[test]
    fn a_lookup_after_a_reopen_uses_the_rebuilt_index() {
        let path = temp_log("index-rebuilt");
        {
            let mut store = TinyLsm::open(&path).unwrap();
            for (key, value) in [
                ("alpha", "one"),
                ("beta", "two"),
                ("gamma", "three"),
                ("delta", "four"),
                ("echo", "five"),
                ("fox", "six"),
            ] {
                store.put(key, value).unwrap();
            }
            store.compact().unwrap();
        }
        // the index is gone with the process; the reopen rebuilds it from
        // the table, and every lookup through it lands on the right record
        let store = TinyLsm::open(&path).unwrap();
        for (key, value) in [
            ("alpha", "one"),
            ("beta", "two"),
            ("gamma", "three"),
            ("delta", "four"),
            ("echo", "five"),
            ("fox", "six"),
        ] {
            assert_eq!(
                store.get(key.as_bytes()).unwrap(),
                Some(value.as_bytes().to_vec()),
                "wrong value for {key}"
            );
        }
        assert_eq!(store.get(b"goose").unwrap(), None);
        cleanup(&path);
    }

    #[test]
    fn reopen_refuses_a_truncated_table() {
        let path = temp_log("table-torn");
        {
            let mut store = TinyLsm::open(&path).unwrap();
            store.put(b"alpha", b"one").unwrap();
            store.compact().unwrap();
        }
        // the table is installed by an atomic rename, so a short table is
        // not a torn write to recover from — it is damage, and it is refused
        let table = path.with_extension("sst");
        let mut bytes = std::fs::read(&table).unwrap();
        bytes.truncate(bytes.len() - 4);
        std::fs::write(&table, &bytes).unwrap();

        let err = TinyLsm::open(&path).err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        cleanup(&path);
    }
}
