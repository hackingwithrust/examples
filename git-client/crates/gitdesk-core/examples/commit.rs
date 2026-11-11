//! commit: turn the index into a tree, and the tree into a commit
//! object. The fixture starts from the state step 6 left behind — the
//! index holds plan.md's staged blob, the worktree still holds the
//! full change — and writes it down: a tree, a commit, and the ref
//! move that makes it history. git itself confirms the result.
//!
//! Run with `cargo run --example commit` from the workspace root.
//! The fixture lands in `gitdesk-commit-demo/`; when it is already
//! there, the program skips creation and just commits.

use gix::hash::Kind;
use std::path::Path;

// HEAD's plan.md — the seed commit's blob.
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

// What the index holds after step 6 staged hunk 2: HEAD's file plus
// the one clicked line, inserted before "merge a branch". This version
// exists in neither HEAD nor the worktree — only in the index.
const PLAN_STAGED: &[u8] = b"# week plan\n\
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
decide what to stage\n\
merge a branch\n\
fetch and push\n\
\n\
later:\n\
cherry-pick a fix\n\
tag a release\n\
rebase a branch\n\
keep the history clean\n";

// The worktree keeps the full change: the two hunks that were never
// clicked are still only here.
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
    let dir = Path::new("gitdesk-commit-demo");
    if !dir.join(".git").try_exists()? {
        make_fixture(dir)?;
    }
    let repo = gix::open(dir)?;

    // Running twice would commit twice — which is what committing
    // does, but this demo's printed output belongs to one run. If the
    // commit is already in, say so and stop.
    let parent = repo.head()?.into_peeled_id()?.detach();
    let head = repo.find_object(parent)?;
    if gix::objs::CommitRef::from_bytes(&head.data, repo.object_hash())?.message != "seed commit\n" {
        println!("already committed — remove {dir:?} to run again");
        return Ok(());
    }
    println!("HEAD (before): {parent}");

    // The index is the commit's content. Whatever it holds becomes the
    // tree — nothing from the worktree joins in, which is why the two
    // unstaged hunks stay out.
    let index = repo.index()?;
    let mut entries = Vec::new();
    for entry in index.entries() {
        let path = String::from_utf8_lossy(entry.path_in(index.path_backing())).into_owned();
        println!(
            "index: {path} — blob {} mode {:o}",
            entry.id,
            entry.mode.bits()
        );
        entries.push(gix::objs::tree::Entry {
            mode: entry
                .mode
                .to_tree_entry_mode()
                .expect("blob, symlink or gitlink"),
            filename: path.into(),
            oid: entry.id,
        });
    }

    // Tree entries sort by path — as raw bytes, with directories
    // sorting as if they ended in a slash. One path here, so the index
    // order is already the tree order.
    let tree = repo
        .write_object(&gix::objs::Tree { entries })?
        .detach();
    println!("tree:  {tree}");

    // The commit: the tree, the parent that makes history a graph,
    // author and committer, the message. The clock is pinned so the
    // digest is stable between runs.
    let who = gix::actor::Signature {
        name: "Demo".into(),
        email: "demo@example.com".into(),
        time: gix::date::Time::new(1_700_001_100, 0),
    };
    let commit = repo
        .write_object(&gix::objs::Commit {
            tree,
            parents: vec![parent].into(),
            author: who.clone(),
            committer: who,
            encoding: None,
            message: "stage the 'decide what to stage' line\n".into(),
            extra_headers: vec![],
        })?
        .detach();
    println!("commit: {commit}");

    // A ref move is all it takes to make the commit history: main
    // points at the new commit, and HEAD still points at main.
    let heads = repo.git_dir().join("refs/heads");
    std::fs::write(heads.join("main"), format!("{commit}\n"))?;
    println!("HEAD (after):  {commit} — refs/heads/main moved, nothing else");
    Ok(())
}

/// Build the state step 6 left behind: a seed commit holding plan.md
/// v1, an index that stages hunk 2's result, and a worktree with the
/// full change.
fn make_fixture(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let repo = gix::init(dir)?;

    let plan_v1 = repo.write_blob(PLAN_V1)?;
    let plan_staged = repo.write_blob(PLAN_STAGED)?;

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

    // The index holds step 6's staged blob; the worktree holds the
    // full change.
    write_index(&repo, &[("plan.md", plan_staged.detach())])?;
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
/// path, SHA-1 trailer. The same bytes steps 1–6 wrote.
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
