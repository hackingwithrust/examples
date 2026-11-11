//! merge: what combining two histories really is. Both sides are
//! diffed against their common ancestor; every region only one side
//! touched is taken as-is, and every region both sides touched is
//! written out as a conflict with markers. The fixture branches
//! twice off one seed: first a pair whose edits land on different
//! lines (the merge is automatic), then a pair that edits the same
//! line (the merge stops, marks it, and the resolution picks a
//! side). git confirms both merge commits.
//!
//! Run with `cargo run --example merge` from the workspace root.
//! The fixture lands in `gitdesk-merge-demo/`; once the second merge
//! commit exists the program says so and exits 0 — delete the
//! fixture directory (or any run that stops midway) to see it again.

use gix::bstr::ByteSlice;
use gix::hash::Kind;
use gix::objs::CommitRef;
use gix::ObjectId;
use std::collections::BTreeMap;
use std::path::Path;

// ---- the fixture data: the one file both branches will fight over --------

// plan.md as the seed commit wrote it — the same bytes, clock and
// author as the fixtures in log.rs and checkout.rs, so the seed
// digest is the one the last two steps printed. Byte-for-byte
// stability matters here: the digest of this blob is what every
// commit below hangs off, and the article quotes those digests.
//
// The content is shaped for merging, not for reading. Every line is
// unique, so a one-line edit produces a one-line region, and the two
// branches of merge 1 are far enough apart that their regions cannot
// touch — that is the whole point of a "clean" merge.
const PLAN_V1: &[u8] = b"# week plan\n\
monday: open the repo\n\
tuesday: status codes\n\
wednesday: diff two blobs\n\
thursday: render a diff\n\
friday: stage by hunk\n\
saturday: write a commit\n\
sunday: browse history\n\
\n\
week after:\n\
check branches\n\
read the commit graph\n\
switch branches\n\
merge a branch\n\
fetch and push\n\
\n\
later:\n\
cherry-pick a fix\n\
tag a release\n\
rebase a branch\n\
keep the history clean\n";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ---- open the fixture, or build it on the first run -----------
    // `try_exists` and not `exists`: an I/O error while stat-ing is
    // reported instead of being read as "no repository here", which
    // would silently rebuild over a half-finished fixture.
    let dir = Path::new("gitdesk-merge-demo");
    let repo = if dir.join(".git").try_exists()? {
        gix::open(dir)?
    } else {
        make_fixture(dir)?
    };
    let heads = repo.git_dir().join("refs/heads");

    // Done-detection: the run ends with main's tip holding two
    // parents. One parent means the fixture is fresh.
    //
    // Branch tips are loose refs — ordinary files under .git, one hex
    // digest and a newline — so reading one is reading text. HEAD is
    // the one ref that is symbolic (`ref: refs/heads/main`) and is
    // never read here: every question this demo asks is about a
    // commit id, and a symbolic ref has none until it is resolved.
    let main_hex = std::fs::read_to_string(heads.join("main"))?;
    let main = ObjectId::from_hex(main_hex.trim().as_bytes())?;
    if parents_of(&repo, main)?.len() == 2 {
        println!("main's tip is already a merge commit with two parents — this");
        println!("run already happened; delete gitdesk-merge-demo/ to see it again.");
        return Ok(());
    }

    // ---- merge 1 --------------------------------------------------
    // main and topic both branch off the seed and edit different
    // lines: ours swaps the thursday line, theirs appends one at the
    // end. The two edit regions share no base line, so the merge
    // takes both and no human is needed.
    //
    // The merge base is the newest commit reachable from both sides;
    // with one commit on each branch that is simply main's only
    // parent, and it is also topic's parent, so the two sides are
    // compared against exactly the same lines.
    //
    // Everything below works on blobs, not on files: a committed file
    // is bytes in the object database, and the worktree copy is never
    // consulted. That is why the fixture can be built without one.
    let seed = parents_of(&repo, main)?[0];
    let topic_hex = std::fs::read_to_string(heads.join("topic"))?;
    let topic = ObjectId::from_hex(topic_hex.trim().as_bytes())?;
    // Three line arrays, one per side. `lines` drops terminators, so
    // these hold content only — position i in each is a base line,
    // not a byte offset.
    let base = lines(&blob_of(&repo, seed)?);
    let ours = lines(&blob_of(&repo, main)?);
    let theirs = lines(&blob_of(&repo, topic)?);

    println!("merge 1 — the two sides edited different lines:");
    println!("  base   {} '{}'", short(seed), subject_of(&repo, seed)?);
    println!(
        "  ours   {} '{}' (main, HEAD)",
        short(main),
        subject_of(&repo, main)?
    );
    println!("  theirs {} '{}' (topic)", short(topic), subject_of(&repo, topic)?);

    let (merged, conflicts) = merge3(&base, &ours, &theirs, "HEAD", "topic");
    println!(
        "  ours changed one line, theirs appended one — the regions are"
    );
    println!("  disjoint, so the merge is automatic ({conflicts} conflicts):");

    // merge3 returns content lines, not a file. Joining with '\n' and
    // appending one puts back the single trailing newline every blob
    // in this fixture has; skipping it would make the new blob differ
    // from the worktree copy in its last byte only.
    let merged_text = merged.join("\n") + "\n";
    println!("\nmerged plan.md:");
    for line in &merged {
        println!("  {line}");
    }

    // ---- the first merge commit: two parents, one tree ------------
    // The merge commit: same tree-or-nothing discipline as every
    // commit so far — the merged blob becomes a tree, the commit
    // carries BOTH tips as parents, and main moves. topic does not:
    // the merge is written on the branch you are on.
    //
    // Parent order is not cosmetic. The first parent is the branch
    // that was checked out, which is what `git log --first-parent`
    // follows and what a later diff against the base means; swapping
    // the two would produce the same tree under a different digest
    // and a confusing history.
    let b_merged = repo.write_blob(merged_text.as_bytes())?.detach();
    let c1 = write_commit(
        &repo,
        write_tree(&repo, b_merged)?,
        &[main, topic],
        1_700_001_300,
        "merge branch 'topic'\n",
    )?;
    std::fs::write(heads.join("main"), format!("{c1}\n"))?;
    std::fs::write(dir.join("plan.md"), merged_text.as_bytes())?;
    println!("\n  committed as {c1} — main moved onto it, topic stayed put.");

    // ---- merge 2 --------------------------------------------------
    // Both sides branch off the first merge commit and edit the same
    // line — sunday — differently. The three-way merge has no
    // automatic answer: it stops and writes markers.
    //
    // The base is now the merge commit itself, so both sides start
    // from identical bytes and each differs from it on exactly one
    // line. Same base range, different replacement: that is the
    // definition of a conflict, and it is the only thing that changed
    // between this merge and the last one.
    let ours2 = swap_line(
        &merged_text,
        "sunday: browse history",
        "sunday: browse the log with swimlanes",
    );
    let theirs2 = swap_line(
        &merged_text,
        "sunday: browse history",
        "sunday: browse history, then sketch the log view",
    );
    let b_ours2 = repo.write_blob(ours2.as_bytes())?.detach();
    let b_theirs2 = repo.write_blob(theirs2.as_bytes())?.detach();
    let m2 = write_commit(
        &repo,
        write_tree(&repo, b_ours2)?,
        &[c1],
        1_700_001_400,
        "browse the log with swimlanes\n",
    )?;
    let t2 = write_commit(
        &repo,
        write_tree(&repo, b_theirs2)?,
        &[c1],
        1_700_001_500,
        "browse history, then sketch the log view\n",
    )?;
    std::fs::write(heads.join("main"), format!("{m2}\n"))?;
    std::fs::write(heads.join("topic"), format!("{t2}\n"))?;

    println!("\nmerge 2 — both sides edited the same line:");
    println!(
        "  base   {} '{}' (the common ancestor now)",
        short(c1),
        subject_of(&repo, c1)?
    );
    println!(
        "  ours   {} '{}' (main, HEAD)",
        short(m2),
        subject_of(&repo, m2)?
    );
    println!("  theirs {} '{}' (topic)", short(t2), subject_of(&repo, t2)?);

    // The base is the merged text already in memory — the same lines
    // the first merge commit recorded — so there is nothing to read
    // back out of the object database.
    let (attempt, conflicts) = merge3(
        &merged,
        &lines(ours2.as_bytes()),
        &lines(theirs2.as_bytes()),
        "HEAD",
        "topic",
    );
    println!(
        "  both sides edited 'sunday: browse history' — the merge stops and"
    );
    println!("  writes the disagreement into the file ({conflicts} conflict):");

    // A conflicted file exists only in the worktree. It has no blob and
    // no tree: the markers are scratch text for a human to edit, and
    // committing them would bake `<<<<<<<` into history.
    let conflicted_text = attempt.join("\n") + "\n";
    std::fs::write(dir.join("plan.md"), conflicted_text.as_bytes())?;
    println!();
    for line in &attempt {
        println!("  {line}");
    }

    // ---- resolution: pick a side, commit, repair the index --------
    // Picking a side: the resolution keeps ours' section of the
    // marked block and drops the markers and theirs. Then the merge
    // commit — two parents, the resolved tree — and the index is
    // rebuilt from the same tree, so git's status comes out clean.
    //
    // Resolving by hand is exactly this: delete the markers and the
    // side you do not want, leave the bytes you do. pick_ours is that
    // edit expressed as a filter, nothing cleverer.
    let resolved = pick_ours(&attempt);
    let resolved_text = resolved.join("\n") + "\n";
    let b_res = repo.write_blob(resolved_text.as_bytes())?.detach();
    let c2 = write_commit(
        &repo,
        write_tree(&repo, b_res)?,
        &[m2, t2],
        1_700_001_600,
        "merge branch 'topic', resolved in ours' favor\n",
    )?;
    std::fs::write(heads.join("main"), format!("{c2}\n"))?;
    std::fs::write(dir.join("plan.md"), resolved_text.as_bytes())?;
    // `git status` compares HEAD against the index and the index
    // against the worktree. A real `git merge` updates the index
    // itself; this one ran in memory, so the index still describes
    // the pre-merge tree and would report plan.md as modified.
    // Rewriting it from the resolved blob is what makes the three
    // states agree — and it is why the demo can claim a clean status.
    let mut files = BTreeMap::new();
    files.insert("plan.md".to_string(), b_res);
    write_index(&repo, &files)?;

    println!("\n  picking a side: ours. The resolved file keeps 'sunday: browse the");
    println!("  log with swimlanes' and is committed as {} with two parents —", short(c2));
    println!("  {} (ours) and {} (theirs). main moved onto it; the index was", short(m2), short(t2));
    println!("  rebuilt from the same tree, so git's status comes out clean.");
    Ok(())
}

/// A three-way merge. Both sides are diffed against `base` — whole
/// lines, longest common subsequence, the diff of step 4 minus the
/// unified formatting — and the two edit scripts are walked in base
/// order. A region only one side touched is taken as-is; regions
/// that share even one base line, or two insertions at the same
/// point, cannot both be taken and come out as one marked conflict.
/// Returns the merged file and the number of conflicts.
fn merge3(
    base: &[String],
    ours: &[String],
    theirs: &[String],
    ours_label: &str,
    theirs_label: &str,
) -> (Vec<String>, usize) {
    // ---- both edit scripts, in one base-ordered list --------------
    // Two separate diffs against the same base, then one sorted list.
    // Sorting by (lo, hi, side) means a single forward sweep can
    // decide who owns each base line, and ours sorts before theirs at
    // an equal position — the order the two halves of a conflict body
    // are printed in.
    let mut all = regions(base, ours, Side::Ours);
    all.extend(regions(base, theirs, Side::Theirs));
    all.sort_by_key(|r| (r.lo, r.hi, r.side as u8));

    let mut out: Vec<String> = Vec::new();
    let mut conflicts = 0;
    let mut pos = 0;
    let mut i = 0;
    while i < all.len() {
        // Grow a group from region i: absorb the regions right after
        // it that clash with the group's combined base range.
        //
        // Widening lo/hi inside the loop is what makes the test
        // transitive: a region that did not clash with the first one
        // can still clash with the range the group has grown into.
        // `lo == hi` says the group is still a pure insertion at a
        // point, where merely touching that point is a clash.
        let (mut lo, mut hi) = (all[i].lo, all[i].hi);
        let mut last = i;
        let mut j = i + 1;
        while j < all.len() && clashes(&all[j], lo, hi, lo == hi) {
            lo = lo.min(all[j].lo);
            hi = hi.max(all[j].hi);
            last = j;
            j += 1;
        }

        // Base lines before the group belong to nobody's edit, so they
        // are copied through verbatim. `pos` only ever moves forward,
        // which is what guarantees no base line is emitted twice.
        out.extend(base[pos..lo].iter().cloned());
        if last == i {
            // A group of one: only one side touched this range, so its
            // replacement text is the answer. No markers, no counter.
            out.extend(all[i].lines.iter().cloned());
        } else {
            // Two or more clashing regions: nobody's text can be
            // picked without losing work, so both are written out with
            // diff3 markers. Both halves are rendered over the same
            // base window [lo, hi) — that is what makes them directly
            // comparable when a human reads the conflict.
            conflicts += 1;
            out.push(format!("<<<<<<< {ours_label}"));
            out.extend(side_text(base, lo, hi, &all[i..=last], Side::Ours));
            out.push("=======".to_string());
            out.extend(side_text(base, lo, hi, &all[i..=last], Side::Theirs));
            out.push(format!(">>>>>>> {theirs_label}"));
        }
        pos = hi;
        i = j;
    }
    // Whatever sits past the last group is untouched by both sides.
    out.extend(base[pos..].iter().cloned());
    (out, conflicts)
}

/// One edit against the base: base[lo..hi] is replaced by `lines` —
/// an empty range is an insertion, empty lines a deletion.
///
/// The invariant that makes this usable: `lo` is fixed once the
/// region opens, `hi` only moves forward on deletes, and `lines` only
/// grows on inserts. So a region is always one contiguous slice of
/// the base replaced by one contiguous slice of the side.
struct Region {
    lo: usize,
    hi: usize,
    lines: Vec<String>,
    side: Side,
}

#[derive(Clone, Copy, PartialEq)]
enum Side {
    Ours,
    Theirs,
}

/// The edit script that turns `base` into `side`: walk the LCS table
/// forward, and every run of lines the two do not share becomes one
/// region. Ties prefer inserting, which keeps the regions minimal
/// for text like this fixture's — a changed line becomes one
/// one-line region, not a run that swallows its neighbours.
fn regions(base: &[String], side: &[String], which: Side) -> Vec<Region> {
    let (n, m) = (base.len(), side.len());
    // ---- the LCS table --------------------------------------------
    // lcs[i][j]: the length of the longest common subsequence of
    // base[i..] and side[j..].
    //
    // Filled backwards so every cell only needs the ones below and to
    // its right, which is the shape the forward walk later reads.
    // Note the cost: an (n+1) x (m+1) table of u32 — fine for a plan
    // with forty lines, out of the question for real files. That is
    // why git's xdiff bounds the work with heuristics before it ever
    // falls back to an exact LCS.
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if base[i] == side[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }

    // ---- the forward walk: table cells become regions -------------
    let mut out: Vec<Region> = Vec::new();
    let mut cur: Option<Region> = None;
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        // A shared line means the current region, if any, is finished:
        // regions never span a line both sides keep.
        if i < n && j < m && base[i] == side[j] {
            if let Some(r) = cur.take() {
                out.push(r);
            }
            i += 1;
            j += 1;
            continue;
        }
        // Delete a base line (side has nothing where it was) or
        // insert a side line (base has nothing for it) — either way
        // it joins the region being built, and a match ends it.
        //
        // `>` and not `>=` is the tie-break: when both moves score the
        // same, take the insert. On a rewritten line that keeps the
        // region at one base line replaced by one side line instead of
        // a delete-then-insert pair that drags in its neighbours.
        let delete = i < n && (j >= m || lcs[i + 1][j] > lcs[i][j + 1]);
        let r = cur.get_or_insert(Region {
            lo: i,
            hi: i,
            lines: Vec::new(),
            side: which,
        });
        if delete {
            r.hi = i + 1;
            i += 1;
        } else {
            r.lines.push(side[j].clone());
            j += 1;
        }
    }
    // A region still open at the end covers the file's tail.
    if let Some(r) = cur.take() {
        out.push(r);
    }
    out
}

/// Two edits clash when their base ranges share interior — at least
/// one base line both sides changed — or when both are insertions at
/// the same point. Edges may touch: ours' replacement ending at line
/// 5 and theirs' starting at line 5 merge cleanly.
///
/// `interior` is the overlap test for half-open ranges; written as
/// `r.lo < hi && lo < r.hi` so that touching edges (r.lo == hi) are
/// not an overlap. That single inequality is the difference between
/// git merging two adjacent edits and flagging them as a conflict.
fn clashes(r: &Region, lo: usize, hi: usize, empty: bool) -> bool {
    let interior = r.lo < hi && lo < r.hi;
    let both_insert = empty && r.lo == r.hi && r.lo == lo;
    interior || both_insert
}

/// One side's version of base[lo..hi]: its regions spliced in, the
/// base lines it did not touch kept.
///
/// Both halves of a conflict go through this with the same `lo`/`hi`,
/// so the two bodies describe the same base window. Read one against
/// the other and the disagreement is exactly the regions.
fn side_text(base: &[String], lo: usize, hi: usize, regs: &[Region], side: Side) -> Vec<String> {
    let mut out = Vec::new();
    let mut p = lo;
    for r in regs.iter().filter(|r| r.side == side) {
        // Base lines this side left alone between its own edits.
        out.extend(base[p..r.lo].iter().cloned());
        out.extend(r.lines.iter().cloned());
        p = r.hi;
    }
    out.extend(base[p..hi].iter().cloned());
    out
}

/// Resolve a conflicted file by keeping ours: everything between the
/// `<<<<<<<` marker and `=======` stays, `=======` through the
/// `>>>>>>>` marker goes, and the markers themselves go too.
///
/// A two-flag state machine rather than one, because the three marker
/// lines do three different things: open (start keeping), close the
/// ours half (stop keeping), close the conflict (start keeping again).
/// The input is text merge3 just produced, so it is well formed by
/// construction and no validation is needed here.
fn pick_ours(text: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let (mut in_conflict, mut skipping_theirs) = (false, false);
    for line in text {
        if line.starts_with("<<<<<<<") {
            in_conflict = true;
            skipping_theirs = false;
            continue;
        }
        if in_conflict && line == "=======" {
            skipping_theirs = true;
            continue;
        }
        if in_conflict && line.starts_with(">>>>>>>") {
            in_conflict = false;
            skipping_theirs = false;
            continue;
        }
        if !skipping_theirs {
            out.push(line.clone());
        }
    }
    out
}

/// A blob's bytes as lines — everything in this fixture is UTF-8
/// text with a trailing newline.
///
/// Two properties of `str::lines` carry the rest of the file:
/// a trailing newline does not produce a final empty line (so line
/// counts match what a diff would show), and a trailing `\r` is
/// stripped with the `\n`. The second one is a real difference from
/// splitting on bytes: on a CRLF file this would quietly lose the
/// `\r`, and a merge that has to preserve bytes must not do that.
fn lines(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::to_string)
        .collect()
}

/// `base` with the one line equal to `find` replaced by `to`. The
/// fixture is fixed, so absence would be a bug, not an error case.
///
/// Rebuilding through `lines` + `join` normalises the file to LF with
/// a single trailing newline. That is what keeps the two sides of a
/// merge byte-identical apart from the one line being tested.
fn swap_line(base: &str, find: &str, to: &str) -> String {
    base.lines()
        .map(|l| if l == find { to.to_string() } else { l.to_string() })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// The bytes of plan.md in a commit's tree.
///
/// Two hops, because a commit does not point at files: commit -> tree
/// -> blob. `peel_to_tree` follows the first hop; the loop then picks
/// the one entry this fixture ever writes. Raw `Vec<u8>`, not `String`
/// — the object database stores bytes and the merge decides for
/// itself that they are text.
fn blob_of(repo: &gix::Repository, commit: ObjectId) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let tree = repo.find_object(commit)?.peel_to_tree()?;
    for entry in tree.iter() {
        let entry = entry?;
        if String::from_utf8_lossy(entry.filename()) == "plan.md" {
            return Ok(repo.find_object(entry.oid().to_owned())?.data.to_vec());
        }
    }
    Err("plan.md is in every tree of this fixture".into())
}

/// First line of a commit's message — the log's one-line form.
///
/// A commit object is plain text (`tree <hex>`, `parent <hex>` per
/// parent, blank line, message), so `CommitRef` can borrow from the
/// buffer instead of copying it; the message keeps its trailing
/// newline, which is why the first line is taken rather than the whole
/// thing.
fn subject_of(
    repo: &gix::Repository,
    id: ObjectId,
) -> Result<String, Box<dyn std::error::Error>> {
    let obj = repo.find_object(id)?;
    let raw = CommitRef::from_bytes(&obj.data, repo.object_hash())?;
    Ok(String::from_utf8_lossy(raw.message)
        .lines()
        .next()
        .unwrap_or("")
        .to_string())
}

/// Every parent of a commit — one for an ordinary commit, two for a
/// merge.
///
/// The header stores parents as hex text, so each one has to be parsed
/// back into an id. The `expect` is deliberate: the bytes came from a
/// commit object this program wrote, and a malformed parent there is
/// corruption, not a case to handle.
fn parents_of(
    repo: &gix::Repository,
    id: ObjectId,
) -> Result<Vec<ObjectId>, Box<dyn std::error::Error>> {
    let obj = repo.find_object(id)?;
    let raw = CommitRef::from_bytes(&obj.data, repo.object_hash())?;
    Ok(raw
        .parents
        .iter()
        .map(|p| ObjectId::from_hex(p.as_bytes()).expect("parent is a valid hex digest"))
        .collect())
}

/// The first seven hex digits. Seven is git's default abbreviation —
/// unique in a fixture this size, and short enough for a transcript to
/// line up.
fn short(id: ObjectId) -> String {
    let hex = id.to_string();
    hex[..7].to_string()
}

/// The fixture: the seed commit, then main and topic branching off
/// it — one commit each, both parents of the first merge. No worktree
/// file and no index: the merge writes both.
///
/// Nothing here shells out to git. A repository is an object database
/// plus refs, so "make a branch that edited thursday" is: write a blob,
/// write a tree naming it, write a commit whose parent is the seed, and
/// write the ref file. The parent list is the only thing that makes it
/// a branch history.
fn make_fixture(dir: &Path) -> Result<gix::Repository, Box<dyn std::error::Error>> {
    let repo = gix::init(dir)?;
    let b_seed = repo.write_blob(PLAN_V1)?.detach();
    // No parents: the seed is a root commit, and merge 1's base is its
    // only child's first parent — the same id on both sides.
    let seed = write_commit(&repo, write_tree(&repo, b_seed)?, &[], 1_700_000_100, "seed commit\n")?;

    // ours: rewrite the thursday line in place.
    let m1 = swap_line(
        std::str::from_utf8(PLAN_V1)?,
        "thursday: render a diff",
        "thursday: render diffs side by side",
    );
    let b_m1 = repo.write_blob(m1.as_bytes())?.detach();
    let m1 = write_commit(
        &repo,
        write_tree(&repo, b_m1)?,
        &[seed],
        1_700_001_100,
        "render diffs side by side\n",
    )?;

    // theirs: append a line at the end. Concatenating raw bytes keeps
    // the seed's trailing newline and adds exactly one line after it,
    // so the region this side produces is an insertion at the tail —
    // the furthest possible point from thursday's rewrite.
    let t1 = [PLAN_V1, b"sketch the log view\n"].concat();
    let b_t1 = repo.write_blob(&t1)?.detach();
    let t1 = write_commit(&repo, write_tree(&repo, b_t1)?, &[seed], 1_700_001_200, "sketch the log view\n")?;

    // Refs by hand: a branch is a file holding one digest. HEAD is the
    // only symbolic ref, and it points at main so `git status` in the
    // fixture has a checked-out branch to report on.
    let heads = repo.git_dir().join("refs/heads");
    std::fs::create_dir_all(&heads)?;
    std::fs::write(heads.join("main"), format!("{m1}\n"))?;
    std::fs::write(heads.join("topic"), format!("{t1}\n"))?;
    std::fs::write(repo.git_dir().join("HEAD"), "ref: refs/heads/main\n")?;
    Ok(repo)
}

// ---- object plumbing: trees, commits, the index --------------------------

/// Write the one-entry tree and hand back its digest — a commit
/// points at a tree object, not at a `Tree` value.
fn write_tree(
    repo: &gix::Repository,
    blob: ObjectId,
) -> Result<ObjectId, Box<dyn std::error::Error>> {
    Ok(repo.write_object(&tree(blob))?.detach())
}

/// One tree, one entry: plan.md points at `blob`.
fn tree(blob: ObjectId) -> gix::objs::Tree {
    gix::objs::Tree {
        entries: vec![gix::objs::tree::Entry {
            mode: gix::objs::tree::EntryKind::Blob.into(),
            filename: "plan.md".into(),
            oid: blob,
        }],
    }
}

/// Write a commit object and hand back its digest. `seconds` is the
/// commit time, pinned by every caller: a real clock would make the
/// digest — and the transcript — different on every run.
///
/// Author and committer are the same person here. They are two fields
/// because they are genuinely two events: who wrote the change and who
/// applied it, which differ after a rebase or a cherry-pick.
fn write_commit(
    repo: &gix::Repository,
    tree: ObjectId,
    parents: &[ObjectId],
    seconds: i64,
    message: &str,
) -> Result<ObjectId, Box<dyn std::error::Error>> {
    let who = gix::actor::Signature {
        name: "Demo".into(),
        email: "demo@example.com".into(),
        time: gix::date::Time::new(seconds, 0),
    };
    let commit = gix::objs::Commit {
        tree,
        // Empty for a root commit, one id for an ordinary commit, two
        // for a merge. This slice is the entire difference.
        parents: parents.to_vec().into(),
        author: who.clone(),
        committer: who,
        encoding: None,
        message: message.into(),
        extra_headers: vec![],
    };
    Ok(repo.write_object(&commit)?.detach())
}

/// Write a version-2 index by hand — `DIRC` header, one entry per
/// path, SHA-1 trailer. The same bytes step 2 wrote.
///
/// The index is not a cache that can be skipped: it is the staging
/// area, and it is what `git status` and `git diff --cached` read.
/// Entries are sorted by path (a BTreeMap gives that for free) and
/// each one carries a stat block *and* the blob digest — git trusts
/// the stat data to decide whether to re-hash, so stale stats cost
/// correctness only in speed, never in content.
fn write_index(
    repo: &gix::Repository,
    files: &BTreeMap<String, ObjectId>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Header: signature, version, entry count — all big-endian.
    let mut buf = Vec::new();
    buf.extend_from_slice(b"DIRC");
    buf.extend_from_slice(&2u32.to_be_bytes()); // version 2
    buf.extend_from_slice(&(files.len() as u32).to_be_bytes());

    for (path, id) in files {
        // Ten stat words. The demo pins mtime and zeros the rest;
        // real `git checkout` fills them in from the filesystem.
        let stat: [u32; 10] = [
            0, 0, // ctime seconds, nanoseconds
            1_700_000_100, 0, // mtime seconds, nanoseconds
            0, 0, // device, inode
            0o100644, // mode: regular file
            0, 0, // uid, gid
            0, // size: unused here, status re-hashes instead
        ];
        for word in stat {
            buf.extend_from_slice(&word.to_be_bytes());
        }
        buf.extend_from_slice(id.as_slice());
        let path = path.as_bytes();
        // The flags word packs the path length in its low 12 bits; the
        // upper bits are stage number and assume-valid, both zero here.
        buf.extend_from_slice(&(path.len() as u16).to_be_bytes()); // flags
        buf.extend_from_slice(path);
        // Version 2 pads each entry with 0..7 NULs so that the next one
        // starts on an 8-byte boundary — measured from the end of the
        // header, hence the `- 12`, and the `+ 1` counts the NUL that
        // every path is guaranteed at least.
        let entry_len = buf.len() - 12;
        let target = (entry_len + 1).div_ceil(8) * 8;
        buf.resize(12 + target, 0);
    }

    // The trailer is a SHA-1 over everything above it, which is what
    // makes a truncated or hand-edited index detectable.
    let mut hasher = gix::hash::hasher(Kind::Sha1);
    hasher.update(&buf);
    buf.extend_from_slice(hasher.try_finalize()?.as_slice());

    std::fs::write(repo.git_dir().join("index"), buf)?;
    Ok(())
}
