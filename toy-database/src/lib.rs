use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// How often the sparse index records a key: every fourth record in the
/// table starts a block, and a lookup reads only the one block it needs.
const BLOCK_RECORDS: usize = 4;

/// How many sorted tables may pile up before compaction merges all of
/// them back into one. Below that, a compaction only flushes the log.
const MERGE_TABLES: usize = 4;

/// A persistent key-value store in one log plus any number of sorted
/// tables: the append-only log absorbs writes, compaction flushes it into
/// a new numbered table — cheap, because it writes only what the log
/// holds — and once `MERGE_TABLES` tables have piled up, a merge folds
/// all of them back into one. Lookups walk the tables newest first, and
/// the in-memory table sits in front of all of them.
pub struct TinyLsm {
    /// Newest writes. `Some(value)` is a live key; `None` is a tombstone
    /// that shadows any older copy of the key in the on-disk tables.
    mem: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    /// Path of the append-only log, the store's write path.
    path: PathBuf,
    /// The sorted tables on disk, newest first. Each flush adds one; a
    /// merge replaces the whole list with a single table.
    tables: Vec<Sstable>,
}

impl TinyLsm {
    /// Open the store at `path`: scan the directory for the store's
    /// numbered tables and load their sparse indexes, then replay the log
    /// on top. The log is newer than every table by construction — a
    /// flush empties the log the moment it folds its records into a
    /// table — so replaying it after loading the tables restores the true
    /// state. A torn tail in the log is cut; a whole record that fails
    /// its checksum stops the open with an error instead of being
    /// dropped.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mut tables = load_tables(&path)?;
        tables.sort_by(|a, b| b.number.cmp(&a.number));
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
        Ok(TinyLsm { mem, path, tables })
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

    /// Return the newest value for `key`. The in-memory table wins over
    /// every on-disk one, and a tombstone anywhere in front of the answer
    /// shadows the older copies behind it. Reading the tables touches the
    /// disk and can fail, so the answer comes back in a `Result` — and
    /// owned, because it may have been read out of a file rather than
    /// borrowed from memory.
    pub fn get(&self, key: &[u8]) -> io::Result<Option<Vec<u8>>> {
        match self.mem.get(key) {
            Some(Some(value)) => return Ok(Some(value.clone())),
            Some(None) => return Ok(None),
            None => {}
        }
        for table in &self.tables {
            match table.lookup(key)? {
                Answer::Value(value) => return Ok(Some(value)),
                Answer::Tombstone => return Ok(None),
                Answer::Missing => {}
            }
        }
        Ok(None)
    }

    /// Remove `key`, appending a tombstone when the key exists — in
    /// memory, in a table, or both. Returns whether it did. A tombstone
    /// found on disk counts as "already deleted", not as a live key, so
    /// deleting twice writes one tombstone, not two.
    pub fn delete(&mut self, key: &[u8]) -> io::Result<bool> {
        let live = match self.mem.get(key) {
            Some(live) => live.is_some(),
            None => {
                let mut live = false;
                for table in &self.tables {
                    match table.lookup(key)? {
                        Answer::Value(_) => {
                            live = true;
                            break;
                        }
                        Answer::Tombstone => break,
                        Answer::Missing => {}
                    }
                }
                live
            }
        };
        if !live {
            return Ok(false);
        }
        let payload = encode(b'D', key, None);
        append(&self.path, &payload)?;
        self.mem.insert(key.to_vec(), None);
        Ok(true)
    }

    /// Make the log small again, in two moves. First a flush: write the
    /// memory table — live keys and tombstones alike — into a new numbered
    /// table, then empty the log. This is the cheap part, and it is all a
    /// compaction does until `MERGE_TABLES` tables have piled up. Then a
    /// merge: fold every table into one new one, oldest first, so each
    /// newer copy of a key overwrites the older. The merged table lands
    /// through a temporary file and a rename; the old files are removed
    /// only after the rename, so a crash part way through leaves the
    /// store correct either way.
    pub fn compact(&mut self) -> io::Result<()> {
        if !self.mem.is_empty() {
            let number = self.next_table_number();
            let table = Sstable::build(&table_path(&self.path, number), number, &self.mem)?;
            self.tables.insert(0, table);
            File::create(&self.path)?; // the table has absorbed the log: empty it
            self.mem.clear();
        }
        if self.tables.len() >= MERGE_TABLES {
            self.merge_tables()?;
        }
        Ok(())
    }

    /// The number for the next table this store writes. Tables are kept
    /// newest first, so the newest table's number, plus one, is a number
    /// no file on disk is using.
    fn next_table_number(&self) -> u64 {
        self.tables.first().map_or(1, |t| t.number + 1)
    }

    /// Fold every table into one new one and remove the old files. The
    /// walk goes oldest first, so a newer put overwrites an older copy of
    /// its key, and a tombstone lands in the merged map over whatever the
    /// older tables held. Tombstones come out the other side alive: the
    /// scan in `load_tables` cannot tell an obsolete file from a live
    /// one, so no merge in this store may drop them.
    fn merge_tables(&mut self) -> io::Result<()> {
        let mut merged: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();
        for table in self.tables.iter().rev() {
            // A merge is the one reader allowed to walk whole tables:
            // it is about to rewrite all of them anyway.
            let bytes = std::fs::read(&table.path)?;
            for (_, payload) in walk(&bytes)? {
                match parse(payload) {
                    Some((b'P', key, value)) => {
                        merged.insert(key.to_vec(), Some(value.to_vec()));
                    }
                    Some((b'D', key, _)) => {
                        merged.insert(key.to_vec(), None);
                    }
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "table is corrupt: unreadable record",
                        ));
                    }
                }
            }
        }
        let number = self.next_table_number();
        let table = Sstable::build(&table_path(&self.path, number), number, &merged)?;
        let old: Vec<PathBuf> = self.tables.iter().map(|t| t.path.clone()).collect();
        self.tables.clear();
        self.tables.push(table);
        for path in old {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }
}

/// Path of table number `n` for a log at `log`: `store.log` keeps its
/// tables in `store.1.sst`, `store.2.sst`, and so on — the `.log`
/// extension is replaced, not appended to.
fn table_path(log: &Path, n: u64) -> PathBuf {
    log.with_extension(format!("{n}.sst"))
}

/// Find every table belonging to the log at `log` — files beside it named
/// `<stem>.<number>.sst` — and load their sparse indexes. The scan is the
/// store's whole notion of which files are live; a table a crashed merge
/// failed to delete looks exactly like a live one, which is why tombstones
/// survive merges. A temporary file mid-write ends in `.tmp` and is
/// skipped.
fn load_tables(log: &Path) -> io::Result<Vec<Sstable>> {
    let dir = log.parent().unwrap_or(Path::new("."));
    let dir = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
    let prefix = format!("{}.", log.file_stem().unwrap_or_default().to_string_lossy());
    let mut tables = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(rest) = name.strip_prefix(&prefix) else {
            continue;
        };
        let Some(number) = rest.strip_suffix(".sst") else {
            continue;
        };
        let Ok(number) = number.parse::<u64>() else {
            continue;
        };
        tables.push(Sstable::load(&entry.path(), number)?);
    }
    Ok(tables)
}

/// What one table knows about a key. Three answers, not two: a live
/// value, a delete that is newer than anything an older table might
/// hold, and nothing at all. The distinction is what makes deletes work
/// across several tables — a tombstone found in a newer table must stop
/// the walk, or an older table's copy of the key would resurface.
enum Answer {
    Value(Vec<u8>),
    Tombstone,
    Missing,
}

/// A sorted table on disk: the framed records from steps 3 and 4 — puts
/// and tombstones alike, written in key order — with a sparse index in
/// memory, every fourth record's key and offset, so a lookup reads one
/// block instead of the whole file.
struct Sstable {
    /// This table's number, the `1` in `tinylsm.1.sst`. Higher is newer.
    number: u64,
    path: PathBuf,
    index: Vec<(Vec<u8>, u64)>,
}

impl Sstable {
    /// Load the sparse index for the table at `path`. The index is
    /// rebuilt by one sequential scan that verifies every checksum on the
    /// way; a real store writes the index into a footer at the end of the
    /// file to skip exactly this scan.
    fn load(path: &Path, number: u64) -> io::Result<Sstable> {
        let bytes = std::fs::read(path)?;
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
        Ok(Sstable {
            number,
            path: path.to_path_buf(),
            index,
        })
    }

    /// What this table knows about `key`, through the sparse index:
    /// binary-search the index for the block that could hold the key,
    /// read that block, and scan it. The scan stops at the first key past
    /// `key` — the table is sorted, so a larger key means the answer is
    /// not in the table.
    fn lookup(&self, key: &[u8]) -> io::Result<Answer> {
        let block = self.index.partition_point(|(k, _)| k.as_slice() <= key);
        if block == 0 {
            return Ok(Answer::Missing); // sorts before the first indexed key
        }
        let (_, start) = self.index[block - 1];
        let end = self.index.get(block).map_or(u64::MAX, |&(_, off)| off);
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        file.take(end - start).read_to_end(&mut bytes)?;
        for (_, payload) in walk(&bytes)? {
            let Some((op, found, value)) = parse(payload) else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "table is corrupt: unreadable record",
                ));
            };
            match found.cmp(key) {
                Ordering::Equal => match op {
                    b'P' => return Ok(Answer::Value(value.to_vec())),
                    b'D' => return Ok(Answer::Tombstone),
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "table is corrupt: unknown operation",
                        ));
                    }
                },
                Ordering::Greater => return Ok(Answer::Missing),
                Ordering::Less => {}
            }
        }
        Ok(Answer::Missing)
    }

    /// Write `entries` as a new table at `path`, atomically: the records
    /// go to a temporary file that is fsynced and renamed into place once
    /// the last one lands. Live keys are written as puts, tombstones as
    /// deletes. The map is written in its own iteration order, which is
    /// key order — sorting is the table's whole point, and it costs
    /// nothing coming out of a `BTreeMap`. Returns the table with its
    /// index.
    fn build(
        path: &Path,
        number: u64,
        entries: &BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    ) -> io::Result<Sstable> {
        let tmp = path.with_extension("tmp");
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
                let op = if value.is_some() { b'P' } else { b'D' };
                let record = frame(&encode(op, key, value.as_deref()));
                out.write_all(&record)?;
                offset += record.len() as u64;
            }
            out.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(Sstable {
            number,
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
/// drop. Both the log and the tables are read through this.
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

    /// A unique scratch directory per test, with the store's log inside
    /// it. The tests in this module run in parallel, and a shared
    /// directory would have them overwriting each other; a directory of
    /// their own also means cleanup can take the tables with it.
    fn temp_log(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("tinylsm-{}-{}", name, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("tinylsm.log")
    }

    /// The store lives in up to four kinds of file: the log, the numbered
    /// tables beside it, and a table's temporary file mid-write. Removing
    /// the directory takes all of them.
    fn cleanup(log: &Path) {
        if let Some(dir) = log.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// The table files beside `log`, sorted by name — which, for the
    /// single-digit numbers this suite produces, is numeric order too.
    fn sst_files(log: &Path) -> Vec<PathBuf> {
        let dir = log.parent().unwrap();
        let prefix = format!("{}.", log.file_stem().unwrap().to_string_lossy());
        let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.starts_with(&prefix) && name.ends_with(".sst")
            })
            .map(|e| e.path())
            .collect();
        found.sort();
        found
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
    fn compact_flushes_the_log_into_a_numbered_table() {
        let path = temp_log("compact-drains");
        let mut store = TinyLsm::open(&path).unwrap();
        store.put(b"alpha", b"one").unwrap();
        store.put(b"beta", b"two").unwrap();
        store.compact().unwrap();

        // the log is drained; the first table beside it holds both
        // records, 25 + 24 bytes in the step 3 format
        assert_eq!(std::fs::read(&path).unwrap().len(), 0);
        let tables = sst_files(&path);
        assert_eq!(tables.len(), 1);
        assert!(tables[0].ends_with("tinylsm.1.sst"));
        assert_eq!(std::fs::read(&tables[0]).unwrap().len(), 25 + 24);

        let store = TinyLsm::open(&path).unwrap();
        assert_eq!(store.get(b"alpha").unwrap(), Some(b"one".to_vec()));
        assert_eq!(store.get(b"beta").unwrap(), Some(b"two".to_vec()));
        cleanup(&path);
    }

    #[test]
    fn a_flush_writes_its_tombstones() {
        let path = temp_log("flush-tombstone");
        let mut store = TinyLsm::open(&path).unwrap();
        store.put(b"alpha", b"one").unwrap();
        store.put(b"alpha", b"two").unwrap();
        store.put(b"beta", b"two").unwrap();
        store.delete(b"alpha").unwrap();
        store.compact().unwrap();

        // the flush collapses superseded values, but the tombstone goes
        // into the table as a delete record and stays there: alpha's
        // delete (22 bytes) plus beta's put (24 bytes)
        let tables = sst_files(&path);
        assert_eq!(tables.len(), 1);
        assert_eq!(std::fs::read(&tables[0]).unwrap().len(), 22 + 24);
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
        let tables = sst_files(&path);
        let table = std::fs::read(&tables[0]).unwrap();
        let len = u32::from_le_bytes(table[..4].try_into().unwrap()) as usize;
        let payload = &table[8..8 + len];
        assert_eq!(payload[0], b'P');
        let klen = u32::from_le_bytes(payload[1..5].try_into().unwrap()) as usize;
        assert_eq!(&payload[5..5 + klen], b"alpha");
        cleanup(&path);
    }

    #[test]
    fn a_tombstone_in_memory_shadows_the_tables() {
        let path = temp_log("shadow");
        let mut store = TinyLsm::open(&path).unwrap();
        store.put(b"alpha", b"one").unwrap();
        store.compact().unwrap(); // alpha now lives only in a table

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
    fn a_tombstone_in_a_newer_table_shadows_an_older_one() {
        let path = temp_log("shadow-tables");
        let mut store = TinyLsm::open(&path).unwrap();
        store.put(b"alpha", b"one").unwrap();
        store.compact().unwrap(); // table 1: alpha's put
        store.put(b"alpha", b"two").unwrap();
        store.compact().unwrap(); // table 2: alpha's newer put
        store.delete(b"alpha").unwrap();
        store.compact().unwrap(); // table 3: alpha's tombstone

        // three tables, and the tombstone in the newest one wins
        assert_eq!(sst_files(&path).len(), 3);
        assert_eq!(store.get(b"alpha").unwrap(), None);
        drop(store);
        let store = TinyLsm::open(&path).unwrap();
        assert_eq!(store.get(b"alpha").unwrap(), None);
        cleanup(&path);
    }

    #[test]
    fn a_lookup_walks_the_tables_newest_first() {
        let path = temp_log("newest-first");
        let mut store = TinyLsm::open(&path).unwrap();
        store.put(b"alpha", b"one").unwrap();
        store.compact().unwrap();
        store.put(b"alpha", b"two").unwrap();
        store.compact().unwrap();

        // both tables hold alpha; the newer one must answer
        assert_eq!(sst_files(&path).len(), 2);
        assert_eq!(store.get(b"alpha").unwrap(), Some(b"two".to_vec()));
        cleanup(&path);
    }

    #[test]
    fn a_second_delete_after_a_flush_writes_nothing() {
        let path = temp_log("delete-twice");
        let mut store = TinyLsm::open(&path).unwrap();
        store.put(b"alpha", b"one").unwrap();
        store.delete(b"alpha").unwrap();
        store.compact().unwrap(); // the tombstone now lives in table 1

        // the tombstone on disk says "already deleted": no second record
        assert!(!store.delete(b"alpha").unwrap());
        assert_eq!(std::fs::read(&path).unwrap().len(), 0);
        cleanup(&path);
    }

    #[test]
    fn compaction_piles_up_tables_then_merges_them() {
        let path = temp_log("merge");
        let mut store = TinyLsm::open(&path).unwrap();
        for (key, value) in [("alpha", "one"), ("beta", "two")] {
            store.put(key, value).unwrap();
        }
        store.compact().unwrap();
        for (key, value) in [("gamma", "three"), ("delta", "four")] {
            store.put(key, value).unwrap();
        }
        store.compact().unwrap();
        for (key, value) in [("echo", "five"), ("fox", "six")] {
            store.put(key, value).unwrap();
        }
        store.compact().unwrap();

        // three flushes, three tables: below MERGE_TABLES nothing merges
        assert_eq!(sst_files(&path).len(), 3);

        store.put(b"goose", b"seven").unwrap();
        store.compact().unwrap();

        // the fourth table trips the threshold, and all four fold into one
        let tables = sst_files(&path);
        assert_eq!(tables.len(), 1);
        let store = TinyLsm::open(&path).unwrap();
        for (key, value) in [
            ("alpha", "one"),
            ("beta", "two"),
            ("gamma", "three"),
            ("delta", "four"),
            ("echo", "five"),
            ("fox", "six"),
            ("goose", "seven"),
        ] {
            assert_eq!(
                store.get(key.as_bytes()).unwrap(),
                Some(value.as_bytes().to_vec()),
                "wrong value for {key}"
            );
        }
        cleanup(&path);
    }

    #[test]
    fn a_merge_keeps_tombstones() {
        let path = temp_log("merge-tombstone");
        let mut store = TinyLsm::open(&path).unwrap();
        store.put(b"alpha", b"one").unwrap();
        store.compact().unwrap(); // table 1: alpha's put
        store.delete(b"alpha").unwrap();
        store.compact().unwrap(); // table 2: alpha's tombstone
        store.put(b"beta", b"two").unwrap();
        store.compact().unwrap(); // table 3
        store.put(b"gamma", b"three").unwrap();
        store.compact().unwrap(); // table 4: the threshold trips

        // the merged table carries alpha's tombstone with it: the delete
        // (22 bytes) plus beta's put (24) and gamma's put (27 — "three"
        // is a byte longer than "two")
        let tables = sst_files(&path);
        assert_eq!(tables.len(), 1);
        assert_eq!(std::fs::read(&tables[0]).unwrap().len(), 22 + 24 + 27);
        let store = TinyLsm::open(&path).unwrap();
        assert_eq!(store.get(b"alpha").unwrap(), None);
        assert_eq!(store.get(b"beta").unwrap(), Some(b"two".to_vec()));
        cleanup(&path);
    }

    #[test]
    fn a_leftover_table_cannot_resurrect_a_deleted_key() {
        let path = temp_log("leftover");
        let mut store = TinyLsm::open(&path).unwrap();
        store.put(b"alpha", b"one").unwrap();
        store.compact().unwrap();
        // keep a copy of table 1, as if a crashed merge had failed to
        // delete it
        let leftover = std::fs::read(path.with_extension("1.sst")).unwrap();

        store.delete(b"alpha").unwrap();
        store.compact().unwrap(); // table 2: alpha's tombstone
        store.put(b"beta", b"two").unwrap();
        store.compact().unwrap(); // table 3
        store.put(b"gamma", b"three").unwrap();
        store.compact().unwrap(); // table 4: merge; table 2's tombstone rides along

        // the merge removed tables 1 to 4; put the "leftover" back
        std::fs::write(path.with_extension("1.sst"), leftover).unwrap();

        // the merged table's tombstone shadows the leftover's old put —
        // this is exactly the read a dropped tombstone would resurrect
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
        // writes after the flush land in the log and shadow the table
        store.put(b"beta", b"two").unwrap();
        store.put(b"alpha", b"three").unwrap();
        drop(store);

        let store = TinyLsm::open(&path).unwrap();
        assert_eq!(store.get(b"alpha").unwrap(), Some(b"three".to_vec()));
        assert_eq!(store.get(b"beta").unwrap(), Some(b"two".to_vec()));
        cleanup(&path);
    }

    #[test]
    fn compacting_an_empty_store_writes_nothing() {
        let path = temp_log("compact-empty");
        let mut store = TinyLsm::open(&path).unwrap();
        store.compact().unwrap();
        // nothing in memory, nothing to flush: no table, no log bytes
        assert_eq!(sst_files(&path).len(), 0);
        assert_eq!(std::fs::read(&path).unwrap().len(), 0);
        cleanup(&path);
    }

    #[test]
    fn compact_leaves_no_temporary_file_behind() {
        let path = temp_log("compact-tmp");
        let mut store = TinyLsm::open(&path).unwrap();
        store.put(b"alpha", b"one").unwrap();
        store.compact().unwrap();

        let dir = path.parent().unwrap();
        let tmps: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(tmps.is_empty());
        cleanup(&path);
    }

    #[test]
    fn a_temporary_file_left_by_a_crash_is_replaced() {
        let path = temp_log("compact-stale-tmp");
        let mut store = TinyLsm::open(&path).unwrap();
        store.put(b"alpha", b"one").unwrap();
        // a crash mid-flush leaves a half-written temp file behind; the
        // next flush truncates it and carries on. The temp file for
        // table 1 is `tinylsm.1.tmp` — the `.sst` is replaced, not
        // extended
        let tmp = path.with_extension("1.tmp");
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
        let table = path.with_extension("1.sst");
        let mut bytes = std::fs::read(&table).unwrap();
        bytes.truncate(bytes.len() - 4);
        std::fs::write(&table, &bytes).unwrap();

        let err = TinyLsm::open(&path).err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        cleanup(&path);
    }
}
