//! Step 6: retrieving and prompting.
//!
//! Same model, chunker, store and index as steps 3-5. This step turns
//! ranked search results into a context window: the query retrieves
//! wider than it needs, near-duplicate chunks are collapsed, the rest
//! is fitted to a token budget counted with the tokenizer, and the
//! passages that survive are assembled into the prompt the next step's
//! model will answer from.

use anyhow::Result;
use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config, DTYPE};
use hf_hub::api::sync::Api;
use rusqlite::{params, params_from_iter, Connection};
use tokenizers::Tokenizer;

const MODEL_ID: &str = "sentence-transformers/all-MiniLM-L6-v2";
const QUESTION: &str = "How much memory does inference use at peak?";

/// Three documents, three topics: the runtime's memory footprint, the
/// chunking trade, and the tokenizer. One paragraph could not show a
/// prompt filling up; three give retrieval something to choose between.
const DOCS: [&str; 3] = [
    "Running a sentence encoder on your own machine is mostly an exercise in memory budgeting. The runtime in this project is candle, which loads the safetensors weights through memory mapping, so nothing is copied until a layer actually runs. Startup cost is dominated entirely by paging the model in on the first forward pass. Its peak memory footprint is about 340 megabytes. That is with three sentences in the batch; a long document pushes the number up. Tokenization itself is cheap, microseconds per sentence, and it happens before anything reaches the model. Embedding a whole document at once is the mistake this step fixes: the vector averages every topic together and retrieval degrades quietly.",
    "Chunking is where retrieval quality is decided. Windows of thirty words with a six-word overlap worked well for short technical prose: small enough that one vector describes one idea, overlapped just enough that a sentence split across a boundary still lands whole in at least one chunk. Bigger windows average more topics into each vector and recall suffers quietly. Smaller windows fragment the answer across chunks and the prompt fills with fragments. No window size is right for every corpus; the overlap and the stride are a trade to measure, not a default to trust.",
    "Tokenization happens before anything reaches the model, and it is the cheap part of the pipeline: microseconds per sentence on CPU. The tokenizer splits text into word pieces drawn from a fixed vocabulary of about thirty thousand entries, and the count it reports is the number of positions the model actually sees. A batch of sentences is padded to its longest member, and the attention mask marks the padding so the pooled embedding ignores it. That count is also the only honest way to budget a context window, which is what this step uses it for.",
];

/// How wide the net is cast, how close two chunks must be before the
/// second counts as a duplicate of the first, and how many tokens the
/// context window may hold.
const CANDIDATES: usize = 15;
const DUP_THRESHOLD: f32 = 0.98;
const CONTEXT_TOKENS: usize = 100;

/// Index size (clusters) and how many of them a query visits.
const K: usize = 8;
const PROBE: usize = 2;
/// Synthetic near-copies per real chunk: standing in for the boilerplate
/// and repeated near-duplicates a real corpus accumulates, and giving
/// the dedupe pass something to collapse.
const COPIES: usize = 3;

/// Cut `doc` into word windows of `window` words, advancing `stride`
/// words each time. Same chunker as step 4.
fn chunk(doc: &str, window: usize, stride: usize) -> Vec<String> {
    let words: Vec<&str> = doc.split_whitespace().collect();
    let mut out = Vec::new();
    let mut start = 0;
    while start < words.len() {
        let end = (start + window).min(words.len());
        out.push(words[start..end].join(" "));
        if end == words.len() {
            break;
        }
        start += stride;
    }
    out
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let len_a = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let len_b = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (len_a * len_b)
}

/// Tokenize, forward, mean-pool: one Vec<f32> of 384 numbers per input.
fn embed(
    model: &BertModel,
    device: &Device,
    tokenizer: &Tokenizer,
    texts: &[String],
) -> Result<Vec<Vec<f32>>> {
    let encodings = tokenizer
        .encode_batch(texts.to_vec(), true)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let len = encodings.iter().map(|e| e.get_ids().len()).max().unwrap();
    let token_ids: Vec<Vec<u32>> = encodings
        .iter()
        .map(|e| {
            let mut ids = e.get_ids().to_vec();
            ids.resize(len, 0);
            ids
        })
        .collect();
    let attention: Vec<Vec<u32>> = encodings
        .iter()
        .map(|e| {
            let mut mask = e.get_attention_mask().to_vec();
            mask.resize(len, 0);
            mask
        })
        .collect();
    let types = vec![vec![0u32; len]; texts.len()];

    let ids = Tensor::new(token_ids, device)?;
    let mask = Tensor::new(attention.clone(), device)?;
    let types = Tensor::new(types, device)?;

    let hidden = model.forward(&ids, &types, Some(&mask))?.to_vec3::<f32>()?;
    Ok(hidden
        .iter()
        .zip(attention.iter())
        .map(|(tokens, mask)| {
            let mut v = vec![0f32; tokens[0].len()];
            let mut n = 0f32;
            for (t, &keep) in tokens.iter().zip(mask) {
                if keep == 1 {
                    n += 1.0;
                    for (v, &x) in v.iter_mut().zip(t) {
                        *v += x;
                    }
                }
            }
            v.iter_mut().for_each(|x| *x /= n);
            v
        })
        .collect())
}

/// A vector becomes a BLOB: 384 little-endian f32s, 1536 bytes.
fn to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// And back. `chunks_exact` panics on a corrupted row rather than
/// silently mis-decoding it.
fn from_blob(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// The first 10 words of a chunk, for the printed table.
fn preview(s: &str) -> String {
    let words: Vec<&str> = s.split_whitespace().take(10).collect();
    let mut out = words.join(" ");
    if s.split_whitespace().count() > 10 {
        out.push_str(" ...");
    }
    out
}

/// Deterministic pseudo-noise, so every run of this program prints the
/// same numbers. Amplitude 0.01 keeps each copy within a hair of its
/// original - a stand-in for the near-neighbours a real corpus produces.
fn jitter(seed: usize, dim: usize) -> f32 {
    ((seed * 31 + dim * 7) as f32 * 0.173).sin() * 0.01
}

/// Lloyd's k-means with a deterministic start: seed 0, then the vector
/// farthest from it, then repeatedly the vector farthest from everything
/// chosen so far. Runs to convergence or `iters` passes, whichever first.
fn kmeans(vectors: &[Vec<f32>], k: usize, iters: usize) -> (Vec<Vec<f32>>, Vec<usize>) {
    let dist = |a: &Vec<f32>, b: &Vec<f32>| 1.0 - cosine(a, b);
    let mut seeds: Vec<usize> = vec![0];
    while seeds.len() < k {
        let (mut best, mut bd) = (0usize, -1.0f32);
        for (i, v) in vectors.iter().enumerate() {
            if seeds.contains(&i) {
                continue;
            }
            let d: f32 = seeds.iter().map(|&s| dist(v, &vectors[s])).sum();
            if d > bd {
                bd = d;
                best = i;
            }
        }
        seeds.push(best);
    }
    let mut centroids: Vec<Vec<f32>> = seeds.iter().map(|&s| vectors[s].clone()).collect();
    let mut assign = vec![0usize; vectors.len()];
    for _ in 0..iters {
        let mut changed = false;
        for (i, v) in vectors.iter().enumerate() {
            let nearest = centroids
                .iter()
                .enumerate()
                .max_by(|a, b| cosine(v, a.1).total_cmp(&cosine(v, b.1)))
                .map(|(ci, _)| ci)
                .unwrap();
            if assign[i] != nearest {
                changed = true;
                assign[i] = nearest;
            }
        }
        for (ci, c) in centroids.iter_mut().enumerate() {
            let members: Vec<&Vec<f32>> = vectors
                .iter()
                .enumerate()
                .filter(|(i, _)| assign[*i] == ci)
                .map(|(_, v)| v)
                .collect();
            if members.is_empty() {
                continue;
            }
            for d in 0..c.len() {
                c[d] = members.iter().map(|v| v[d]).sum::<f32>() / members.len() as f32;
            }
        }
        if !changed {
            break;
        }
    }
    (centroids, assign)
}

/// One search result: its cosine score against the question, the
/// chunk's full text, and the decoded vector - kept alongside the text
/// because the dedupe pass below needs it.
#[derive(Clone)]
struct Hit {
    score: f32,
    text: String,
    vector: Vec<f32>,
}

/// The token count under the model's own tokenizer. A different model
/// has a different vocabulary and would count differently - but
/// counting beats estimating, and the budget is a dial either way.
fn token_count(tokenizer: &Tokenizer, text: &str) -> usize {
    tokenizer
        .encode(text, false)
        .map(|e| e.get_ids().len())
        .unwrap_or(0)
}

/// Indexed search, as step 5 built it: rank the centroids, then read
/// only the rows assigned to the `probe` nearest clusters. Returns the
/// clusters visited, the rows read, and up to `top` hits.
fn search_index(
    conn: &Connection,
    q: &[f32],
    probe: usize,
    top: usize,
) -> Result<(Vec<(i64, f32)>, usize, Vec<Hit>)> {
    let mut stmt = conn.prepare("SELECT id, vector FROM centroids")?;
    let mut rows = stmt.query([])?;
    let mut clusters: Vec<(i64, f32)> = Vec::new();
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let c = from_blob(&row.get::<_, Vec<u8>>(1)?);
        clusters.push((id, cosine(&c, q)));
    }
    clusters.sort_by(|a, b| b.1.total_cmp(&a.1));
    let chosen: Vec<i64> = clusters[..probe].iter().map(|c| c.0).collect();

    let list = chosen
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let mut stmt = conn.prepare(&format!(
        "SELECT v.chunk_id, v.embedding, c.text
         FROM vectors v
         JOIN chunks c ON c.id = v.chunk_id
         JOIN assignments a ON a.chunk_id = v.chunk_id
         WHERE a.centroid_id IN ({list})"
    ))?;
    let mut rows = stmt.query(params_from_iter(chosen.iter()))?;
    let mut scored: Vec<Hit> = Vec::new();
    while let Some(row) = rows.next()? {
        let vector = from_blob(&row.get::<_, Vec<u8>>(1)?);
        let text: String = row.get(2)?;
        let score = cosine(&vector, q);
        scored.push(Hit {
            score,
            text,
            vector,
        });
    }
    let scanned = scored.len();
    scored.sort_by(|a, b| b.score.total_cmp(&a.score));
    scored.truncate(top);
    let visited: Vec<(i64, f32)> = clusters[..probe].to_vec();
    Ok((visited, scanned, scored))
}

/// Print a ranked candidate list with each chunk's token count.
fn print_candidates(cands: &[Hit], tokenizer: &Tokenizer) {
    for (rank, h) in cands.iter().enumerate() {
        println!(
            "  {:2}. [{:.4}] {:>3}t  {}",
            rank + 1,
            h.score,
            token_count(tokenizer, &h.text),
            preview(&h.text)
        );
    }
}

/// True when `v` is within `threshold` cosine of anything already
/// kept. The threshold is deliberately generous: it exists to catch
/// near-copies, not related chunks.
fn is_duplicate(v: &[f32], kept: &[Vec<f32>], threshold: f32) -> bool {
    kept.iter().any(|k| cosine(v, k) > threshold)
}

/// Walk the ranked candidates, keep the first of every distinct chunk,
/// and stop at the first passage that no longer fits the budget. The
/// rest are all counted over-budget: chunks are near-equal size, so
/// nothing behind an oversized one fits either. Returns the kept
/// passages and the drop counts.
fn select(
    candidates: &[Hit],
    tokenizer: &Tokenizer,
    threshold: f32,
    budget: usize,
) -> (Vec<Hit>, usize, usize) {
    let mut kept: Vec<Hit> = Vec::new();
    let mut vectors: Vec<Vec<f32>> = Vec::new();
    let mut used = 0usize;
    let (mut dupes, mut over) = (0usize, 0usize);
    for cand in candidates {
        if is_duplicate(&cand.vector, &vectors, threshold) {
            dupes += 1;
            continue;
        }
        let tokens = token_count(tokenizer, &cand.text);
        if used + tokens > budget {
            over = candidates.len() - kept.len() - dupes;
            break;
        }
        used += tokens;
        vectors.push(cand.vector.clone());
        kept.push(cand.clone());
    }
    (kept, dupes, over)
}

/// The prompt the next step's model will answer from: an instruction,
/// the numbered passages, and the question last.
fn build_prompt(passages: &[String], question: &str) -> String {
    let mut prompt = String::from(
        "Answer the question using only the passages below. \
         If the passages do not contain the answer, say so.\n\n",
    );
    for (i, passage) in passages.iter().enumerate() {
        prompt.push_str(&format!("[{}] {}\n\n", i + 1, passage));
    }
    prompt.push_str("Question: ");
    prompt.push_str(question);
    prompt
}

fn main() -> Result<()> {
    let device = Device::Cpu;
    let started = std::time::Instant::now();

    let api = Api::new()?;
    let repo = api.model(MODEL_ID.to_string());
    let config: Config = serde_json::from_str(&std::fs::read_to_string(repo.get("config.json")?)?)?;
    let tokenizer = Tokenizer::from_file(repo.get("tokenizer.json")?)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    // The published tokenizer.json ships with fixed padding to 128 -
    // sentence-transformers tunes it for the model, not for counting.
    // Every text would otherwise count as 128 tokens.
    let mut tokenizer = tokenizer;
    tokenizer.with_padding(None);
    let weights = repo.get("model.safetensors")?;
    let vb = unsafe { VarBuilder::from_mmaped_safetensors(&[weights], DTYPE, &device)? };
    let model = BertModel::load(vb, &config)?;
    println!("model loaded in {:.1?} on {device:?}", started.elapsed());

    // Same chunker as steps 4 and 5: 30-word windows, 6-word overlap,
    // now applied to each document in turn.
    let mut chunks: Vec<(usize, String)> = Vec::new();
    for (d, doc) in DOCS.iter().enumerate() {
        for c in chunk(doc, 30, 24) {
            chunks.push((d, c));
        }
    }

    // The store lives in the temp directory; delete any copy from an
    // earlier run so the numbers below start from an empty database.
    let db_path = std::env::temp_dir().join("llm-step6-vectors.db");
    let _ = std::fs::remove_file(&db_path);
    let conn = Connection::open(&db_path)?;
    conn.execute_batch(
        "CREATE TABLE chunks (id INTEGER PRIMARY KEY, text TEXT NOT NULL);
         CREATE TABLE vectors (
             chunk_id  INTEGER PRIMARY KEY REFERENCES chunks(id),
             embedding BLOB NOT NULL);
         CREATE TABLE centroids (id INTEGER PRIMARY KEY, vector BLOB NOT NULL);
         CREATE TABLE assignments (
             chunk_id    INTEGER PRIMARY KEY REFERENCES chunks(id),
             centroid_id INTEGER NOT NULL REFERENCES centroids(id));",
    )?;

    let mut texts: Vec<String> = chunks.iter().map(|(_, c)| c.clone()).collect();
    texts.push(QUESTION.to_string());
    let vectors = embed(&model, &device, &tokenizer, &texts)?;
    let question = vectors.last().unwrap().clone();

    // Full text goes into the store this step: the prompt is built from
    // rows, and a preview cannot answer a question.
    let real = chunks.len();
    for (i, v) in vectors[..real].iter().enumerate() {
        conn.execute(
            "INSERT INTO chunks (id, text) VALUES (?1, ?2)",
            params![i as i64, &chunks[i].1],
        )?;
        conn.execute(
            "INSERT INTO vectors (chunk_id, embedding) VALUES (?1, ?2)",
            params![i as i64, to_blob(v)],
        )?;
    }

    // Near-copies of every chunk: same text, wobbled vector. They stand
    // in for the boilerplate and repeated near-duplicates a real corpus
    // accumulates.
    for (i, v) in vectors[..real].iter().enumerate() {
        for j in 0..COPIES {
            let id = (real + i * COPIES + j) as i64;
            let noisy: Vec<f32> = v
                .iter()
                .enumerate()
                .map(|(d, x)| x + jitter(id as usize, d))
                .collect();
            conn.execute(
                "INSERT INTO chunks (id, text) VALUES (?1, ?2)",
                params![id, &chunks[i].1],
            )?;
            conn.execute(
                "INSERT INTO vectors (chunk_id, embedding) VALUES (?1, ?2)",
                params![id, to_blob(&noisy)],
            )?;
        }
    }
    let total: i64 = conn.query_row("SELECT COUNT(*) FROM vectors", [], |r| r.get(0))?;
    println!(
        "stored {real} chunk vectors from {} documents, {COPIES} near-copies each ({total} rows)",
        DOCS.len()
    );

    // The index from step 5, built the same way: read every vector back
    // as a blob, cluster, write the cluster map back.
    let mut stmt = conn.prepare("SELECT chunk_id, embedding FROM vectors")?;
    let mut rows = stmt.query([])?;
    let mut ids: Vec<i64> = Vec::new();
    let mut all: Vec<Vec<f32>> = Vec::new();
    while let Some(row) = rows.next()? {
        ids.push(row.get(0)?);
        all.push(from_blob(&row.get::<_, Vec<u8>>(1)?));
    }
    let (centroids, assign) = kmeans(&all, K, 20);
    for (ci, c) in centroids.iter().enumerate() {
        conn.execute(
            "INSERT INTO centroids (id, vector) VALUES (?1, ?2)",
            params![ci as i64, to_blob(c)],
        )?;
    }
    for (&chunk_id, &ci) in ids.iter().zip(assign.iter()) {
        conn.execute(
            "INSERT INTO assignments (chunk_id, centroid_id) VALUES (?1, ?2)",
            params![chunk_id, ci as i64],
        )?;
    }
    let mut sizes = vec![0usize; K];
    for &ci in &assign {
        sizes[ci] += 1;
    }
    sizes.sort();
    println!("index built: {K} clusters, sizes {sizes:?}");

    // Retrieve wider than the prompt needs: the dedupe pass and the
    // budget both need room to work.
    let (visited, scanned, hits) = search_index(&conn, &question, PROBE, CANDIDATES)?;
    let visited = visited
        .iter()
        .map(|(id, s)| format!("{id} ({s:.4})"))
        .collect::<Vec<_>>()
        .join(", ");
    println!(
        "indexed search: cluster {visited} nearest to the question, scanned {scanned} of {total} rows"
    );

    println!("\n{CANDIDATES} candidates, best score first:");
    print_candidates(&hits, &tokenizer);

    let (kept, dupes, over) = select(&hits, &tokenizer, DUP_THRESHOLD, CONTEXT_TOKENS);
    let kept_tokens: usize = kept
        .iter()
        .map(|h| token_count(&tokenizer, &h.text))
        .sum();
    println!(
        "\nkept {} passages, {} of {CONTEXT_TOKENS} budget tokens; dropped {dupes} duplicates, {over} over budget",
        kept.len(),
        kept_tokens
    );
    for (i, h) in kept.iter().enumerate() {
        println!(
            "  [{}] [{:.4}] {}t  {}",
            i + 1,
            h.score,
            token_count(&tokenizer, &h.text),
            preview(&h.text)
        );
    }

    let prompt = build_prompt(
        &kept.iter().map(|h| h.text.clone()).collect::<Vec<_>>(),
        QUESTION,
    );
    let bar = "-".repeat(56);
    println!(
        "\nprompt, {} tokens:\n{bar}\n{prompt}\n{bar}",
        token_count(&tokenizer, &prompt)
    );

    Ok(())
}
