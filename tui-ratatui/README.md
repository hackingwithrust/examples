# rlog — a terminal process and log monitor

Track: <https://hackingwithrust.com/projects/tui-ratatui/>

**Status: step 8 of 8 published. The track is complete.**

Tags: `tui-ch02` … `tui-ch08` (one per article). See [`../README.md`](../README.md) for what the tags mean.

Step 8 is the release pass. The program gained the one check a terminal
application owes its user — stdout has to be a terminal, because that is where
the frame goes, and the errno that comes back without the check names no
program and suggests no fix — and `Cargo.toml` gained a release profile tuned
for a binary that leaves the machine: `strip`, `lto`, one codegen unit, and
`panic = "abort"` (the panic hook still restores the terminal before the
abort). The article measures what each line is worth and builds the same
source for both Apple architectures and for Linux.

```sh
cargo run --release -- ./app.log   # follow a file; q quits
cargo test                          # 36 tests
cargo build --release --target x86_64-unknown-linux-musl   # needs a linker stanza, see the article
```

Keys: `q` quit · `/` filter (enter keeps it, esc clears) · `j`/`k` or the arrows
move the selection · `s` sort · `r` reverse · `space` pause the tail ·
`PgUp`/`PgDn` scroll the log pane · `t` next colour theme · `?` help · the wheel
scrolls the pane under the pointer.

In another terminal, `echo 'another line' >> app.log` appends and
`printf 'fresh start\n' > app.log` truncates.
