//! log: walk the commit graph the way a log view has to. The fixture
//! starts from the state step 7 left behind — two commits on main —
//! and grows it a branch and a merge, then walks the graph three ways:
//! the date order a naive client prints, the topological order that
//! keeps parents below their children, and the swimlanes a GUI draws.
//! git itself confirms every ordering.
//!
//! Run with `cargo run --example log` from the workspace root.
//! The fixture lands in `gitdesk-log-demo/`; when it is already there,
//! the program skips creation, so the output is the same every run.

use gix::bstr::ByteSlice;
use gix::objs::CommitRef;
use gix::ObjectId;
use std::collections::BTreeMap;
use std::path::Path;

// The three versions of plan.md step 7's history passes through,
// verbatim: the seed commit's blob, the staged version, the worktree
// version. What the branch and the merge add is concatenated on top
// of these in make_fixture — same bytes, whatever the shape.

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

// Everything a walk needs from one commit, parsed once. CommitRef
// is zero-copy: its `parents` are still raw hex, its `committer`
// is still a raw header line — `commit.committer()?` does the parse.
struct Node {
    parents: Vec<ObjectId>,
    time: i64,
    subject: String,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = Path::new("gitdesk-log-demo");
    if !dir.join(".git").try_exists()? {
        make_fixture(dir)?;
    }
    let repo = gix::open(dir)?;

    // A log view shows every branch, so the walk starts from every
    // ref under refs/heads — main first, then the rest by name.
    let heads = repo.git_dir().join("refs/heads");
    let mut names: Vec<String> = std::fs::read_dir(&heads)?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    if let Some(i) = names.iter().position(|n| n == "main") {
        let main = names.remove(i);
        names.insert(0, main);
    }
    let mut tips: Vec<(String, ObjectId)> = Vec::new();
    println!("tips — a log view walks from every branch:");
    for name in &names {
        let hex = std::fs::read_to_string(heads.join(name))?;
        let id = ObjectId::from_hex(hex.trim().as_bytes())?;
        println!("  {name:<6} {id}");
        tips.push((name.clone(), id));
    }

    // Load every commit the walk can reach: tips, then parents all
    // the way down to the root. Six objects here; a real log reads
    // them lazily instead of up front.
    let mut commits: BTreeMap<ObjectId, Node> = BTreeMap::new();
    let mut stack: Vec<ObjectId> = tips.iter().map(|(_, id)| *id).collect();
    while let Some(id) = stack.pop() {
        if commits.contains_key(&id) {
            continue;
        }
        let obj = repo.find_object(id)?;
        let raw = CommitRef::from_bytes(&obj.data, repo.object_hash())?;
        let node = Node {
            parents: raw
                .parents
                .iter()
                .map(|p| ObjectId::from_hex(p.as_bytes()).expect("parent is a valid hex digest"))
                .collect(),
            // SignatureRef keeps the raw time string for lossless
            // round-tripping; the seconds are its first token.
            time: raw
                .committer()?
                .time
                .split_whitespace()
                .next()
                .and_then(|s| s.parse::<i64>().ok())
                .expect("committer timestamp starts with unix seconds"),
            subject: String::from_utf8_lossy(raw.message)
                .lines()
                .next()
                .unwrap_or("")
                .to_string(),
        };
        stack.extend(node.parents.iter().copied());
        commits.insert(id, node);
    }

    // Date order: keep a frontier, always pop the newest committer
    // time. This is what `git log` prints by default, and it is what
    // a naive client draws — watch what it does with the branch tip.
    let mut frontier: Vec<ObjectId> = tips.iter().map(|(_, id)| *id).collect();
    let mut queued: BTreeMap<ObjectId, ()> = frontier.iter().map(|id| (*id, ())).collect();
    println!("\ndate order — always print the newest commit in the frontier:");
    while !frontier.is_empty() {
        frontier.sort_by(|a, b| commits[b].time.cmp(&commits[a].time).then(a.cmp(b)));
        let id = frontier.remove(0);
        let c = &commits[&id];
        println!("  {} {} {}", short(id), c.time, c.subject);
        for parent in &c.parents {
            if queued.insert(*parent, ()).is_none() {
                frontier.push(*parent);
            }
        }
    }

    // Topological order: count children first, then only print a
    // commit once every commit that has it as a parent is printed.
    // Children before parents — no exceptions, whatever the clocks say.
    println!("\ntopo order — no commit prints before all of its children:");
    for id in topo_order(&commits) {
        let c = &commits[&id];
        println!("  {} {} {}", short(id), c.time, c.subject);
    }

    // Swimlanes: one lane per first-parent chain. main's chain takes
    // lane 0; a branch tip starts a new lane, and the lane ends where
    // its chain walks into a commit that is already on another lane.
    let mut lanes: Vec<(String, Vec<ObjectId>)> = Vec::new();
    let mut lane_of: BTreeMap<ObjectId, usize> = BTreeMap::new();
    for (name, tip) in &tips {
        let mut chain = Vec::new();
        let mut cur = *tip;
        while !lane_of.contains_key(&cur) {
            chain.push(cur);
            match commits[&cur].parents.first() {
                Some(&p) => cur = p,
                None => break,
            }
        }
        if chain.is_empty() {
            continue;
        }
        let lane = lanes.len();
        for id in &chain {
            lane_of.insert(*id, lane);
        }
        lanes.push((name.clone(), chain));
    }
    println!("\nswimlanes — one lane per first-parent chain:");
    for (i, (name, chain)) in lanes.iter().enumerate() {
        let shorts: Vec<String> = chain.iter().map(|id| short(*id)).collect();
        println!("  lane {i} ({name}): {}", shorts.join(" "));
    }

    // The drawing. Merge rows carry a second column; a lane that
    // walks into an occupied lane closes with `|/`. This handles the
    // two-lane shape; git's renderer handles arbitrary graphs.
    println!("\nthe drawing:");
    for id in topo_order(&commits) {
        let c = &commits[&id];
        let lane = lane_of[&id];
        match c.parents.len() {
            0 => println!("* {} {}", short(id), c.subject),
            1 => {
                if lane == 0 {
                    println!("* {} {}", short(id), c.subject);
                } else {
                    println!("| * {} {}", short(id), c.subject);
                    if lane_of[&c.parents[0]] == 0 {
                        println!("|/");
                    }
                }
            }
            _ => {
                println!("*   {} {}", short(id), c.subject);
                println!("|\\");
            }
        }
    }
    Ok(())
}

/// Children-before-parents order, ties broken by committer time
/// (newest first). Count how many children each commit has, then
/// only queue a commit once the last of its children has printed.
fn topo_order(commits: &BTreeMap<ObjectId, Node>) -> Vec<ObjectId> {
    let mut children: BTreeMap<ObjectId, usize> = BTreeMap::new();
    for node in commits.values() {
        for parent in &node.parents {
            *children.entry(*parent).or_insert(0) += 1;
        }
    }
    let mut ready: Vec<ObjectId> = commits
        .keys()
        .copied()
        .filter(|id| !children.contains_key(id))
        .collect();
    let mut order = Vec::with_capacity(commits.len());
    while !ready.is_empty() {
        ready.sort_by(|a, b| commits[b].time.cmp(&commits[a].time).then(a.cmp(b)));
        let id = ready.remove(0);
        for parent in &commits[&id].parents {
            let n = children.get_mut(parent).expect("parent is in the walk");
            *n -= 1;
            if *n == 0 {
                ready.push(*parent);
            }
        }
        order.push(id);
    }
    order
}

fn short(id: ObjectId) -> String {
    let hex = id.to_string();
    hex[..7].to_string()
}

/// Build the history: step 7's two commits, a branch off the second
/// one, two more commits on main, then the merge. Every clock is
/// pinned, so the digests are stable between runs — and the branch
/// tip's clock is the newest in the whole repository, which is the
/// whole point of the demo.
fn make_fixture(dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let repo = gix::init(dir)?;

    let blob = |bytes: &[u8]| -> Result<ObjectId, Box<dyn std::error::Error>> {
        Ok(repo.write_blob(bytes)?.detach())
    };
    let b_seed = blob(PLAN_V1)?;
    let b_staged = blob(PLAN_STAGED)?;
    // The branch adds one line to step 7's staged plan; main adds two
    // to step 7's worktree plan while the branch is out; the merge
    // records both edits — one combined snapshot.
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
    Ok(())
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
