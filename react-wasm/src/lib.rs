use std::sync::OnceLock;
use wasm_bindgen::prelude::*;

// Numbers cross the boundary as plain JavaScript numbers.
#[wasm_bindgen]
pub fn add(left: i32, right: i32) -> i32 {
    left + right
}

// 64-bit integers do not fit in a JavaScript number, so they cross as BigInt.
#[wasm_bindgen]
pub fn expand(n: u64) -> u64 {
    n * 1_000_000_000
}

// A &str argument is copied into wasm memory; the returned String is a fresh
// allocation that is copied back out. To keep bytes that only cross once,
// take a String instead — load_dataset below does exactly that.
#[wasm_bindgen]
pub fn shout(text: &str) -> String {
    text.to_uppercase() + "!!!"
}

// The scoring function from step 1, ported. -1 still means no match, so the
// JavaScript and Rust versions can be compared call for call.
#[wasm_bindgen]
pub fn fuzzy_score(query: &str, text: &str) -> i32 {
    let q: Vec<char> = query.to_lowercase().chars().collect();
    score_folded(&q, text)
}

// The walk both scorers share: the query arrives already folded, the text
// folds here. fuzzy_score and search call this, which is what keeps their
// scores identical to each other and to step 1's JavaScript.
fn score_folded(q: &[char], text: &str) -> i32 {
    let t = text.to_lowercase();
    let mut score = 0;
    let mut qi = 0;
    let mut streak = 0;
    for tc in t.chars() {
        if qi == q.len() {
            break;
        }
        if tc == q[qi] {
            qi += 1;
            streak += 1;
            score += 2 + streak as i32;
        } else {
            streak = 0;
        }
    }
    if qi < q.len() {
        return -1;
    }
    score
}

// The dataset once it has crossed: the corpus keeps the bytes exactly as they
// arrived — one string, one item per line, title and description split by a
// tab — and ranges records where each item's two fields sit inside it.
static DATASET: OnceLock<(String, Vec<[u32; 4]>)> = OnceLock::new();

// Called once. Taking the corpus as a String means Rust owns the bytes the
// glue just copied in, so they are never copied again: every later scan reads
// them in place. JavaScript reaches the same bytes through a view —
// check-search.mjs shows the view and the one thing that invalidates it.
#[wasm_bindgen]
pub fn load_dataset(corpus: String) -> usize {
    let mut offset = 0usize;
    let mut ranges = Vec::new();
    for line in corpus.split('\n') {
        if let Some((title, description)) = line.split_once('\t') {
            ranges.push([
                offset as u32, // title start
                (offset + title.len()) as u32, // title end
                (offset + title.len() + 1) as u32, // description start
                (offset + title.len() + 1 + description.len()) as u32, // description end
            ]);
        }
        offset += line.len() + 1;
    }
    let n = ranges.len();
    let _ = DATASET.set((corpus, ranges)); // a second call is ignored
    n
}

// One crossing per keystroke: the query in, packed index/score pairs out.
// The titles never cross back — JavaScript kept its own copy for display.
#[wasm_bindgen]
pub fn search(query: &str, limit: usize) -> Vec<u32> {
    let Some((corpus, ranges)) = DATASET.get() else {
        panic!("search called before load_dataset");
    };
    let q: Vec<char> = query.to_lowercase().chars().collect();

    let mut hits: Vec<(i32, u32)> = Vec::new(); // (score, item index)
    for (index, &[ts, te, ds, de]) in ranges.iter().enumerate() {
        let score = score_folded(&q, &corpus[ts as usize..te as usize])
            .max(score_folded(&q, &corpus[ds as usize..de as usize]));
        if score >= 0 {
            hits.push((score, index as u32));
        }
    }
    // Stable sort, biggest score first: ties stay in index order, the same
    // order JavaScript's Array#sort keeps.
    hits.sort_by(|a, b| b.0.cmp(&a.0));
    hits.truncate(limit);

    let mut out = Vec::with_capacity(hits.len() * 2);
    for (score, index) in hits {
        out.push(index);
        out.push(score as u32);
    }
    out
}
