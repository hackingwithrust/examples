# mini-redis

A Redis clone in Rust, built one article step at a time. This is step 4:
Tokio, one task per connection, one shared store, and the first commands
that touch it — `SET`, `GET`, `DEL`, `LPUSH` and `LRANGE` alongside
`PING` and `ECHO`.

## Build and run

```sh
cargo run
```

The server listens on 127.0.0.1:6379, the port real Redis uses, and
answers every command above. With the real `redis-cli` installed:

```sh
redis-cli ping                    # PONG
redis-cli set greeting hello      # OK
redis-cli get greeting            # "hello"
redis-cli lpush queue job1 job2   # (integer) 2
redis-cli lrange queue 0 -1       # 1) "job2" 2) "job1"
```

On a machine where another redis-server already owns 6379, `cargo run`
fails with `AddrInUse`; copy the tree, move the port, and point
`redis-cli -p <port>` at the copy.

## Test

```sh
cargo test
```

Thirty-two tests: every frame type round-trips, the exact bytes the real
redis-cli sends for `PING` decode to the expected frame, a frame cut in
half is `Incomplete` rather than invalid, a frame split across two reads
is reassembled over a real socket, pipelined commands are all answered,
a buffered second frame needs no read, a protocol error closes the
connection without a reply, two connections share one store, and a
half-sent command does not block another connection.

## Tags

Each step of the series is a tag. `mini-ch04` is this state; every later
step grows from it, so `git checkout mini-ch05` is a server that runs.
