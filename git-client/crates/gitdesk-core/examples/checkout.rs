//! checkout: what switching branches really is — write the worktree
//! files named by the target commit's tree, rebuild the index from
//! the same tree, and move HEAD last. The fixture starts from the
//! state step 8 left behind — two branches and a merge, no worktree,
//! no index — and checks out twice: once onto a branch, where HEAD
//! becomes a symbolic ref, and once detached onto the seed commit,
//! where HEAD becomes a bare digest. git confirms both.
//!
//! Run with `cargo run --example checkout` from the workspace root.
//! The fixture lands in `gitdesk-checkout-demo/`; once HEAD has
//! moved, the program says so and exits 0 — delete the fixture
//! directory to see the run again.

use gix::bstr::ByteSlice;
use gix::hash::Kind;
use gix::objs::CommitRef;
use gix::ObjectId;
use std::collections::BTreeMap;
use std::path::Path;

// The three versions of plan.md the step 8 history passes through,
// verbatim — same bytes as log.rs, so the commit digests carry over.

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
    let dir = Path::new("gitdesk-checkout-demo");
    let repo = if dir.join(".git").try_exists()? {
        gix::open(dir)?
    } else {
        make_fixture(dir)?
    };
    let git_dir = repo.git_dir().to_path_buf();

    // HEAD is just a file, and reading it is the whole mystery. A
    // symbolic ref is one level of indirection: HEAD names a branch,
    // the branch file names a digest. Real resolution loops — a ref
    // may name another ref — but one hop is all HEAD ever needs.
    let raw = std::fs::read_to_string(git_dir.join("HEAD"))?;
    println!("HEAD, the raw file:");
    println!("  {}", raw.trim_end());
    let Some(main_ref) = raw.strip_prefix("ref: ") else {
        println!("\nHEAD is already detached — this run already happened;");
        println!("delete gitdesk-checkout-demo/ to see it again.");
        return Ok(());
    };
    let main_ref = main_ref.trim_end();
    let hex = std::fs::read_to_string(git_dir.join(main_ref))?;
    let main = ObjectId::from_hex(hex.trim().as_bytes())?;
    println!("resolving it by hand:");
    println!("  HEAD -> {main_ref} -> {main}");
    println!("  that is '{}'", subject_of(&repo, main)?);

    // Every branch is one file under refs/heads. Listing them is
    // reading a directory.
    let heads = git_dir.join("refs/heads");
    let mut names: Vec<String> = std::fs::read_dir(&heads)?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    println!("\nbranches — each is one file under refs/heads:");
    for name in &names {
        let hex = std::fs::read_to_string(heads.join(name))?;
        println!("  {name:<6} {}", hex.trim());
    }

    // Creating a branch is writing one more ref file — `git branch
    // exp` does exactly this, no more. This one points at main's
    // first parent, one commit back from the tip.
    let exp = first_parent(&repo, main)?.expect("merge has a first parent");
    std::fs::write(heads.join("exp"), format!("{exp}\n"))?;
    println!(
        "\ncreated exp at {exp} ('{}') — one ref file written;",
        subject_of(&repo, exp)?
    );
    println!("the worktree, the index and HEAD did not change.");

    // Checkout number one: onto a branch. HEAD gets a symbolic ref,
    // so future commits land on topic.
    let topic_hex = std::fs::read_to_string(heads.join("topic"))?;
    let topic = ObjectId::from_hex(topic_hex.trim().as_bytes())?;
    checkout(&repo, dir, topic, "ref: refs/heads/topic\n")?;

    // Checkout number two: detached. Walk topic's first-parent chain
    // to the root, then check it out with a bare digest in HEAD — no
    // branch name involved, nothing to move forward on a commit.
    let mut seed = topic;
    while let Some(p) = first_parent(&repo, seed)? {
        seed = p;
    }
    checkout(&repo, dir, seed, &format!("{seed}\n"))?;
    println!(
        "\nHEAD now holds a bare digest, not a ref name — that is a detached HEAD, and this git's status says so plainly: 'Not currently on any branch.'"
    );
    Ok(())
}

/// Materialize `target` in the worktree and index, then move HEAD to
/// `head_line` — a `ref: ...` line for a branch checkout, a bare
/// digest for a detached one. The order matters: both writes the
/// status columns compare (worktree against index, index against
/// HEAD's tree) must hold before anything reads HEAD.
fn checkout(
    repo: &gix::Repository,
    dir: &Path,
    target: ObjectId,
    head_line: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    println!(
        "\ncheckout {} '{}':",
        short(target),
        subject_of(repo, target)?
    );

    // The tree is the instruction sheet: path -> blob digest.
    let tree = repo.find_object(target)?.peel_to_tree()?;
    let mut wanted: BTreeMap<String, ObjectId> = BTreeMap::new();
    for entry in tree.iter() {
        let entry = entry?;
        wanted.insert(
            String::from_utf8_lossy(entry.filename()).into_owned(),
            entry.oid().to_owned(),
        );
    }

    // Worktree: remove what the target does not have, write what it
    // does. Both of this fixture's trees hold only plan.md, so the
    // removal loop has nothing to remove — the code is here because
    // a checkout that never deletes would leak files forever.
    for name in worktree_names(dir)? {
        if !wanted.contains_key(&name) {
            std::fs::remove_file(dir.join(&name))?;
            println!("  removed {name} from the worktree");
        }
    }
    for (name, id) in &wanted {
        let bytes = &repo.find_object(*id)?.data;
        std::fs::write(dir.join(name), bytes)?;
        println!("  wrote {name} ({} bytes)", bytes.len());
    }

    // Index: rebuilt from the same tree, so the column that compares
    // the index against HEAD's tree comes out empty.
    write_index(repo, &wanted)?;
    println!(
        "  index rebuilt: {} {}",
        wanted.len(),
        if wanted.len() == 1 { "entry" } else { "entries" }
    );

    // HEAD last. A symbolic ref keeps the branch name — the ref file
    // itself never changes on checkout; a digest pins the commit.
    std::fs::write(repo.git_dir().join("HEAD"), head_line)?;
    println!("  HEAD: {}", head_line.trim_end());
    Ok(())
}

/// First line of a commit's message.
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

/// First parent of a commit, or None at the root.
fn first_parent(
    repo: &gix::Repository,
    id: ObjectId,
) -> Result<Option<ObjectId>, Box<dyn std::error::Error>> {
    let obj = repo.find_object(id)?;
    let raw = CommitRef::from_bytes(&obj.data, repo.object_hash())?;
    Ok(raw
        .parents
        .first()
        .map(|p| ObjectId::from_hex(p.as_bytes()).expect("parent is a valid hex digest")))
}

/// Every regular file under `dir`, relative paths, `.git` skipped.
fn worktree_names(dir: &Path) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut names = Vec::new();
    walk(dir, dir, &mut names)?;
    names.sort();
    Ok(names)
}

fn walk(root: &Path, dir: &Path, names: &mut Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_name() == ".git" {
            continue;
        }
        if path.is_dir() {
            walk(root, &path, names)?;
        } else {
            names.push(path.strip_prefix(root)?.to_string_lossy().into_owned());
        }
    }
    Ok(())
}

/// Write a version-2 index by hand — `DIRC` header, one entry per
/// path, SHA-1 trailer. The same bytes step 2 wrote.
fn write_index(
    repo: &gix::Repository,
    files: &BTreeMap<String, ObjectId>,
) -> Result<(), Box<dyn std::error::Error>> {
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

fn short(id: ObjectId) -> String {
    let hex = id.to_string();
    hex[..7].to_string()
}

/// Build the history: exactly the state step 8 left behind — the two
/// step 7 commits, a branch off the second, two more commits on main,
/// the merge, and ref files for main and topic. No worktree files and
/// no index: a log never needed them, and checkout is what creates
/// them. Every clock is pinned, so the digests are stable.
fn make_fixture(dir: &Path) -> Result<gix::Repository, Box<dyn std::error::Error>> {
    let repo = gix::init(dir)?;

    let blob = |bytes: &[u8]| -> Result<ObjectId, Box<dyn std::error::Error>> {
        Ok(repo.write_blob(bytes)?.detach())
    };
    let b_seed = blob(PLAN_V1)?;
    let b_staged = blob(PLAN_STAGED)?;
    let b_topic = blob(&[PLAN_STAGED, b"sketch the log view\n"].concat())?;
    let b_v3 = blob(PLAN_V3)?;
    let b_main = blob(&[PLAN_V3, b"draw swimlanes in the log\n"].concat())?;
    let b_merged = blob(
        &[PLAN_V3, b"draw swimlanes in the log\n", b"sketch the log view\n"].concat(),
    )?;

    let tree = |blob: ObjectId| -> gix::objs::Tree {
        gix::objs::Tree {
            entries: vec![gix::objs::tree::Entry {
                mode: gix::objs::tree::EntryKind::Blob.into(),
                filename: "plan.md".into(),
                oid: blob,
            }],
        }
    };
    let t_seed = repo.write_object(&tree(b_seed))?.detach();
    let t_staged = repo.write_object(&tree(b_staged))?.detach();
    let t_topic = repo.write_object(&tree(b_topic))?.detach();
    let t_v3 = repo.write_object(&tree(b_v3))?.detach();
    let t_main = repo.write_object(&tree(b_main))?.detach();
    let t_merged = repo.write_object(&tree(b_merged))?.detach();

    let c1 = write_commit(&repo, t_seed, &[], 1_700_000_100, "seed commit\n")?;
    // Same tree, same clock, same message as step 7 — same digest.
    let c2 = write_commit(
        &repo,
        t_staged,
        &[c1],
        1_700_001_100,
        "stage the 'decide what to stage' line\n",
    )?;
    let c3 = write_commit(&repo, t_v3, &[c2], 1_700_001_200, "commit the remaining hunks\n")?;
    let c4 = write_commit(&repo, t_main, &[c3], 1_700_001_300, "note the swimlanes\n")?;
    let c_topic = write_commit(&repo, t_topic, &[c2], 1_700_001_900, "sketch the log view\n")?;
    let c_merge = write_commit(
        &repo,
        t_merged,
        &[c4, c_topic],
        1_700_001_400,
        "merge branch 'topic'\n",
    )?;

    let heads = repo.git_dir().join("refs/heads");
    std::fs::create_dir_all(&heads)?;
    std::fs::write(heads.join("main"), format!("{c_merge}\n"))?;
    std::fs::write(heads.join("topic"), format!("{c_topic}\n"))?;
    std::fs::write(repo.git_dir().join("HEAD"), "ref: refs/heads/main\n")?;
    Ok(repo)
}

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
        parents: parents.to_vec().into(),
        author: who.clone(),
        committer: who,
        encoding: None,
        message: message.into(),
        extra_headers: vec![],
    };
    Ok(repo.write_object(&commit)?.detach())
}
