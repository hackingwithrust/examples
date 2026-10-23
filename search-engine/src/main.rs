use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

/// An inverted index: for each term, the documents that contain it.
#[derive(Debug)]
struct Index {
    /// term -> document ids. Every list is ascending and duplicate-free.
    postings: HashMap<String, Vec<u32>>,
    /// document id -> the path the document was indexed under.
    documents: Vec<String>,
}

impl Index {
    fn new() -> Self {
        Index {
            postings: HashMap::new(),
            documents: Vec::new(),
        }
    }

    /// Index one document. `path` is its id; `text` is its contents.
    /// Returns the id the document was given.
    fn add_document(&mut self, path: &str, text: &str) -> u32 {
        let id = self.documents.len() as u32;
        self.documents.push(path.to_string());

        // The terms this document contributes, each at most once.
        let mut terms: Vec<String> = tokenize(text).collect();
        terms.sort_unstable();
        terms.dedup();

        for term in terms {
            self.postings.entry(term).or_default().push(id);
        }

        id
    }

    /// The ids of the documents containing `term`, in insertion order.
    /// Empty when the index has never seen the term.
    ///
    /// The term has to be normalised the way the tokeniser normalises
    /// text: `search("borrowing")` finds nothing, because the dictionary
    /// holds "borrow". Normalising the query is the caller's job.
    fn search(&self, term: &str) -> &[u32] {
        self.postings.get(term).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The path a document was indexed under.
    fn path(&self, id: u32) -> &str {
        &self.documents[id as usize]
    }

    fn documents(&self) -> usize {
        self.documents.len()
    }

    fn terms(&self) -> usize {
        self.postings.len()
    }

    /// Every term with its postings, in sorted order — the order `save`
    /// writes them in.
    fn postings(&self) -> Vec<(&str, &[u32])> {
        let mut entries: Vec<(&str, &[u32])> = self
            .postings
            .iter()
            .map(|(term, ids)| (term.as_str(), ids.as_slice()))
            .collect();
        entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
        entries
    }

    /// Write the index to `path` as one binary file: the magic and the
    /// version, the document paths, then the dictionary with its postings
    /// lists, terms in sorted order.
    ///
    /// The file is built in memory and written in one call. At this size
    /// that is cheaper than a buffered writer, and it makes the whole
    /// file one value that can be compared, hashed or diffed.
    fn save(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&(self.documents.len() as u32).to_le_bytes());
        for document in &self.documents {
            put_str(&mut out, document);
        }
        out.extend_from_slice(&(self.terms() as u32).to_le_bytes());
        for (term, ids) in self.postings() {
            put_str(&mut out, term);
            out.extend_from_slice(&(ids.len() as u32).to_le_bytes());
            for id in ids {
                out.extend_from_slice(&id.to_le_bytes());
            }
        }
        fs::write(path, out)
    }

    /// Read an index back from `path`. Reading the file and interpreting
    /// it are two steps that fail differently: the first with an io
    /// error, the second with a `LoadError`.
    fn load(path: impl AsRef<Path>) -> Result<Index, LoadError> {
        let bytes = fs::read(path).map_err(LoadError::Io)?;
        Index::load_bytes(&bytes)
    }

    /// Parse the bytes of an index file. Every field is length-prefixed,
    /// so the reader never has to guess where one field ends and the
    /// next begins — and a file that ends early is a `LoadError`, not a
    /// panic.
    fn load_bytes(bytes: &[u8]) -> Result<Index, LoadError> {
        let mut reader = Reader::new(bytes);
        if reader.take(MAGIC.len())? != MAGIC {
            return Err(LoadError::BadMagic);
        }
        let version = reader.u32()?;
        if version != VERSION {
            return Err(LoadError::BadVersion(version));
        }

        let mut index = Index::new();
        for _ in 0..reader.u32()? {
            index.documents.push(reader.string()?);
        }
        for _ in 0..reader.u32()? {
            let term = reader.string()?;
            let mut ids = Vec::new();
            for _ in 0..reader.u32()? {
                ids.push(reader.u32()?);
            }
            index.postings.insert(term, ids);
        }
        if reader.pos != reader.data.len() {
            return Err(LoadError::TrailingBytes);
        }
        Ok(index)
    }
}

/// Append a length-prefixed UTF-8 string: a u32 byte count, then the
/// bytes. A binary file has no separator to scan for, so the length is
/// the separator.
fn put_str(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&(text.len() as u32).to_le_bytes());
    out.extend_from_slice(text.as_bytes());
}

/// Everything that can be wrong with an index file. The disk it sits on
/// is a separate concern, carried along as `Io`.
#[derive(Debug)]
enum LoadError {
    /// The file does not start with the magic this engine writes.
    BadMagic,
    /// The magic is right, but the version is not one this build reads.
    BadVersion(u32),
    /// The file ends where another field was expected.
    Truncated,
    /// A length-prefixed field holds bytes that are not UTF-8.
    BadUtf8,
    /// Bytes were left over after the last field the format describes.
    TrailingBytes,
    /// The file could not be read at all.
    Io(io::Error),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadError::BadMagic => write!(f, "not a haystack index: the magic bytes are wrong"),
            LoadError::BadVersion(v) => write!(f, "unsupported index version {v}"),
            LoadError::Truncated => write!(f, "the file ends where another field was expected"),
            LoadError::BadUtf8 => write!(f, "a length-prefixed field is not valid UTF-8"),
            LoadError::TrailingBytes => write!(f, "the file has bytes after the last field"),
            LoadError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl Error for LoadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            LoadError::Io(e) => Some(e),
            _ => None,
        }
    }
}

/// A cursor over an index file's bytes. Every read checks the end first,
/// so a truncated file is a `LoadError` instead of a panic.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Reader { data, pos: 0 }
    }

    /// The next `len` bytes, or `Truncated` when the file ends first.
    fn take(&mut self, len: usize) -> Result<&'a [u8], LoadError> {
        let end = self.pos.checked_add(len).ok_or(LoadError::Truncated)?;
        let bytes = self.data.get(self.pos..end).ok_or(LoadError::Truncated)?;
        self.pos = end;
        Ok(bytes)
    }

    /// The next four bytes, as a little-endian u32.
    fn u32(&mut self) -> Result<u32, LoadError> {
        let bytes: [u8; 4] = self.take(4)?.try_into().unwrap();
        Ok(u32::from_le_bytes(bytes))
    }

    /// The next length-prefixed UTF-8 string.
    fn string(&mut self) -> Result<String, LoadError> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| LoadError::BadUtf8)
    }
}

/// The first eight bytes of every index file: eight ASCII bytes that no
/// file this engine reads by accident starts with.
const MAGIC: &[u8] = b"haystack";

/// The format version. A file announcing any other number is refused.
const VERSION: u32 = 1;

/// Where the demo writes its index. The file is output, not source: it
/// is gitignored and rebuilt by every run.
const INDEX_PATH: &str = "haystack.idx";

fn main() -> Result<(), Box<dyn Error>> {
    let documents = [
        (
            "docs/ownership.md",
            "Ownership and borrowing are the two ideas Rust is built on. Every value has one owner, and the owner going out of scope drops it.",
        ),
        (
            "docs/borrow-checker.md",
            "The borrow checker enforces the borrowing rules at compile time. A borrow must not outlive the owner of the borrowed value.",
        ),
        (
            "docs/lifetimes.md",
            "A lifetime is the region of code for which a reference is valid. The borrow checker uses lifetimes to keep every reference in scope.",
        ),
        (
            "docs/iterators.md",
            "Iterators are lazy: each call to next advances the iterator by one. An adapter such as map borrows the iterator it wraps.",
        ),
        (
            "docs/error-handling.md",
            "Recoverable errors are values of type Result. The question mark operator returns an error to the caller early.",
        ),
    ];

    let mut index = Index::new();
    for (path, text) in &documents {
        index.add_document(path, text);
    }
    println!(
        "{} documents, {} distinct terms",
        index.documents(),
        index.terms()
    );

    index.save(INDEX_PATH)?;
    let written = fs::read(INDEX_PATH)?;
    println!("\nsaved to {INDEX_PATH}: {} bytes", written.len());

    // The dictionary is written in sorted order, so the same index
    // always produces the same bytes.
    index.save(INDEX_PATH)?;
    let rewritten = fs::read(INDEX_PATH)?;
    println!(
        "saved again: {} bytes, byte-identical: {}",
        rewritten.len(),
        rewritten == written
    );

    let loaded = Index::load(INDEX_PATH)?;
    println!(
        "\nloaded back: {} documents, {} terms",
        loaded.documents(),
        loaded.terms()
    );

    println!("\nquery \"borrow\" against the loaded index:");
    for id in loaded.search("borrow") {
        println!("  {}", loaded.path(*id));
    }

    println!("\ntwo files the loader refuses:");
    if let Err(e) = Index::load("no-such-file.idx") {
        println!("  a file that is not there: {e}");
    }
    let head = &written[..20];
    if let Err(e) = Index::load_bytes(head) {
        println!("  the first {} bytes of the real file: {e}", head.len());
    }

    Ok(())
}

/// The stop words: terms so common that indexing them costs memory and
/// buys nothing. This is the classic short English list; a production
/// engine keeps a longer one, and one per language.
const STOP_WORDS: &[&str] = &[
    "a", "am", "an", "and", "are", "as", "at", "be", "been", "being", "but", "by", "can", "could",
    "did", "do", "does", "for", "from", "had", "has", "have", "he", "her", "his", "how", "i", "if",
    "in", "into", "is", "it", "its", "may", "might", "must", "no", "nor", "not", "of", "on", "or",
    "our", "shall", "she", "should", "so", "such", "than", "that", "the", "their", "them", "then",
    "there", "these", "they", "this", "those", "through", "to", "us", "was", "we", "were", "what",
    "when", "where", "which", "while", "who", "whom", "will", "with", "would", "you", "your",
];

fn is_stop_word(word: &str) -> bool {
    STOP_WORDS.contains(&word)
}

/// Shortest stem worth keeping. Without the guard "ring" stems to "r".
const MIN_STEM: usize = 3;

/// Strip `suffix`, but only when a stem of at least `MIN_STEM`
/// characters is left.
fn strip_suffix(word: &str, suffix: &str) -> Option<String> {
    let base = word.strip_suffix(suffix)?;
    (base.chars().count() >= MIN_STEM).then_some(base.to_string())
}

/// "runn" -> "run", "dropp" -> "drop": a doubled final consonant is
/// usually the suffix's doing. l, s and z double naturally, so
/// "class", "fall" and "buzz" keep both letters.
fn undouble(stem: String) -> String {
    let mut chars: Vec<char> = stem.chars().collect();
    if chars.len() >= 2 {
        let (last, before) = (chars[chars.len() - 1], chars[chars.len() - 2]);
        if last == before && !matches!(last, 'l' | 's' | 'z') {
            chars.pop();
        }
    }
    chars.into_iter().collect()
}

/// "boxes" -> "box", "watches" -> "watch", "classes" -> "class":
/// after a sibilant that is not a bare "s", the plural's "es" detaches.
/// Everywhere else the "s" alone is the marker, so "causes" -> "cause"
/// and "buses" -> "buse", which is a known miss.
fn strip_sibilant_es(word: &str) -> Option<String> {
    let base = word.strip_suffix("es")?;
    let sibilant = base.ends_with('x')
        || base.ends_with('z')
        || base.ends_with("ss")
        || base.ends_with("ch")
        || base.ends_with("sh");
    if !sibilant || base.chars().count() < MIN_STEM {
        return None;
    }
    Some(base.to_string())
}

/// A small rule-based stemmer: strip one inflectional suffix, then fix
/// the doubling it left behind. "borrow", "borrows", "borrowed" and
/// "borrowing" all come out as "borrow".
///
/// It produces terms, not lemmas: "built" ends in no suffix it knows
/// and stays whole, and "ownership" does not stem at all. Porter's
/// algorithm is the full version of this idea; this is its small
/// cousin.
fn stem(word: &str) -> String {
    if let Some(base) = strip_suffix(word, "ing") {
        return undouble(base);
    }
    if let Some(base) = strip_suffix(word, "ed") {
        return undouble(base);
    }
    if let Some(base) = strip_sibilant_es(word) {
        return base;
    }
    if let Some(base) = strip_suffix(word, "s") {
        return base;
    }
    word.to_string()
}

/// Split text into terms: lowercase, split on anything that is not a
/// letter or a digit, drop stop words, and stem what is left.
///
/// The terms are owned. Step 2's version handed back slices of the text
/// it was given; lowercasing a word builds a new String, and so does
/// stemming one, so the output cannot borrow `text` any more.
fn tokenize(text: &str) -> impl Iterator<Item = String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .filter(|word| !is_stop_word(word))
        .map(|word| stem(&word))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Index {
        let mut index = Index::new();
        index.add_document("a.md", "The Quick Brown Fox");
        index.add_document("b.md", "the lazy dog, the quick cat");
        index
    }

    #[test]
    fn add_document_returns_the_id_it_gave() {
        let mut index = Index::new();
        assert_eq!(index.add_document("a.md", "fox"), 0);
        assert_eq!(index.add_document("b.md", "fox"), 1);
    }

    #[test]
    fn a_term_in_two_documents_lists_both() {
        let index = sample();
        assert_eq!(index.search("quick"), &[0, 1]);
    }

    #[test]
    fn a_term_repeated_in_one_document_lists_it_once() {
        let mut index = Index::new();
        index.add_document("a.md", "fox fox fox");
        assert_eq!(index.search("fox"), &[0]);
    }

    #[test]
    fn ids_are_in_insertion_order() {
        let index = sample();
        assert_eq!(index.path(0), "a.md");
        assert_eq!(index.path(1), "b.md");
        assert_eq!(index.search("dog"), &[1]);
    }

    #[test]
    fn a_missing_term_is_empty_not_an_error() {
        let index = sample();
        assert!(index.search("wolf").is_empty());
    }

    #[test]
    fn casing_and_punctuation_are_folded_away() {
        let index = sample();
        assert_eq!(index.search("fox"), &[0]);
        assert_eq!(index.search("dog"), &[1]);
    }

    #[test]
    fn stop_words_are_not_indexed() {
        let index = sample();
        assert!(index.search("the").is_empty());
        assert_eq!(index.search("quick"), &[0, 1]);
    }

    #[test]
    fn inflections_share_one_term() {
        let mut index = Index::new();
        index.add_document("a.md", "borrowing");
        index.add_document("b.md", "borrowed");
        index.add_document("c.md", "borrows");
        assert_eq!(index.search("borrow"), &[0, 1, 2]);
    }

    #[test]
    fn a_doubled_consonant_from_the_suffix_is_collapsed() {
        let mut index = Index::new();
        index.add_document("a.md", "running dropped");
        assert_eq!(index.search("run"), &[0]);
        assert_eq!(index.search("drop"), &[0]);
    }

    #[test]
    fn doubling_that_is_not_the_suffixs_fault_survives() {
        let mut index = Index::new();
        index.add_document("a.md", "classes");
        index.add_document("b.md", "boxes");
        index.add_document("c.md", "buzzes");
        assert_eq!(index.search("class"), &[0]);
        assert_eq!(index.search("box"), &[1]);
        assert_eq!(index.search("buzz"), &[2]);
    }

    #[test]
    fn a_stem_needs_three_characters() {
        let mut index = Index::new();
        index.add_document("a.md", "ring");
        assert_eq!(index.search("ring"), &[0]);
        assert!(index.search("r").is_empty());
    }

    #[test]
    fn case_folding_is_unicode_aware() {
        let mut index = Index::new();
        index.add_document("a.md", "Ärger");
        assert_eq!(index.search("ärger"), &[0]);
    }

    /// A scratch path in the OS temp directory. Each test picks its own
    /// name, so the tests can run in parallel.
    fn scratch(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("haystack-{name}.idx"))
    }

    #[test]
    fn a_saved_index_loads_back_identical() {
        let index = sample();
        let path = scratch("roundtrip");
        index.save(&path).unwrap();
        let loaded = Index::load(&path).unwrap();
        assert_eq!(loaded.documents(), index.documents());
        assert_eq!(loaded.terms(), index.terms());
        for (term, ids) in index.postings() {
            assert_eq!(loaded.search(term), ids, "postings for {term:?}");
        }
        for id in 0..index.documents() as u32 {
            assert_eq!(loaded.path(id), index.path(id));
        }
    }

    #[test]
    fn saving_twice_writes_the_same_bytes() {
        let index = sample();
        let path = scratch("determinism");
        index.save(&path).unwrap();
        let first = fs::read(&path).unwrap();
        index.save(&path).unwrap();
        assert_eq!(fs::read(&path).unwrap(), first);
    }

    #[test]
    fn the_dictionary_is_written_in_sorted_order() {
        let index = sample();
        let path = scratch("sorted");
        index.save(&path).unwrap();
        let bytes = fs::read(&path).unwrap();

        // Walk the saved file the way the loader does, collecting the
        // terms as they appear on disk.
        let mut reader = Reader::new(&bytes);
        reader.take(MAGIC.len()).unwrap();
        reader.u32().unwrap();
        for _ in 0..reader.u32().unwrap() {
            reader.string().unwrap();
        }
        let mut terms = Vec::new();
        for _ in 0..reader.u32().unwrap() {
            let term = reader.string().unwrap();
            let ids = reader.u32().unwrap();
            reader.take(ids as usize * 4).unwrap(); // the postings list
            terms.push(term);
        }
        assert!(
            terms.windows(2).all(|pair| pair[0] < pair[1]),
            "not sorted: {terms:?}"
        );
    }

    #[test]
    fn a_truncated_file_is_refused() {
        let index = sample();
        let path = scratch("truncated");
        index.save(&path).unwrap();
        let bytes = fs::read(&path).unwrap();
        let error = Index::load_bytes(&bytes[..20]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "the file ends where another field was expected"
        );
    }

    #[test]
    fn a_wrong_magic_is_refused() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"haystacs"); // one letter off
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        let error = Index::load_bytes(&bytes).unwrap_err();
        assert_eq!(
            error.to_string(),
            "not a haystack index: the magic bytes are wrong"
        );
    }

    #[test]
    fn a_newer_version_is_refused() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&2u32.to_le_bytes());
        let error = Index::load_bytes(&bytes).unwrap_err();
        assert_eq!(error.to_string(), "unsupported index version 2");
    }

    #[test]
    fn trailing_bytes_are_refused() {
        let index = sample();
        let path = scratch("trailing");
        index.save(&path).unwrap();
        let mut bytes = fs::read(&path).unwrap();
        bytes.push(0);
        let error = Index::load_bytes(&bytes).unwrap_err();
        assert_eq!(error.to_string(), "the file has bytes after the last field");
    }

    #[test]
    fn a_term_that_is_not_utf8_is_refused() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes()); // no documents
        bytes.extend_from_slice(&1u32.to_le_bytes()); // one term
        bytes.extend_from_slice(&1u32.to_le_bytes()); // one byte long
        bytes.push(0xFF); // and not UTF-8
        let error = Index::load_bytes(&bytes).unwrap_err();
        assert_eq!(
            error.to_string(),
            "a length-prefixed field is not valid UTF-8"
        );
    }

    #[test]
    fn a_missing_file_is_an_io_error() {
        let path = scratch("never-written");
        assert!(matches!(Index::load(&path), Err(LoadError::Io(_))));
    }

    #[test]
    fn an_empty_index_round_trips() {
        let index = Index::new();
        let path = scratch("empty");
        index.save(&path).unwrap();
        let loaded = Index::load(&path).unwrap();
        assert_eq!(loaded.documents(), 0);
        assert_eq!(loaded.terms(), 0);
    }
}
