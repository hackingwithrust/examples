//! packs: what the 533-byte stream of step 12 becomes when the
//! history is big. The server gets 100,000 commits written loose —
//! one file per object, the way every step so far stored things —
//! and a fetch carries all of them to the client in one pack. Then
//! the numbers: what opening the repository costs, what one object
//! lookup costs on each side, and what walking the whole graph costs.
//!
//! Run with `cargo run --example packs` from the workspace root.
//! Both fixtures are rebuilt from scratch on every run. The timings
//! printed are from one run on one machine; the ratios are the point.

use gix::actor::Signature;
use gix::hash::ObjectId;
use gix::progress::Discard;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

const SERVER: &str = "gitdesk-packs-demo-server";
const CLIENT: &str = "gitdesk-packs-demo-client";
const COMMITS: usize = 100_000;

// ---- small helpers ---------------------------------------------------

/// 100002 → "100,002".
fn separate(mut n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, b) in digits.bytes().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(b as char);
    }
    let _ = &mut n;
    out
}

/// Median of a list of nanosecond durations, in microseconds.
fn median_us(times: &mut Vec<u128>) -> f64 {
    times.sort_unstable();
    times[times.len() / 2] as f64 / 1000.0
}

fn reset_dir(path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    if p.exists() {
        std::fs::remove_dir_all(&p).expect("fixture dir is removable");
    }
    std::fs::create_dir_all(&p).expect("fixture dir is creatable");
    p
}

fn who(time: i64) -> Signature {
    Signature {
        name: "Demo".into(),
        email: "demo@example.com".into(),
        time: gix::date::Time::new(time, 0),
    }
}

fn blob_entry(name: &str, oid: ObjectId) -> gix::objs::tree::Entry {
    gix::objs::tree::Entry {
        mode: gix::objs::tree::EntryKind::Blob.into(),
        filename: name.into(),
        oid,
    }
}

// ---- the fixtures ----------------------------------------------------

/// The big repository: `COMMITS` commits on one chain, written loose.
/// Every commit shares one tree — the graph size is the point, not
/// the contents — so the whole history is one blob, one tree, and
/// 100,000 commit objects, each in its own file.
fn make_server() -> Result<ObjectId, Box<dyn std::error::Error>> {
    let dir = reset_dir(SERVER);
    let repo = gix::init_bare(&dir)?;
    std::fs::write(repo.git_dir().join("HEAD"), "ref: refs/heads/main\n")?;

    let blob = repo.write_blob(b"# demo\n\nthe same single file, forever\n")?;
    let tree = repo
        .write_object(&gix::objs::Tree {
            entries: vec![blob_entry("notes.md", blob.detach())],
        })?
        .detach();

    let mut prev: Option<ObjectId> = None;
    for i in 1..=COMMITS as i64 {
        let parents: Vec<ObjectId> = prev.into_iter().collect();
        let commit = repo.write_object(&gix::objs::Commit {
            tree: tree.into(),
            parents: parents.into(),
            author: who(1_700_000_000 + i),
            committer: who(1_700_000_000 + i),
            encoding: None,
            message: format!("commit {i}\n").into(),
            extra_headers: vec![],
        })?;
        prev = Some(commit.detach());
    }
    let tip = prev.expect("one hundred thousand commits were written");
    std::fs::write(repo.git_dir().join("refs/heads/main"), format!("{tip}\n"))?;
    Ok(tip)
}

/// The client: an empty repository. One fetch fills it — with a pack.
fn make_client() -> Result<(), Box<dyn std::error::Error>> {
    let dir = reset_dir(CLIENT);
    let repo = gix::init(&dir)?;
    std::fs::write(repo.git_dir().join("HEAD"), "ref: refs/heads/main\n")?;
    Ok(())
}

// ---- the fetch that makes the pack ------------------------------------

/// The same four calls the worker thread made in step 12, with the
/// progress thrown away: the numbers this demo wants come from the
/// disk afterwards, not from the wire.
fn fetch_history(client: &Path, server: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let repo = gix::open(client)?;
    let mut remote = repo.remote_at(server)?;
    remote.replace_refspecs(["+refs/heads/*:refs/heads/*"], gix::remote::Direction::Fetch)?;
    let connection = remote.connect(gix::remote::Direction::Fetch)?;
    let prepare = connection.prepare_fetch(
        Discard,
        gix::remote::ref_map::Options::default(),
    )?;
    let never = AtomicBool::new(false);
    let outcome = prepare.receive(Discard, &never)?;
    match outcome.status {
        gix::remote::fetch::Status::Change { .. } => Ok(()),
        _ => Err("nothing to fetch".into()),
    }
}

// ---- the disk, counted by hand ----------------------------------------

/// (object count, total bytes) across the loose fan-out directories.
fn loose_stats(repo: &gix::Repository) -> std::io::Result<(u64, u64)> {
    let objects = repo.git_dir().join("objects");
    let (mut count, mut bytes) = (0u64, 0u64);
    for fan in std::fs::read_dir(&objects)? {
        let fan = fan?;
        let name = fan.file_name().to_string_lossy().into_owned();
        if name.len() != 2 || !fan.path().is_dir() {
            continue; // "info", "pack" — not a fan-out directory
        }
        for file in std::fs::read_dir(fan.path())? {
            let file = file?;
            count += 1;
            bytes += file.metadata()?.len();
        }
    }
    Ok((count, bytes))
}

/// Every file in `objects/pack`, sorted by name.
fn pack_files(repo: &gix::Repository) -> std::io::Result<Vec<(String, u64)>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(repo.git_dir().join("objects/pack"))? {
        let e = e?;
        let meta = e.metadata()?;
        if meta.is_file() {
            out.push((e.file_name().to_string_lossy().into_owned(), meta.len()));
        }
    }
    out.sort();
    Ok(out)
}

// ---- the measurements --------------------------------------------------

/// Walk the first-parent chain from `head` to the root, sampling
/// every 100th id for the lookup benchmark. Returns the commit count.
fn walk_first_parent(
    repo: &gix::Repository,
    head: ObjectId,
    sample: &mut Vec<ObjectId>,
) -> Result<usize, Box<dyn std::error::Error>> {
    let (mut n, mut cur) = (0usize, head);
    loop {
        if n % 100 == 0 {
            sample.push(cur);
        }
        let obj = repo.find_object(cur)?;
        let commit = obj.to_commit_ref();
        match commit.parents().next() {
            Some(parent) => cur = parent,
            None => return Ok(n + 1),
        }
        n += 1;
    }
}

/// Median lookup time in microseconds over `ids`.
fn lookup_median(
    repo: &gix::Repository,
    ids: &[ObjectId],
) -> Result<f64, Box<dyn std::error::Error>> {
    let mut times = Vec::with_capacity(ids.len());
    for id in ids {
        let t = Instant::now();
        if repo.try_find_object(*id)?.is_none() {
            return Err("a sampled id was missing".into());
        }
        times.push(t.elapsed().as_nanos());
    }
    Ok(median_us(&mut times))
}

// ---- the demo ----------------------------------------------------------

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("fixtures: '{SERVER}' (bare, loose) and '{CLIENT}' (empty)");
    let server_tip = {
        let t = Instant::now();
        let tip = make_server()?;
        let server = gix::open(SERVER)?;
        let (count, bytes) = loose_stats(&server)?;
        println!("\nbuilding the loose history ({:.1} s):", t.elapsed().as_secs_f64());
        println!("  {} commits on one chain, one tree and one blob between them", separate(COMMITS as u64));
        println!(
            "  wrote {} objects as {} files: {} bytes",
            separate(count),
            separate(count),
            separate(bytes)
        );
        tip
    };

    let client_tip = {
        make_client()?;
        let t = Instant::now();
        fetch_history(Path::new(CLIENT), Path::new(SERVER))?;
        let client = gix::open(CLIENT)?;
        let files = pack_files(&client)?;
        let pack_bytes = files
            .iter()
            .filter(|(n, _)| n.ends_with(".pack"))
            .map(|(_, l)| l)
            .sum::<u64>();
        let idx_bytes = files
            .iter()
            .filter(|(n, _)| n.ends_with(".idx"))
            .map(|(_, l)| l)
            .sum::<u64>();
        println!(
            "\nfetch: the whole history crosses the file transport as one pack ({:.1} s)",
            t.elapsed().as_secs_f64()
        );
        println!(
            "  {} bytes of .pack + {} bytes of .idx in {} file(s)",
            separate(pack_bytes),
            separate(idx_bytes),
            files.len()
        );
        println!(
            "  {} loose bytes → {} packed, {} files instead of {}",
            separate(loose_stats(&gix::open(SERVER)?)?.1),
            separate(pack_bytes + idx_bytes),
            files.len(),
            separate(COMMITS as u64 + 2)
        );
        client.head_id()?.detach()
    };
    if client_tip != server_tip {
        return Err("fetch did not land the tip".into());
    }

    // launch: open repeatedly, resolve HEAD repeatedly, then one cold
    // lookup — the numbers a window pays on every start.
    let client = gix::open(CLIENT)?;
    let mut opens = Vec::new();
    for _ in 0..25 {
        let t = Instant::now();
        gix::open(CLIENT)?;
        opens.push(t.elapsed().as_nanos());
    }
    let mut heads = Vec::new();
    for _ in 0..25 {
        let t = Instant::now();
        client.head_id()?;
        heads.push(t.elapsed().as_nanos());
    }
    let cold = {
        let t = Instant::now();
        client.try_find_object(client_tip)?;
        t.elapsed()
    };

    // walk the whole graph on the packed client, then the loose server.
    let mut sample = Vec::new();
    let walk_packed = {
        let t = Instant::now();
        let n = walk_first_parent(&client, client_tip, &mut sample)?;
        (n, t.elapsed())
    };
    let mut sample2 = Vec::new();
    let walk_loose = {
        let server = gix::open(SERVER)?;
        let t = Instant::now();
        let n = walk_first_parent(&server, server_tip, &mut sample2)?;
        (n, t.elapsed())
    };
    if walk_packed.0 != walk_loose.0 {
        return Err("the two walks disagreed on the commit count".into());
    }
    let warm_loose = lookup_median(&gix::open(SERVER)?, &sample)?;
    let warm_packed = lookup_median(&client, &sample)?;

    println!("\nlaunch, as a window sees it (medians, packed client):");
    println!("  gix::open: {:.0} µs", median_us(&mut opens));
    println!("  resolve HEAD: {:.0} µs", median_us(&mut heads));
    println!("  first object lookup: {:.0} µs", cold.as_nanos() as f64 / 1000.0);
    println!(
        "  object lookup, median of {} ids: {:.2} µs",
        sample.len(),
        warm_packed
    );
    println!("  same lookups on the loose server: {:.2} µs", warm_loose);
    println!(
        "\nwalking HEAD → root, {} commits:",
        separate(walk_packed.0 as u64)
    );
    println!("  packed client: {} ms", walk_packed.1.as_millis());
    println!("  loose server: {} ms", walk_loose.1.as_millis());
    Ok(())
}
