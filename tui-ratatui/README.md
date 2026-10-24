# rlog — a terminal process and log monitor

Track: <https://hackingwithrust.com/projects/tui-ratatui/>

**Status: step 2 of 8 published.**

Tags: `tui-ch02` … (one per article). See [`../README.md`](../README.md) for what the tags mean.

Step 2 draws the first frame: crossterm puts the terminal into raw mode and the
alternate screen, ratatui renders one bordered box, and the loop redraws it
every 250 ms until `q`, `Esc` or `Ctrl-C`.

```sh
cargo run    # the first frame; q quits
```

Nothing is written to disk and nothing is read from it yet — the log tail and
the process table arrive in steps 5 and 6.
