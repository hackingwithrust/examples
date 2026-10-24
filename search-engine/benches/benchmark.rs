//! The benchmark. `cargo bench` runs it.
//!
//! Three questions, in order:
//!
//! 1. **Latency.** How long the engine takes to build an index and to
//!    answer a query, measured by criterion — and how long the obvious
//!    implementation takes to answer the same query by reading every
//!    document. That second number is the comparison the index exists to
//!    win.
//! 2. **Recall.** Whether the answers are right. The corpus is generated
//!    with queries whose relevant documents are known by construction, so
//!    recall is a measurement rather than a feeling.
//! 3. **Memory.** What the index costs to keep: bytes on disk against the
//!    bytes of text it was built from.
//!
//! The corpus is deterministic — the same seed grows the same documents on
//! every machine — so the recall numbers are the same everywhere. The
//! latencies are not: they come from one machine, one run, and they move
//! between runs and between machines. Criterion keeps its own baseline, so
//! a second run prints a change line next to the first run's numbers.

use criterion::Criterion;
use haystack::{idf, tf_score, tokenize, Index, B, K1};
use std::hint::black_box;
use std::time::{Duration, Instant};

/// The seed. A constant, so a benchmark run anywhere measures the same
/// documents — a corpus that changed every run would be measured as much
/// as the code was.
const SEED: u64 = 31337;

/// How many distinct terms the corpus's dictionary holds.
const VOCABULARY: u64 = 512;

/// How many documents the benchmark indexes.
const CORPUS: u64 = 2_000;

/// How many queries carry known answers.
const QUERIES: u64 = 24;

/// How many documents each labelled query is planted in.
const RELEVANT: u64 = 5;

/// How many times the query term is planted in each relevant document —
/// against the one or two incidental mentions everywhere else.
const PLANTED: u64 = 8;

/// A 64-bit linear congruential generator. Not a good random number
/// generator; a reproducible one, which is what a benchmark needs. The
/// same seed produces the same sequence on every machine.
struct Rng {
    state: u64,
}

impl Rng {
    fn new(seed: u64) -> Self {
        Rng { state: seed }
    }

    fn next(&mut self) -> u64 {
        self.state = self
            .state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.state
    }

    /// A number below `bound`.
    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

/// The label of the term at `rank`: `term0000` for the most common,
/// `term0511` for the least.
///
/// The labels are not English, and that is deliberate. Every one is its
/// own stem, none is a stop word, and lowercasing changes nothing, so the
/// tokeniser is the identity on this corpus: the benchmark measures the
/// index, not step 3's normaliser. The run asserts this below.
fn term(rank: u64) -> String {
    format!("term{rank:04}")
}

/// The Zipf weights, as a cumulative table: entry `i` is the total weight
/// of terms `0..=i`. A uniform draw lands on the first term whose
/// cumulative weight passes it, which makes common terms common — a
/// handful of terms in a large slice of the corpus, a long tail of terms
/// in a handful of documents each.
fn zipf_table() -> Vec<u64> {
    let mut table = Vec::with_capacity(VOCABULARY as usize);
    let mut total = 0;
    for rank in 0..VOCABULARY {
        total += 1_000_000 / (rank + 1);
        table.push(total);
    }
    table
}

/// The term a uniform draw lands on under the Zipf table.
fn sample_term(table: &[u64], draw: u64) -> u64 {
    let total = *table.last().unwrap();
    table.partition_point(|&weight| weight <= draw % total) as u64
}

/// One generated document: the path it is indexed under, its text, and the
/// terms that text is made of.
struct Doc {
    path: String,
    text: String,
    terms: Vec<u64>,
}

/// Grow `count` documents: lengths between 40 and 160 terms, every term
/// drawn from the Zipf table.
fn generate_documents(rng: &mut Rng, count: u64, table: &[u64]) -> Vec<Doc> {
    (0..count)
        .map(|id| {
            let length = 40 + rng.below(121);
            let terms: Vec<u64> = (0..length).map(|_| sample_term(table, rng.next())).collect();
            Doc {
                path: format!("docs/doc{id:05}.md"),
                text: terms.iter().map(|&rank| term(rank)).collect::<Vec<_>>().join(" "),
                terms,
            }
        })
        .collect()
}

/// A query with known answers: the term as the index holds it, the
/// documents it was planted in, and the same term with one character
/// changed — what a reader types by mistake.
struct Labelled {
    /// The term's rank in the vocabulary.
    rank: u64,
    /// The documents the term is planted in: the relevant ones.
    relevant: Vec<u64>,
    /// The term with its first character changed, one edit away.
    typo: String,
}

/// One character of the label changed. On this dictionary the typo is one
/// edit from the intended term and two from its neighbours, which is what
/// makes the radius-2 measurement below interesting.
fn typo_of(label: &str) -> String {
    let mut chars: Vec<char> = label.chars().collect();
    chars[0] = 's';
    chars.into_iter().collect()
}

/// Grow `count` labelled queries, planting each one's term into fresh
/// documents. The planting is what makes recall measurable: the relevant
/// documents are known because they were chosen, not because someone read
/// the text and decided.
fn generate_queries(rng: &mut Rng, docs: &mut [Doc], count: u64) -> Vec<Labelled> {
    (0..count)
        .map(|_| {
            // A middle-of-the-dictionary term: common enough to have a
            // postings list worth ranking, rare enough not to be in
            // every document.
            let rank = 40 + rng.below(200);
            let mut relevant: Vec<u64> = Vec::new();
            while relevant.len() < RELEVANT as usize {
                let candidate = rng.below(docs.len() as u64);
                if !relevant.contains(&candidate) {
                    relevant.push(candidate);
                }
            }
            relevant.sort_unstable();
            for &id in &relevant {
                let doc = &mut docs[id as usize];
                for _ in 0..PLANTED {
                    let at = rng.below(doc.terms.len() as u64) as usize;
                    doc.terms[at] = rank;
                }
                doc.text = doc.terms.iter().map(|&rank| term(rank)).collect::<Vec<_>>().join(" ");
            }
            Labelled {
                rank,
                relevant,
                typo: typo_of(&term(rank)),
            }
        })
        .collect()
}

/// The baseline: for one term, read every document, count the term, score
/// it, sort. No postings, no dictionary — the obvious way to search, and
/// the honest comparison, because it is what an inverted index replaces.
///
/// The documents arrive pre-tokenised, which flatters the baseline: a real
/// scan would pay the tokeniser too. The comparison errs against the
/// index, which is the safe direction to err in.
fn brute_force(docs: &[Doc], rank: u64) -> Vec<(u64, f64)> {
    let n = docs.len();
    // The idf needs the document frequency, and the only way to know it
    // is to look at every document first.
    let df = docs.iter().filter(|doc| doc.terms.contains(&rank)).count();
    if df == 0 {
        return Vec::new();
    }
    let weight = idf(df, n);
    let avgdl = docs.iter().map(|doc| doc.terms.len() as f64).sum::<f64>() / n as f64;
    let mut scored: Vec<(u64, f64)> = docs
        .iter()
        .enumerate()
        .filter_map(|(id, doc)| {
            let tf = doc.terms.iter().filter(|&&t| t == rank).count() as u32;
            (tf > 0).then(|| {
                (
                    id as u64,
                    weight * tf_score(tf, doc.terms.len() as f64, avgdl, K1, B),
                )
            })
        })
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    scored
}

/// The baseline as a program without an index actually runs it: read the
/// text, split it into terms, count. No pre-tokenised shortcut, because
/// there is nowhere to cache the terms — every query pays the tokeniser
/// again. This is the slower of the two baselines and the honest one for a
/// collection that arrives as text.
fn brute_force_text(docs: &[Doc], rank: u64) -> Vec<(u64, f64)> {
    let n = docs.len();
    let label = term(rank);
    // One pass: tokenise every document and count the term in it. The
    // document frequency falls out of the same pass.
    let counts: Vec<(u64, u32, usize)> = docs
        .iter()
        .enumerate()
        .filter_map(|(id, doc)| {
            let tf = tokenize(&doc.text)
                .filter(|word| word.as_str() == label.as_str())
                .count() as u32;
            (tf > 0).then(|| (id as u64, tf, doc.terms.len()))
        })
        .collect();
    let df = counts.len();
    if df == 0 {
        return Vec::new();
    }
    let weight = idf(df, n);
    let avgdl = docs.iter().map(|doc| doc.terms.len() as f64).sum::<f64>() / n as f64;
    let mut scored: Vec<(u64, f64)> = counts
        .iter()
        .map(|&(id, tf, length)| {
            (
                id,
                weight * tf_score(tf, length as f64, avgdl, K1, B),
            )
        })
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    scored
}

/// The median of a list of durations.
fn median(mut times: Vec<Duration>) -> Duration {
    times.sort_unstable();
    times[times.len() / 2]
}

fn main() {
    let mut rng = Rng::new(SEED);
    let table = zipf_table();
    let mut docs = generate_documents(&mut rng, CORPUS, &table);
    let labelled = generate_queries(&mut rng, &mut docs, QUERIES);

    let text_bytes: usize = docs.iter().map(|doc| doc.text.len()).sum();
    let slots: usize = docs.iter().map(|doc| doc.terms.len()).sum();
    println!("corpus: {} documents, {} distinct terms, {slots} term slots, {text_bytes} bytes of text", docs.len(), VOCABULARY);
    println!(
        "queries: {QUERIES} labelled, {RELEVANT} relevant documents each, planted {PLANTED} times"
    );

    // The tokeniser is the identity on this corpus — every label is its
    // own stem and none is a stop word — so the index's terms are the
    // generator's terms, and the baseline's counts are the index's.
    for doc in docs.iter().take(50) {
        let tokenised: Vec<String> = tokenize(&doc.text).collect();
        assert_eq!(tokenised.len(), doc.terms.len());
        for (word, &rank) in tokenised.iter().zip(&doc.terms) {
            assert_eq!(*word, term(rank));
        }
    }
    println!("checked: the tokeniser is the identity on the first 50 documents");

    let mut index = Index::new();
    for doc in &docs {
        index.add_document(&doc.path, &doc.text);
    }
    println!(
        "index: {} documents, {} terms, average length {:.1}",
        index.documents(),
        index.terms(),
        index.average_length()
    );
    let pair = adjacent_pair(&docs);
    let words = [term(pair.0), term(pair.1)];
    let refs: Vec<&str> = words.iter().map(String::as_str).collect();
    println!(
        "the phrase benchmark asks about \"{}\": {} documents",
        refs.join(" "),
        index.phrase(&refs).len()
    );

    let mut c = Criterion::default()
        .warm_up_time(Duration::from_millis(500))
        .measurement_time(Duration::from_secs(1))
        .sample_size(50);

    // The one-time cost: building the index over the whole corpus. Every
    // query after this is free of it, which is the trade the scaling table
    // below puts numbers on.
    {
        let mut group = c.benchmark_group("index/build");
        group
            .sample_size(10)
            .measurement_time(Duration::from_secs(3))
            .bench_function("2000", |b| {
                b.iter(|| {
                    let mut index = Index::new();
                    for doc in black_box(&docs) {
                        index.add_document(&doc.path, &doc.text);
                    }
                    index.terms()
                })
            });
    }

    // The per-query costs. The index is built once, outside the closure:
    // a benchmark that rebuilt it would be measuring the build.
    let exact = term(labelled[0].rank);
    let common = term(0);
    let second = term(labelled[1].rank);
    let typo = &labelled[0].typo;
    c.bench_function("query/exact", |b| {
        b.iter(|| index.query(black_box(&exact)).unwrap().len())
    });
    c.bench_function("query/common", |b| {
        b.iter(|| index.query(black_box(&common)).unwrap().len())
    });
    c.bench_function("query/boolean", |b| {
        b.iter(|| {
            index
                .query(black_box(&format!("{exact} AND {second}")))
                .unwrap()
                .len()
        })
    });
    c.bench_function("query/phrase", |b| {
        // A pair of terms that really are adjacent somewhere in the
        // corpus, found by a scan, so the phrase benchmark asks a
        // question that has an answer.
        let pair = adjacent_pair(&docs);
        let words = [term(pair.0), term(pair.1)];
        let refs: Vec<&str> = words.iter().map(String::as_str).collect();
        b.iter(|| index.phrase(black_box(&refs)).len())
    });
    c.bench_function("query/fuzzy", |b| {
        b.iter(|| index.query(black_box(&format!("{typo}~2"))).unwrap().len())
    });

    // The baseline: the same query, answered by reading every document.
    // Slower per query, and it never gets faster — but it has no build
    // cost and holds no index, which is the other half of the comparison.
    c.bench_function("brute/scan", |b| {
        b.iter(|| brute_force(black_box(&docs), black_box(labelled[0].rank)).len())
    });
    {
        let mut group = c.benchmark_group("brute/tokenise");
        group
            .sample_size(10)
            .measurement_time(Duration::from_secs(3))
            .bench_function("2000", |b| {
                b.iter(|| brute_force_text(black_box(&docs), black_box(labelled[0].rank)).len())
            });
    }

    println!("\nthree corpus sizes, this machine:");
    println!("the build column is the median of three builds; the query columns average the {QUERIES} labelled queries, five rounds for the index and the pre-tokenised scan, three for the tokenising scan");
    println!(
        "  {:>7}  {:>10}  {:>12}  {:>12}  {:>12}  {:>8}  {:>8}",
        "corpus", "build", "query avg", "scan avg", "scan+text avg", "vs scan", "vs scan+text"
    );
    for &size in &[200u64, 2_000, 20_000] {
        let mut rng = Rng::new(SEED);
        let docs = generate_documents(&mut rng, size, &table);
        let mut builds = Vec::new();
        for _ in 0..3 {
            let start = Instant::now();
            let mut index = Index::new();
            for doc in &docs {
                index.add_document(&doc.path, &doc.text);
            }
            builds.push(start.elapsed());
        }
        // The same index for both query columns: the average of the
        // labelled queries is a workload, not a microbenchmark.
        let mut index = Index::new();
        for doc in &docs {
            index.add_document(&doc.path, &doc.text);
        }
        let mut query_times = Vec::new();
        let mut brute_times = Vec::new();
        for _ in 0..5 {
            let start = Instant::now();
            for query in &labelled {
                black_box(index.query(&term(query.rank)).unwrap().len());
            }
            query_times.push(start.elapsed() / QUERIES as u32);
            let start = Instant::now();
            for query in &labelled {
                black_box(brute_force(&docs, query.rank).len());
            }
            brute_times.push(start.elapsed() / QUERIES as u32);
        }
        let mut text_times = Vec::new();
        for _ in 0..3 {
            let start = Instant::now();
            for query in &labelled {
                black_box(brute_force_text(&docs, query.rank).len());
            }
            text_times.push(start.elapsed() / QUERIES as u32);
        }
        let query = median(query_times);
        let brute = median(brute_times);
        let text = median(text_times);
        let build = median(builds);
        println!(
            "  {:>7}  {:>10}  {:>12}  {:>12}  {:>12}  {:>7.0}x  {:>7.0}x",
            size,
            format_duration(build),
            format_duration(query),
            format_duration(brute),
            format_duration(text),
            brute.as_secs_f64() / query.as_secs_f64(),
            text.as_secs_f64() / query.as_secs_f64()
        );
        // The other half of the comparison: the build is paid once, the
        // per-query saving is paid back on every query.
        println!(
            "           break-even against the tokenising scan: {:.0} queries",
            build.as_secs_f64() / (text.as_secs_f64() - query.as_secs_f64())
        );
    }

    println!("\nrecall against the labelled answers:");
    // Exactness first: the index and the brute-force scan must rank the
    // same documents in the same order, to the last bit of the score.
    let mut identical = 0;
    for query in &labelled {
        let ranked = index.rank(&term(query.rank));
        let scanned = brute_force(&docs, query.rank);
        let same = ranked.len() == scanned.len()
            && ranked
                .iter()
                .zip(&scanned)
                .all(|((doc, _, score), (other, other_score))| {
                    *doc as u64 == *other && score == other_score
                });
        if same {
            identical += 1;
        }
    }
    println!(
        "  ranked lists identical to the brute-force scan: {identical}/{QUERIES} queries"
    );

    let exact_recall = recall_at(&index, &labelled, 0);
    let typo_one = recall_at(&index, &labelled, 1);
    let (typo_two_recall, typo_two_precision) = recall_at_typo(&index, &labelled, 2);
    let variants = index.fuzzy(&labelled[0].typo, 2).len();
    println!(
        "  the radius-2 typo matches {variants} dictionary terms ({} documents in the union)",
        index.query(&format!("{}~2", labelled[0].typo)).unwrap().len()
    );
    println!(
        "  recall@10, exact term:            {:.2}  ({} of {} relevant documents in the top ten)",
        exact_recall.0,
        exact_recall.1,
        QUERIES * RELEVANT
    );
    println!(
        "  recall@10, typo at radius 1:      {:.2}  ({} of {} relevant documents in the top ten)",
        typo_one.0,
        typo_one.1,
        QUERIES * RELEVANT
    );
    println!(
        "  recall@10, typo at radius 2:      {:.2}  ({} of {} relevant documents in the top ten)",
        typo_two_recall.0, typo_two_recall.1, QUERIES * RELEVANT
    );
    println!(
        "  precision@10, typo at radius 2:   {:.2}",
        typo_two_precision
    );

    let index_bytes = index.save_bytes().len();
    println!("\nmemory: {text_bytes} bytes of text, {index_bytes} bytes of index ({:.1}x)", index_bytes as f64 / text_bytes as f64);

    c.final_summary();
}

/// The first pair of distinct terms that sit next to each other somewhere
/// in the corpus.
fn adjacent_pair(docs: &[Doc]) -> (u64, u64) {
    for doc in docs {
        for window in doc.terms.windows(2) {
            if window[0] != window[1] {
                return (window[0], window[1]);
            }
        }
    }
    (docs[0].terms[0], docs[0].terms[0])
}

/// Recall at ten for the exact term of every labelled query: the share of
/// the known relevant documents that land in the top ten results.
fn recall_at(index: &Index, labelled: &[Labelled], radius: usize) -> (f64, u64) {
    let mut found = 0;
    let mut total = 0;
    for query in labelled {
        let asked = match radius {
            0 => term(query.rank),
            _ => format!("{}~{}", query.typo, radius),
        };
        let ranked = index.query(&asked).unwrap();
        let top: Vec<u64> = ranked.iter().take(10).map(|(doc, _)| *doc as u64).collect();
        found += query
            .relevant
            .iter()
            .filter(|doc| top.contains(doc))
            .count() as u64;
        total += query.relevant.len() as u64;
    }
    (found as f64 / total as f64, found)
}

/// Recall and precision at ten for the typo'd queries at a radius. The
/// typo matches more than the intended term, so the extra documents are
/// counted as false positives.
fn recall_at_typo(index: &Index, labelled: &[Labelled], radius: usize) -> ((f64, u64), f64) {
    let mut found = 0;
    let mut total = 0;
    let mut hits = 0;
    for query in labelled {
        let asked = format!("{}~{}", query.typo, radius);
        let ranked = index.query(&asked).unwrap();
        let top: Vec<u64> = ranked.iter().take(10).map(|(doc, _)| *doc as u64).collect();
        found += query
            .relevant
            .iter()
            .filter(|doc| top.contains(doc))
            .count() as u64;
        total += query.relevant.len() as u64;
        hits += top
            .iter()
            .filter(|doc| query.relevant.contains(doc))
            .count() as u64;
    }
    ((found as f64 / total as f64, found), hits as f64 / (labelled.len() * 10) as f64)
}

/// A duration in the unit that reads best: microseconds below a
/// millisecond, milliseconds below a second, seconds above.
fn format_duration(d: Duration) -> String {
    let micros = d.as_secs_f64() * 1_000_000.0;
    if micros < 1_000.0 {
        format!("{micros:.1} µs")
    } else if micros < 1_000_000.0 {
        format!("{:.2} ms", micros / 1_000.0)
    } else {
        format!("{:.2} s", micros / 1_000_000.0)
    }
}
