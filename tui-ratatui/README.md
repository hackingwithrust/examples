# rlog — a terminal process and log monitor

Track: <https://hackingwithrust.com/projects/tui-ratatui/>

**Status: step 4 of 8 published.**

Tags: `tui-ch02` … `tui-ch04` (one per article). See [`../README.md`](../README.md) for what the tags mean.

Step 4 splits the single box into panes: a process `Table` on one side, a log
`List` on the other, and a one-row status line. The layout is responsive —
side by side at 80 columns and above, stacked below — and the mouse wheel
scrolls whichever pane the pointer is over. Both panes hold fixed demo data;
step 5 replaces it with live data.

```sh
cargo run    # panes at the terminal's current size; q quits
cargo test   # the scroll clamp and the layout rects
```

Nothing is read from disk yet — the log tail and the real process table arrive
in step 5.
