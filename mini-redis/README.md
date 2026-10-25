# mini-redis

A Redis clone in Rust, built one article step at a time. This is step 2: the
RESP2 protocol written by hand — encode a frame, decode a frame — and a
server that answers `PING`.

## Build and run

```sh
cargo run
```

The server listens on 127.0.0.1:6379, the port real Redis uses, and answers
`PING`. With the real `redis-cli` installed:

```sh
redis-cli ping        # PONG
redis-cli ping hello  # "hello"
```

## Test

```sh
cargo test
```

Seven tests: every frame type round-trips, the exact bytes the real
redis-cli sends for `PING` decode to the expected frame, a frame cut in half
is `Incomplete` rather than invalid, and one connection carries a real
socket round trip.

## Tags

Each step of the series is a tag. `mini-ch02` is this state; every later
step grows from it, so `git checkout mini-ch03` is a server that runs.
