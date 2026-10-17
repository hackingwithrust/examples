use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;

/// A persistent key-value store: an append-only log on disk, indexed in
/// memory by a `BTreeMap`.
pub struct TinyLsm {
    map: BTreeMap<Vec<u8>, Vec<u8>>,
    log: std::fs::File,
}

impl TinyLsm {
    /// Open the log at `path` and start from an empty state.
    ///
    /// The file is truncated: this step has no recovery yet, so reopening a
    /// store discards what the previous one wrote. Step 4 replaces this.
    pub fn new(path: impl AsRef<Path>) -> io::Result<Self> {
        let log = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)?;
        Ok(TinyLsm {
            map: BTreeMap::new(),
            log,
        })
    }

    /// Insert or overwrite `key`. The write hits the log before the map.
    pub fn put(&mut self, key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> io::Result<()> {
        let key = key.into();
        let value = value.into();
        let payload = encode(b'P', &key, Some(&value));
        self.append(&payload)?;
        self.map.insert(key, value);
        Ok(())
    }

    /// Return the newest value for `key`, borrowed from the in-memory index.
    pub fn get(&self, key: &[u8]) -> Option<&[u8]> {
        self.map.get(key).map(Vec::as_slice)
    }

    /// Remove `key`, appending a tombstone when it existed. Returns whether
    /// it did — a delete of a missing key writes nothing to the log.
    pub fn delete(&mut self, key: &[u8]) -> io::Result<bool> {
        if self.map.get(key).is_none() {
            return Ok(false);
        }
        let payload = encode(b'D', key, None);
        self.append(&payload)?;
        self.map.remove(key);
        Ok(true)
    }

    /// Frame one payload as a record and append it to the log.
    fn append(&mut self, payload: &[u8]) -> io::Result<()> {
        let mut record = Vec::with_capacity(8 + payload.len());
        record.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        record.extend_from_slice(&checksum(payload).to_le_bytes());
        record.extend_from_slice(payload);
        self.log.write_all(&record)?;
        self.log.flush()
    }
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

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn put_then_get() {
        let path = temp_log("put-get");
        let mut store = TinyLsm::new(&path).unwrap();
        store.put(b"alpha", b"one").unwrap();
        assert_eq!(store.get(b"alpha"), Some(&b"one"[..]));
        cleanup(&path);
    }

    #[test]
    fn get_missing_is_none() {
        let path = temp_log("missing");
        let store = TinyLsm::new(&path).unwrap();
        assert_eq!(store.get(b"nope"), None);
        cleanup(&path);
    }

    #[test]
    fn put_overwrites() {
        let path = temp_log("overwrite");
        let mut store = TinyLsm::new(&path).unwrap();
        store.put(b"alpha", b"one").unwrap();
        store.put(b"alpha", b"two").unwrap();
        assert_eq!(store.get(b"alpha"), Some(&b"two"[..]));
        cleanup(&path);
    }

    #[test]
    fn delete_removes_and_reports() {
        let path = temp_log("delete");
        let mut store = TinyLsm::new(&path).unwrap();
        store.put(b"alpha", b"one").unwrap();
        assert!(store.delete(b"alpha").unwrap());
        assert_eq!(store.get(b"alpha"), None);
        assert!(!store.delete(b"alpha").unwrap());
        cleanup(&path);
    }

    #[test]
    fn keys_are_independent() {
        let path = temp_log("independent");
        let mut store = TinyLsm::new(&path).unwrap();
        store.put(b"alpha", b"one").unwrap();
        store.put(b"beta", b"two").unwrap();
        assert_eq!(store.get(b"beta"), Some(&b"two"[..]));
        store.delete(b"alpha").unwrap();
        assert_eq!(store.get(b"beta"), Some(&b"two"[..]));
        cleanup(&path);
    }

    #[test]
    fn put_appends_one_record() {
        let path = temp_log("append");
        let mut store = TinyLsm::new(&path).unwrap();
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
        let mut store = TinyLsm::new(&path).unwrap();
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
        let mut store = TinyLsm::new(&path).unwrap();
        assert!(!store.delete(b"ghost").unwrap());
        assert_eq!(std::fs::read(&path).unwrap().len(), 0);
        cleanup(&path);
    }

    #[test]
    fn reopening_starts_empty() {
        let path = temp_log("reopen");
        {
            let mut store = TinyLsm::new(&path).unwrap();
            store.put(b"alpha", b"one").unwrap();
        }
        let store = TinyLsm::new(&path).unwrap();
        assert_eq!(store.get(b"alpha"), None);
        cleanup(&path);
    }
}
