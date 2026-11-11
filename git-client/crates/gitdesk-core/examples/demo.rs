//! demo: build a repository from nothing, then open it and read it back.
//!
//! Run with `cargo run --example demo` from the workspace root. The
//! fixture lands in `gitdesk-demo/`; when it is already there, the
//! program skips creation and just reads it.

use gix::hash::Kind;
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = Path::new("gitdesk-demo");
    if !dir.join(".git").try_exists()? {
        make_demo(dir)?;
    }
    open_and_read(dir)
}

/// Build a repository from nothing: three objects, two ref files, one
/// hand-written index. Nothing here calls `git`.
fn make_demo(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let repo = gix::init(dir)?;

    // A blob is stored under the SHA-1 of its own bytes plus a small
    // header — the same digest `git hash-object` computes.
    let blob = repo.write_blob(b"hello, gitdesk\n")?;

    // A tree is a sorted list of (mode, name, oid) entries.
    let tree = gix::objs::Tree {
        entries: vec![gix::objs::tree::Entry {
            mode: gix::objs::tree::EntryKind::Blob.into(),
            filename: "README.md".into(),
            oid: blob.detach(),
        }],
    };
    let tree = repo.write_object(&tree)?;

    // A commit names one tree, records who wrote it and when, and
    // carries the message. Pin the clock, or the digest changes.
    let who = gix::actor::Signature {
        name: "Demo".into(),
        email: "demo@example.com".into(),
        time: gix::date::Time::new(1_700_000_000, 0),
    };
    let commit = gix::objs::Commit {
        tree: tree.into(),
        parents: vec![].into(),
        author: who.clone(),
        committer: who,
        encoding: None,
        message: "first commit\n".into(),
        extra_headers: vec![],
    };
    let commit = repo.write_object(&commit)?.detach();

    // A branch is a one-line text file holding a digest; HEAD points at
    // the branch by name. Both are files, so both can be written by hand.
    let heads = repo.git_dir().join("refs/heads");
    std::fs::create_dir_all(&heads)?;
    std::fs::write(heads.join("main"), format!("{commit}\n"))?;
    std::fs::write(repo.git_dir().join("HEAD"), "ref: refs/heads/main\n")?;

    write_index(&repo, blob.detach())?;
    Ok(())
}

/// Write a version-2 index by hand — `DIRC` header, one 62-byte entry,
/// SHA-1 trailer — to show the format has nothing to hide.
fn write_index(
    repo: &gix::Repository,
    blob: gix::ObjectId,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"DIRC");
    buf.extend_from_slice(&2u32.to_be_bytes()); // version 2
    buf.extend_from_slice(&1u32.to_be_bytes()); // one entry follows

    let path = b"README.md";
    // Ten stat words, the oid, and a 2-byte flags word make 62 fixed
    // bytes; then the path, NUL-padded to fill a multiple of 8.
    let stat: [u32; 10] = [
        0, 0, // ctime seconds, nanoseconds
        1_700_000_000, 0, // mtime seconds, nanoseconds
        0, 0, // device, inode
        0o100644, // mode: regular file
        0, 0, // uid, gid
        15, // size in bytes
    ];
    for word in stat {
        buf.extend_from_slice(&word.to_be_bytes());
    }
    buf.extend_from_slice(blob.as_slice());
    buf.extend_from_slice(&(path.len() as u16).to_be_bytes()); // flags: name length, stage 0
    buf.extend_from_slice(path);
    // Pad the entry (everything after the 12-byte header) up to the next
    // multiple of 8, always leaving at least one NUL.
    let entry_len = buf.len() - 12;
    let target = (entry_len + 1).div_ceil(8) * 8;
    buf.resize(12 + target, 0);

    let mut hasher = gix::hash::hasher(Kind::Sha1);
    hasher.update(&buf);
    buf.extend_from_slice(hasher.try_finalize()?.as_slice());

    std::fs::write(repo.git_dir().join("index"), buf)?;
    Ok(())
}

/// Open the repository and walk its four stores.
fn open_and_read(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    println!("opened {}", dir.display());
    let repo = gix::open(dir)?;

    // HEAD is a file; its target is a name, not a digest.
    let head = repo.head()?;
    match head.referent_name() {
        Some(name) => println!("HEAD reads \"ref: {name}\""),
        None => println!("HEAD is detached"),
    }
    let id = head.into_peeled_id()?;
    println!("  -> {id}");

    let obj = repo.find_object(id)?;
    println!("{id} is a {:?}, {} bytes", obj.kind, obj.data.len());
    let tree = obj.peel_to_tree()?;
    for entry in tree.iter() {
        let entry = entry?;
        println!("tree: {:o} {}", entry.mode().value(), entry.filename());
    }

    for r in repo.references()?.local_branches()? {
        let r = r?;
        println!("ref {} -> {}", r.name(), r.id());
    }

    let index = repo.index()?;
    println!("index entries: {}", index.entries().len());
    for e in index.entries() {
        println!("  {} -> {}", e.path(&index), e.id);
    }
    Ok(())
}
