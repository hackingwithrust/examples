# tinylsm — a log-structured key-value store

Track: <https://hackingwithrust.com/projects/toy-database/>

**Status: step 11 of 12 published.**

Tags: `lsm-ch02` … `lsm-ch11`.

The store itself uses only the standard library, so it builds and tests with no setup at all:

```sh
cargo test
```

The benchmarks added in step 11 compare it against sled and RocksDB and do need
dependencies (criterion, sled, RocksDB — the first build of RocksDB takes a while):

```sh
cargo bench
```

See [`../README.md`](../README.md) for what the tags mean.
