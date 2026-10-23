# haystack — a full-text search engine

Track: <https://hackingwithrust.com/projects/search-engine/>

**Status: step 4 of 12 published.**

Tags: `search-ch02` … `search-ch12`. See [`../README.md`](../README.md) for what the tags mean.

The engine uses only the standard library until step 9, so it builds and runs with no setup at all:

```sh
cargo run
cargo test
```

Since step 4, `cargo run` also writes `haystack.idx` — the index file the demo
saves and loads back. It is output, not source: it is gitignored and rebuilt by
every run.
