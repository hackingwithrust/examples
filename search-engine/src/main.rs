use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

/// One entry in a term's postings list: the document, and how many
/// times the term appears in it.
#[derive(Debug, PartialEq)]
struct Posting {
    /// The document this posting refers to.
    doc: u32,
    /// How many times the term occurs in that document. At least one:
    /// a posting exists because the term is there.
    tf: u32,
}

/// One document the index has seen: the path it was indexed under,
/// and how many terms the tokeniser took out of it.
#[derive(Debug)]
struct Document {
    path: String,
    /// The document's length in terms, stop words excluded.
    length: u32,
}

/// An inverted index: for each term, the documents that contain it.
#[derive(Debug)]
struct Index {
    /// term -> its postings. Each list is ascending in document id,
    /// with one entry per document that contains the term.
    postings: HashMap<String, Vec<Posting>>,
    /// document id -> the document itself.
    documents: Vec<Document>,
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
        // The terms this document contributes, sorted so that equal
        // terms sit next to each other and can be counted in runs.
        let mut terms: Vec<String> = tokenize(text).collect();
        terms.sort_unstable();

        let id = self.documents.len() as u32;
        self.documents.push(Document {
            path: path.to_string(),
            length: terms.len() as u32,
        });

        // Each distinct term becomes one posting, carrying the size of
        // its run as the term frequency.
        for (term, tf) in count_terms(&terms) {
            self.postings
                .entry(term.to_string())
                .or_default()
                .push(Posting { doc: id, tf });
        }

        id
    }

    /// The postings for `term`. Empty when the index has never seen
    /// the term.
    ///
    /// The term has to be normalised the way the tokeniser normalises
    /// text: `search("borrowing")` finds nothing, because the dictionary
    /// holds "borrow". Normalising the query is the caller's job.
    fn search(&self, term: &str) -> &[Posting] {
        self.postings.get(term).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The path a document was indexed under.
    fn path(&self, id: u32) -> &str {
        &self.documents[id as usize].path
    }

    /// How many terms a document holds, stop words excluded. They are
    /// not in the index, so they are not in the length either.
    fn length(&self, id: u32) -> u32 {
        self.documents[id as usize].length
    }

    /// The average document length in terms — the reference point BM25
    /// measures every document against. An empty index has no average
    /// and says 0.
    fn average_length(&self) -> f64 {
        if self.documents.is_empty() {
            return 0.0;
        }
        let total: u32 = self.documents.iter().map(|d| d.length).sum();
        total as f64 / self.documents.len() as f64
    }

    fn documents(&self) -> usize {
        self.documents.len()
    }

    fn terms(&self) -> usize {
        self.postings.len()
    }

    /// Every term with its postings, in sorted order — the order `save`
    /// writes them in.
    fn postings(&self) -> Vec<(&str, &[Posting])> {
        let mut entries: Vec<(&str, &[Posting])> = self
            .postings
            .iter()
            .map(|(term, postings)| (term.as_str(), postings.as_slice()))
            .collect();
        entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
        entries
    }

    /// Rank the documents containing `term` by BM25, best first. Each
    /// result is (document, term frequency, score).
    ///
    /// The score is the term's weight — the inverse document frequency
    /// from step 5 — times BM25's term-frequency component: a count that
    /// saturates as it grows, over a length measured against the
    /// collection average. A query with several terms sums one
    /// contribution per term; this step asks about one at a time.
    ///
    /// Equal scores keep the input order, and postings arrive in
    /// ascending document order, so ties come out in document order.
    fn rank(&self, term: &str) -> Vec<(u32, u32, f64)> {
        let postings = self.search(term);
        let weight = idf(postings.len(), self.documents());
        let avgdl = self.average_length();
        let mut ranked: Vec<(u32, u32, f64)> = postings
            .iter()
            .map(|p| {
                (
                    p.doc,
                    p.tf,
                    weight * tf_score(p.tf, self.length(p.doc) as f64, avgdl, K1, B),
                )
            })
            .collect();
        // f64 is not Ord, because of NaN. total_cmp gives every float a
        // place in a total order, and sort_by is stable, so equal scores
        // keep the order they arrived in.
        ranked.sort_by(|a, b| b.2.total_cmp(&a.2));
        ranked
    }

    /// Write the index to `path` as one binary file: the magic and the
    /// version, the documents with their lengths, then the dictionary
    /// with its postings lists, terms in sorted order.
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
            put_str(&mut out, &document.path);
            out.extend_from_slice(&document.length.to_le_bytes());
        }
        out.extend_from_slice(&(self.terms() as u32).to_le_bytes());
        for (term, postings) in self.postings() {
            put_str(&mut out, term);
            out.extend_from_slice(&(postings.len() as u32).to_le_bytes());
            for posting in postings {
                out.extend_from_slice(&posting.doc.to_le_bytes());
                out.extend_from_slice(&posting.tf.to_le_bytes());
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
            let path = reader.string()?;
            let length = reader.u32()?;
            index.documents.push(Document { path, length });
        }
        for _ in 0..reader.u32()? {
            let term = reader.string()?;
            let mut postings = Vec::new();
            let mut previous: Option<u32> = None;
            for _ in 0..reader.u32()? {
                let doc = reader.u32()?;
                let tf = reader.u32()?;
                // Step 2 promised ascending, duplicate-free lists, and
                // the ranker assumes it: a hand-edited file that breaks
                // the order is refused here rather than trusted.
                if previous.is_some_and(|last| doc <= last) {
                    return Err(LoadError::BadPostings(term));
                }
                previous = Some(doc);
                postings.push(Posting { doc, tf });
            }
            index.postings.insert(term, postings);
        }
        if reader.pos != reader.data.len() {
            return Err(LoadError::TrailingBytes);
        }
        Ok(index)
    }
}

/// Count a sorted term list in runs: each distinct term once, with the
/// number of times it occurred. ["dog", "fox", "fox", "fox"] counts
/// "dog" once and "fox" three times.
fn count_terms(sorted: &[String]) -> Vec<(&str, u32)> {
    let mut counts = Vec::new();
    let mut start = 0;
    while start < sorted.len() {
        let mut end = start + 1;
        while end < sorted.len() && sorted[end] == sorted[start] {
            end += 1;
        }
        counts.push((sorted[start].as_str(), (end - start) as u32));
        start = end;
    }
    counts
}

/// How much information a term carries: the natural log of the document
/// count over the number of documents containing the term.
///
/// A term in every document scores zero — it cannot tell two documents
/// apart. A term in one document scores the most. The zero for an empty
/// postings list is for callers that pass one: a term in no document is
/// not in the dictionary at all.
fn idf(df: usize, n: usize) -> f64 {
    if df == 0 {
        return 0.0;
    }
    (n as f64 / df as f64).ln()
}

/// BM25's two constants, at the values Lucene ships. They are the only
/// numbers in the formula that are not counted from the corpus.
///
/// k1 saturates term frequency: the higher it is, the longer extra
/// mentions keep mattering, with the ceiling at k1 + 1. b divides out
/// document length: 0 ignores length, 1 scales the whole k1 by the
/// document's length against the average.
const K1: f64 = 1.2;
const B: f64 = 0.75;

/// BM25's term-frequency component: how much a term's `tf` occurrences
/// in a document of `dl` terms are worth in a collection whose average
/// document is `avgdl` terms long.
///
/// The numerator grows with tf; so does the denominator, and faster —
/// which is what saturates the curve. As tf grows the score approaches
/// k1 + 1 and stops: at Lucene's k1 the ceiling is 2.2, so the twentieth
/// mention of a term adds very little to the tenth. k1 = 0 makes every
/// match worth the same, however often the term repeats.
///
/// b sets how much of the length difference is divided out. At b = 0
/// length is ignored and the bias step 5 demonstrated comes straight
/// back; at b = 1 the whole k1 scales with dl / avgdl, which punishes
/// long documents even when the repetition is genuine.
///
/// `avgdl` must be positive. A posting exists because its term is in
/// the document, so any document with a posting has length at least
/// one, and an index with postings averages above zero.
fn tf_score(tf: u32, dl: f64, avgdl: f64, k1: f64, b: f64) -> f64 {
    tf as f64 * (k1 + 1.0) / (tf as f64 + k1 * (1.0 - b + b * dl / avgdl))
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
    /// A postings list whose document ids are not ascending.
    BadPostings(String),
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
            LoadError::BadPostings(term) => {
                write!(f, "the postings list for {term:?} is not in ascending document order")
            }
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
/// Version 2 adds a length to every document and a term frequency to
/// every posting; a version 1 file has neither, and this build will
/// not guess them.
const VERSION: u32 = 2;

/// Where the demo writes its index. The file is output, not source: it
/// is gitignored and rebuilt by every run.
const INDEX_PATH: &str = "haystack.idx";

/// The demo corpus: one file is one document, and the path is the id.
///
/// The last entry is deliberately long and mentions "borrow" five times
/// in passing. It is the document length bias in one file: more terms
/// to say less.
fn corpus() -> [(&'static str, &'static str); 6] {
    [
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
        (
            "docs/style-guide.md",
            "House style for these pages: prefer short sentences, and name the thing you mean. When a function borrows a value, say what it borrows and for how long. A struct that borrows from its input should say so in its name. Tests borrow fixtures from the module above them, and an example that borrows code from another chapter says where it came from. Prefer the active voice, cut words like simply and just, and keep paragraphs under six lines. One idea per paragraph, a heading every screenful, and the exception to a rule written next to the rule.",
        ),
    ]
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut index = Index::new();
    for (path, text) in &corpus() {
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

    // What a posting carries now: the document, how often the term
    // appears in it, and how long the document is.
    println!("\npostings for \"borrow\" — term frequency, then document length:");
    for posting in loaded.search("borrow") {
        println!(
            "  {:<26} tf={}  len={}",
            loaded.path(posting.doc),
            posting.tf,
            loaded.length(posting.doc)
        );
    }

    println!(
        "\naverage document length: {:.2} terms",
        loaded.average_length()
    );

    println!("\nquery \"borrow\", ranked by BM25:");
    for (doc, tf, score) in loaded.rank("borrow") {
        println!(
            "  {score:>7.4}  {:<26} tf={tf}  len={}",
            loaded.path(doc),
            loaded.length(doc)
        );
    }

    println!("\nquery \"owner\", ranked by BM25:");
    for (doc, tf, score) in loaded.rank("owner") {
        println!(
            "  {score:>7.4}  {:<26} tf={tf}  len={}",
            loaded.path(doc),
            loaded.length(doc)
        );
    }

    // The version field, doing its job: the same file with the version
    // patched to 1 is a file this build does not read.
    let mut stale = written.clone();
    stale[8..12].copy_from_slice(&1u32.to_le_bytes());
    if let Err(e) = Index::load_bytes(&stale) {
        println!("\na version 1 file: {e}");
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

    /// The document ids in a term's postings, for the tests that only
    /// care which documents matched.
    fn ids(index: &Index, term: &str) -> Vec<u32> {
        index.search(term).iter().map(|p| p.doc).collect()
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
        assert_eq!(ids(&index, "quick"), [0, 1]);
    }

    #[test]
    fn a_term_repeated_in_one_document_lists_it_once() {
        let mut index = Index::new();
        index.add_document("a.md", "fox fox fox");
        assert_eq!(ids(&index, "fox"), [0]);
    }

    #[test]
    fn a_repeated_term_records_how_often_it_repeated() {
        let mut index = Index::new();
        index.add_document("a.md", "fox fox fox");
        assert_eq!(index.search("fox"), &[Posting { doc: 0, tf: 3 }]);
    }

    #[test]
    fn distinct_terms_in_one_document_get_one_posting_each() {
        let mut index = Index::new();
        index.add_document("a.md", "fox dog fox");
        assert_eq!(index.search("fox"), &[Posting { doc: 0, tf: 2 }]);
        assert_eq!(index.search("dog"), &[Posting { doc: 0, tf: 1 }]);
    }

    #[test]
    fn document_length_counts_the_terms_the_tokeniser_kept() {
        let mut index = Index::new();
        index.add_document("a.md", "The Quick Brown Fox");
        assert_eq!(index.length(0), 3); // "the" is a stop word
    }

    #[test]
    fn ids_are_in_insertion_order() {
        let index = sample();
        assert_eq!(index.path(0), "a.md");
        assert_eq!(index.path(1), "b.md");
        assert_eq!(ids(&index, "dog"), [1]);
    }

    #[test]
    fn a_missing_term_is_empty_not_an_error() {
        let index = sample();
        assert!(index.search("wolf").is_empty());
    }

    #[test]
    fn casing_and_punctuation_are_folded_away() {
        let index = sample();
        assert_eq!(ids(&index, "fox"), [0]);
        assert_eq!(ids(&index, "dog"), [1]);
    }

    #[test]
    fn stop_words_are_not_indexed() {
        let index = sample();
        assert!(index.search("the").is_empty());
        assert_eq!(ids(&index, "quick"), [0, 1]);
    }

    #[test]
    fn inflections_share_one_term() {
        let mut index = Index::new();
        index.add_document("a.md", "borrowing");
        index.add_document("b.md", "borrowed");
        index.add_document("c.md", "borrows");
        assert_eq!(ids(&index, "borrow"), [0, 1, 2]);
    }

    #[test]
    fn a_doubled_consonant_from_the_suffix_is_collapsed() {
        let mut index = Index::new();
        index.add_document("a.md", "running dropped");
        assert_eq!(ids(&index, "run"), [0]);
        assert_eq!(ids(&index, "drop"), [0]);
    }

    #[test]
    fn doubling_that_is_not_the_suffixs_fault_survives() {
        let mut index = Index::new();
        index.add_document("a.md", "classes");
        index.add_document("b.md", "boxes");
        index.add_document("c.md", "buzzes");
        assert_eq!(ids(&index, "class"), [0]);
        assert_eq!(ids(&index, "box"), [1]);
        assert_eq!(ids(&index, "buzz"), [2]);
    }

    #[test]
    fn a_stem_needs_three_characters() {
        let mut index = Index::new();
        index.add_document("a.md", "ring");
        assert_eq!(ids(&index, "ring"), [0]);
        assert!(index.search("r").is_empty());
    }

    #[test]
    fn case_folding_is_unicode_aware() {
        let mut index = Index::new();
        index.add_document("a.md", "Ärger");
        assert_eq!(ids(&index, "ärger"), [0]);
    }

    #[test]
    fn idf_is_zero_for_a_term_in_every_document() {
        assert_eq!(idf(2, 2), 0.0);
    }

    #[test]
    fn idf_falls_as_the_document_frequency_rises() {
        assert!(idf(1, 6) > idf(2, 6));
        assert!(idf(2, 6) > idf(5, 6));
        assert_eq!(idf(1, 6), 6.0f64.ln());
    }

    #[test]
    fn bm25_puts_the_highest_score_first() {
        let mut index = Index::new();
        index.add_document("a.md", "fox");
        index.add_document("b.md", "fox fox fox fox fox");
        index.add_document("c.md", "dog");
        let ranked = index.rank("fox");
        assert_eq!(ranked[0].0, 1);
        assert!(ranked[0].2 > ranked[1].2);
    }

    #[test]
    fn equal_documents_keep_document_order() {
        let mut index = Index::new();
        index.add_document("a.md", "fox");
        index.add_document("b.md", "fox");
        let ranked = index.rank("fox");
        // "fox" is in every document, so its weight is zero and the
        // scores are equal — and the stable sort keeps document order.
        assert_eq!(ranked.iter().map(|r| r.0).collect::<Vec<_>>(), [0, 1]);
        assert!(ranked.iter().all(|(_, _, score)| *score == 0.0));
    }

    /// The bias step 5 demonstrated, now fixed: on the demo corpus
    /// "borrow" ranks the short page about borrowing above the long
    /// page that mentions it in passing.
    #[test]
    fn bm25_lets_the_shorter_document_win() {
        let mut index = Index::new();
        for (path, text) in &corpus() {
            index.add_document(path, text);
        }
        let ranked = index.rank("borrow");
        assert_eq!(index.path(ranked[0].0), "docs/borrow-checker.md");
        assert_eq!(index.path(ranked[1].0), "docs/style-guide.md");
        // The gap is not a rounding error.
        assert!(ranked[0].2 > ranked[1].2 + 0.05);
    }

    /// Term frequency still does honest work when the more frequent
    /// document is also the more relevant one.
    #[test]
    fn a_frequent_term_in_a_relevant_document_still_wins() {
        let mut index = Index::new();
        for (path, text) in &corpus() {
            index.add_document(path, text);
        }
        let ranked = index.rank("owner");
        assert_eq!(index.path(ranked[0].0), "docs/ownership.md");
    }

    #[test]
    fn term_frequency_saturates() {
        let avg = 20.0;
        // Each doubling of the count adds less than the last...
        let once = tf_score(1, avg, avg, K1, B);
        let twice = tf_score(2, avg, avg, K1, B);
        let four = tf_score(4, avg, avg, K1, B);
        assert!(twice - once > four - twice);
        // ...and the ceiling is k1 + 1, never above it.
        let huge = tf_score(1_000_000, avg, avg, K1, B);
        assert!(huge < K1 + 1.0);
        assert!(huge > K1 + 0.99);
    }

    #[test]
    fn k1_of_zero_makes_frequency_irrelevant() {
        let avg = 20.0;
        assert_eq!(tf_score(1, avg, avg, 0.0, B), 1.0);
        assert_eq!(tf_score(50, avg, avg, 0.0, B), 1.0);
    }

    #[test]
    fn length_normalisation_off_leaves_length_alone() {
        // Same count, very different lengths, b = 0: equal scores.
        let short = tf_score(3, 10.0, 20.0, K1, 0.0);
        let long = tf_score(3, 40.0, 20.0, K1, 0.0);
        assert_eq!(short, long);
    }

    #[test]
    fn length_normalisation_on_favours_the_short_document() {
        let short = tf_score(3, 10.0, 20.0, K1, 1.0);
        let long = tf_score(3, 40.0, 20.0, K1, 1.0);
        assert!(short > long);
    }

    #[test]
    fn average_length_is_the_mean_of_the_document_lengths() {
        let index = sample();
        assert_eq!(index.average_length(), 3.5); // 3 and 4 terms
    }

    #[test]
    fn an_empty_index_has_no_average_length() {
        assert_eq!(Index::new().average_length(), 0.0);
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
        for (term, postings) in index.postings() {
            assert_eq!(loaded.search(term), postings, "postings for {term:?}");
        }
        for id in 0..index.documents() as u32 {
            assert_eq!(loaded.path(id), index.path(id));
            assert_eq!(loaded.length(id), index.length(id));
        }
    }

    #[test]
    fn a_terms_frequency_survives_a_reload() {
        let mut index = Index::new();
        index.add_document("a.md", "fox fox fox");
        index.add_document("b.md", "fox");
        let path = scratch("frequencies");
        index.save(&path).unwrap();
        let loaded = Index::load(&path).unwrap();
        assert_eq!(
            loaded.search("fox"),
            &[Posting { doc: 0, tf: 3 }, Posting { doc: 1, tf: 1 }]
        );
        assert_eq!(loaded.length(0), 3);
        assert_eq!(loaded.length(1), 1);
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
            reader.u32().unwrap(); // the document's length
        }
        let mut terms = Vec::new();
        for _ in 0..reader.u32().unwrap() {
            let term = reader.string().unwrap();
            let ids = reader.u32().unwrap();
            reader.take(ids as usize * 8).unwrap(); // doc and tf, four bytes each
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
        bytes.extend_from_slice(&3u32.to_le_bytes());
        let error = Index::load_bytes(&bytes).unwrap_err();
        assert_eq!(error.to_string(), "unsupported index version 3");
    }

    #[test]
    fn a_version_1_file_is_refused() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&1u32.to_le_bytes());
        let error = Index::load_bytes(&bytes).unwrap_err();
        assert_eq!(error.to_string(), "unsupported index version 1");
    }

    #[test]
    fn a_postings_list_that_is_not_ascending_is_refused() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes()); // no documents
        bytes.extend_from_slice(&1u32.to_le_bytes()); // one term
        bytes.extend_from_slice(&1u32.to_le_bytes()); // one byte long
        bytes.push(b'x');
        bytes.extend_from_slice(&2u32.to_le_bytes()); // two postings
        bytes.extend_from_slice(&2u32.to_le_bytes()); // document 2
        bytes.extend_from_slice(&1u32.to_le_bytes()); // term frequency 1
        bytes.extend_from_slice(&1u32.to_le_bytes()); // document 1 — out of order
        bytes.extend_from_slice(&1u32.to_le_bytes()); // term frequency 1
        let error = Index::load_bytes(&bytes).unwrap_err();
        assert_eq!(
            error.to_string(),
            "the postings list for \"x\" is not in ascending document order"
        );
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
