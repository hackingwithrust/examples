# rlog — a terminal process and log monitor

Track: <https://hackingwithrust.com/projects/tui-ratatui/>

**Status: step 5 of 8 published.**

Tags: `tui-ch02` … `tui-ch05` (one per article). See [`../README.md`](../README.md) for what the tags mean.

Step 5 makes the log pane live. `rlog` follows the file given on the command
line (`./app.log` by default): it remembers its offset, reads whatever grew,
joins a line a writer left half-finished, starts over when the file is
truncated, and waits for a file that does not exist yet. The loop runs on two
schedules — a 250 ms tick for events, a 250 ms poll for the file — and a dirty
flag gates the redraw, so an idle program draws its first frame and then writes
nothing to the terminal at all. The process table is still the fourteen demo
rows; step 6 reads the real one.

```sh
cargo run -- ./app.log   # follow a file; q quits
cargo run                # the same, with the default path
cargo test               # the tail, the offset clamp, the dirty flag, the layout
```

In another terminal, `echo 'another line' >> app.log` appends and
`printf 'fresh start\n' > app.log` truncates.
