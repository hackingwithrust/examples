use std::collections::HashMap;

/// An inverted index: for each term, the documents that contain it.
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

fn main() {
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

    println!("\npostings for \"borrow\": {:?}", index.search("borrow"));

    println!("\nquery \"borrow\":");
    for id in index.search("borrow") {
        println!("  {}", index.path(*id));
    }

    println!("\nthe same documents from a scan:");
    for (path, text) in &documents {
        if tokenize(text).any(|term| term == "borrow") {
            println!("  {}", path);
        }
    }

    println!("\nthe queries that used to miss:");
    for query in ["ownership", "valid"] {
        println!("  query {:?}:", query);
        for id in index.search(query) {
            println!("    {}", index.path(*id));
        }
    }

    println!("\ninflections fold onto one term:");
    for query in ["borrow", "borrowing"] {
        let terms: Vec<String> = tokenize(query).collect();
        println!("  query {query:?} -> terms {terms:?}");
        for term in &terms {
            println!("    {term:?}: {:?}", index.search(term));
        }
    }

    println!("\nstop words are not terms:");
    for query in ["the", "is"] {
        println!(
            "  query {:?}: {} documents",
            query,
            index.search(query).len()
        );
    }
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
}
