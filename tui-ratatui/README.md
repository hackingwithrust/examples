# rlog — a terminal process and log monitor

Track: <https://hackingwithrust.com/projects/tui-ratatui/>

**Status: step 7 of 8 published.**

Tags: `tui-ch02` … `tui-ch07` (one per article). See [`../README.md`](../README.md) for what the tags mean.

Step 7 adds the bottom pane: every refresh records one CPU sample per process
into a bounded ring buffer (60 samples, one per second), and the pane draws the
selected process's window as a sparkline. The subject is a pid, so the pane
follows the selection while the table reorders underneath it. `?` overlays the
key list, `t` cycles three colour themes, and `space` freezes the tail — the
key step 1 promised and this step finally builds.

```sh
cargo run -- ./app.log   # follow a file; q quits
cargo run                # the same, with the default path
cargo test               # history, themes, overlay, pause, sort, tail, render sweep
```

Keys: `q` quit · `/` filter (enter keeps it, esc clears) · `j`/`k` or the arrows
move the selection · `s` sort · `r` reverse · `space` pause the tail ·
`PgUp`/`PgDn` scroll the log pane · `t` next colour theme · `?` help · the wheel
scrolls the pane under the pointer.

In another terminal, `echo 'another line' >> app.log` appends and
`printf 'fresh start\n' > app.log` truncates.
