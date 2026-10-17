//! Throughput benchmarks: `tinylsm` against sled and RocksDB, on the
//! same machine, over the same workloads. Run with `cargo bench`.

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use tempfile::TempDir;

use tinylsm::TinyLsm;

/// Distinct keys the read benchmark populates before measuring.
const KEYS: usize = 2_000;
/// Gets per measured iteration.
const READS: usize = 1_000;
/// Puts per measured iteration.
const WRITES: usize = 100;
/// Key length, in bytes.
const KEY_BYTES: usize = 16;
/// Value length, in bytes.
const VALUE_BYTES: usize = 64;

/// The `i`-th key: the decimal digits of `i`, zero-padded to 16 bytes,
/// so every engine sees exactly the same key shapes.
fn key(i: usize) -> [u8; KEY_BYTES] {
    let mut k = [b'0'; KEY_BYTES];
    let digits = format!("{i:05}");
    k[KEY_BYTES - digits.len()..].copy_from_slice(digits.as_bytes());
    k
}

/// A fresh 64-byte value.
fn value() -> Vec<u8> {
    vec![b'x'; VALUE_BYTES]
}

/// The `i`-th key the read benchmark touches: a fixed stride over the
/// populated keys, coprime with `KEYS`, so the thousand reads cycle
/// through every key before repeating — the same pattern for every
/// engine.
fn read_key(i: usize) -> [u8; KEY_BYTES] {
    key((i * 7_919) % KEYS)
}

fn writes(c: &mut Criterion) {
    let mut group = c.benchmark_group("writes");
    group.throughput(Throughput::Elements(WRITES as u64));

    // tinylsm syncs the log after every write — the durability bar step
    // 8 set — so each put pays its own fsync.
    group.bench_function("tinylsm", |b| {
        b.iter_batched(
            || TempDir::new().unwrap(),
            |dir| {
                let mut store = TinyLsm::open(dir.path().join("tinylsm.log")).unwrap();
                for i in 0..WRITES {
                    store.put(key(i).as_slice(), value()).unwrap();
                }
            },
            BatchSize::PerIteration,
        );
    });

    // sled batches: the inserts land in its log buffer, and one fsync at
    // the flush covers all hundred of them.
    group.bench_function("sled", |b| {
        b.iter_batched(
            || TempDir::new().unwrap(),
            |dir| {
                let db = sled::Config::new().path(dir.path()).open().unwrap();
                for i in 0..WRITES {
                    db.insert(key(i).as_slice(), value()).unwrap();
                }
                db.flush().unwrap();
            },
            BatchSize::PerIteration,
        );
    });

    // RocksDB's default write reaches the operating system, not the
    // disk: the WAL entry is written, nothing is fsynced.
    group.bench_function("rocksdb", |b| {
        b.iter_batched(
            || TempDir::new().unwrap(),
            |dir| {
                let db = rocksdb::DB::open_default(dir.path()).unwrap();
                for i in 0..WRITES {
                    db.put(key(i).as_slice(), value()).unwrap();
                }
            },
            BatchSize::PerIteration,
        );
    });

    // The same writes with sync on: one fsync per put, the durability
    // bar tinylsm holds on every write.
    group.bench_function("rocksdb-sync", |b| {
        b.iter_batched(
            || TempDir::new().unwrap(),
            |dir| {
                let db = rocksdb::DB::open_default(dir.path()).unwrap();
                let mut opts = rocksdb::WriteOptions::new();
                opts.set_sync(true);
                for i in 0..WRITES {
                    db.put_opt(key(i).as_slice(), value(), &opts).unwrap();
                }
            },
            BatchSize::PerIteration,
        );
    });

    group.finish();
}

fn gets(c: &mut Criterion) {
    let mut group = c.benchmark_group("gets");
    group.throughput(Throughput::Elements(READS as u64));

    // tinylsm: populated and compacted once, so every read lands in a
    // table and goes through the bloom filter and the block read.
    {
        let dir = TempDir::new().unwrap();
        let mut store = TinyLsm::open(dir.path().join("tinylsm.log")).unwrap();
        for i in 0..KEYS {
            store.put(key(i).as_slice(), value()).unwrap();
        }
        store.compact().unwrap();
        group.bench_function("tinylsm", |b| {
            b.iter(|| {
                for i in 0..READS {
                    black_box(store.get(read_key(i).as_slice()).unwrap());
                }
            });
        });
    }

    // sled: the same population, flushed, read back the same way.
    {
        let dir = TempDir::new().unwrap();
        let db = sled::Config::new().path(dir.path()).open().unwrap();
        for i in 0..KEYS {
            db.insert(key(i).as_slice(), value()).unwrap();
        }
        db.flush().unwrap();
        group.bench_function("sled", |b| {
            b.iter(|| {
                for i in 0..READS {
                    black_box(db.get(read_key(i).as_slice()).unwrap());
                }
            });
        });
    }

    // RocksDB: the same population, read back the same way.
    {
        let dir = TempDir::new().unwrap();
        let db = rocksdb::DB::open_default(dir.path()).unwrap();
        for i in 0..KEYS {
            db.put(key(i).as_slice(), value()).unwrap();
        }
        group.bench_function("rocksdb", |b| {
            b.iter(|| {
                for i in 0..READS {
                    black_box(db.get(read_key(i).as_slice()).unwrap());
                }
            });
        });
    }

    group.finish();
}

criterion_group!(benches, writes, gets);
criterion_main!(benches);
