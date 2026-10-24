use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::thread;

/// One entry in a term's postings list: the document, and where each
/// occurrence of the term sits in it.
#[derive(Debug, PartialEq)]
struct Posting {
    /// The document this posting refers to.
    doc: u32,
    /// Where each occurrence sits in the document's term sequence,
    /// ascending. One position per occurrence, so the term frequency
    /// is the length of this list.
    positions: Vec<u32>,
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
    /// path -> document id. The reverse lookup `documents` cannot
    /// answer without a scan, and the one question incremental
    /// indexing asks over and over is "is this path already here?".
    /// It is derived state: rebuilt from `documents` on load, never
    /// written to the file.
    paths: HashMap<String, u32>,
}

impl Index {
    fn new() -> Self {
        Index {
            postings: HashMap::new(),
            documents: Vec::new(),
            paths: HashMap::new(),
        }
    }

    /// Index one document. `path` is its id; `text` is its contents.
    /// Returns the id the document has.
    ///
    /// A path the index has seen before is a new version of that
    /// document, not a second copy: the old postings are removed and
    /// the new text takes the same id. Everything derived — the
    /// document count, the lengths, the frequencies — moves with it,
    /// because it is all counted from what the index holds rather
    /// than accumulated alongside it.
    fn add_document(&mut self, path: &str, text: &str) -> u32 {
        // A known path is replaced: its old postings leave first, so
        // what comes back carries only the new text's terms.
        let existing = self.paths.get(path).copied();
        if let Some(id) = existing {
            self.remove_postings(id);
        }

        // The terms this document contributes, each tagged with its
        // position: the index of the term in the sequence the
        // tokeniser produced. Positions are attached before the sort,
        // because the sort is about the dictionary and the positions
        // are about the document.
        let mut terms: Vec<(u32, String)> = tokenize(text)
            .enumerate()
            .map(|(position, term)| (position as u32, term))
            .collect();
        // Sorted by term, then position, so one term's occurrences sit
        // next to each other in position order.
        terms.sort_unstable_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));

        // The id: the one this document already had, or the next free
        // one — which is the largest id in the index, so a new
        // document's postings append to lists that are already
        // ascending and stay that way.
        let id = match existing {
            Some(id) => {
                self.documents[id as usize] = Document {
                    path: path.to_string(),
                    length: terms.len() as u32,
                };
                id
            }
            None => {
                let id = self.documents.len() as u32;
                self.documents.push(Document {
                    path: path.to_string(),
                    length: terms.len() as u32,
                });
                id
            }
        };

        // Each distinct term becomes one posting, carrying its run's
        // positions — the count of which is the term frequency. The
        // insert keeps the list ascending, whether the document is
        // new (its id is the largest) or replaced (its old posting was
        // just removed, and the rest of the list is in order).
        for (term, run) in count_terms(&terms) {
            insert_posting(
                self.postings.entry(term.to_string()).or_default(),
                Posting {
                    doc: id,
                    positions: run,
                },
            );
        }

        self.paths.insert(path.to_string(), id);
        id
    }

    /// Remove every posting that belongs to document `id`, and the
    /// terms left with no postings at all.
    ///
    /// A term whose last posting goes is a term the index has stopped
    /// knowing, so it leaves the dictionary: `terms` counts what the
    /// index can answer questions about, not what it once could.
    fn remove_postings(&mut self, id: u32) {
        let mut emptied: Vec<String> = Vec::new();
        for (term, postings) in self.postings.iter_mut() {
            postings.retain(|p| p.doc != id);
            if postings.is_empty() {
                emptied.push(term.clone());
            }
        }
        for term in emptied {
            self.postings.remove(&term);
        }
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

    /// The posting for `term` in `doc`, when the document contains
    /// the term. The postings list is ascending in document id, so
    /// this is a binary search rather than a scan.
    fn posting(&self, term: &str, doc: u32) -> Option<&Posting> {
        let postings = self.search(term);
        let found = postings.binary_search_by_key(&doc, |p| p.doc);
        found.ok().map(|i| &postings[i])
    }

    /// Where each occurrence of `term` sits in document `doc`'s term
    /// sequence. Empty when the document does not contain the term.
    fn positions(&self, term: &str, doc: u32) -> &[u32] {
        match self.posting(term, doc) {
            Some(p) => &p.positions,
            None => &[],
        }
    }

    /// Every dictionary term within `max` edits of `term`, nearest
    /// first, each with its distance.
    ///
    /// This step's whole idea: the query term is not in the dictionary
    /// — a typo guarantees it — so the dictionary is searched instead
    /// of looked up. The exact term is its own neighbour, at distance
    /// zero.
    ///
    /// The order is deterministic. HashMap iteration is not, so the
    /// hits are sorted by distance and then by term.
    fn fuzzy(&self, term: &str, max: usize) -> Vec<(&str, usize)> {
        let mut hits: Vec<(&str, usize)> = self
            .postings
            .keys()
            .filter_map(|candidate| {
                let distance = edit_distance_within(term, candidate, max)?;
                Some((candidate.as_str(), distance))
            })
            .collect();
        hits.sort_unstable_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(b.0)));
        hits
    }

    /// The documents where `terms` appear one after another, in
    /// order: the phrase "borrow checker" matches "the borrow checker
    /// enforces" and not "the checker borrows".
    ///
    /// The terms are the normalised ones — what the index holds, not
    /// what the reader typed. An empty phrase matches nothing: there
    /// is no adjacency to find.
    fn phrase(&self, terms: &[&str]) -> Vec<u32> {
        // A document can only match if it holds every term, so the
        // candidates are the first term's postings: every document
        // the phrase matches is in that list.
        let Some(first) = terms.first() else {
            return Vec::new();
        };
        let mut matches = Vec::new();
        'postings: for posting in self.search(first) {
            // One occurrence of the first term has to be followed by
            // the second one position later, the third two later,
            // and so on.
            for &start in &posting.positions {
                let consecutive = terms[1..]
                    .iter()
                    .enumerate()
                    .all(|(offset, term)| {
                        self.positions(term, posting.doc)
                            .contains(&(start + 1 + offset as u32))
                    });
                if consecutive {
                    matches.push(posting.doc);
                    continue 'postings;
                }
            }
        }
        matches
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
    /// contribution per term; this method asks about one at a time.
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
                let tf = p.positions.len() as u32;
                (
                    p.doc,
                    tf,
                    weight * tf_score(tf, self.length(p.doc) as f64, avgdl, K1, B),
                )
            })
            .collect();
        // f64 is not Ord, because of NaN. total_cmp gives every float a
        // place in a total order, and sort_by is stable, so equal scores
        // keep the order they arrived in.
        ranked.sort_by(|a, b| b.2.total_cmp(&a.2));
        ranked
    }

    /// BM25 for every document in `docs`, summing one contribution per
    /// term in `terms`: a query about two terms scores a document by
    /// both, which is what makes a multi-term query rank.
    ///
    /// Best first. Equal scores keep the input order — and the input is
    /// ascending document ids, from the set operations below — so ties
    /// come out in document order, the same rule `rank` follows.
    fn score(&self, docs: &[u32], terms: &[&str]) -> Vec<(u32, f64)> {
        let avgdl = self.average_length();
        // One weight per term, worked out once: the idf depends on how
        // many documents hold the term, not on which document is being
        // scored.
        let weights: Vec<f64> = terms
            .iter()
            .map(|term| idf(self.search(term).len(), self.documents()))
            .collect();
        let mut scored: Vec<(u32, f64)> = docs
            .iter()
            .map(|&doc| {
                let total: f64 = terms
                    .iter()
                    .zip(&weights)
                    .map(|(term, weight)| {
                        let tf = self.positions(term, doc).len() as u32;
                        weight * tf_score(tf, self.length(doc) as f64, avgdl, K1, B)
                    })
                    .sum();
                (doc, total)
            })
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored
    }

    /// The documents one query token matches, and the terms it scores
    /// by.
    ///
    /// A quoted token is a phrase: its words go through the tokeniser,
    /// so stop words drop out and what is left has to be adjacent. A
    /// bare word is one word, normalised the same way — and a word
    /// that normalises to several terms ("borrow-checker") is the
    /// phrase they form, because that is what the hyphen meant in the
    /// text.
    fn operand(&self, token: &str) -> Result<(BTreeSet<u32>, Vec<String>), QueryError> {
        let text = token
            .strip_prefix('"')
            .map(|inside| inside.strip_suffix('"').unwrap_or(inside))
            .unwrap_or(token);
        // A quoted token is a phrase, and a phrase is exact: the tilde
        // applies to bare words only.
        if !token.starts_with('"') {
            if let Some((word, max)) = parse_fuzzy(text)? {
                return Ok(self.fuzzy_operand(word, max));
            }
        }
        let terms: Vec<String> = tokenize(text).collect();
        let refs: Vec<&str> = terms.iter().map(String::as_str).collect();
        let docs = match refs.as_slice() {
            [] => BTreeSet::new(),
            [only] => self.search(only).iter().map(|p| p.doc).collect(),
            phrase => self.phrase(phrase).into_iter().collect(),
        };
        Ok((docs, terms))
    }

    /// The documents one fuzzy word matches, and the terms they score
    /// by: every dictionary term within the radius, and the union of
    /// their postings.
    ///
    /// The word the reader typed is not in the dictionary — that is
    /// what makes it fuzzy — so it cannot score anything. Each variant
    /// contributes its own BM25 score, exactly as the terms of an OR
    /// do; a nearer variant is not boosted over a farther one, which
    /// is a simplification of Lucene's distance-blended rewrite.
    ///
    /// The word is normalised first, by the tokeniser the index used.
    /// A stop word matches nothing, exactly as it does without the
    /// tilde. A word that normalises to several terms —
    /// "borrow-checker", say — is a phrase this operator does not
    /// cover: it matches nothing rather than guessing which half the
    /// typo was in.
    fn fuzzy_operand(&self, word: &str, max: usize) -> (BTreeSet<u32>, Vec<String>) {
        let terms: Vec<String> = tokenize(word).collect();
        if terms.len() != 1 {
            return (BTreeSet::new(), Vec::new());
        }
        let hits = self.fuzzy(&terms[0], max);
        let docs: BTreeSet<u32> = hits
            .iter()
            .flat_map(|&(term, _)| self.search(term).iter().map(|p| p.doc))
            .collect();
        let scored_by: Vec<String> = hits.iter().map(|&(term, _)| term.to_string()).collect();
        (docs, scored_by)
    }

    /// Evaluate a boolean query: the documents that match, and the
    /// terms their scores come from.
    ///
    /// The grammar is terms, "quoted phrases", and the operators AND,
    /// OR and NOT, evaluated left to right with equal precedence —
    /// "a OR b AND c" is "(a OR b) AND c", which is not how Lucene
    /// reads it. NOT applies to the operand that follows it, and
    /// between two operands it reads as AND: "borrow NOT checker" is
    /// "borrow AND NOT checker". Two operands with no operator
    /// between them mean AND as well, so "borrow checker" and
    /// "borrow AND checker" ask the same question.
    fn evaluate(&self, query: &str) -> Result<Matches, QueryError> {
        let tokens = query_tokens(query);
        if tokens.is_empty() {
            return Err(QueryError::Empty);
        }

        let mut matched: Option<BTreeSet<u32>> = None;
        let mut terms: Vec<String> = Vec::new();
        // The operator waiting for its right-hand operand. AND is the
        // default: two operands in a row are ANDed.
        let mut op = Operator::And;
        let mut negate = false;
        // Whether the next token has to be an operand. True at the
        // start — a query opens with an operand, NOT may come first —
        // and true again after every operator.
        let mut want_operand = true;

        for token in tokens {
            if !token.starts_with('"') {
                match token.as_str() {
                    "AND" | "OR" => {
                        if want_operand {
                            // Two operators in a row, or a query that
                            // opens with one.
                            return Err(QueryError::DanglingOperator);
                        }
                        op = if token == "AND" {
                            Operator::And
                        } else {
                            Operator::Or
                        };
                        want_operand = true;
                        continue;
                    }
                    "NOT" => {
                        negate = !negate;
                        want_operand = true;
                        continue;
                    }
                    _ => {}
                }
            }

            let (docs, operand_terms) = self.operand(&token)?;
            let negated = negate;
            negate = false;
            let docs = if negated {
                // NOT is the complement inside the collection: every
                // document the index has, minus the operand's.
                let all: BTreeSet<u32> = (0..self.documents() as u32).collect();
                all.difference(&docs).copied().collect()
            } else {
                docs
            };
            matched = Some(match matched {
                None => docs,
                Some(so_far) => match op {
                    Operator::And => so_far.intersection(&docs).copied().collect(),
                    Operator::Or => so_far.union(&docs).copied().collect(),
                },
            });
            if !negated {
                terms.extend(operand_terms);
            }
            op = Operator::And;
            want_operand = false;
        }

        if want_operand {
            // The query ended on an operator, or on a NOT.
            return Err(QueryError::DanglingOperator);
        }
        Ok(Matches {
            docs: matched.into_iter().flatten().collect(),
            terms,
        })
    }

    /// The documents a boolean query matches, ascending.
    fn boolean(&self, query: &str) -> Result<Vec<u32>, QueryError> {
        Ok(self.evaluate(query)?.docs)
    }

    /// Run a query and rank what it matched by BM25 over the query's
    /// positive terms.
    ///
    /// A query with no positive terms — "NOT checker", say — still
    /// returns its documents, but every score is zero and the order is
    /// document order: there is nothing to rank on.
    fn query(&self, query: &str) -> Result<Vec<(u32, f64)>, QueryError> {
        let matches = self.evaluate(query)?;
        let refs: Vec<&str> = matches.terms.iter().map(String::as_str).collect();
        Ok(self.score(&matches.docs, &refs))
    }

    /// The bytes of the index file: the magic and the version, the
    /// documents with their lengths, then the dictionary with its
    /// postings lists, terms in sorted order.
    ///
    /// The file is built in memory in one piece. At this size that is
    /// cheaper than a buffered writer, and it makes the whole file one
    /// value that can be compared, hashed or diffed.
    fn save_bytes(&self) -> Vec<u8> {
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
                out.extend_from_slice(&(posting.positions.len() as u32).to_le_bytes());
                for position in &posting.positions {
                    out.extend_from_slice(&position.to_le_bytes());
                }
            }
        }
        out
    }

    /// Write the index to `path` as one binary file.
    ///
    /// The bytes are written beside the target and renamed into
    /// place: a rename inside one directory is atomic, so a crash
    /// mid-write leaves the old file whole instead of half of the new
    /// one. That matters more once saving is incremental — every
    /// added document saves the whole index again.
    fn save(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let path = path.as_ref();
        let temporary = path.with_extension("tmp");
        fs::write(&temporary, self.save_bytes())?;
        fs::rename(&temporary, path)
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
            // The reverse lookup is rebuilt here rather than read from
            // the file: it is derived from the document list, and a
            // file that carried it could disagree with the list.
            let id = index.documents.len() as u32;
            index.paths.insert(path.clone(), id);
            index.documents.push(Document { path, length });
        }
        for _ in 0..reader.u32()? {
            let term = reader.string()?;
            let mut postings = Vec::new();
            let mut previous: Option<u32> = None;
            for _ in 0..reader.u32()? {
                let doc = reader.u32()?;
                let count = reader.u32()? as usize;
                let mut positions = Vec::with_capacity(count);
                for _ in 0..count {
                    positions.push(reader.u32()?);
                }
                // Step 2 promised ascending, duplicate-free lists, and
                // the ranker assumes it: a hand-edited file that breaks
                // the order is refused here rather than trusted.
                if previous.is_some_and(|last| doc <= last) {
                    return Err(LoadError::BadPostings(term));
                }
                previous = Some(doc);
                postings.push(Posting { doc, positions });
            }
            index.postings.insert(term, postings);
        }
        if reader.pos != reader.data.len() {
            return Err(LoadError::TrailingBytes);
        }
        Ok(index)
    }
}

/// One hit in a search response: the document, and what BM25 made of
/// the query for it.
#[derive(Debug, Serialize)]
struct Hit {
    /// The path the document was indexed under.
    path: String,
    /// The document's BM25 score for this query.
    score: f64,
}

/// The body of a successful search: the query as it was asked, how many
/// documents matched, and the matches best first.
#[derive(Debug, Serialize)]
struct SearchResponse {
    /// The query, echoed back the way the reader asked it.
    query: String,
    /// How many documents matched — the length of `results`.
    total: usize,
    /// The matches, best first.
    results: Vec<Hit>,
}

/// The body of a refused search. One field, because there is one thing
/// to say.
#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

/// What can be wrong with a search request. Both variants answer 400,
/// but they say different things: a request without `q` never reached
/// the grammar, and a `q` the grammar refuses did.
enum ApiError {
    /// No `q` parameter at all.
    MissingQuery,
    /// A `q` the grammar refuses: empty, an operator missing its
    /// operand, a radius past the cap.
    BadQuery(QueryError),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let message = match self {
            ApiError::MissingQuery => "the query parameter q is required".to_string(),
            ApiError::BadQuery(e) => e.to_string(),
        };
        // The status and the body travel together: a JSON API answers a
        // bad request in JSON, not in axum's plain text.
        (StatusCode::BAD_REQUEST, Json(ErrorBody { error: message })).into_response()
    }
}

/// The query string of a search request: `/search?q=borrow`.
///
/// `q` is optional so that a request without one gets this engine's
/// error, in this engine's JSON — axum's own rejection of a missing
/// field is also a 400, but plain text, and it names the field in a
/// shape the rest of the API does not use.
#[derive(Debug, serde::Deserialize)]
struct SearchParams {
    q: Option<String>,
}

/// `GET /search?q=...`: run the query and rank what it matched.
///
/// The two extractors are the whole request: `State` is the index the
/// router was given, `Query` is the query string. The return type is
/// the contract — `Json<SearchResponse>` on success, `ApiError` on a
/// request this engine refuses — and both implement `IntoResponse`,
/// which is what lets a handler return a `Result` at all.
async fn search(
    State(index): State<Arc<Index>>,
    Query(params): Query<SearchParams>,
) -> Result<Json<SearchResponse>, ApiError> {
    let query = params.q.ok_or(ApiError::MissingQuery)?;
    let ranked = index.query(&query).map_err(ApiError::BadQuery)?;
    let results: Vec<Hit> = ranked
        .iter()
        .map(|&(doc, score)| Hit {
            path: index.path(doc).to_string(),
            score,
        })
        .collect();
    Ok(Json(SearchResponse {
        query,
        total: results.len(),
        results,
    }))
}

/// Load the index from `path` and serve the API until the process is
/// killed.
///
/// The index is shared, not copied: every request reads the same one,
/// and `Arc` is what makes that legal across axum's worker tasks. The
/// listener binds localhost — this is a demo over a local corpus; a
/// service other machines reach binds 0.0.0.0.
async fn serve(path: &str) -> Result<(), Box<dyn Error>> {
    let index = Index::load(path)?;
    println!(
        "loaded {path}: {} documents, {} terms",
        index.documents(),
        index.terms()
    );
    let app = Router::new()
        .route("/search", get(search))
        .with_state(Arc::new(index));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8000").await?;
    println!("serving GET /search on http://127.0.0.1:8000");
    axum::serve(listener, app).await?;
    Ok(())
}

/// How the next operand combines with the documents matched so far.
/// Two operands in a row mean AND, so this is also the default.
#[derive(Clone, Copy, PartialEq)]
enum Operator {
    And,
    Or,
}

/// What can be wrong with a query. A query is typed by a person, so it
/// can be malformed in ways an index file cannot.
#[derive(Debug)]
enum QueryError {
    /// Nothing to ask: the query holds no operands at all.
    Empty,
    /// An operator with nothing on one side of it: the query opens
    /// with AND or OR, ends with an operator or a NOT, or has two
    /// operators in a row.
    DanglingOperator,
    /// A tilde with no radius after it that this engine has:
    /// "borow~3" asks for more edits than the cap allows, and
    /// "borow~x" is not a radius at all.
    BadFuzzy(String),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueryError::Empty => write!(f, "the query is empty"),
            QueryError::DanglingOperator => {
                write!(f, "an operator in this query is missing its operand")
            }
            QueryError::BadFuzzy(token) => {
                write!(f, "the fuzzy operator on {token:?} needs an edit count between 0 and {MAX_EDITS}")
            }
        }
    }
}

impl Error for QueryError {}

/// What a query matched: the documents, and the terms their scores are
/// built from. A NOT'ed operand's terms are left out — they selected
/// documents to remove, not documents to rank.
struct Matches {
    docs: Vec<u32>,
    terms: Vec<String>,
}

/// Split a query into tokens: bare words, "quoted phrases", and the
/// operators AND, OR and NOT. The operators are capitals only, so a
/// document about the word "and" stays searchable. Whitespace between
/// tokens is dropped; whitespace inside quotes is kept, and the quotes
/// stay on the token so the parser can tell `"borrow checker"` — one
/// phrase — from `borrow checker` — two terms with AND between them.
fn query_tokens(query: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut in_quotes = false;
    for c in query.chars() {
        if c == '"' {
            in_quotes = !in_quotes;
            token.push(c);
            if !in_quotes {
                // A closing quote ends the token even mid-word.
                tokens.push(std::mem::take(&mut token));
            }
        } else if c.is_whitespace() && !in_quotes {
            if !token.is_empty() {
                tokens.push(std::mem::take(&mut token));
            }
        } else {
            token.push(c);
        }
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    tokens
}

/// The largest radius the `~` operator accepts. Lucene caps its fuzzy
/// edits at two for the same reason: past two edits a radius stops
/// correcting typos and starts matching half the dictionary.
const MAX_EDITS: usize = 2;

/// The radius a bare `~` means when no number follows it. Two, which
/// is Lucene's default too.
const DEFAULT_MAX_EDITS: usize = 2;

/// Split a fuzzy token: "borow~2" is the word "borow" with a radius
/// of 2, and "borow~" is the same word with the default radius.
/// `Ok(None)` means the token carries no tilde at all, and the
/// caller's ordinary exact path applies.
///
/// A tilde followed by anything that is not a radius this engine has
/// is an error, the way a dangling operator is: the query said
/// something the grammar does not cover, and guessing is worse than
/// refusing.
fn parse_fuzzy(token: &str) -> Result<Option<(&str, usize)>, QueryError> {
    let Some((word, rest)) = token.split_once('~') else {
        return Ok(None);
    };
    let max = if rest.is_empty() {
        DEFAULT_MAX_EDITS
    } else {
        match rest.parse::<usize>() {
            Ok(max) if max <= MAX_EDITS => max,
            _ => return Err(QueryError::BadFuzzy(token.to_string())),
        }
    };
    Ok(Some((word, max)))
}

/// The Levenshtein distance between `a` and `b`: the fewest
/// single-character insertions, deletions and substitutions that turn
/// one into the other. Counted over characters, not bytes, because
/// the tokeniser is Unicode-aware and a distance that is not would be
/// its own kind of wrong.
///
/// Transpositions are not edits here, so "teh" is two edits from
/// "the" — two substitutions. Lucene's default counts a transposition
/// as one, and that gap is a known difference.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    // Two rows of the table. Row i holds the distance between the
    // first i characters of `a` and every prefix of `b`, and row i+1
    // is computed from row i alone, so m * n entries are filled with
    // n + 1 of them alive at a time.
    //
    // The first row is the empty prefix of `a`: turning it into the
    // first j characters of `b` takes j insertions.
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        // The first column is the empty prefix of `b`: turning the
        // first i + 1 characters of `a` into nothing takes i + 1
        // deletions.
        current[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            current[j + 1] = (previous[j + 1] + 1) // a deletion
                .min(current[j] + 1) // an insertion
                .min(previous[j] + cost); // a match, or a substitution
        }
        std::mem::swap(&mut previous, &mut current);
    }
    // The last swap left the final row in `previous`.
    previous[b.len()]
}

/// The distance between `a` and `b` when it is at most `max`, and
/// `None` when it is not.
///
/// The length check comes first because it is O(1) against the DP's
/// O(m * n): the distance can never be smaller than the difference in
/// lengths, so a candidate that differs in length by more than the
/// radius is out without a table being filled.
fn edit_distance_within(a: &str, b: &str, max: usize) -> Option<usize> {
    if a.chars().count().abs_diff(b.chars().count()) > max {
        return None;
    }
    let distance = edit_distance(a, b);
    (distance <= max).then_some(distance)
}

/// Count a sorted term list in runs: each distinct term once, with the
/// positions of its occurrences. [("fox", 0), ("dog", 1), ("fox", 2)]
/// counts "fox" at positions 0 and 2, and "dog" at 1.
fn count_terms(sorted: &[(u32, String)]) -> Vec<(&str, Vec<u32>)> {
    let mut counts = Vec::new();
    let mut start = 0;
    while start < sorted.len() {
        let mut end = start + 1;
        while end < sorted.len() && sorted[end].1 == sorted[start].1 {
            end += 1;
        }
        // The run is in position order — the sort put it there — so the
        // positions come out ascending.
        let positions: Vec<u32> = sorted[start..end].iter().map(|(pos, _)| *pos).collect();
        counts.push((sorted[start].1.as_str(), positions));
        start = end;
    }
    counts
}

/// Put a posting into its document's slot in an ascending list, found
/// by binary search. A document the list does not hold yet is
/// inserted at that point; one it already holds is replaced there.
///
/// This is what keeps the step-2 promise — every postings list
/// ascending in document id — true of an index that grew after it was
/// written to disk. A new document's id is larger than every id in
/// the list, so its slot is the end: an append. A replaced document's
/// slot is the one its old posting just left.
fn insert_posting(postings: &mut Vec<Posting>, posting: Posting) {
    match postings.binary_search_by_key(&posting.doc, |p| p.doc) {
        Ok(at) => postings[at] = posting,
        Err(at) => postings.insert(at, posting),
    }
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
/// Version 2 added a length to every document and a term frequency to
/// every posting; version 3 keeps the lengths and replaces the
/// frequency with the positions of each occurrence — the frequency is
/// their count, and a file carrying both could disagree with itself.
const VERSION: u32 = 3;

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

/// `haystack` runs the demo; `haystack serve` serves the API;
/// `haystack add <file>...` indexes files into the index on disk.
#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    match std::env::args().nth(1).as_deref() {
        Some("serve") => serve(INDEX_PATH).await,
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

    /// Three documents: "fox" alone, "dog" alone, and both.
    fn three_docs() -> Index {
        let mut index = Index::new();
        index.add_document("a.md", "fox");
        index.add_document("b.md", "dog");
        index.add_document("c.md", "fox dog");
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
    fn a_repeated_term_records_where_each_copy_sits() {
        let mut index = Index::new();
        index.add_document("a.md", "fox fox fox");
        assert_eq!(
            index.search("fox"),
            &[Posting {
                doc: 0,
                positions: vec![0, 1, 2],
            }]
        );
    }

    #[test]
    fn distinct_terms_in_one_document_get_one_posting_each() {
        let mut index = Index::new();
        index.add_document("a.md", "fox dog fox");
        assert_eq!(
            index.search("fox"),
            &[Posting {
                doc: 0,
                positions: vec![0, 2],
            }]
        );
        assert_eq!(
            index.search("dog"),
            &[Posting {
                doc: 0,
                positions: vec![1],
            }]
        );
    }

    #[test]
    fn the_term_frequency_is_the_position_count() {
        let mut index = Index::new();
        index.add_document("a.md", "fox dog fox bird fox");
        assert_eq!(index.positions("fox", 0), [0, 2, 4]);
    }

    #[test]
    fn positions_are_ascending() {
        let mut index = Index::new();
        index.add_document("a.md", "fox dog cat fox bird fox");
        let positions = index.positions("fox", 0);
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn positions_skip_the_stop_words_around_them() {
        let mut index = Index::new();
        // "the" is dropped, so "fox" is the first term the index sees
        // and sits at position 0.
        index.add_document("a.md", "the fox and the dog");
        assert_eq!(index.positions("fox", 0), [0]);
        assert_eq!(index.positions("dog", 0), [1]);
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

    // --- phrase queries ---

    #[test]
    fn a_phrase_matches_adjacent_terms_in_order() {
        let mut index = Index::new();
        index.add_document("a.md", "the quick brown fox");
        assert_eq!(index.phrase(&["quick", "brown"]), [0]);
        // The other order is a different phrase.
        assert!(index.phrase(&["brown", "quick"]).is_empty());
    }

    #[test]
    fn a_phrase_needs_the_terms_next_to_each_other() {
        let mut index = Index::new();
        index.add_document("a.md", "quick brown fox");
        assert!(index.phrase(&["quick", "fox"]).is_empty());
    }

    #[test]
    fn a_phrase_matches_inside_a_longer_document() {
        let mut index = Index::new();
        index.add_document("a.md", "one two borrow checker three four");
        assert_eq!(index.phrase(&["borrow", "checker"]), [0]);
    }

    #[test]
    fn a_phrase_across_several_documents() {
        let mut index = Index::new();
        index.add_document("a.md", "borrow checker");
        index.add_document("b.md", "checker borrow");
        index.add_document("c.md", "nothing here");
        assert_eq!(index.phrase(&["borrow", "checker"]), [0]);
    }

    /// Stop words are dropped before positions are counted, so a
    /// phrase can match across one: "owner value" finds "owner of the
    /// value". That is the trade-off step 3's tokeniser made, showing
    /// up here.
    #[test]
    fn a_phrase_can_match_across_a_dropped_stop_word() {
        let mut index = Index::new();
        index.add_document("a.md", "the owner of the value");
        assert_eq!(index.phrase(&["owner", "value"]), [0]);
    }

    #[test]
    fn an_empty_phrase_matches_nothing() {
        let index = Index::new();
        assert!(index.phrase(&[]).is_empty());
    }

    #[test]
    fn a_one_word_phrase_is_that_words_postings() {
        let mut index = Index::new();
        index.add_document("a.md", "fox");
        index.add_document("b.md", "dog fox");
        assert_eq!(index.phrase(&["fox"]), [0, 1]);
    }

    // --- boolean queries ---

    #[test]
    fn and_keeps_the_documents_with_both_terms() {
        let index = three_docs();
        assert_eq!(index.boolean("fox AND dog").unwrap(), [2]);
    }

    #[test]
    fn or_keeps_the_documents_with_either_term() {
        let index = three_docs();
        assert_eq!(index.boolean("fox OR dog").unwrap(), [0, 1, 2]);
    }

    #[test]
    fn not_removes_the_documents_with_the_term() {
        let index = three_docs();
        assert_eq!(index.boolean("fox NOT dog").unwrap(), [0]);
        assert_eq!(index.boolean("NOT fox").unwrap(), [1]);
    }

    #[test]
    fn two_terms_with_no_operator_mean_and() {
        let index = three_docs();
        assert_eq!(
            index.boolean("fox dog").unwrap(),
            index.boolean("fox AND dog").unwrap()
        );
    }

    /// The documented evaluation order: left to right, equal
    /// precedence. "fox OR dog AND cat" is "(fox OR dog) AND cat".
    #[test]
    fn operators_evaluate_left_to_right() {
        let mut index = Index::new();
        index.add_document("a.md", "fox");
        index.add_document("b.md", "dog");
        index.add_document("c.md", "cat");
        index.add_document("d.md", "fox cat");
        // (fox OR dog) AND cat: a and b drop out for want of "cat",
        // d has both. The other reading would also return a.
        assert_eq!(index.boolean("fox OR dog AND cat").unwrap(), [3]);
    }

    #[test]
    fn a_quoted_phrase_needs_adjacency_but_the_same_words_do_not() {
        let mut index = Index::new();
        index.add_document("a.md", "borrow checker");
        index.add_document("b.md", "checker and borrow far apart");
        // b holds both terms, but not next to each other.
        assert_eq!(index.boolean("\"borrow checker\"").unwrap(), [0]);
        assert_eq!(index.boolean("borrow checker").unwrap(), [0, 1]);
    }

    /// A hyphenated word is one word that normalises to two terms, so
    /// the query treats it as the phrase they form — which is what
    /// the hyphen meant in the text.
    #[test]
    fn a_hyphenated_word_is_a_phrase_in_disguise() {
        let mut index = Index::new();
        index.add_document("a.md", "borrow checker");
        index.add_document("b.md", "checker borrow");
        assert_eq!(index.boolean("borrow-checker").unwrap(), [0]);
    }

    #[test]
    fn a_stop_word_in_a_query_matches_nothing() {
        let index = three_docs();
        assert!(index.boolean("the").unwrap().is_empty());
        // An empty set on the right of an OR changes nothing.
        assert_eq!(index.boolean("fox OR the").unwrap(), [0, 2]);
    }

    #[test]
    fn double_negation_cancels() {
        let index = three_docs();
        assert_eq!(index.boolean("NOT NOT fox").unwrap(), [0, 2]);
    }

    #[test]
    fn a_query_that_breaks_the_grammar_is_refused() {
        let index = three_docs();
        assert!(index.boolean("fox AND").is_err());
        assert!(index.boolean("AND fox").is_err());
        assert!(index.boolean("fox NOT").is_err());
        assert!(index.boolean("fox AND OR dog").is_err());
    }

    #[test]
    fn an_empty_query_is_refused() {
        let index = three_docs();
        assert!(index.boolean("").is_err());
        assert!(index.boolean("   ").is_err());
    }

    #[test]
    fn a_multi_term_query_sums_one_contribution_per_term() {
        let index = three_docs();
        let both = index.query("fox AND dog").unwrap();
        let fox = index.query("fox").unwrap();
        let dog = index.query("dog").unwrap();
        // The document with both terms scores the sum of what each
        // term alone gives it.
        let both_score = both.iter().find(|(doc, _)| *doc == 2).unwrap().1;
        let fox_score = fox.iter().find(|(doc, _)| *doc == 2).unwrap().1;
        let dog_score = dog.iter().find(|(doc, _)| *doc == 2).unwrap().1;
        assert!((both_score - (fox_score + dog_score)).abs() < 1e-12);
    }

    #[test]
    fn a_query_with_no_positive_terms_scores_zero() {
        let index = three_docs();
        // "NOT fox" removes the documents to rank by, so the one that
        // is left has nothing to score on.
        let ranked = index.query("NOT fox").unwrap();
        assert_eq!(ranked.iter().map(|(doc, _)| *doc).collect::<Vec<_>>(), [1]);
        assert_eq!(ranked[0].1, 0.0);
    }

    // --- fuzzy matching ---

    #[test]
    fn the_distance_between_equal_terms_is_zero() {
        assert_eq!(edit_distance("borrow", "borrow"), 0);
        assert_eq!(edit_distance("", ""), 0);
    }

    #[test]
    fn the_distance_counts_each_kind_of_edit() {
        assert_eq!(edit_distance("fox", "foxes"), 2); // two insertions
        assert_eq!(edit_distance("foxes", "fox"), 2); // two deletions
        assert_eq!(edit_distance("fox", "box"), 1); // one substitution
    }

    #[test]
    fn the_classic_examples_come_out_right() {
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("saturday", "sunday"), 3);
        assert_eq!(edit_distance("flaw", "lawn"), 2);
    }

    /// A transposition is two edits here — two substitutions — because
    /// the DP knows three operations, not four. Lucene's default
    /// counts it as one, and that is a known difference.
    #[test]
    fn a_transposition_costs_two_edits() {
        assert_eq!(edit_distance("teh", "the"), 2);
    }

    #[test]
    fn the_distance_is_counted_over_characters_not_bytes() {
        // One substitution each, though both are two bytes in UTF-8.
        assert_eq!(edit_distance("arger", "ärger"), 1);
        assert_eq!(edit_distance("strase", "straße"), 1);
    }

    #[test]
    fn an_empty_term_is_all_insertions() {
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("abc", ""), 3);
    }

    #[test]
    fn a_candidate_too_different_in_length_is_out_without_a_table() {
        // The length check: "ownership" is four edits from "owner",
        // so it is out at any radius below four.
        assert_eq!(edit_distance_within("owner", "ownership", 3), None);
        assert_eq!(edit_distance_within("owner", "ownership", 4), Some(4));
    }

    #[test]
    fn the_radius_caps_the_distance() {
        assert_eq!(edit_distance_within("kitten", "sitting", 2), None);
        assert_eq!(edit_distance_within("kitten", "sitting", 3), Some(3));
    }

    #[test]
    fn fuzzy_finds_the_term_a_typo_was_aiming_at() {
        let mut index = Index::new();
        for (path, text) in &corpus() {
            index.add_document(path, text);
        }
        assert_eq!(index.fuzzy("borow", 1), [("borrow", 1)]);
    }

    /// One typo can reach several terms: "valu" is one edit from
    /// "value" and two from both "call" (v->c, u->l) and "valid".
    /// That is a radius of two doing what it says — it is wide.
    #[test]
    fn one_typo_can_reach_several_terms() {
        let mut index = Index::new();
        for (path, text) in &corpus() {
            index.add_document(path, text);
        }
        assert_eq!(
            index.fuzzy("valu", 2),
            [("value", 1), ("call", 2), ("valid", 2)]
        );
    }

    #[test]
    fn fuzzy_lists_the_exact_term_at_distance_zero() {
        let mut index = Index::new();
        for (path, text) in &corpus() {
            index.add_document(path, text);
        }
        assert_eq!(index.fuzzy("borrow", 1).first(), Some(&("borrow", 0)));
    }

    #[test]
    fn fuzzy_returns_nothing_when_no_term_is_close() {
        let mut index = Index::new();
        for (path, text) in &corpus() {
            index.add_document(path, text);
        }
        assert!(index.fuzzy("zzzzz", 2).is_empty());
    }

    #[test]
    fn fuzzy_hits_are_ordered_by_distance_then_term() {
        let mut index = Index::new();
        index.add_document("a.md", "fox fog dog");
        // "fx" is one edit from "fox" and two from "fog"; "dog" is
        // three away and out at a radius of two.
        assert_eq!(index.fuzzy("fx", 2), [("fox", 1), ("fog", 2)]);
    }

    #[test]
    fn a_fuzzy_query_scores_by_every_variant_it_matched() {
        let mut index = Index::new();
        index.add_document("a.md", "fox");
        index.add_document("b.md", "fog");
        index.add_document("c.md", "fox fog");
        // "fx~2" matches fox and fog, and the document holding both
        // scores by both — the way an OR's does.
        let ranked = index.query("fx~2").unwrap();
        assert_eq!(ranked[0].0, 2);
        let fox = index.query("fox").unwrap();
        let fog = index.query("fog").unwrap();
        let both = ranked.iter().find(|(doc, _)| *doc == 2).unwrap().1;
        let fox_score = fox.iter().find(|(doc, _)| *doc == 2).unwrap().1;
        let fog_score = fog.iter().find(|(doc, _)| *doc == 2).unwrap().1;
        assert!((both - (fox_score + fog_score)).abs() < 1e-12);
    }

    #[test]
    fn a_typo_query_answers_what_the_correct_spelling_answers() {
        let mut index = Index::new();
        for (path, text) in &corpus() {
            index.add_document(path, text);
        }
        assert_eq!(
            index.query("borow~").unwrap(),
            index.query("borrow").unwrap()
        );
        assert_eq!(
            index.boolean("ower~1").unwrap(),
            index.boolean("owner").unwrap()
        );
    }

    #[test]
    fn a_bare_tilde_means_the_default_radius() {
        let mut index = Index::new();
        index.add_document("a.md", "fox fog");
        assert_eq!(
            index.boolean("fx~").unwrap(),
            index.boolean("fx~2").unwrap()
        );
    }

    #[test]
    fn a_radius_of_zero_is_an_exact_match() {
        let mut index = Index::new();
        index.add_document("a.md", "fox");
        assert_eq!(index.boolean("fox~0").unwrap(), [0]);
        assert!(index.boolean("fx~0").unwrap().is_empty());
    }

    #[test]
    fn a_stop_word_with_a_tilde_matches_nothing() {
        let index = three_docs();
        assert!(index.boolean("the~").unwrap().is_empty());
    }

    /// The fuzzy word goes through the tokeniser before the scan, so
    /// casing and punctuation fold away exactly as they do for an
    /// exact term.
    #[test]
    fn an_uppercase_fuzzy_word_is_lowercased() {
        let mut index = Index::new();
        index.add_document("a.md", "fox");
        assert_eq!(index.boolean("FOx~1").unwrap(), [0]);
    }

    /// The tilde covers one word. A word that normalises to two terms
    /// is a phrase, and this operator does not do phrases.
    #[test]
    fn a_hyphenated_word_with_a_tilde_matches_nothing() {
        let mut index = Index::new();
        index.add_document("a.md", "borrow checker");
        assert!(index.boolean("borrow-checker~").unwrap().is_empty());
    }

    #[test]
    fn not_applies_to_a_fuzzy_operand() {
        let mut index = Index::new();
        index.add_document("a.md", "fox");
        index.add_document("b.md", "fox dog");
        index.add_document("c.md", "dog");
        // "fox~1" matches a and b; NOT dog leaves a.
        assert_eq!(index.boolean("fox~1 NOT dog").unwrap(), [0]);
    }

    #[test]
    fn a_radius_past_the_cap_is_refused() {
        let index = three_docs();
        assert!(index.query("fox~3").is_err());
    }

    #[test]
    fn a_tilde_with_no_radius_after_it_is_refused() {
        let index = three_docs();
        assert!(index.query("fox~x").is_err());
        assert!(index.query("fox~-1").is_err());
        assert!(index.query("fox~1~2").is_err());
    }

    #[test]
    fn the_refusal_names_the_token() {
        let index = three_docs();
        assert_eq!(
            index.query("fox~3").unwrap_err().to_string(),
            "the fuzzy operator on \"fox~3\" needs an edit count between 0 and 2"
        );
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
    fn a_terms_positions_survive_a_reload() {
        let mut index = Index::new();
        index.add_document("a.md", "fox fox fox");
        index.add_document("b.md", "fox");
        let path = scratch("positions");
        index.save(&path).unwrap();
        let loaded = Index::load(&path).unwrap();
        assert_eq!(
            loaded.search("fox"),
            &[
                Posting {
                    doc: 0,
                    positions: vec![0, 1, 2],
                },
                Posting {
                    doc: 1,
                    positions: vec![0],
                },
            ]
        );
        // A document that does not hold the term has no positions.
        assert!(loaded.positions("fox", 2).is_empty());
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
            for _ in 0..reader.u32().unwrap() {
                reader.u32().unwrap(); // the document
                let count = reader.u32().unwrap(); // how many positions
                reader.take(count as usize * 4).unwrap(); // the positions
            }
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
        bytes.extend_from_slice(&4u32.to_le_bytes());
        let error = Index::load_bytes(&bytes).unwrap_err();
        assert_eq!(error.to_string(), "unsupported index version 4");
    }

    #[test]
    fn a_version_1_file_is_refused() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&1u32.to_le_bytes());
        let error = Index::load_bytes(&bytes).unwrap_err();
        assert_eq!(error.to_string(), "unsupported index version 1");
    }

    /// The format this step replaced. A version 2 file has term
    /// frequencies where the positions are now, and this build does
    /// not read it.
    #[test]
    fn a_version_2_file_is_refused() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&2u32.to_le_bytes());
        let error = Index::load_bytes(&bytes).unwrap_err();
        assert_eq!(error.to_string(), "unsupported index version 2");
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
        bytes.extend_from_slice(&1u32.to_le_bytes()); // one position
        bytes.extend_from_slice(&0u32.to_le_bytes()); // position 0
        bytes.extend_from_slice(&1u32.to_le_bytes()); // document 1 — out of order
        bytes.extend_from_slice(&1u32.to_le_bytes()); // one position
        bytes.extend_from_slice(&0u32.to_le_bytes()); // position 0
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

    // --- the HTTP layer ---

    #[test]
    fn a_request_without_q_is_a_bad_request() {
        let response = ApiError::MissingQuery.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn a_query_the_grammar_refuses_is_a_bad_request() {
        let error = ApiError::BadQuery(QueryError::BadFuzzy("borow~3".to_string()));
        assert_eq!(error.into_response().status(), StatusCode::BAD_REQUEST);
    }

    // --- incremental indexing ---

    #[test]
    fn a_loaded_index_takes_a_new_document() {
        let mut loaded = Index::load_bytes(&sample().save_bytes()).unwrap();
        assert_eq!(loaded.documents(), 2);
        // The new document gets the next id — the largest so far.
        assert_eq!(loaded.add_document("c.md", "fox bird"), 2);
        assert_eq!(loaded.documents(), 3);
        assert_eq!(ids(&loaded, "fox"), [0, 2]);
        assert_eq!(ids(&loaded, "bird"), [2]);
        // The documents that were already there are untouched.
        assert_eq!(ids(&loaded, "quick"), [0, 1]);
    }

    #[test]
    fn an_added_documents_postings_stay_in_order() {
        let mut loaded = Index::load_bytes(&sample().save_bytes()).unwrap();
        loaded.add_document("c.md", "quick fox cat");
        for term in ["quick", "fox", "cat"] {
            let docs: Vec<u32> = loaded.search(term).iter().map(|p| p.doc).collect();
            assert!(
                docs.windows(2).all(|pair| pair[0] < pair[1]),
                "{term}: {docs:?}"
            );
        }
    }

    #[test]
    fn a_known_path_is_reindexed_not_duplicated() {
        let mut loaded = Index::load_bytes(&sample().save_bytes()).unwrap();
        // a.md comes back with different text: same id, new contents.
        assert_eq!(loaded.add_document("a.md", "red fox"), 0);
        assert_eq!(loaded.documents(), 2);
        assert_eq!(loaded.path(0), "a.md");
        assert_eq!(loaded.path(1), "b.md");
        assert_eq!(ids(&loaded, "fox"), [0]);
        assert_eq!(loaded.positions("fox", 0), [1]);
        // "quick" was in a.md and b.md; it survives in b.md only.
        assert_eq!(ids(&loaded, "quick"), [1]);
        // No query answers with the same path twice.
        let ranked = loaded.query("fox").unwrap();
        let paths: Vec<&str> = ranked.iter().map(|&(doc, _)| loaded.path(doc)).collect();
        assert_eq!(paths, ["a.md"]);
    }

    #[test]
    fn a_replaced_documents_terms_leave_when_they_go() {
        let mut index = Index::new();
        index.add_document("a.md", "alpha beta");
        index.add_document("b.md", "beta gamma");
        assert_eq!(index.terms(), 3);
        // b.md loses "gamma" and gains "delta".
        index.add_document("b.md", "beta delta");
        assert_eq!(index.terms(), 3);
        assert!(index.search("gamma").is_empty());
        assert_eq!(ids(&index, "delta"), [1]);
        assert_eq!(ids(&index, "beta"), [0, 1]);
    }

    #[test]
    fn a_saved_and_loaded_index_survives_adding_byte_for_byte() {
        let mut loaded = Index::load_bytes(&sample().save_bytes()).unwrap();
        loaded.add_document("c.md", "fox bird");
        // Saving what was just loaded writes the same bytes the
        // in-memory index would have: the round trip loses nothing.
        let mut grown = sample();
        grown.add_document("c.md", "fox bird");
        assert_eq!(loaded.save_bytes(), grown.save_bytes());
    }

    #[test]
    fn average_length_moves_with_the_documents() {
        let mut loaded = Index::load_bytes(&sample().save_bytes()).unwrap();
        assert_eq!(loaded.average_length(), 3.5); // 3 and 4 terms
        loaded.add_document("c.md", "one two three four five six"); // 6 terms
        assert_eq!(loaded.average_length(), 13.0 / 3.0);
        // Replacing a document moves the average too.
        loaded.add_document("a.md", "fox"); // 3 terms -> 1
        assert_eq!(loaded.average_length(), 11.0 / 3.0);
    }

    #[test]
    fn a_new_document_can_top_the_ranking() {
        let mut loaded = Index::load_bytes(&sample().save_bytes()).unwrap();
        loaded.add_document("c.md", "fox fox fox fox fox");
        let ranked = loaded.rank("fox");
        assert_eq!(loaded.path(ranked[0].0), "c.md");
    }

    #[test]
    fn reindexing_a_document_leaves_the_others_alone() {
        let mut loaded = Index::load_bytes(&sample().save_bytes()).unwrap();
        assert_eq!(ids(&loaded, "quick"), [0, 1]);
        loaded.add_document("c.md", "quick");
        assert_eq!(ids(&loaded, "quick"), [0, 1, 2]);
        // Now replace c.md: quick's list loses document 2 and nothing
        // else, and b.md's own postings never moved.
        loaded.add_document("c.md", "slow");
        assert_eq!(ids(&loaded, "quick"), [0, 1]);
        assert_eq!(loaded.search("slow")[0].doc, 2);
        assert_eq!(loaded.positions("quick", 1), [2]);
    }
}
