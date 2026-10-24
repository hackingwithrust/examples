//! `haystack` on the command line: the demo, the HTTP server, and the
//! incremental `add`. The engine itself lives in the library — this step
//! split it out so a benchmark can import it, and the binary kept what
//! belongs to a binary: arguments, the demo, and the index file's path.

use haystack::{corpus, Index};
use std::error::Error;
use std::fs;
use std::sync::{Arc, RwLock};
use std::thread;

/// Where the demo writes its index. The file is output, not source: it
/// is gitignored and rebuilt by every run.
const INDEX_PATH: &str = "haystack.idx";

/// `haystack` runs the demo; `haystack serve` serves the API;
/// `haystack add <file>...` indexes files into the index on disk.
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    match std::env::args().nth(1).as_deref() {
        Some("serve") => haystack::serve(INDEX_PATH).await,
        Some("add") => {
            let files: Vec<String> = std::env::args().skip(2).collect();
            add(INDEX_PATH, &files)
        }
        Some(other) => {
            eprintln!("unknown command {other:?} - the commands are `serve` and `add`");
            Ok(())
        }
        None => demo(),
    }
}

/// Index files into the index on disk: read each file, add it under
/// its path, save. This is the incremental path the demo walks by
/// hand — load, add, save — with the file reading a real run needs.
///
/// A file that is already in the index is re-indexed in place: the
/// path is the document's id, so the same path is a new version of
/// the same document rather than a second copy.
fn add(index_path: &str, files: &[String]) -> Result<(), Box<dyn Error>> {
    if files.is_empty() {
        eprintln!("usage: haystack add <file>...");
        return Ok(());
    }
    let mut index = Index::load(index_path).map_err(|e| {
        format!("cannot load {index_path}: {e} - run `cargo run` first to create it")
    })?;
    for file in files {
        let text = fs::read_to_string(file)?;
        let id = index.add_document(file, &text);
        println!(
            "indexed {file} as document {id} — {} documents, {} terms",
            index.documents(),
            index.terms()
        );
    }
    index.save(index_path)?;
    println!("saved {index_path}");
    Ok(())
}

/// The demo: build the corpus index and save it, then do what this
/// step is about — add a document to the index on disk, replace one,
/// and read while a writer writes. `cargo run` runs it; `cargo run --
/// serve` starts the HTTP API; `cargo run -- add <file>...` indexes
/// new files into the saved index.
fn demo() -> Result<(), Box<dyn Error>> {
    let mut index = Index::new();
    for (path, text) in &corpus() {
        index.add_document(path, text);
    }
    println!(
        "built the corpus: {} documents, {} distinct terms",
        index.documents(),
        index.terms()
    );
    index.save(INDEX_PATH)?;
    let written = fs::read(INDEX_PATH)?;
    println!("saved to {INDEX_PATH}: {} bytes", written.len());

    // The state every later run starts from: the index is on disk,
    // and the texts that built it are not. A new document arrives.
    let mut index = Index::load(INDEX_PATH)?;
    println!(
        "\nloaded back: {} documents, {} terms",
        index.documents(),
        index.terms()
    );
    let id = index.add_document(
        "docs/testing.md",
        "A test that asserts one property at a time is easier to trust than a test that asserts ten. Unit tests need no network.",
    );
    println!("added docs/testing.md as document {id}");
    index.save(INDEX_PATH)?;
    let mut index = Index::load(INDEX_PATH)?;
    println!(
        "saved and reloaded: {} documents, {} terms",
        index.documents(),
        index.terms()
    );
    println!("query \"test\":");
    for (doc, score) in index.query("test")? {
        println!("  {score:>7.4}  {}", index.path(doc));
    }

    // The same path again. Not a second copy — a new version of the
    // same document, under the same id.
    let id = index.add_document(
        "docs/testing.md",
        "A test that asserts one property at a time is easier to trust. Integration tests exercise the whole path.",
    );
    println!("\nre-indexed docs/testing.md, still document {id}");
    index.save(INDEX_PATH)?;
    let index = Index::load(INDEX_PATH)?;
    println!(
        "saved and reloaded: {} documents, {} terms",
        index.documents(),
        index.terms()
    );
    println!(
        "  length now {}, \"integration\" at {:?}",
        index.length(id),
        index.positions("integration", id)
    );

    // One writer, several readers, one index behind a lock: every
    // reader sees the index whole — never documents without their
    // postings, never postings without their documents.
    let shared = Arc::new(RwLock::new(Index::load(INDEX_PATH)?));
    let base = shared.read().unwrap().documents();
    let mut readers = Vec::new();
    for _ in 0..3 {
        let shared = Arc::clone(&shared);
        readers.push(thread::spawn(move || {
            let mut queries = 0;
            for _ in 0..50 {
                let index = shared.read().unwrap();
                let documents = index.documents();
                // What a query matched is inside the collection, and
                // every document in it can be looked up.
                for (doc, _) in index.query("test OR borrow").unwrap() {
                    assert!((doc as usize) < documents);
                }
                // A document the writer has added is searchable in
                // the same snapshot that counts it.
                if documents > base {
                    assert!(!index.query("test").unwrap().is_empty());
                }
                queries += 1;
            }
            queries
        }));
    }
    for (path, text) in &late() {
        shared.write().unwrap().add_document(path, text);
    }
    let mut total = 0;
    let mut consistent = true;
    for reader in readers {
        match reader.join() {
            Ok(queries) => total += queries,
            Err(_) => consistent = false,
        }
    }
    println!(
        "\n{total} queries across 3 readers while 3 documents were added; consistent: {consistent}"
    );

    // The engine answers the questions it answered before, off the
    // index the writer left behind.
    let index = shared.read().unwrap();
    println!("\nquery \"borrow\", ranked by BM25:");
    for (doc, tf, score) in index.rank("borrow") {
        println!(
            "  {score:>7.4}  {:<26} tf={tf}  len={}",
            index.path(doc),
            index.length(doc)
        );
    }
    println!("\n\"value AND owner\":");
    for doc in index.boolean("value AND owner").unwrap() {
        println!("  {}", index.path(doc));
    }

    // What the writer added is on disk now as well: saving is just
    // another write of the same bytes, and the next process — a
    // query, the API, another `add` — picks up from there.
    index.save(INDEX_PATH)?;
    println!(
        "\nsaved: {} documents in {INDEX_PATH}",
        index.documents()
    );

    println!("\nadd a file: cargo run -- add docs/notes.md");
    println!("query the same index over HTTP: cargo run -- serve");
    println!("measure it: cargo bench");

    Ok(())
}

/// The documents that arrive after the index was built: what the
/// writer adds while the readers read.
fn late() -> [(&'static str, &'static str); 3] {
    [
        (
            "docs/traits.md",
            "A trait is a contract: every type that implements it answers the same questions. A trait object dispatches at runtime.",
        ),
        (
            "docs/closures.md",
            "A closure captures its environment by reference, by mutable reference, or by move. The compiler picks the least it can.",
        ),
        (
            "docs/concurrency.md",
            "Threads share nothing by default. A channel moves messages between them, and ownership travels with the message.",
        ),
    ]
}
