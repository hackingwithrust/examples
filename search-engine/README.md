# haystack — a full-text search engine

Track: <https://hackingwithrust.com/projects/search-engine/>

**Status: step 8 of 12 published.**

Tags: `search-ch02` … `search-ch12`. See [`../README.md`](../README.md) for what the tags mean.

The engine uses only the standard library until step 9, so it builds and runs with no setup at all:

```sh
cargo run
cargo test
```

Since step 4, `cargo run` also writes `haystack.idx` — the index file the demo
saves and loads back. It is output, not source: it is gitignored and rebuilt by
every run.

Since step 6, the demo ranks its queries with BM25 — k1 = 1.2 and b = 0.75, the
values Lucene ships — and prints each document's score next to its term
frequency and length.

Since step 7, every posting carries the positions of its occurrences, and the
demo answers phrase queries ("borrow checker") and boolean queries (AND, OR,
NOT). The index file is version 3: every document carries its length, and every
posting carries a position per occurrence — the term frequency is their count.

Since step 8, a query word can carry a tilde — `borow~` — and the engine
searches the dictionary by edit distance instead of looking the term up,
so a typo finds what it was aiming at. The default radius is two edits,
`borow~1` asks for one, and the grammar refuses a radius it does not
have rather than guessing.
