# mini-redis

A Redis clone in Rust, built one article step at a time. This is step 5:
a deadline on every key, `EXPIRE` and `TTL`, lazy expiry on the read
path, and a sweeper task that reclaims expired keys once a second —
alongside `SET`, `GET`, `DEL`, `LPUSH` and `LRANGE` and `PING` and
`ECHO`.

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
redis-cli expire greeting 100     # (integer) 1
redis-cli ttl greeting            # (integer) 100
```

On a machine where another redis-server already owns 6379, `cargo run`
fails with `AddrInUse`; copy the tree, move the port, and point
`redis-cli -p <port>` at the copy.

## Test

```sh
cargo test
```

Fifty tests: every frame type round-trips, the exact bytes the real
redis-cli sends for `PING` decode to the expected frame, a frame cut in
half is `Incomplete` rather than invalid, a frame split across two reads
is reassembled over a real socket, pipelined commands are all answered,
a buffered second frame needs no read, a protocol error closes the
connection without a reply, two connections share one store, and a
half-sent command does not block another connection — plus the expiry
set: `TTL` answers -1 without a deadline and -2 for a missing key, a
non-positive `EXPIRE` deletes, `SET` drops the deadline, an expired key
is gone on the next read, and the sweeper removes only what has
actually expired.

## Tags

Each step of the series is a tag. `mini-ch05` is this state; every later
step grows from it, so `git checkout mini-ch06` is a server that runs.
