# mini-redis

A Redis clone in Rust, built one article step at a time. This is step 3: the
RESP2 codec behind a connection buffer — a frame split across two packets is
reassembled instead of refused, a second frame in the same read is answered
instead of dropped — and a server that answers `PING` and `ECHO`.

## Build and run

```sh
cargo run
```

The server listens on 127.0.0.1:6379, the port real Redis uses, and answers
`PING` and `ECHO`. With the real `redis-cli` installed:

```sh
redis-cli ping        # PONG
redis-cli echo hello  # "hello"
```

## Test

```sh
cargo test
```

Thirteen tests: every frame type round-trips, the exact bytes the real
redis-cli sends for `PING` decode to the expected frame, a frame cut in half
is `Incomplete` rather than invalid, a frame split across two reads is
reassembled over a real socket, pipelined commands are all answered, a
buffered second frame needs no read, and a protocol error closes the
connection without a reply.

## Tags

Each step of the series is a tag. `mini-ch03` is this state; every later
step grows from it, so `git checkout mini-ch04` is a server that runs.
