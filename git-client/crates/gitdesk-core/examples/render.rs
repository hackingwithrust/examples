//! render: turn the edit script of step 4 into rows a side-by-side
//! view can draw. Line numbers from both files on every row, the
//! changed bytes inside a rewritten line marked, and whitespace —
//! tabs, trailing spaces — made visible instead of invisible.
//!
//! Run with `cargo run --example render` from the workspace root.
//! The fixture lands in `gitdesk-render-demo/`; when it is already
//! there, the program skips creation and just renders.

use gix::hash::Kind;
use std::collections::BTreeMap;
use std::path::Path;

// ---- the fixture data: one file, three kinds of invisible edit -----------

// One file, three kinds of edit: line 5 rewrites the middle of a
// line, line 6 is inserted outright, and the last line loses its
// three trailing spaces — a change no terminal will show you.
//
// All three land in one hunk's worth of distance, which is deliberate:
// the renderer draws every line of the file, so what matters is that
// each edit has a visibly different shape on screen — a marked span,
// a half-empty row, and a dot where a space used to be.
const PLAN_V1: &[u8] = b"# week plan\n\
monday: open the repo\n\
tuesday: status codes\n\
wednesday: diff two blobs\n\
thursday: render side by side\n\
friday: stage by hunk\n\
weekend: rest   \n";
const PLAN_V2: &[u8] = b"# week plan\n\
monday: open the repo\n\
tuesday: status codes\n\
wednesday: diff two blobs\n\
thursday: render diffs side by side\n\
saturday: fetch and push\n\
friday: stage by hunk\n\
weekend: rest\n";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ---- fixture, then the two sides to compare -------------------
    let dir = Path::new("gitdesk-render-demo");
    if !dir.join(".git").try_exists()? {
        make_fixture(dir)?;
    }
    let repo = gix::open(dir)?;

    // Same direction as `git diff --cached`: HEAD is the left column,
    // the index is the right one.
    let head = head_tree_files(&repo)?;
    let index = index_files(&repo)?;

    // Sorted by path, because the index map is ordered; a renderer that
    // walked a hash map would print files in a different order per run.
    for (path, staged) in &index {
        // A path HEAD has never heard of needs an empty left side,
        // not a neighbour; additions come later in the track.
        let Some(committed) = head.get(path) else {
            continue;
        };
        // Same digest, same bytes: nothing to draw.
        //
        // Unlike the text diff, this check is also what keeps the
        // output from being a wall of identical rows — there are no
        // context windows here, so every unchanged file would print
        // its whole content twice.
        if committed.id == *staged {
            continue;
        }
        render_path(&repo, path, committed, *staged)?;
    }
    Ok(())
}

/// One path's stake in the object database: which blob HEAD records.
/// The mode travels with the entry but a text renderer never shows
/// it, so this step stops carrying it.
struct BlobRef {
    id: gix::ObjectId,
}

// ---- the fixture: one path, staged one version ahead ---------------------

/// Build a repository with one tracked file whose staged version
/// differs from the committed one in three ways: a mid-line rewrite,
/// an inserted line, and a trailing-whitespace cleanup.
///
/// The commit records v1, the index stages v2, and the worktree holds
/// v2 so `git status` shows one staged change and nothing else. Only
/// the first two matter to the renderer; the third keeps the fixture
/// consistent for anyone who inspects it with real git.
fn make_fixture(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let repo = gix::init(dir)?;

    let plan_v1 = repo.write_blob(PLAN_V1)?;
    let plan_v2 = repo.write_blob(PLAN_V2)?;

    // The committed snapshot names v1. There is exactly one entry, so
    // the tree walk later has nothing to recurse into.
    let tree = gix::objs::Tree {
        entries: vec![blob_entry("plan.md", plan_v1.detach())],
    };
    let tree = repo.write_object(&tree)?;

    // Pin the clock, or the commit digest changes between runs.
    let who = gix::actor::Signature {
        name: "Demo".into(),
        email: "demo@example.com".into(),
        time: gix::date::Time::new(1_700_000_100, 0),
    };
    let commit = gix::objs::Commit {
        tree: tree.into(),
        parents: vec![].into(),
        author: who.clone(),
        committer: who,
        encoding: None,
        message: "seed commit\n".into(),
        extra_headers: vec![],
    };
    let commit = repo.write_object(&commit)?.detach();
    let heads = repo.git_dir().join("refs/heads");
    std::fs::create_dir_all(&heads)?;
    std::fs::write(heads.join("main"), format!("{commit}\n"))?;
    std::fs::write(repo.git_dir().join("HEAD"), "ref: refs/heads/main\n")?;

    // The index stages the v2 blob; the worktree agrees with it.
    write_index(&repo, &[("plan.md", plan_v2.detach())])?;
    std::fs::write(dir.join("plan.md"), PLAN_V2)?;
    Ok(())
}

fn blob_entry(name: &str, oid: gix::ObjectId) -> gix::objs::tree::Entry {
    gix::objs::tree::Entry {
        mode: gix::objs::tree::EntryKind::Blob.into(),
        filename: name.into(),
        oid,
    }
}

// ---- plumbing: the index file, and reading the two trees -----------------

/// Write a version-2 index by hand — `DIRC` header, one record per
/// path, SHA-1 trailer. The same bytes steps 2–4 wrote.
///
/// The stat block is zeroed on purpose: nothing here ever consults it.
/// The blob id in each record is what makes the two sides comparable,
/// and a renderer that trusted stat data would be guessing about a
/// file it never stat-ed.
fn write_index(
    repo: &gix::Repository,
    entries: &[(&str, gix::ObjectId)],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"DIRC");
    buf.extend_from_slice(&2u32.to_be_bytes()); // version 2
    buf.extend_from_slice(&(entries.len() as u32).to_be_bytes());

    for (path, id) in entries {
        let stat: [u32; 10] = [
            0, 0, // ctime seconds, nanoseconds
            1_700_000_100, 0, // mtime seconds, nanoseconds
            0, 0, // device, inode
            0o100644, // mode: regular file
            0, 0, // uid, gid
            0, // size: unused here, diff re-reads the blob instead
        ];
        for word in stat {
            buf.extend_from_slice(&word.to_be_bytes());
        }
        buf.extend_from_slice(id.as_slice());
        let path = path.as_bytes();
        buf.extend_from_slice(&(path.len() as u16).to_be_bytes()); // flags
        buf.extend_from_slice(path);
        let entry_len = buf.len() - 12;
        let target = (entry_len + 1).div_ceil(8) * 8;
        buf.resize(12 + target, 0);
    }

    let mut hasher = gix::hash::hasher(Kind::Sha1);
    hasher.update(&buf);
    buf.extend_from_slice(hasher.try_finalize()?.as_slice());

    std::fs::write(repo.git_dir().join("index"), buf)?;
    Ok(())
}

/// HEAD's tree as a map: path -> blob digest and mode. The fixture
/// never nests, so one level is the whole story.
fn head_tree_files(
    repo: &gix::Repository,
) -> Result<BTreeMap<String, BlobRef>, Box<dyn std::error::Error>> {
    let id = repo.head()?.into_peeled_id()?;
    let tree = repo.find_object(id)?.peel_to_tree()?;
    let mut files = BTreeMap::new();
    for entry in tree.iter() {
        let entry = entry?;
        files.insert(
            String::from_utf8_lossy(entry.filename()).into_owned(),
            BlobRef {
                id: entry.oid().to_owned(),
            },
        );
    }
    Ok(files)
}

/// The index as the same kind of map.
fn index_files(
    repo: &gix::Repository,
) -> Result<BTreeMap<String, gix::ObjectId>, Box<dyn std::error::Error>> {
    let index = repo.index()?;
    let mut files = BTreeMap::new();
    for e in index.entries() {
        files.insert(String::from_utf8_lossy(e.path(&index)).into_owned(), e.id);
    }
    Ok(files)
}

/// One path, one screen: the file name, then rows — or a refusal for
/// a binary pair, which has no lines to draw.
///
/// No `diff --git` header and no `@@` lines: this is not a patch, it
/// is a view. Every line of the file becomes exactly one row, so the
/// height of the output is the height of the file, not the height of
/// the change.
fn render_path(
    repo: &gix::Repository,
    path: &str,
    old: &BlobRef,
    new_id: gix::ObjectId,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("{path}");
    let old_obj = repo.find_object(old.id)?;
    let new_obj = repo.find_object(new_id)?;
    let old_bytes: &[u8] = &old_obj.data;
    let new_bytes: &[u8] = &new_obj.data;

    // Step 4's verdict stands: a NUL byte anywhere in the first 8000
    // bytes means no text algorithm runs, so there are no rows either.
    // Refusing here rather than drawing escaped bytes keeps one rule
    // for "is this text" across the whole track.
    if is_binary(old_bytes) || is_binary(new_bytes) {
        println!("  binary — nothing to draw");
        return Ok(());
    }

    let a = split_lines(old_bytes);
    let b = split_lines(new_bytes);
    let steps = myers(&a, &b);
    let table = rows(&a, &b, &steps);

    // Both columns line up: every text cell pads to the widest one,
    // every line number to the widest number.
    //
    // The widths have to be measured before anything is printed —
    // terminal output cannot go back and widen a column. `display_len`
    // counts decorated characters plus the marker bytes, not raw line
    // length, so a row with visible spaces still pads correctly.
    let text_w = table
        .iter()
        .flat_map(|r| [r.old.as_ref(), r.new.as_ref()].into_iter().flatten())
        .map(Side::display_len)
        .max()
        .unwrap_or(0);
    let no_w = table
        .iter()
        .flat_map(|r| [r.old.as_ref(), r.new.as_ref()].into_iter().flatten())
        .map(|s| s.no.to_string().len())
        .max()
        .unwrap_or(1);

    for row in &table {
        let left = cell(row.old.as_ref(), no_w, text_w, "[-", "-]");
        // The right column is last: nothing follows it, so it does
        // not pad. Passing 0 as the width makes every right cell
        // exactly as wide as its text.
        let right = cell(row.new.as_ref(), no_w, 0, "{+", "+}");
        println!("{left}│ {right}");
    }
    Ok(())
}

fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(8000)].contains(&0)
}

/// One side of a row: which line, and which byte range inside it
/// changed. `change` is `None` for an untouched line — and for a side
/// whose middle came out empty, like the old half of a pure insertion.
struct Side {
    no: usize, // 1-based, as a viewer shows it
    line: Vec<u8>,
    change: Option<(usize, usize)>,
}

impl Side {
    /// How wide the rendered cell is: the visible text, plus four
    /// characters when a change is marked on top of it.
    ///
    /// Four is not magic: two-character markers on each side of the
    /// span — `[-` … `-]` on the left, `{+` … `+}` on the right — and
    /// they are printed, so they take column space like any character.
    fn display_len(&self) -> usize {
        let vis = decorate(&self.line).len();
        vis + if self.change.is_some_and(|(a, b)| b > a) {
            4
        } else {
            0
        }
    }
}

/// One screen row: the two versions of the same position. `None` on a
/// side means that side has no line here — it is not an empty line,
/// and the two must not be confused when padding.
struct Row {
    old: Option<Side>,
    new: Option<Side>,
}

/// The screen rows for one edit script. Keeps become rows with both
/// sides; a run of deletes and inserts pairs up in order — the first
/// deleted line against the first inserted one, and whichever side
/// runs out first leaves its rows half empty.
///
/// This pairing is the whole trick of a side-by-side view: a diff
/// tells you "three lines went, two came", a renderer has to decide
/// which old line sits next to which new one. Index order is the
/// cheapest honest answer, and for a rewritten line it puts the
/// rewrite on one row where the eye expects it.
fn rows(a: &[&[u8]], b: &[&[u8]], steps: &[Step]) -> Vec<Row> {
    let mut out = Vec::new();
    let (mut oi, mut ni) = (0usize, 0usize);
    let mut i = 0;
    while i < steps.len() {
        if let Step::Keep = steps[i] {
            // Both files advance in step here, so one row carries both
            // line numbers. `no` is 1-based: the viewer shows lines,
            // not offsets.
            out.push(Row {
                old: Some(Side {
                    no: oi + 1,
                    line: strip_newline(a[oi]),
                    change: None,
                }),
                new: Some(Side {
                    no: ni + 1,
                    line: strip_newline(b[ni]),
                    change: None,
                }),
            });
            oi += 1;
            ni += 1;
            i += 1;
            continue;
        }
        // One change run: pair the deletes and inserts index-wise.
        //
        // The script groups its own way — all deletes then all inserts
        // within the run — so the two lists are collected separately
        // and then zipped by position rather than by script order.
        let start = i;
        while i < steps.len() && !matches!(steps[i], Step::Keep) {
            i += 1;
        }
        let dels: Vec<usize> = steps[start..i]
            .iter()
            .filter_map(|s| match *s {
                Step::Delete { old } => Some(old),
                _ => None,
            })
            .collect();
        let ins: Vec<usize> = steps[start..i]
            .iter()
            .filter_map(|s| match *s {
                Step::Insert { new } => Some(new),
                _ => None,
            })
            .collect();
        // max, not min: the longer list sets the row count, and the
        // shorter one simply stops supplying cells — `get(k)` returns
        // None from then on.
        for k in 0..dels.len().max(ins.len()) {
            let mut old = dels.get(k).map(|&o| Side {
                no: o + 1,
                line: strip_newline(a[o]),
                change: None,
            });
            let mut new = ins.get(k).map(|&n| Side {
                no: n + 1,
                line: strip_newline(b[n]),
                change: None,
            });
            // A row with a line on both sides is a rewrite: find the
            // bytes the two versions share at each end, mark the rest.
            //
            // Both markers are stored, not just ours: the two columns
            // usually highlight different spans (`side by side` vs
            // `diffs side by side`), and each column must draw its own.
            if let (Some(o), Some(n)) = (&mut old, &mut new) {
                let (oc, nc) = intraline(&o.line, &n.line);
                o.change = oc;
                n.change = nc;
            }
            // Exactly one side may be None here — that is the "half
            // empty" row, and it is information: a pure deletion gets a
            // blank right cell, a pure insertion a blank left one.
            out.push(Row { old, new });
        }
        // The two files advanced by however much each side actually
        // consumed, which is why the counters follow the lists and not
        // the row count.
        oi += dels.len();
        ni += ins.len();
    }
    out
}

/// The bytes both versions of a line share at each end; whatever sits
/// between is the change. Greedy prefix-then-suffix — the cheapest
/// honest answer to "what changed in this line". git's --word-diff
/// marks words instead of byte ranges, but the idea is the same.
fn intraline(a: &[u8], b: &[u8]) -> (Option<(usize, usize)>, Option<(usize, usize)>) {
    // Longest common prefix: the first byte where the two disagree is
    // where the change starts, on both sides.
    let p = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    // Longest common suffix, measured from the end. It shares no state
    // with the prefix, so it can overrun it — see the clamp below.
    let s = a
        .iter()
        .rev()
        .zip(b.iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    // The suffix must not reach into the prefix.
    //
    // Without this, `a = "ab"` and `b = "abab"` would report a prefix
    // of 4-2 and a matching suffix, and the two ranges would overlap —
    // producing a marker that wraps nothing, or worse, a slice that
    // panics when one string is shorter.
    let s = s.min(a.len() - p).min(b.len() - p);
    // A range is only a change if something is left between the
    // prefix and the suffix; otherwise the whole line matched, and
    // there is nothing to mark on that side.
    let old = (a.len() - s > p).then_some((p, a.len() - s));
    let new = (b.len() - s > p).then_some((p, b.len() - s));
    (old, new)
}

/// Tabs and trailing spaces rendered where a GUI would paint them:
/// every tab becomes `→`, every space in the trailing whitespace run
/// becomes `·`. One byte in, one character out, so the changed byte
/// ranges still line up — the result is a `Vec<char>` the renderer
/// can slice by offset. (The ranges are byte offsets and the fixture
/// is ASCII, so byte offsets and character positions agree; a real
/// client renders UTF-8 and tracks display columns.)
///
/// "Trailing" is computed first, walking back over spaces and tabs, so
/// only the run at the end of the line becomes dots. Interior spaces
/// stay spaces — otherwise every indented line would be unreadable.
fn decorate(line: &[u8]) -> Vec<char> {
    let mut end = line.len();
    while end > 0 && matches!(line[end - 1], b' ' | b'\t') {
        end -= 1;
    }
    line.iter()
        .enumerate()
        .map(|(i, &b)| match b {
            // One byte to one char, so index i here is the same offset
            // the change range refers to.
            b'\t' => '→',
            b' ' if i >= end => '·',
            _ => b as char,
        })
        .collect()
}

/// One cell: right-aligned line number, the visible text with any
/// changed range wrapped in markers, padded to the column width. A
/// row half has `None` here when the row has no line on that side.
fn cell(side: Option<&Side>, no_w: usize, width: usize, open: &str, close: &str) -> String {
    let Some(s) = side else {
        // No line on this side: still emit the line-number column and
        // the separator, so this row's separator lands directly under
        // the one above it. An empty string here would shift the grid.
        return format!("{:nw$} │ {:width$}", "", "", nw = no_w, width = width);
    };
    let vis = decorate(&s.line);
    let text = match s.change {
        // Splitting the decorated text at the change range is legal
        // only because decorate is byte-for-character; the markers are
        // inserted around the slice, not into it, so the range still
        // describes the original bytes.
        Some((a, b)) if b > a => {
            let mut t: String = vis[..a].iter().collect();
            t.push_str(open);
            t.extend(&vis[a..b]);
            t.push_str(close);
            t.extend(&vis[b..]);
            t
        }
        _ => vis.iter().collect(),
    };
    // The pad target has to grow by exactly the markers' width when
    // they were inserted — `width` was computed from marker-free
    // display lengths, so padding to it would make this cell shorter
    // than its neighbours and break the column.
    let pad = if text.chars().count() > vis.len() {
        width + open.len() + close.len()
    } else {
        width
    };
    format!("{:>nw$} │ {:<pad$}", s.no, text, nw = no_w, pad = pad)
}

/// Lines including their `\n`, so the edit script compares real file
/// bytes and a missing final newline counts as a difference. Same rule
/// as step 4; only the printer strips terminators.
fn split_lines(data: &[u8]) -> Vec<&[u8]> {
    data.split_inclusive(|c| *c == b'\n').collect()
}

/// The line as the screen shows it: everything but the final `\n`.
/// Cells never contain a newline — the row owns the line break.
fn strip_newline(line: &[u8]) -> Vec<u8> {
    line.strip_suffix(b"\n").unwrap_or(line).to_vec()
}

// ---- the algorithm: the same edit script step 4 built --------------------

/// Myers' greedy algorithm: the shortest edit script that turns `a`
/// into `b`. One edit per step of distance `d`; each path slides down
/// a diagonal for as long as the lines agree. The V array records how
/// far each diagonal reaches at depth `d`, and the trace of those
/// snapshots is what the backwards walk replays to recover the
/// script. The same function step 4 ran — an example cannot import
/// from another example, so the code travels with the file until the
/// move into the library.
///
/// Nothing here knows about columns or markers: it produces the script
/// `rows` consumes, and the renderer's only requirement is that
/// `Keep` steps stay in order. Change the tie-break and the rows change
/// with it, which is why this copy must stay identical to step 4's.
fn myers(a: &[&[u8]], b: &[&[u8]]) -> Vec<Step> {
    let n = a.len() as isize;
    let m = b.len() as isize;
    let max = n + m;
    let off = max; // diagonal k lives at v[(off + k) as usize]
    // One slot per diagonal from -(n+m) to +(n+m), with `off` as the
    // zero point. At depth d only diagonals of d's parity are reachable,
    // which is why the loop below steps by 2.
    let mut v = vec![0isize; 2 * off as usize + 1];
    // Snapshots of `v`, one per depth, taken before that depth updates
    // it — the backwards replay needs the state as it was *before* d.
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
            // The snake: equal lines are traversed for free, so the
            // cost of the search tracks the number of differences, not
            // the length of the file.
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[(off + k) as usize] = x;
            // Reaching the far corner ends the search: d only grows, so
            // this depth is the shortest script there is.
            if x >= n && y >= m {
                break 'outer;
            }
        }
    }

    // Replay the trace from the end to recover the script.
    //
    // Each snapshot says how far a diagonal had reached *before* depth
    // d, which is what lets the walk step back one edit at a time; the
    // steps come out backwards and are reversed below.
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
        // The shared lines of this diagonal, walked back to front.
        while x > prev_x && y > prev_y {
            x -= 1;
            y -= 1;
            steps.push(Step::Keep);
        }
        if d > 0 {
            // Same diagonal means a vertical move (insert); otherwise
            // a horizontal one (delete). At d == 0 there is no edit —
            // that is the start point.
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

/// One step of the edit script: a line the two files share, a line
/// only the old file has (with its position there), or a line only
/// the new one has.
#[derive(Clone, Copy)]
enum Step {
    Keep,
    Delete { old: usize },
    Insert { new: usize },
}
