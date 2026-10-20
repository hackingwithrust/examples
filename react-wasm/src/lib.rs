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
// allocation that is copied back out. Nothing is shared — step 5 gets to that.
#[wasm_bindgen]
pub fn shout(text: &str) -> String {
    text.to_uppercase() + "!!!"
}

// The scoring function from step 1, ported. -1 still means no match, so the
// JavaScript and Rust versions can be compared call for call.
#[wasm_bindgen]
pub fn fuzzy_score(query: &str, text: &str) -> i32 {
    let q: Vec<char> = query.to_lowercase().chars().collect();
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
