//! status: compare HEAD, the index and the worktree, and print what
//! `git status --porcelain` would print.
//!
//! Run with `cargo run --example status` from the workspace root. The
//! fixture lands in `gitdesk-status-demo/`; when it is already there,
//! the program skips creation and just compares.

use gix::hash::Kind;
use std::collections::BTreeMap;
use std::path::Path;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = Path::new("gitdesk-status-demo");
    if !dir.join(".git").try_exists()? {
        make_fixture(dir)?;
    }
    let repo = gix::open(dir)?;

    let head = head_tree_files(&repo)?;
    let index = index_files(&repo)?;
    let worktree = worktree_files(dir)?;

    print_status(&head, &index, &worktree);
    Ok(())
}

/// Build a repository whose three views deliberately disagree: one
/// staged edit, one unstaged edit, one file staged but missing from
/// the worktree, one file git has never been told about.
fn make_fixture(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let repo = gix::init(dir)?;

    // Four blobs. HEAD's tree gets the v1 pair; the index stages a
    // newer README, keeps app.rs as committed, and adds todo.md.
    let readme_v1 = repo.write_blob(b"# demo\n\nplanned work\n")?;
    let app_v1 = repo.write_blob(b"fn main() {}\n")?;
    let readme_v2 = repo.write_blob(b"# demo\n\nplanned work\n\ndone so far\n")?;
    let todo_v1 = repo.write_blob(b"- ship it\n")?;

    let tree = gix::objs::Tree {
        entries: vec![
            blob_entry("README.md", readme_v1.detach()),
            blob_entry("app.rs", app_v1.detach()),
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

    write_index(
        &repo,
        &[
            ("README.md", readme_v2.detach()),
            ("app.rs", app_v1.detach()),
            ("todo.md", todo_v1.detach()),
        ],
    )?;

    // The worktree agrees with the index about README.md, disagrees
    // about app.rs, never had todo.md, and holds one extra file.
    std::fs::write(dir.join("README.md"), b"# demo\n\nplanned work\n\ndone so far\n")?;
    std::fs::write(dir.join("app.rs"), b"fn main() {\n    println!(\"running\");\n}\n")?;
    std::fs::write(dir.join("notes.txt"), b"coffee first\n")?;
    Ok(())
}

fn blob_entry(name: &str, oid: gix::ObjectId) -> gix::objs::tree::Entry {
    gix::objs::tree::Entry {
        mode: gix::objs::tree::EntryKind::Blob.into(),
        filename: name.into(),
        oid,
    }
}

/// Write a version-2 index by hand — `DIRC` header, one 62-byte-plus
/// entry per path, SHA-1 trailer. The same bytes step 2 wrote, in a
/// loop.
fn write_index(
    repo: &gix::Repository,
    entries: &[(&str, gix::ObjectId)],
) -> Result<(), Box<dyn std::error::Error>> {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"DIRC");
    buf.extend_from_slice(&2u32.to_be_bytes()); // version 2
    buf.extend_from_slice(&(entries.len() as u32).to_be_bytes());

    for (path, id) in entries {
        // Ten stat words. The demo pins mtime and zeros the rest;
        // real `git add` fills them in from the filesystem.
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

/// HEAD's tree as a flat map: path -> blob digest. The fixture never
/// nests, so one level is the whole story; a real tree nests, and the
/// walk just gets recursive.
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

/// The worktree as the same kind of map: every regular file, hashed
/// the way a blob would be. Skipping `.git` is the only rule.
fn worktree_files(
    dir: &Path,
) -> Result<BTreeMap<String, gix::ObjectId>, Box<dyn std::error::Error>> {
    let mut files = BTreeMap::new();
    walk(dir, dir, &mut files)?;
    Ok(files)
}

fn walk(
    root: &Path,
    dir: &Path,
    files: &mut BTreeMap<String, gix::ObjectId>,
) -> Result<(), Box<dyn std::error::Error>> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_name() == ".git" {
            continue;
        }
        if path.is_dir() {
            walk(root, &path, files)?;
        } else {
            let bytes = std::fs::read(&path)?;
            let rel = path.strip_prefix(root)?.to_string_lossy().into_owned();
            files.insert(rel, blob_oid(&bytes));
        }
    }
    Ok(())
}

/// The digest `git hash-object` would assign: a type header and NUL,
/// then the bytes, all through SHA-1.
fn blob_oid(bytes: &[u8]) -> gix::ObjectId {
    let mut hasher = gix::hash::hasher(Kind::Sha1);
    hasher.update(format!("blob {}\0", bytes.len()).as_bytes());
    hasher.update(bytes);
    hasher.try_finalize().expect("sha1 never fails")
}

/// The comparison itself. Porcelain's two letters: the first is what
/// HEAD -> index changed, the second what index -> worktree changed.
fn print_status(
    head: &BTreeMap<String, gix::ObjectId>,
    index: &BTreeMap<String, gix::ObjectId>,
    worktree: &BTreeMap<String, gix::ObjectId>,
) {
    let mut rows: BTreeMap<&String, (char, char)> = BTreeMap::new();

    // Staged column: what the index does that HEAD does not.
    for (path, id) in index {
        let x = match head.get(path) {
            None => 'A',
            Some(h) if h != id => 'M',
            Some(_) => ' ',
        };
        rows.entry(path).or_insert((' ', ' ')).0 = x;
    }
    for path in head.keys() {
        if !index.contains_key(path) {
            rows.entry(path).or_insert((' ', ' ')).0 = 'D';
        }
    }

    // Unstaged column: what the worktree does that the index does not.
    for (path, id) in index {
        let y = match worktree.get(path) {
            None => 'D',
            Some(w) if w != id => 'M',
            Some(_) => ' ',
        };
        rows.entry(path).or_insert((' ', ' ')).1 = y;
    }
    // Anything the index has never heard of is untracked. Git keeps
    // these out of the sorted tracked list and prints them last.
    let mut untracked: Vec<&String> = Vec::new();
    for path in worktree.keys() {
        if !index.contains_key(path) {
            untracked.push(path);
        }
    }

    for (path, (x, y)) in &rows {
        if *x != ' ' || *y != ' ' {
            println!("{x}{y} {path}");
        }
    }
    for path in untracked {
        println!("?? {path}");
    }
}
