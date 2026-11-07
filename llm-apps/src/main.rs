//! Step 5: a vector store in SQLite.
//!
//! Same model, chunker and embed pipeline as steps 3 and 4. The chunk
//! vectors become BLOBs in a SQLite database; a question is answered by
//! scanning every row - brute force - and then by an index: k-means
//! clusters the stored vectors, and the query only visits the clusters
//! nearest to it. On this step's 405-row store the index reads under a
//! third of the rows and returns the same top hits.

use anyhow::Result;
use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config, DTYPE};
use hf_hub::api::sync::Api;
use rusqlite::{params, params_from_iter, Connection};
use tokenizers::Tokenizer;

const MODEL_ID: &str = "sentence-transformers/all-MiniLM-L6-v2";
const DOC: &str = "Running a sentence encoder on your own machine is mostly an exercise in memory budgeting. The runtime in this project is candle, which loads the safetensors weights through memory mapping, so nothing is copied until a layer actually runs. Startup cost is dominated entirely by paging the model in on the first forward pass. Its peak memory footprint is about 340 megabytes. That is with three sentences in the batch; a long document pushes the number up. Tokenization itself is cheap, microseconds per sentence, and it happens before anything reaches the model. Embedding a whole document at once is the mistake this step fixes: the vector averages every topic together and retrieval degrades quietly.";
const QUESTION: &str = "How much memory does inference use at peak?";

/// Index size (clusters) and how many of them a query visits.
const K: usize = 8;
const PROBE: usize = 2;
/// Synthetic near-copies per real chunk, to give the store a realistic size.
const COPIES: usize = 80;

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

type Hit = (i64, f32, String);

/// Brute force: read every row, decode the blob, cosine in Rust.
fn search_brute(conn: &Connection, q: &[f32], top: usize) -> Result<(usize, Vec<Hit>)> {
    let mut stmt = conn.prepare(
        "SELECT v.chunk_id, v.embedding, c.text
         FROM vectors v JOIN chunks c ON c.id = v.chunk_id",
    )?;
    let mut rows = stmt.query([])?;
    let mut scored: Vec<Hit> = Vec::new();
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let blob: Vec<u8> = row.get(1)?;
        let text: String = row.get(2)?;
        scored.push((id, cosine(&from_blob(&blob), q), text));
    }
    let scanned = scored.len();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    scored.truncate(top);
    Ok((scanned, scored))
}

/// Indexed search: rank the centroids, then read only the rows assigned
/// to the `probe` nearest clusters. Returns the clusters visited, the
/// rows read, and the hits.
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
        let id: i64 = row.get(0)?;
        let blob: Vec<u8> = row.get(1)?;
        let text: String = row.get(2)?;
        scored.push((id, cosine(&from_blob(&blob), q), text));
    }
    let scanned = scored.len();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    scored.truncate(top);
    let visited: Vec<(i64, f32)> = clusters[..probe].to_vec();
    Ok((visited, scanned, scored))
}

fn print_hits(hits: &[Hit]) {
    for (rank, (_, score, text)) in hits.iter().enumerate() {
        println!("  {}. [{score:.4}] {text}", rank + 1);
    }
}

fn main() -> Result<()> {
    let device = Device::Cpu;
    let started = std::time::Instant::now();

    let api = Api::new()?;
    let repo = api.model(MODEL_ID.to_string());
    let config: Config = serde_json::from_str(&std::fs::read_to_string(repo.get("config.json")?)?)?;
    let tokenizer = Tokenizer::from_file(repo.get("tokenizer.json")?)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let weights = repo.get("model.safetensors")?;
    let vb = unsafe { VarBuilder::from_mmaped_safetensors(&[weights], DTYPE, &device)? };
    let model = BertModel::load(vb, &config)?;
    println!("model loaded in {:.1?} on {device:?}", started.elapsed());

    // Same chunker, document and question as step 4: 30-word windows,
    // 6-word overlap.
    let chunks = chunk(DOC, 30, 24);

    // The store lives in the temp directory; delete any copy from an
    // earlier run so the numbers below start from an empty database.
    let db_path = std::env::temp_dir().join("llm-step5-vectors.db");
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

    let mut texts = chunks.clone();
    texts.push(QUESTION.to_string());
    let vectors = embed(&model, &device, &tokenizer, &texts)?;
    let question = vectors.last().unwrap().clone();

    for (i, v) in vectors[..vectors.len() - 1].iter().enumerate() {
        conn.execute(
            "INSERT INTO chunks (id, text) VALUES (?1, ?2)",
            params![i as i64, format!("chunk {i}: {}", preview(&chunks[i]))],
        )?;
        conn.execute(
            "INSERT INTO vectors (chunk_id, embedding) VALUES (?1, ?2)",
            params![i as i64, to_blob(v)],
        )?;
    }
    println!("stored {} chunk vectors in the database", vectors.len() - 1);

    let (scanned, hits) = search_brute(&conn, &question, 3)?;
    println!("\nbrute force over {scanned} rows:");
    print_hits(&hits);

    // Scale the store up: 80 near-copies of each real chunk, 400 rows,
    // standing in for the neighbours a real corpus produces.
    for i in 0..5usize {
        for j in 0..COPIES {
            let id = (5 + i * COPIES + j) as i64;
            let noisy: Vec<f32> = vectors[i]
                .iter()
                .enumerate()
                .map(|(d, x)| x + jitter(id as usize, d))
                .collect();
            conn.execute(
                "INSERT INTO chunks (id, text) VALUES (?1, ?2)",
                params![id, format!("chunk {i} copy #{j}")],
            )?;
            conn.execute(
                "INSERT INTO vectors (chunk_id, embedding) VALUES (?1, ?2)",
                params![id, to_blob(&noisy)],
            )?;
        }
    }
    let total: i64 = conn.query_row("SELECT COUNT(*) FROM vectors", [], |r| r.get(0))?;
    let (scanned, brute_hits) = search_brute(&conn, &question, 3)?;
    println!("\nscaled to {total} rows; brute force over {scanned} rows:");
    print_hits(&brute_hits);

    // The index is built from what the store holds: every vector is read
    // back as a blob, clustered, and the cluster map is written back.
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
    println!("\nindex built: {K} clusters, sizes {sizes:?}");

    let (visited, scanned, hits) = search_index(&conn, &question, PROBE, 3)?;
    let pct = 100.0 * scanned as f64 / total as f64;
    let visited = visited
        .iter()
        .map(|(id, s)| format!("{id} ({s:.4})"))
        .collect::<Vec<_>>()
        .join(", ");
    println!(
        "\nindexed search: cluster {visited} nearest to the question, scanned {scanned} of {total} rows ({pct:.0}%)"
    );
    print_hits(&hits);
    let overlap = hits
        .iter()
        .filter(|(id, _, _)| brute_hits.iter().any(|(bid, _, _)| bid == id))
        .count();
    println!("top-3 overlap with brute force: {overlap} of 3");

    Ok(())
}
