//! stage: write part of a change into the index. The diff between
//! HEAD and the worktree splits into hunks; a staging UI shows them
//! numbered, and the ones the reader clicks get applied to HEAD's
//! blob and written into the index. The worktree keeps everything.
//!
//! Run with `cargo run --example stage` from the workspace root.
//! The fixture lands in `gitdesk-stage-demo/`; when it is already
//! there, the program skips creation and just stages.

use gitdesk_core::diff::{print_hunk, split_lines, unified_hunks, Hunk};
use gix::hash::Kind;
use std::collections::BTreeMap;
use std::path::Path;

// The click: hunk 2 — the insertion — is staged. The two rewrites
// stay in the worktree, still unstaged.
const SELECTED: &[usize] = &[2];

// One tracked file, three edits between HEAD and the worktree, kept
// far enough apart that git's U3 grouping prints them as three
// hunks: line 5 is rewritten, a line is inserted before old line 14,
// and line 21 is rewritten.
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

const PLAN_V3: &[u8] = b"# week plan\n\
monday: open the repo\n\
tuesday: status codes\n\
wednesday: diff two blobs\n\
thursday: render diffs side by side\n\
friday: stage by hunk\n\
saturday: write a commit\n\
sunday: browse history\n\
\n\
week after:\n\
check branches\n\
read the commit graph\n\
switch branches\n\
decide what to stage\n\
merge a branch\n\
fetch and push\n\
\n\
later:\n\
cherry-pick a fix\n\
tag a release\n\
rebase a branch\n\
keep the history clean, one commit at a time\n";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = Path::new("gitdesk-stage-demo");
    if !dir.join(".git").try_exists()? {
        make_fixture(dir)?;
    }
    let repo = gix::open(dir)?;

    let head = head_tree_files(&repo)?;
    let Some(&committed) = head.get("plan.md") else {
        panic!("plan.md is committed");
    };

    // Staging works from HEAD against the worktree. The index still
    // agrees with HEAD here, so diffing against either gives the same
    // hunks; when the index already holds changes, `git add -p` diffs
    // worktree against the index instead. This demo starts clean.
    let old_bytes: &[u8] = &repo.find_object(committed)?.data;
    let worktree = std::fs::read(dir.join("plan.md"))?;

    // Step 3's verdict stands: a NUL byte in the first 8000 bytes
    // means binary, and no text algorithm runs — including staging.
    if old_bytes[..old_bytes.len().min(8000)].contains(&0)
        || worktree[..worktree.len().min(8000)].contains(&0)
    {
        println!("binary — nothing to stage by line");
        return Ok(());
    }

    let hunks = unified_hunks(old_bytes, &worktree);
    println!("plan.md — HEAD against the worktree: {} hunks", hunks.len());
    for (i, h) in hunks.iter().enumerate() {
        println!();
        println!(
            "hunk {} of {} — staged: {}",
            i + 1,
            hunks.len(),
            SELECTED.contains(&(i + 1))
        );
        print_hunk(h);
    }

    // What the index should hold: HEAD with the selected hunks
    // applied. Write it as a blob, point the index entry at it.
    let staged = apply_hunks(&split_lines(old_bytes), &hunks, SELECTED);
    let staged_id = repo.write_blob(&staged)?.detach();
    write_index(&repo, &[("plan.md", staged_id)])?;
    println!();
    println!("staged {staged_id} — the index now holds HEAD plus the selected hunks");

    // The round trip: diff the staged blob against the worktree, and
    // what remains is exactly what the reader did not click.
    let rest = unified_hunks(&staged, &worktree);
    println!();
    println!("still only in the worktree: {} unstaged hunk(s)", rest.len());
    for h in &rest {
        println!();
        print_hunk(h);
    }
    Ok(())
}

/// HEAD with the selected hunks applied. A selected hunk keeps its
/// context and insertion lines and drops its deletions; an unselected
/// one contributes nothing, and the old lines it covers pass through
/// unchanged. Every line ends in `\n` — git's `\ No newline at end of
/// file` case is a flag in the index this demo does not model.
fn apply_hunks(a: &[&[u8]], hunks: &[Hunk], selected: &[usize]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut cursor = 0usize; // 0-based position in `a`
    for (i, h) in hunks.iter().enumerate() {
        let start = h.old_start - 1;
        out.extend_from_slice(&a[cursor..start].concat());
        cursor = start + h.old_len;
        if !selected.contains(&(i + 1)) {
            out.extend_from_slice(&a[start..cursor].concat());
            continue;
        }
        for (kind, text) in &h.lines {
            match kind {
                // Deletions stay behind in the worktree.
                '-' => {}
                _ => {
                    out.extend_from_slice(text);
                    out.push(b'\n');
                }
            }
        }
    }
    out.extend_from_slice(&a[cursor..].concat());
    out
}

/// Build a repository with one tracked file whose worktree version
/// differs from HEAD in three well-separated edits, and an index that
/// still agrees with HEAD — nothing staged yet.
fn make_fixture(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let repo = gix::init(dir)?;

    let plan_v1 = repo.write_blob(PLAN_V1)?;

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

    // The index agrees with HEAD; the worktree holds all three edits.
    write_index(&repo, &[("plan.md", plan_v1.detach())])?;
    std::fs::write(dir.join("plan.md"), PLAN_V3)?;
    Ok(())
}

fn blob_entry(name: &str, oid: gix::ObjectId) -> gix::objs::tree::Entry {
    gix::objs::tree::Entry {
        mode: gix::objs::tree::EntryKind::Blob.into(),
        filename: name.into(),
        oid,
    }
}

/// Write a version-2 index by hand — `DIRC` header, one record per
/// path, SHA-1 trailer. The same bytes steps 1–4 wrote.
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

/// HEAD's tree as a map: path -> blob digest. The fixture never
/// nests, so one level is the whole story.
fn head_tree_files(
    repo: &gix::Repository,
) -> Result<BTreeMap<String, gix::ObjectId>, Box<dyn std::error::Error>> {
    let id = repo.head()?.into_peeled_id()?;
    let tree = repo.find_object(id)?.peel_to_tree()?;
    let mut files = BTreeMap::new();
    for entry in tree.iter() {
        let entry = entry?;
        files.insert(
            String::from_utf8_lossy(entry.filename()).into_owned(),
            entry.oid().to_owned(),
        );
    }
    Ok(files)
}
