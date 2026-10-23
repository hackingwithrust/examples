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
        let mut terms: Vec<&str> = tokenize(text).collect();
        terms.sort_unstable();
        terms.dedup();

        for term in terms {
            self.postings.entry(term.to_string()).or_default().push(id);
        }

        id
    }

    /// The ids of the documents containing `term`, in insertion order.
    /// Empty when the index has never seen the term.
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

/// Split text into terms. Whitespace only, for now: casing, stop words and
/// stemming arrive in step 3, and nothing outside this function changes then.
fn tokenize(text: &str) -> impl Iterator<Item = &str> {
    text.split_whitespace()
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

    println!("\nqueries that miss:");
    for query in ["ownership", "valid"] {
        let hits = index.search(query);
        if hits.is_empty() {
            println!("  {:?} -> no documents", query);
        } else {
            for id in hits {
                println!("  {:?} -> {}", query, index.path(*id));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Index {
        let mut index = Index::new();
        index.add_document("a.md", "the quick brown fox");
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
        assert_eq!(index.search("the"), &[0, 1]);
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
        assert_eq!(index.search("dog,"), &[1]);
    }

    #[test]
    fn a_missing_term_is_empty_not_an_error() {
        let index = sample();
        assert!(index.search("wolf").is_empty());
    }

    #[test]
    fn casing_and_punctuation_stay_part_of_the_term_for_now() {
        let index = sample();
        assert!(index.search("Fox").is_empty());
        assert_eq!(index.search("fox"), &[0]);
        assert!(index.search("dog").is_empty());
    }
}
