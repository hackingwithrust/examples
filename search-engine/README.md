# haystack — a full-text search engine

Track: <https://hackingwithrust.com/projects/search-engine/>

**Status: step 9 of 12 published.**

Tags: `search-ch02` … `search-ch12`. See [`../README.md`](../README.md) for what the tags mean.

Steps 2 through 8 are standard library only. Step 9 adds an HTTP front end,
which needs three crates — axum, tokio and serde — so from here on the build
fetches dependencies the first time:

```sh
cargo run          # the demo
cargo test
cargo run -- serve # the HTTP API on http://127.0.0.1:8000
```

The engine itself is unchanged: `serve` loads the same index file the demo
writes and answers the same queries the demo prints. The index file stays the
hand-rolled binary from step 4 — serde is used for the API's JSON, never for
the file.

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

Since step 9, `GET /search?q=...` answers the same queries as JSON:
`{"query": "...", "total": n, "results": [{"path": "...", "score": ...}]}`.
A request without `q`, or a `q` the grammar refuses, is a 400 with
`{"error": "..."}` — the grammar's own message, not a framework default.
