use std::collections::BTreeMap;

/// An in-memory key-value store. The interface every later step of the
/// track keeps; what sits underneath it changes from here on.
pub struct TinyLsm {
    map: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl TinyLsm {
    pub fn new() -> Self {
        TinyLsm {
            map: BTreeMap::new(),
        }
    }

    /// Insert or overwrite `key`. Both sides are stored as owned bytes.
    pub fn put(&mut self, key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) {
        self.map.insert(key.into(), value.into());
    }

    /// Return the newest value for `key`, borrowed from the store.
    pub fn get(&self, key: &[u8]) -> Option<&[u8]> {
        self.map.get(key).map(Vec::as_slice)
    }

    /// Remove `key`. Returns true when the key existed.
    pub fn delete(&mut self, key: &[u8]) -> bool {
        self.map.remove(key).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_then_get() {
        let mut store = TinyLsm::new();
        store.put(b"alpha", b"one");
        assert_eq!(store.get(b"alpha"), Some(&b"one"[..]));
    }

    #[test]
    fn get_missing_is_none() {
        let store = TinyLsm::new();
        assert_eq!(store.get(b"nope"), None);
    }

    #[test]
    fn put_overwrites() {
        let mut store = TinyLsm::new();
        store.put(b"alpha", b"one");
        store.put(b"alpha", b"two");
        assert_eq!(store.get(b"alpha"), Some(&b"two"[..]));
    }

    #[test]
    fn delete_removes_and_reports() {
        let mut store = TinyLsm::new();
        store.put(b"alpha", b"one");
        assert!(store.delete(b"alpha"));
        assert_eq!(store.get(b"alpha"), None);
        assert!(!store.delete(b"alpha"));
    }

    #[test]
    fn keys_are_independent() {
        let mut store = TinyLsm::new();
        store.put(b"alpha", b"one");
        store.put(b"beta", b"two");
        assert_eq!(store.get(b"beta"), Some(&b"two"[..]));
        store.delete(b"alpha");
        assert_eq!(store.get(b"beta"), Some(&b"two"[..]));
    }
}
