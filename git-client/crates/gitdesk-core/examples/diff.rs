//! diff: compare the blob HEAD records against the blob the index
//! stages, path by path, and print what `git diff --cached` would
//! print — hunks, binary files, and nothing for paths whose digests
//! already agree.
//!
//! Run with `cargo run --example diff` from the workspace root. The
//! fixture lands in `gitdesk-diff-demo/`; when it is already there,
//! the program skips creation and just diffs.

use gix::hash::Kind;
use std::collections::BTreeMap;
use std::path::Path;

// ---- the fixture data: two text versions, two binary ones ----------------

// The README pair rewrites one line in the intro and one line at the
// very end. They are far enough apart that `diff -U3` cannot join them
// into one hunk — that is the point: the reader sees two hunks and
// learns that a hunk is a *window*, not a file.
const README_V1: &[u8] = b"# demo\n\nA scratch plan for the client.\n\n\
Monday: write the diff tool\nTuesday: test the diff tool\n\
Wednesday: stage by hunk\nThursday: render side by side\n\
Friday: open the repository\nSaturday: fetch and push\nSunday: rest\n";
const README_V2: &[u8] = b"# demo\n\nA scratch plan for the client, revised.\n\n\
Monday: write the diff tool\nTuesday: test the diff tool\n\
Wednesday: stage by hunk\nThursday: render side by side\n\
Friday: open the repository\nSaturday: fetch and push\nSunday: rest (earned)\n";
// A real PNG header, cut off after sixteen bytes. The two versions
// differ in one length field, so the digests differ and the path is
// worth printing — but the NUL bytes make it binary, and a line-based
// diff must refuse to pretend otherwise.
const LOGO_V1: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR\x00\x00\x00\x10\x00\x00\x00\x10";
const LOGO_V2: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0eIHDR\x00\x00\x00\x10\x00\x00\x00\x10";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ---- build the fixture on the first run, reuse it after -------
    let dir = Path::new("gitdesk-diff-demo");
    if !dir.join(".git").try_exists()? {
        make_fixture(dir)?;
    }
    let repo = gix::open(dir)?;

    // ---- the two sides of `git diff --cached` ---------------------
    // The index is the "new" side and HEAD's tree is the "old" one.
    // Getting that direction right matters: the printed hunk says what
    // would be committed if you ran `git commit` right now, so every
    // `-` line is what HEAD has and every `+` line is what is staged.
    let head = head_tree_files(&repo)?;
    let index = index_files(&repo)?;

    // Iterating the index (a BTreeMap) walks paths in sorted order, so
    // the output is stable — the same reason git prints files sorted.
    for (path, staged) in &index {
        // A path HEAD has never heard of needs an empty side, not a
        // neighbour; additions and deletions come later in the track.
        let Some(committed) = head.get(path) else {
            continue;
        };
        // Same digest, same bytes: no diff, and nothing printed. This
        // shortcut is what makes every printed hunk worth reading.
        //
        // It is also an invariant, not an optimisation: identical
        // content has identical digests, so two equal ids can never
        // produce a hunk. Checking ids first means the text diff only
        // ever runs on pairs that really differ.
        if committed.id == *staged {
            continue;
        }
        print_diff(&repo, path, committed, *staged)?;
    }
    Ok(())
}

/// One path's stake in the object database: which blob, with the mode
/// its tree entry carries.
struct BlobRef {
    id: gix::ObjectId,
    // The mode is not decoration: the `index a..b 100644` header line
    // prints it, and it is what would tell a reader that a file became
    // executable or a symlink. Read from the tree entry, not from disk.
    mode: String, // octal, e.g. "100644"
}

// ---- the fixture: three paths, two of them staged ------------------------

/// Build a repository where two tracked paths have a staged edit and
/// one does not: README.md gets two separate one-line rewrites, the
/// logo gets a byte-level change a text diff must refuse to guess at,
/// and app.rs does not change at all.
///
/// Three states have to be written for the fixture to be honest:
/// the commit (what HEAD records), the index (what is staged) and the
/// worktree files (what is on disk). They are separate on purpose —
/// everything this program prints is the difference between the first
/// two, and the third is only there so `git status` agrees.
fn make_fixture(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let repo = gix::init(dir)?;

    // Blobs are content-addressed: writing the same bytes twice yields
    // the same id. Only the bytes exist here — no filenames yet.
    let readme_v1 = repo.write_blob(README_V1)?;
    let readme_v2 = repo.write_blob(README_V2)?;
    let app_v1 = repo.write_blob(b"fn main() {}\n")?;
    let logo_v1 = repo.write_blob(LOGO_V1)?;
    let logo_v2 = repo.write_blob(LOGO_V2)?;

    // The tree is where names and modes enter: it maps paths to blob
    // ids and is itself an object with its own digest. This is the
    // committed snapshot, so it names the v1 blobs.
    let tree = gix::objs::Tree {
        entries: vec![
            blob_entry("README.md", readme_v1.detach()),
            blob_entry("app.rs", app_v1.detach()),
            blob_entry("logo.bin", logo_v1.detach()),
        ],
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
        // Empty: a root commit. Nothing is being diffed *as history*
        // here — the comparison is HEAD's tree against the index.
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

    // The index stages the v2 blobs of both edited files and keeps
    // app.rs as committed. Note what is *not* here: no tree, no commit.
    // The index is a flat list of paths; it does not nest.
    write_index(
        &repo,
        &[
            ("README.md", readme_v2.detach()),
            ("app.rs", app_v1.detach()),
            ("logo.bin", logo_v2.detach()),
        ],
    )?;

    // The worktree agrees with the index, so a plain `git status`
    // reads the fixture as two staged edits and nothing else. If these
    // files held the v1 bytes instead, status would additionally report
    // two unstaged modifications — the diff printed below would not
    // change, but the fixture would no longer be self-explanatory.
    std::fs::write(dir.join("README.md"), README_V2)?;
    std::fs::write(dir.join("app.rs"), b"fn main() {}\n")?;
    std::fs::write(dir.join("logo.bin"), LOGO_V2)?;
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
/// path, SHA-1 trailer. The same bytes steps 2 and 3 wrote.
///
/// The ten stat words are a fast path git uses to skip re-hashing a
/// file whose mtime and size look unchanged. This program never looks
/// at them: it reads the blob id, so the digests alone decide what
/// differs. Filling them with zeros is safe precisely because of that.
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
        // Version 2 has no length field for the path: the flags word
        // carries it in its low 12 bits, and the path runs from the
        // next byte to the first NUL.
        buf.extend_from_slice(&(path.len() as u16).to_be_bytes()); // flags
        buf.extend_from_slice(path);
        // Every record is padded to a multiple of 8 bytes counted from
        // the end of the 12-byte header, and always has at least one
        // NUL after the path.
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
///
/// `head()` resolves HEAD the ref; `into_peeled_id()` follows it to a
/// commit, and `peel_to_tree` makes the last hop to the tree object.
/// Only then is there a flat list of entries to walk.
fn head_tree_files(
    repo: &gix::Repository,
) -> Result<BTreeMap<String, BlobRef>, Box<dyn std::error::Error>> {
    let id = repo.head()?.into_peeled_id()?;
    let tree = repo.find_object(id)?.peel_to_tree()?;
    let mut files = BTreeMap::new();
    for entry in tree.iter() {
        let entry = entry?;
        let mode = String::from_utf8_lossy(entry.mode().kind().as_octal_str()).into_owned();
        files.insert(
            String::from_utf8_lossy(entry.filename()).into_owned(),
            BlobRef {
                id: entry.oid().to_owned(),
                mode,
            },
        );
    }
    Ok(files)
}

/// The index as the same kind of map. No tree walk is involved: the
/// index is already a flat, path-sorted list, so this is a straight
/// copy into something with O(log n) lookup.
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

/// The header block git prints per changed path, then either one line
/// for a binary pair or the hunks for a text one.
///
/// `old` comes from HEAD and `new_id` from the index; the caller has
/// already established that the two digests differ.
fn print_diff(
    repo: &gix::Repository,
    path: &str,
    old: &BlobRef,
    new_id: gix::ObjectId,
) -> Result<(), Box<dyn std::error::Error>> {
    // Two header lines before any content: the `a/` and `b/` prefixes
    // are what makes the output patch-appliable (`-p1` strips them),
    // and the `index` line names both blob ids so a reader can fetch
    // them from the object database.
    println!("diff --git a/{path} b/{path}");
    println!("index {}..{} {}", old.id, new_id, old.mode);
    let old_obj = repo.find_object(old.id)?;
    let new_obj = repo.find_object(new_id)?;
    let old_bytes: &[u8] = &old_obj.data;
    let new_bytes: &[u8] = &new_obj.data;

    // Git's heuristic: a NUL byte anywhere in the first 8000 bytes
    // marks the file as binary, and no text algorithm is applied.
    // Both sides are tested: a text file that *becomes* binary must
    // not be diffed as if its new bytes were lines.
    if is_binary(old_bytes) || is_binary(new_bytes) {
        println!("Binary files a/{path} and b/{path} differ");
        return Ok(());
    }

    // The per-file body: one `---`/`+++` pair, then the hunks. A file
    // with no differing lines never reaches here — main filtered it on
    // the digests — so an empty body has no meaning.
    println!("--- a/{path}");
    println!("+++ b/{path}");
    for hunk in unified_hunks(old_bytes, new_bytes) {
        print_hunk(&hunk);
    }
    Ok(())
}

/// Git's binary test, and only that: the first 8000 bytes, looking for
/// one NUL. It is a heuristic, not a declaration — a UTF-16 text file
/// is "binary" to it, and a PDF with no NUL in its first page is not.
/// The slice is clamped to the buffer, so short files are fine.
fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(8000)].contains(&0)
}

/// Three lines of context on each side of every change. This is the
/// `3` in `diff -U3` and the number every Git forge shows by default;
/// it is enough to recognise surrounding code without burying the
/// change.
const CONTEXT: usize = 3;

// ---- the algorithm: edit script, groups, hunks ---------------------------

/// One step of the edit script: a line the two files share (with its
/// position in the old file), a line only the old file has, or a line
/// only the new one has.
///
/// `Keep` carries `old` and not `new` because the two indices differ
/// by a running offset anyway; `Delete` and `Insert` carry the index
/// into the file they remove from or add to. All positions are
/// 0-based here and become 1-based only when the header is printed.
#[derive(Clone, Copy)]
enum Step {
    Keep { old: usize },
    Delete { old: usize },
    Insert { new: usize },
}

/// One printed hunk: a window of both files, with the changed lines
/// still marked. Starts are 1-based because that is what `@@` prints;
/// lengths count lines, and a length of 0 means the window is a pure
/// insertion or deletion at that point.
struct Hunk {
    old_start: usize, // 1-based, as printed in the @@ header
    old_len: usize,
    new_start: usize,
    new_len: usize,
    func: Vec<u8>,               // section label git appends to the @@ line
    lines: Vec<(char, Vec<u8>)>, // ' ', '-' or '+', text without newline
}

/// The whole text diff: line the files up, turn the differences into
/// hunks, and let the caller print them.
///
/// Three phases, and each one is a separate idea:
///   1. split into lines and run Myers -> an edit script of steps;
///   2. collapse adjacent steps -> groups (a group is one changed
///      region, described by where it starts and how much it changes);
///   3. cluster nearby groups -> hunks, then materialise the lines
///      each hunk prints, with context.
fn unified_hunks(old: &[u8], new: &[u8]) -> Vec<Hunk> {
    // ---- 1. bytes to lines to an edit script ----------------------
    let a = split_lines(old);
    let b = split_lines(new);
    let steps = myers(&a, &b);

    // ---- 2. steps to groups ---------------------------------------
    // Runs of consecutive edits become groups, recorded the way
    // xdiff records them: where the group starts in each file, and
    // how many lines it removes or adds.
    //
    // The positions are "before" positions: old_before is the number
    // of old lines that precede the group, which is also the index of
    // the first line it deletes.
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
                    // A rewrite arrives as deletes followed by inserts, so
                    // the second half of one change would look like the
                    // start of a new group. The coordinate check is what
                    // says "these are the same edit": the previous group
                    // ends exactly where this step begins.
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

    // ---- 3. groups to hunks ---------------------------------------
    // Groups whose unchanged gap is at most 2 * CONTEXT share a hunk —
    // the rule git's xdiff applies for `diff -U3`. The reason for
    // exactly twice the context: each group wants CONTEXT lines below
    // it and the next wants CONTEXT above it, so a gap of 6 still
    // prints as continuous lines. One line more and the two windows
    // would have a gap, which means a new hunk.
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
        // The window: CONTEXT lines around the outermost groups,
        // clamped at both ends of the file. `saturating_sub` is what
        // makes a change on line 1 print without negative context.
        let old_lo = first.old_before.saturating_sub(CONTEXT);
        let old_hi = (last.old_before + last.old_chg + CONTEXT).min(a.len());
        let new_lo = first.new_before.saturating_sub(CONTEXT);
        let new_hi = (last.new_before + last.new_chg + CONTEXT).min(b.len());

        // Walk the script again and keep only the lines inside the
        // window. Within a change group deletions print before
        // insertions — the order git uses.
        //
        // Rewalking, rather than slicing the step list, is what keeps
        // the `-`/`+` ordering right: a group may interleave deletes
        // and inserts in the script, but the printed form always shows
        // all the old lines, then all the new ones.
        let mut lines: Vec<(char, Vec<u8>)> = Vec::new();
        let mut i = 0usize;
        while i < steps.len() {
            if let Step::Keep { old, .. } = steps[i] {
                // Context line: kept only if it falls inside this
                // hunk's window, which is why the same step can be
                // printed by one hunk and skipped by the next.
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
            // 0-based internally, 1-based in the header. An empty range
            // is the exception: there is no first line to count from,
            // so git prints the number *before* the insertion point —
            // `old_lo` as-is, not `old_lo + 1`.
            old_start: if old_hi == old_lo { old_lo } else { old_lo + 1 },
            old_len: old_hi - old_lo,
            new_start: if new_hi == new_lo { new_lo } else { new_lo + 1 },
            new_len: new_hi - new_lo,
            // The label is looked up from where the window starts, not
            // from the first change: git's rule is "the nearest
            // function line above the hunk".
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
fn print_hunk(h: &Hunk) {
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

/// Split on '\n' but keep the terminator, so a line's bytes are exactly
/// what the file had — `strip_newline` removes it only at print time.
/// A last line without a newline comes out as one short slice, and an
/// empty file comes out as no lines at all. Slicing on bytes rather
/// than `str` means this works on files that are not UTF-8.
fn split_lines(data: &[u8]) -> Vec<&[u8]> {
    data.split_inclusive(|c| *c == b'\n').collect()
}

/// The printable form of a line: everything but the final `\n`. The
/// diff algorithm compares lines *with* their terminators, so
/// "no newline at end of file" is a difference like any other; only
/// the printer drops it, because git signals that case in a `\ No
/// newline` remark rather than by printing a bare newline.
fn strip_newline(line: &[u8]) -> Vec<u8> {
    line.strip_suffix(b"\n").unwrap_or(line).to_vec()
}

/// Myers' greedy algorithm: the shortest edit script that turns `a`
/// into `b`. One edit per step of distance `d`; each path slides down
/// a diagonal for as long as the lines agree. The V array records how
/// far each diagonal reaches at depth `d`, and the trace of those
/// snapshots is what the backwards walk replays to recover the
/// script.
///
/// Coordinates are the only confusing part. A point (x, y) sits on
/// diagonal k = x - y; moving right is a delete (consumes an `a` line),
/// moving down is an insert (consumes a `b` line). At depth `d` only
/// diagonals with the same parity as `d` are reachable, which is why
/// the loop steps by 2 — and why `v` needs 2 * (n + m) + 1 slots:
/// every diagonal from -(n+m) to +(n+m), shifted by `off` to index it.
fn myers(a: &[&[u8]], b: &[&[u8]]) -> Vec<Step> {
    let n = a.len() as isize;
    let m = b.len() as isize;
    let max = n + m;
    let off = max; // diagonal k lives at v[(off + k) as usize]
    let mut v = vec![0isize; 2 * off as usize + 1];
    let mut trace: Vec<Vec<isize>> = Vec::new();

    'outer: for d in 0..=max {
        // Snapshot before this depth touches `v`: the replay needs the
        // depths below d to recover where each path came from. Keeping
        // every snapshot costs O(d^2) memory — acceptable here, and the
        // reason git has a cheaper "divide and conquer" variant for
        // large inputs.
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
            // The snake: while the two lines agree, slide down the
            // diagonal for free. Free is the point — a run of equal
            // lines costs no edits, which is what keeps the algorithm
            // proportional to the number of differences, not to the
            // file size.
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[(off + k) as usize] = x;
            // Both files consumed: this is the shortest script at depth
            // d, and deeper searches can only be longer.
            if x >= n && y >= m {
                break 'outer;
            }
        }
    }

    // Replay the trace from the end to recover the script. Going
    // backwards, each move is a step that has already happened, so the
    // reconstruction runs end to start and the result is reversed at
    // the bottom. `prev_k` re-applies the same preference rule used
    // above — the two must agree, or the walk would reconstruct a path
    // the forward pass never took.
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
        // Walk back up the diagonal the forward pass slid down: every
        // step here is a line both files share.
        while x > prev_x && y > prev_y {
            x -= 1;
            y -= 1;
            steps.push(Step::Keep { old: x as usize });
        }
        if d > 0 {
            // The one edit this depth was reached by: same diagonal
            // means the move was vertical (insert), otherwise
            // horizontal (delete). `d == 0` is the start, with no edit.
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
