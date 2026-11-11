// The line diff that every view of a change builds on: Myers' edit
// script, and the hunks git's unified format prints it as. Step 6
// moved it here out of the examples — `stage` and `render` both walk
// the same script, and an example cannot import from another
// example, but both can import from the library.

pub const CONTEXT: usize = 3;

/// One step of the edit script: a line the two files share (with its
/// position in the old file), a line only the old file has, or a
/// line only the new one has.
#[derive(Clone, Copy)]
pub enum Step {
    Keep { old: usize },
    Delete { old: usize },
    Insert { new: usize },
}

/// One unified-format hunk: where it sits in both files, the section
/// label after the `@@` header, and the lines it prints.
pub struct Hunk {
    pub old_start: usize, // 1-based, as printed in the @@ header
    pub old_len: usize,
    pub new_start: usize,
    pub new_len: usize,
    pub func: Vec<u8>,               // section label git appends to the @@ line
    pub lines: Vec<(char, Vec<u8>)>, // ' ', '-' or '+', text without newline
}

/// The whole text diff: line the files up, turn the differences into
/// hunks, and hand them to the caller.
pub fn unified_hunks(old: &[u8], new: &[u8]) -> Vec<Hunk> {
    let a = split_lines(old);
    let b = split_lines(new);
    let steps = myers(&a, &b);

    // Runs of consecutive edits become groups, recorded the way
    // xdiff records them: where the group starts in each file, and
    // how many lines it removes or adds.
    struct Group {
        old_before: usize,
        new_before: usize,
        old_chg: usize,
        new_chg: usize,
    }
    let mut groups: Vec<Group> = Vec::new();
    let (mut oi, mut ni) = (0usize, 0usize);
    for step in &steps {
        match *step {
            Step::Keep { .. } => {
                oi += 1;
                ni += 1;
            }
            Step::Delete { .. } | Step::Insert { .. } => {
                match groups.last_mut() {
                    Some(g) if oi == g.old_before + g.old_chg && ni == g.new_before + g.new_chg => {
                        // adjacent to the previous group: grow it
                    }
                    _ => groups.push(Group {
                        old_before: oi,
                        new_before: ni,
                        old_chg: 0,
                        new_chg: 0,
                    }),
                }
                let g = groups.last_mut().unwrap();
                match step {
                    Step::Delete { .. } => {
                        g.old_chg += 1;
                        oi += 1;
                    }
                    Step::Insert { .. } => {
                        g.new_chg += 1;
                        ni += 1;
                    }
                    Step::Keep { .. } => unreachable!(),
                }
            }
        }
    }

    // Groups whose unchanged gap is at most 2 * CONTEXT share a hunk —
    // the rule git's xdiff applies for `diff -U3`.
    let mut hunks: Vec<Vec<&Group>> = Vec::new();
    for g in &groups {
        let merge = matches!(
            hunks.last(),
            Some(cur) if g.old_before
                - (cur.last().unwrap().old_before + cur.last().unwrap().old_chg)
                <= 2 * CONTEXT
        );
        if merge {
            hunks.last_mut().unwrap().push(g);
        } else {
            hunks.push(vec![g]);
        }
    }

    let mut out = Vec::new();
    for hunk in &hunks {
        let first = hunk[0];
        let last = *hunk.last().unwrap();
        let old_lo = first.old_before.saturating_sub(CONTEXT);
        let old_hi = (last.old_before + last.old_chg + CONTEXT).min(a.len());
        let new_lo = first.new_before.saturating_sub(CONTEXT);
        let new_hi = (last.new_before + last.new_chg + CONTEXT).min(b.len());

        // Walk the script again and keep only the lines inside the
        // window. Within a change group deletions print before
        // insertions — the order git uses.
        let mut lines: Vec<(char, Vec<u8>)> = Vec::new();
        let mut i = 0usize;
        while i < steps.len() {
            if let Step::Keep { old, .. } = steps[i] {
                if old >= old_lo && old < old_hi {
                    lines.push((' ', strip_newline(a[old])));
                }
                i += 1;
                continue;
            }
            // One change group: deletions first, then insertions.
            let group_start = i;
            while i < steps.len() && !matches!(steps[i], Step::Keep { .. }) {
                i += 1;
            }
            for s in &steps[group_start..i] {
                if let Step::Delete { old } = *s {
                    if old >= old_lo && old < old_hi {
                        lines.push(('-', strip_newline(a[old])));
                    }
                }
            }
            for s in &steps[group_start..i] {
                if let Step::Insert { new } = *s {
                    if new >= new_lo && new < new_hi {
                        lines.push(('+', strip_newline(b[new])));
                    }
                }
            }
        }

        out.push(Hunk {
            old_start: if old_hi == old_lo { old_lo } else { old_lo + 1 },
            old_len: old_hi - old_lo,
            new_start: if new_hi == new_lo { new_lo } else { new_lo + 1 },
            new_len: new_hi - new_lo,
            func: func_context(&a, old_lo),
            lines,
        });
    }
    out
}

/// Print one hunk in unified format. The `@@` line carries the range
/// of the old file and of the new one; a range of one line drops the
/// length, so `@@ -5 +5 @@`, the way git does. After the closing `@@`
/// git prints a section label: the nearest line above the hunk that
/// reads like a function heading.
pub fn print_hunk(h: &Hunk) {
    if h.func.is_empty() {
        println!(
            "@@ -{} +{} @@",
            range(h.old_start, h.old_len),
            range(h.new_start, h.new_len)
        );
    } else {
        println!(
            "@@ -{} +{} @@ {}",
            range(h.old_start, h.old_len),
            range(h.new_start, h.new_len),
            String::from_utf8_lossy(&h.func)
        );
    }
    for (kind, text) in &h.lines {
        println!("{kind}{}", String::from_utf8_lossy(text));
    }
}

/// git's default heuristic for the section label: scan backwards from
/// the line above the hunk and take the first non-blank line that
/// starts with a letter, `_` or `$`. Truncated to 80 bytes, like
/// git's own emit code caps it.
fn func_context(a: &[&[u8]], before: usize) -> Vec<u8> {
    let mut i = before as isize - 1;
    while i >= 0 {
        let line = a[i as usize];
        if !line.is_empty()
            && (line[0].is_ascii_alphabetic() || line[0] == b'_' || line[0] == b'$')
        {
            let mut text = strip_newline(line);
            text.truncate(80);
            return text;
        }
        i -= 1;
    }
    Vec::new()
}

/// A one-line range prints as just its start; anything else as
/// `start,length`. An empty range prints as `0,0` when the hunk sits
/// at the very top of the file.
fn range(start: usize, len: usize) -> String {
    if len == 1 {
        format!("{start}")
    } else {
        format!("{start},{len}")
    }
}

pub fn split_lines(data: &[u8]) -> Vec<&[u8]> {
    data.split_inclusive(|c| *c == b'\n').collect()
}

fn strip_newline(line: &[u8]) -> Vec<u8> {
    line.strip_suffix(b"\n").unwrap_or(line).to_vec()
}

/// Myers' greedy algorithm: the shortest edit script that turns `a`
/// into `b`. One edit per step of distance `d`; each path slides down
/// a diagonal for as long as the lines agree. The V array records how
/// far each diagonal reaches at depth `d`, and the trace of those
/// snapshots is what the backwards walk replays to recover the
/// script. Step 4 verified the output against `git diff --cached`
/// byte for byte.
pub fn myers(a: &[&[u8]], b: &[&[u8]]) -> Vec<Step> {
    let n = a.len() as isize;
    let m = b.len() as isize;
    let max = n + m;
    let off = max; // diagonal k lives at v[(off + k) as usize]
    let mut v = vec![0isize; 2 * off as usize + 1];
    let mut trace: Vec<Vec<isize>> = Vec::new();

    'outer: for d in 0..=max {
        trace.push(v.clone());
        for k in (-d..=d).rev().step_by(2) {
            // Down (an insert) or right (a delete)? Git prefers the
            // move whose diagonal reached further; k == -d forces a
            // down move, k == d a right one.
            let mut x = if k == -d
                || (k != d && v[(off + k - 1) as usize] < v[(off + k + 1) as usize])
            {
                v[(off + k + 1) as usize]
            } else {
                v[(off + k - 1) as usize] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[(off + k) as usize] = x;
            if x >= n && y >= m {
                break 'outer;
            }
        }
    }

    // Replay the trace from the end to recover the script.
    let mut steps: Vec<Step> = Vec::new();
    let (mut x, mut y) = (n, m);
    for d in (0..trace.len() as isize).rev() {
        let v = &trace[d as usize];
        let k = x - y;
        let prev_k = if k == -d
            || (k != d && v[(off + k - 1) as usize] < v[(off + k + 1) as usize])
        {
            k + 1
        } else {
            k - 1
        };
        let prev_x = v[(off + prev_k) as usize];
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            x -= 1;
            y -= 1;
            steps.push(Step::Keep { old: x as usize });
        }
        if d > 0 {
            if x == prev_x {
                y -= 1;
                steps.push(Step::Insert { new: y as usize });
            } else {
                x -= 1;
                steps.push(Step::Delete { old: x as usize });
            }
        }
        x = prev_x;
        y = prev_y;
    }
    steps.reverse();
    steps
}
