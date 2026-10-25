# rlog — a terminal process and log monitor

Track: <https://hackingwithrust.com/projects/tui-ratatui/>

**Status: step 6 of 8 published.**

Tags: `tui-ch02` … `tui-ch06` (one per article). See [`../README.md`](../README.md) for what the tags mean.

Step 6 reads the real process table. `sysinfo` walks the machine once a
second, the snapshot is sorted by CPU (descending) with `s` cycling the sort
column and `r` flipping the direction, and a selected row is highlighted — the
selection is stored as a pid, so it stays on the same process while the table
reorders underneath it. `/` opens a fuzzy filter built on `tui-input` and
`nucleo` that narrows the process table and the log pane at once: `ngx` finds
nginx, `1042` finds a pid. `Enter` closes the editor and keeps the filter
active; `Esc` clears it. The log pane is still the live tail of step 5.

```sh
cargo run -- ./app.log   # follow a file; q quits
cargo run                # the same, with the default path
cargo test               # sort, selection, filter, tail, layout, render sweep
```

Keys: `q` quit · `/` filter · `j`/`k` or the arrows move the selection · `s`
sort · `r` reverse · `PgUp`/`PgDn` scroll the log pane · the wheel scrolls the
pane under the pointer.

In another terminal, `echo 'another line' >> app.log` appends and
`printf 'fresh start\n' > app.log` truncates.
