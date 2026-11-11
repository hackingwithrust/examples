//! fetch-push: the first bytes this client ever exchanges with a
//! remote. The fetch runs on a worker thread; the UI thread draws
//! progress from a channel and cancels through an Arc<AtomicBool> —
//! the exact flag gix's own receive() takes. Push to a path remote
//! is done by hand: walk the objects the remote is missing, write
//! them, move its ref. (gix 0.90 speaks the fetch side of the wire
//! protocol but not push; for a path remote a push *is* objects plus
//! a ref update, and the window story is the same worker thread.)
//!
//! Run with `cargo run --example fetch-push` from the workspace root.
//! Both fixtures are rebuilt from scratch on every run so the
//! transcript is the same each time: the server lands in
//! `gitdesk-fetch-demo-server/` (bare), the client in
//! `gitdesk-fetch-demo-client/`.

use gix::actor::Signature;
use gix::hash::ObjectId;
use gix::progress::{
    Count, Id, MessageLevel, NestedProgress, Progress, Step, StepShared, Unit, UNKNOWN,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

const SERVER: &str = "gitdesk-fetch-demo-server";
const CLIENT: &str = "gitdesk-fetch-demo-client";

// ---- what the worker tells the UI thread ---------------------------------

/// One event crossing from the worker to the UI thread. Progress
/// arrives as snapshots; the outcome arrives exactly once.
///
/// This enum is the entire interface between the two threads. Nothing
/// else is shared: no repository handle, no lock, no callback. The
/// worker can only say "here is a fact about the transfer", and the UI
/// can only read it after the send completes — which is what makes the
/// split safe to reason about without a memory model in your head.
enum Event {
    Started {
        name: String,
        max: Option<Step>,
    },
    Tick {
        name: String,
        step: Step,
        max: Option<Step>,
    },
    Note(String),
    Fetched(FetchReport),
    Cancelled,
    Failed(String),
}

/// The summary a finished fetch hands back: everything a status bar
/// would want, nothing it would not.
///
/// It travels by value inside `Event::Fetched`, so the worker builds it
/// last and then stops touching the repository — the UI reads counts
/// that are already frozen.
struct FetchReport {
    advertised: usize,
    rounds: usize,
    objects: u32,
    pack_bytes: u64,
    head_before: Option<ObjectId>,
    head_after: ObjectId,
}

// ---- the progress gix drives, the UI receives ----------------------------

/// Every call gix makes on its progress object turns into an event
/// on the channel. `Sender::send` takes `&self`, so the whole
/// implementation is cloneable and thread-safe with no locks.
///
/// Every method below runs on the worker thread, inside gix's own call
/// stack — this object is not "read by" the fetch, it *is* the fetch's
/// reporting channel. That is why the fields are atomics: gix may hold
/// several clones at once (a nested progress tree) and update them from
/// the same thread, and no `&mut` is ever available to us.
#[derive(Clone)]
struct ChannelProgress {
    tx: std::sync::mpsc::Sender<Event>,
    // Arc because the name is shared with the UI's clone of the event
    // stream, not because two threads mutate it.
    name: Arc<String>,
    // Relaxed throughout: these counters are decoration. The events are
    // the synchronisation; the numbers riding along only need to be
    // individually consistent, never ordered against each other.
    current: Arc<AtomicUsize>,
    max: Arc<AtomicUsize>,
}

impl ChannelProgress {
    fn new(tx: std::sync::mpsc::Sender<Event>) -> Self {
        Self {
            tx,
            name: Arc::new("worker".into()),
            current: Arc::new(AtomicUsize::new(0)),
            max: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// A child progress with its own counters but the same channel.
    /// gix's `NestedProgress` splits one transfer into phases
    /// ("negotiating", "receiving pack"); each phase gets one of these,
    /// and the UI can tell them apart by name.
    fn named(&self, name: &str) -> Self {
        Self {
            tx: self.tx.clone(),
            name: Arc::new(name.into()),
            current: Arc::new(AtomicUsize::new(0)),
            max: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// `None` means "gix has not told us a total", which is not the
    /// same as zero — a bar with an unknown length draws differently
    /// from a bar at 0%.
    fn max_of(&self) -> Option<Step> {
        match self.max.load(Ordering::Relaxed) {
            0 => None,
            m => Some(m),
        }
    }
}

impl Count for ChannelProgress {
    fn set(&self, step: Step) {
        self.current.store(step, Ordering::Relaxed);
        // Sending can fail only if the receiver is gone — the window
        // closed. Progress is not worth an error path, so the result
        // is dropped.
        let _ = self.tx.send(Event::Tick {
            name: (*self.name).clone(),
            step,
            max: self.max_of(),
        });
    }

    fn step(&self) -> Step {
        self.current.load(Ordering::Relaxed)
    }

    fn inc_by(&self, n: Step) {
        self.set(self.step().saturating_add(n));
    }

    /// gix offers a shared counter for callers that want to hand one
    /// out; a channel-based UI never reads it, so the default (an
    /// inert counter) is the right answer and the real state stays in
    /// `current`.
    fn counter(&self) -> StepShared {
        Default::default()
    }
}

impl Progress for ChannelProgress {
    /// Called once per phase, before any `set`. The max is recorded
    /// first so the very first Tick already carries it — otherwise a
    /// bar would briefly look infinite.
    fn init(&mut self, max: Option<Step>, _unit: Option<Unit>) {
        self.max.store(max.unwrap_or(0), Ordering::Relaxed);
        let _ = self.tx.send(Event::Started {
            name: (*self.name).clone(),
            max,
        });
    }

    fn set_name(&mut self, name: String) {
        self.name = Arc::new(name);
    }

    fn name(&self) -> Option<String> {
        Some((*self.name).clone())
    }

    fn id(&self) -> Id {
        UNKNOWN
    }

    fn message(&self, _level: MessageLevel, message: String) {
        let _ = self.tx.send(Event::Note(message));
    }
}

impl NestedProgress for ChannelProgress {
    type SubProgress = ChannelProgress;

    fn add_child(&mut self, name: impl Into<String>) -> Self::SubProgress {
        self.named(&name.into())
    }

    fn add_child_with_id(&mut self, name: impl Into<String>, _id: Id) -> Self::SubProgress {
        self.named(&name.into())
    }
}

// ---- the worker thread ---------------------------------------------------

/// The fetch itself, as the worker runs it: connect, negotiate,
/// receive — with the cancel flag handed straight to gix.
///
/// Runs entirely on the worker thread and opens its own repository
/// handle from the path it is given. That is deliberate: a repository
/// handle is stateful (an object cache, a loaded config), and nothing
/// of it needs to cross the thread boundary. Only paths go in, only a
/// `FetchReport` comes out.
fn run_fetch(
    client_path: &Path,
    server_path: &Path,
    progress: ChannelProgress,
    should_interrupt: &AtomicBool,
) -> Result<FetchReport, Box<dyn std::error::Error>> {
    let repo = gix::open(client_path)?;
    let mut remote = repo.remote_at(server_path)?;
    // `+` means "allow a non-fast-forward update": the fetch may move a
    // local branch backwards. Without it, a rewritten upstream would be
    // refused. The mapping is heads to heads — tags, notes and other
    // namespaces are simply not requested.
    remote.replace_refspecs(["+refs/heads/*:refs/heads/*"], gix::remote::Direction::Fetch)?;
    // HEAD before the fetch is what makes the transcript's
    // `main: (nothing) → ...` line possible; an empty repo has no
    // HEAD to resolve yet, hence the `ok()`.
    let head_before = repo.head_id().ok().map(|id| id.detach());
    // Connecting is where the ref advertisement arrives: the server
    // lists every ref and its tip, which is how the client learns what
    // it might be missing.
    let connection = remote.connect(gix::remote::Direction::Fetch)?;
    // Prepare splits the work in two: negotiation (have/want exchange,
    // one round per batch) and the pack transfer. Each gets its own
    // progress object so the two phases report separately.
    let prepare = connection.prepare_fetch(
        progress.named("negotiating"),
        gix::remote::ref_map::Options::default(),
    )?;
    // The cancel flag goes straight into gix: it polls the same
    // `&AtomicBool` while reading the pack, so a click can abort
    // mid-transfer rather than between files.
    let outcome = prepare.receive(progress.named("receiving pack"), should_interrupt)?;

    let (advertised, rounds, objects, pack_bytes) = match &outcome.status {
        // `Change` means the refs actually moved. Anything else would
        // mean an up-to-date client, which this fixture never is —
        // hence the error rather than a zeroed report.
        gix::remote::fetch::Status::Change {
            negotiate,
            write_pack_bundle,
            ..
        } => {
            // The bytes on disk, not a guess: a pack's size is only
            // known after it has been written. `data_path` is the
            // .pack file; its .idx sibling is what `index` below is.
            let pack_bytes = write_pack_bundle
                .data_path
                .as_ref()
                .and_then(|p| std::fs::metadata(p).ok())
                .map(|m| m.len())
                .unwrap_or(0);
            (
                // How many refs the server offered, how many negotiation
                // rounds it took to agree on a cut, how many objects came
                // in the pack, and how big the pack was.
                outcome.ref_map.remote_refs.len(),
                negotiate.rounds.len(),
                write_pack_bundle.index.num_objects,
                pack_bytes,
            )
        }
        _ => return Err("nothing to fetch — the fixture was not reset".into()),
    };

    // The pack has been indexed and its refs updated by now, so HEAD
    // resolves — and on this fixture main did not exist before.
    let head_after = repo.head_id()?.detach();
    Ok(FetchReport {
        advertised,
        rounds,
        objects,
        pack_bytes,
        head_before,
        head_after,
    })
}

// ---- push, by hand -------------------------------------------------------

/// Collect the objects the server is missing, walking back from
/// `want` and stopping wherever the server already has the object —
/// the same negotiation `git push` performs with have/want lines.
///
/// The stop condition is the whole idea: the moment the server owns an
/// object, so does it own everything that object can reach, because it
/// could only have got it by accepting its history. That is what keeps
/// a push proportional to the new commits instead of the whole repo.
fn collect_missing(
    client: &gix::Repository,
    server: &gix::Repository,
    want: ObjectId,
) -> Result<Vec<ObjectId>, Box<dyn std::error::Error>> {
    // An explicit stack, not recursion: history can be deeper than the
    // call stack allows.
    let mut todo = vec![want];
    let mut out = Vec::new();
    // Commits share trees and parents, so the same object is reachable
    // by many paths; without this set the walk would explode.
    let mut seen = BTreeSet::new();
    while let Some(id) = todo.pop() {
        // "Have": stop here, and do not enqueue anything below.
        if server.try_find_object(id)?.is_some() {
            continue;
        }
        if !seen.insert(id) {
            continue;
        }
        let obj = client.find_object(id)?;
        match obj.kind {
            gix::object::Kind::Commit => {
                // A commit is worth sending only once its tree and
                // parents can be resolved, so both are queued. The order
                // in `out` still ends up children-before-parents, which
                // is harmless for loose objects: a dangling object is
                // unreadable but not corrupt.
                let commit = obj.to_commit_ref();
                todo.push(commit.tree());
                todo.extend(commit.parents());
                out.push(id);
            }
            gix::object::Kind::Tree => {
                // Entries are blobs or subtrees; both are objects the
                // server needs. `decode` is what turns the raw entry
                // list into name/mode/oid triples.
                let tree = obj.try_into_tree()?;
                let tree = tree.decode()?;
                todo.extend(tree.entries.iter().map(|e| ObjectId::from(e.oid)));
                out.push(id);
            }
            gix::object::Kind::Blob => out.push(id),
            gix::object::Kind::Tag => return Err("tags are not pushed here".into()),
        }
    }
    Ok(out)
}

/// The push: write the missing objects into the remote's object
/// database, then move its ref. For a path remote this is the whole
/// transaction; over the network the same two moves happen inside a
/// packfile conversation (step 13).
///
/// Two steps, and their order is the safety property: objects first,
/// ref last. A crash between them leaves unreachable objects — wasted
/// space, not broken history. The reverse order would let the server
/// advertise a commit it cannot read.
fn run_push(
    client: &gix::Repository,
    server: &gix::Repository,
    want: ObjectId,
) -> Result<(Vec<ObjectId>, ObjectId, ObjectId), Box<dyn std::error::Error>> {
    let missing = collect_missing(client, server, want)?;
    for id in &missing {
        let obj = client.find_object(*id)?;
        // Each object is written loose — the same zlib-deflated,
        // header-prefixed file `git hash-object` would create. A real
        // push would assemble one pack instead, which is smaller and
        // atomic; loose is enough to prove the objects are complete.
        match obj.kind {
            gix::object::Kind::Blob => {
                server.write_blob(&obj.data)?;
            }
            gix::object::Kind::Commit => {
                server.write_object(gix::objs::ObjectRef::Commit(obj.to_commit_ref()))?;
            }
            gix::object::Kind::Tree => {
                server.write_object(gix::objs::ObjectRef::Tree(obj.try_into_tree()?.decode()?))?;
            }
            _ => return Err("tags are not pushed here".into()),
        }
    }
    // The ref update is the commit point of the whole operation: before
    // this line the new objects are invisible, after it they are the
    // branch's history.
    //
    // The old value is read only for the transcript. A real push sends
    // it along and the server refuses the update if it no longer
    // matches — the compare-and-swap that stops two people pushing at
    // once from silently clobbering each other.
    let ref_file = server.git_dir().join("refs/heads/main");
    let old = ObjectId::from_hex(std::fs::read(&ref_file)?.trim_ascii())?;
    std::fs::write(&ref_file, format!("{want}\n"))?;
    Ok((missing, old, want))
}

// ---- fixtures ------------------------------------------------------------

/// Both fixtures are thrown away and rebuilt on every run. That is not
/// laziness: a fetch is only interesting when the client is behind, and
/// a second run against the same directories would find nothing to do.
/// The transcript then stays identical run after run, which is what
/// lets the article quote it.
fn reset_dir(path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    if p.exists() {
        std::fs::remove_dir_all(&p).expect("fixture dir is removable");
    }
    std::fs::create_dir_all(&p).expect("fixture dir is creatable");
    p
}

/// The one identity every fixture commit uses. The time is pinned per
/// commit, so digests are reproducible — with a real clock they would
/// differ on every run.
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

/// The "remote": a bare repository with two commits on main, built
/// the way step 6 built commits — blobs, tree, commit, ref file.
///
/// Bare means no worktree: the repository is just .git's contents, so
/// nothing can be checked out and nothing can be edited in place. That
/// is all a server needs, and it is why `init_bare` writes no index.
///
/// Two commits, not one, so the fetch has to transfer a parent chain —
/// one commit would prove less about negotiation.
fn make_server() -> Result<(ObjectId, ObjectId), Box<dyn std::error::Error>> {
    let dir = reset_dir(SERVER);
    let repo = gix::init_bare(&dir)?;
    // HEAD exists in a bare repo only to say which branch is the
    // default; here it also makes the transcript's branch name stable.
    std::fs::write(repo.git_dir().join("HEAD"), "ref: refs/heads/main\n")?;

    let readme_v1 = repo.write_blob(b"# demo\n\nshared history\n")?;
    let tree_v1 = repo.write_object(&gix::objs::Tree {
        entries: vec![blob_entry("README.md", readme_v1.detach())],
    })?;
    let c1 = repo.write_object(&gix::objs::Commit {
        tree: tree_v1.into(),
        parents: vec![].into(),
        author: who(1_700_000_100),
        committer: who(1_700_000_100),
        encoding: None,
        message: "seed commit\n".into(),
        extra_headers: vec![],
    })?;

    let readme_v2 = repo.write_blob(b"# demo\n\nshared history\n\nfetched work\n")?;
    let notes = repo.write_blob(b"- from the server\n")?;
    let tree_v2 = repo.write_object(&gix::objs::Tree {
        entries: vec![
            blob_entry("README.md", readme_v2.detach()),
            blob_entry("notes.md", notes.detach()),
        ],
    })?;
    let c2 = repo.write_object(&gix::objs::Commit {
        tree: tree_v2.into(),
        parents: vec![c1.into()].into(),
        author: who(1_700_000_200),
        committer: who(1_700_000_200),
        encoding: None,
        message: "more work on the server\n".into(),
        extra_headers: vec![],
    })?;

    std::fs::write(repo.git_dir().join("refs/heads/main"), format!("{c2}\n"))?;
    Ok((c1.detach(), c2.detach()))
}

/// The client: an empty repository. Fetch fills it.
fn make_client() -> Result<gix::Repository, Box<dyn std::error::Error>> {
    let dir = reset_dir(CLIENT);
    let repo = gix::init(&dir)?;
    std::fs::write(repo.git_dir().join("HEAD"), "ref: refs/heads/main\n")?;
    Ok(repo)
}

// ---- the demo ------------------------------------------------------------

/// The UI half of one fetch: draw every event, hand back when the
/// worker is done. This is the loop an eframe app would run on each
/// repaint, fed by `try_recv` instead of a blocking `for`.
///
/// The loop ends on exactly one of three events — `Fetched`,
/// `Cancelled`, `Failed` — or when the channel disconnects. Those are
/// the only terminal states the worker can produce, which is why the
/// match below is exhaustive rather than defensive.
fn draw_events(rx: &Receiver<Event>) {
    // A real window redraws a bar in place; a transcript has no
    // such place, so the last snapshot per phase is what gets
    // printed, once, when the fetch lands. The `note` events carry
    // timings — speed lines differ between runs and mean nothing
    // on paper, so they are dropped here.
    //
    // So this is a small reducer: fold events into "the latest state
    // per phase", then render once. A GUI would render the same state
    // on every repaint instead of once at the end.
    let mut latest: Vec<(String, Step, Option<Step>)> = Vec::new();
    loop {
        // A timeout, not a blocking recv: a real UI must return to the
        // event loop to stay responsive even when nothing has arrived.
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Event::Started { name, max }) => match latest.iter_mut().find(|(n, ..)| n == &name) {
                Some(entry) => {
                    entry.1 = 0;
                    entry.2 = max;
                }
                None => latest.push((name, 0, max)),
            },
            Ok(Event::Tick { name, step, max }) => match latest.iter_mut().find(|(n, ..)| n == &name) {
                // Phases arrive interleaved only if gix nests them, so
                // the lookup is by name and the vector stays tiny — a
                // HashMap would be more machinery than the data needs.
                Some(entry) => {
                    entry.1 = step;
                    entry.2 = max;
                }
                None => latest.push((name, step, max)),
            },
            Ok(Event::Note(_)) => {}
            Ok(Event::Fetched(report)) => {
                for (name, step, max) in &latest {
                    if *step == 0 && max.is_none() {
                        continue; // a phase that never moved — no line
                    }
                    match max {
                        // With a total, print a fraction; without one,
                        // print the raw count. Two shapes because
                        // dividing by an unknown total is meaningless.
                        Some(m) => println!("  {name}: {step}/{m}"),
                        None => println!("  {name}: {step}"),
                    }
                }
                println!("  advertised refs: {}", report.advertised);
                println!("  negotiation rounds: {}", report.rounds);
                println!(
                    "  pack: {} objects, {} bytes",
                    report.objects, report.pack_bytes
                );
                match report.head_before {
                    Some(before) => println!("  main: {before} → {}", report.head_after),
                    // The client was empty: there was no HEAD to read
                    // before the fetch, only after it.
                    None => println!("  main: (nothing) → {}", report.head_after),
                }
                return;
            }
            Ok(Event::Cancelled) => {
                println!("  worker noticed the flag and stopped");
                return;
            }
            Ok(Event::Failed(e)) => {
                println!("  worker failed: {e}");
                return;
            }
            // Nothing yet. Looping here is the UI staying alive.
            Err(RecvTimeoutError::Timeout) => continue,
            // Every sender is gone without a terminal event: the worker
            // panicked. Nothing more will arrive, so stop waiting.
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// A staged slow transfer so cancellation is watchable: the ticks
/// stand in for a pack that takes seconds instead of microseconds.
/// The flag is the same one the real fetch above passed to gix.
///
/// The check happens between chunks, not inside one — which is exactly
/// the contract gix's `should_interrupt` has. Cancellation is
/// cooperative: the worker decides when to look, so the worst case is
/// "one more chunk", never a hung process.
fn slow_transfer(tx: std::sync::mpsc::Sender<Event>, interrupt: Arc<AtomicBool>) {
    for step in 1..=8 {
        // Load before doing the work, not after: a flag set during
        // chunk n stops chunk n+1, and the chunk already in flight is
        // still reported. Either way the loop exits through the same
        // `Cancelled` event.
        if interrupt.load(Ordering::Relaxed) {
            let _ = tx.send(Event::Cancelled);
            return;
        }
        let _ = tx.send(Event::Tick {
            name: "slow fetch".into(),
            step,
            max: Some(8),
        });
        std::thread::sleep(Duration::from_millis(80));
    }
    let _ = tx.send(Event::Note("all 8 chunks arrived".into()));
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ---- the fixture: a server two commits ahead of an empty client ----
    println!("fixtures: '{SERVER}' (bare, the remote) and '{CLIENT}' (empty)");
    make_server()?;
    let client = make_client()?;
    // Opened here, on the main thread, and kept for the push at the
    // end. The fetch does not use it — that one lives in the worker.
    let server = gix::open(SERVER)?;

    // ---- act 1: fetch on a worker thread --------------------------
    println!("\nfetch on a worker thread — the UI draws, it does not compute:");
    let (tx, rx) = channel();
    // The cancel flag is created here and shared by `Arc`: the UI holds
    // one handle to set it, gix gets a reference to the other. Both
    // point at the same byte, so "cancel" needs no message in either
    // direction — which is the point, since the worker is busy reading
    // a socket and cannot service a queue.
    let interrupt = Arc::new(AtomicBool::new(false));
    let handle = {
        // A block so the cloned sender and Arc can be moved into the
        // closure and then dropped, leaving `tx` itself owned by this
        // function. Without that, `tx` would stay alive and the UI loop
        // would never see the channel close.
        let tx = tx.clone();
        let interrupt = Arc::clone(&interrupt);
        let client_path = PathBuf::from(CLIENT);
        let server_path = PathBuf::from(SERVER);
        // `move` transfers ownership of the paths, the sender and the
        // flag. Nothing is borrowed across the boundary, which is what
        // keeps the spawned closure from needing a lifetime.
        std::thread::spawn(move || {
            let progress = ChannelProgress::new(tx.clone());
            // All progress from here on is emitted from inside gix's
            // call stack, on this thread, and read on the other one.
            match run_fetch(&client_path, &server_path, progress, &interrupt) {
                // Exactly one terminal event, always sent — including
                // on failure. The UI waits for one of these and would
                // hang on a silent error.
                Ok(report) => {
                    let _ = tx.send(Event::Fetched(report));
                }
                Err(e) => {
                    let _ = tx.send(Event::Failed(e.to_string()));
                }
            }
        })
    };
    draw_events(&rx);
    // The UI thread waited here, but only to keep the demo to one
    // screen. In an eframe app nothing blocks: the channel is
    // drained with try_recv on every repaint.
    handle.join().expect("worker did not panic");
    // Read through this thread's own handle. The repository object
    // opened before the fetch reflects the on-disk state, and the pack
    // is already indexed by the time the worker sent `Fetched`.
    println!("  client HEAD is now {}", client.head_id()?);

    // ---- act 2: cancellation, on a simulated slow remote ----------
    println!("\ncancellation — a slow remote, a user who clicks cancel:");
    let (tx2, rx2) = channel();
    let interrupt2 = Arc::new(AtomicBool::new(false));
    let slow = {
        let tx = tx2.clone();
        let interrupt = Arc::clone(&interrupt2);
        std::thread::spawn(move || slow_transfer(tx, interrupt))
    };
    // The UI drops its own handle to the channel: the loop below
    // ends when every sender is gone, which is exactly when the
    // worker is done and nothing else will ever send.
    //
    // This is what makes a plain `for event in rx2` safe: it is not an
    // infinite loop, it is a loop bounded by the lifetime of the
    // senders, and the worker's sender dies with the thread.
    drop(tx2);
    for event in rx2 {
        match event {
            Event::Tick { name, step, max } => {
                println!("  {name}: {step}/{}", max.unwrap_or(0));
                // The click: one `store`, read by the worker between
                // chunks. No ack travels back — the next event the UI
                // sees is the worker's own `Cancelled`.
                if step == 3 && !interrupt2.load(Ordering::Relaxed) {
                    println!("  user clicks cancel — flag goes up");
                    interrupt2.store(true, Ordering::Relaxed);
                }
            }
            Event::Note(text) => println!("  note: {text}"),
            Event::Cancelled => println!("  worker noticed the flag and stopped"),
            _ => println!("  (unexpected event)"),
        }
    }
    slow.join().expect("slow worker did not panic");

    // ---- act 3: push, the other direction -------------------------
    println!("\npush — the client contributes a commit of its own:");
    // A real commit on the client: new README, a new local file, and
    // the notes.md that arrived with the fetch — re-written because the
    // tree has to name all three. Only one blob and one commit are
    // actually new to the server, which is exactly what the push below
    // has to work out for itself.
    let readme_v3 = client.write_blob(b"# demo\n\nshared history\n\nfetched work\n\nmade here\n")?;
    let local = client.write_blob(b"- written on the client\n")?;
    let notes = client.write_blob(b"- from the server\n")?;
    let tree_v3 = client.write_object(&gix::objs::Tree {
        entries: vec![
            blob_entry("README.md", readme_v3.detach()),
            blob_entry("local.md", local.detach()),
            blob_entry("notes.md", notes.detach()),
        ],
    })?;
    let c3 = client.write_object(&gix::objs::Commit {
        tree: tree_v3.into(),
        parents: vec![client.head_id()?.detach()].into(),
        author: who(1_700_000_300),
        committer: who(1_700_000_300),
        encoding: None,
        message: "client work\n".into(),
        extra_headers: vec![],
    })?;
    let c3 = c3.detach();
    // Moving the local branch is the client's side of the transaction
    // and happens before the push — the push sends a commit that
    // already exists locally, never one it invents.
    std::fs::write(client.git_dir().join("refs/heads/main"), format!("{c3}\n"))?;
    println!("  client main is now {c3}; the server is one behind");

    // The returned triple is "what was sent", "what the remote's ref
    // pointed at", "what it points at now" — the three facts a push
    // transcript needs.
    let (missing, old, new) = run_push(&client, &server, c3)?;
    println!("  push: {} objects written to the remote", missing.len());
    for id in &missing {
        // Looked up on the client because the server's handle is the
        // one that was just written to; both now hold identical bytes.
        let kind = client.find_object(*id)?.kind;
        println!("    {kind} {id}");
    }
    println!("  remote main: {old} → {new}");
    Ok(())
}
