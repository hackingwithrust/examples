use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::path::Path;

/// A persistent key-value store: an append-only log on disk, indexed in
/// memory by a `BTreeMap`.
pub struct TinyLsm {
    map: BTreeMap<Vec<u8>, Vec<u8>>,
    log: std::fs::File,
}

impl TinyLsm {
    /// Open the log at `path` and replay it: every whole record with a
    /// valid checksum rebuilds the index, and the file is cut back to the
    /// last whole record. A record that is complete but fails its checksum
    /// stops the open with an error instead of being dropped.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let mut log = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)?;
        let mut bytes = Vec::new();
        log.read_to_end(&mut bytes)?;
        let mut map = BTreeMap::new();
        let valid = replay(&bytes, &mut map)?;
        log.set_len(valid as u64)?;
        Ok(TinyLsm { map, log })
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
        let record = frame(payload);
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

/// Frame one payload as a record: payload length, checksum, payload.
fn frame(payload: &[u8]) -> Vec<u8> {
    let mut record = Vec::with_capacity(8 + payload.len());
    record.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    record.extend_from_slice(&checksum(payload).to_le_bytes());
    record.extend_from_slice(payload);
    record
}

/// Walk the log byte by byte, applying each whole record to `map`, and
/// return the offset where the valid prefix ends. Everything after that
/// offset is a torn tail — a record a crash cut short — and the caller
/// truncates the file back to it.
fn replay(bytes: &[u8], map: &mut BTreeMap<Vec<u8>, Vec<u8>>) -> io::Result<usize> {
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
                "log is corrupt: record checksum mismatch",
            ));
        }
        apply(payload, map)?;
        rest = tail;
        pos += 8 + len;
    }
    Ok(pos)
}

/// Apply one record payload to the map: op byte, key, value. `Err` when the
/// payload is not shaped like an operation — with the checksum already
/// verified, that is a bug in the writer, not a damaged file.
fn apply(payload: &[u8], map: &mut BTreeMap<Vec<u8>, Vec<u8>>) -> io::Result<()> {
    let bad = |what: &str| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("log is corrupt: {what}"),
        )
    };
    let Some((&op, rest)) = payload.split_first() else {
        return Err(bad("empty payload"));
    };
    let Some((klen, rest)) = read_u32(rest) else {
        return Err(bad("no key length"));
    };
    let Some((key, rest)) = take(klen as usize, rest) else {
        return Err(bad("key runs past the payload"));
    };
    let Some((vlen, rest)) = read_u32(rest) else {
        return Err(bad("no value length"));
    };
    let Some((value, rest)) = take(vlen as usize, rest) else {
        return Err(bad("value runs past the payload"));
    };
    if !rest.is_empty() {
        return Err(bad("trailing bytes after the value"));
    }
    match op {
        b'P' => {
            map.insert(key.to_vec(), value.to_vec());
            Ok(())
        }
        b'D' => {
            map.remove(key);
            Ok(())
        }
        _ => Err(bad("unknown operation")),
    }
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

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn put_then_get() {
        let path = temp_log("put-get");
        let mut store = TinyLsm::open(&path).unwrap();
        store.put(b"alpha", b"one").unwrap();
        assert_eq!(store.get(b"alpha"), Some(&b"one"[..]));
        cleanup(&path);
    }

    #[test]
    fn get_missing_is_none() {
        let path = temp_log("missing");
        let store = TinyLsm::open(&path).unwrap();
        assert_eq!(store.get(b"nope"), None);
        cleanup(&path);
    }

    #[test]
    fn put_overwrites() {
        let path = temp_log("overwrite");
        let mut store = TinyLsm::open(&path).unwrap();
        store.put(b"alpha", b"one").unwrap();
        store.put(b"alpha", b"two").unwrap();
        assert_eq!(store.get(b"alpha"), Some(&b"two"[..]));
        cleanup(&path);
    }

    #[test]
    fn delete_removes_and_reports() {
        let path = temp_log("delete");
        let mut store = TinyLsm::open(&path).unwrap();
        store.put(b"alpha", b"one").unwrap();
        assert!(store.delete(b"alpha").unwrap());
        assert_eq!(store.get(b"alpha"), None);
        assert!(!store.delete(b"alpha").unwrap());
        cleanup(&path);
    }

    #[test]
    fn keys_are_independent() {
        let path = temp_log("independent");
        let mut store = TinyLsm::open(&path).unwrap();
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
        assert_eq!(store.get(b"alpha"), Some(&b"one"[..]));
        assert_eq!(store.get(b"beta"), Some(&b"two"[..]));
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
        assert_eq!(store.get(b"alpha"), Some(&b"two"[..]));
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
        assert_eq!(store.get(b"alpha"), None);
        assert_eq!(store.get(b"beta"), Some(&b"two"[..]));
        cleanup(&path);
    }

    #[test]
    fn open_a_missing_file_starts_empty() {
        let path = temp_log("fresh");
        let store = TinyLsm::open(&path).unwrap();
        assert_eq!(store.get(b"alpha"), None);
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
        assert_eq!(store.get(b"alpha"), Some(&b"one"[..]));
        assert_eq!(store.get(b"beta"), None);
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
        assert_eq!(store.get(b"beta"), Some(&b"two"[..]));
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
}
