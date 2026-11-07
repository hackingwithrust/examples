//! Step 7: an HTTP API for questions.
//!
//! Same model, chunker, store, index and prompt as steps 3-6, now behind
//! a server. Startup embeds the corpus and builds the index once; then
//! `POST /ask` takes a question, retrieves and fits a context window the
//! way step 6 did, and streams the answer back over Server-Sent Events:
//! the retrieved passages first, then the answer model's tokens as they
//! arrive, then a final event with the counts. Generation runs on ollama
//! (step 2's runtime), which streams newline-delimited JSON that this
//! program relays as SSE.

use anyhow::Result;
use axum::{
    extract::State,
    response::sse::{Event, KeepAlive, KeepAliveStream, Sse},
    routing::post,
    Json, Router,
};
use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config, DTYPE};
use futures_util::StreamExt;
use hf_hub::api::sync::Api;
use rusqlite::{params, params_from_iter, Connection};
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use tokenizers::Tokenizer;
use tokio_stream::wrappers::ReceiverStream;

const MODEL_ID: &str = "sentence-transformers/all-MiniLM-L6-v2";

/// The answer model: step 2's ollama runtime, streaming. This program
/// never loads its weights - generation is an HTTP call away.
const ANSWER_MODEL: &str = "qwen2.5:0.5b";
const OLLAMA_URL: &str = "http://127.0.0.1:11434/api/generate";

/// Hard cap on the answer's length, in tokens, passed to ollama. A
/// question-answering endpoint that never stops writing is a bug.
const MAX_ANSWER_TOKENS: u32 = 120;
const DEFAULT_PORT: u16 = 8091;

/// Three documents, three topics: the runtime's memory footprint, the
/// chunking trade, and the tokenizer. Same corpus as step 6.
const DOCS: [&str; 3] = [
    "Running a sentence encoder on your own machine is mostly an exercise in memory budgeting. The runtime in this project is candle, which loads the safetensors weights through memory mapping, so nothing is copied until a layer actually runs. Startup cost is dominated entirely by paging the model in on the first forward pass. Its peak memory footprint is about 340 megabytes. That is with three sentences in the batch; a long document pushes the number up. Tokenization itself is cheap, microseconds per sentence, and it happens before anything reaches the model. Embedding a whole document at once is the mistake this step fixes: the vector averages every topic together and retrieval degrades quietly.",
    "Chunking is where retrieval quality is decided. Windows of thirty words with a six-word overlap worked well for short technical prose: small enough that one vector describes one idea, overlapped just enough that a sentence split across a boundary still lands whole in at least one chunk. Bigger windows average more topics into each vector and recall suffers quietly. Smaller windows fragment the answer across chunks and the prompt fills with fragments. No window size is right for every corpus; the overlap and the stride are a trade to measure, not a default to trust.",
    "Tokenization happens before anything reaches the model, and it is the cheap part of the pipeline: microseconds per sentence on CPU. The tokenizer splits text into word pieces drawn from a fixed vocabulary of about thirty thousand entries, and the count it reports is the number of positions the model actually sees. A batch of sentences is padded to its longest member, and the attention mask marks the padding so the pooled embedding ignores it. That count is also the only honest way to budget a context window, which is what this step uses it for.",
];

/// How wide the net is cast, how close two chunks must be before the
/// second counts as a duplicate of the first, and how many tokens the
/// context window may hold. Same dials as step 6.
const CANDIDATES: usize = 15;
const DUP_THRESHOLD: f32 = 0.98;
const CONTEXT_TOKENS: usize = 100;

/// Index size (clusters) and how many of them a query visits.
const K: usize = 8;
const PROBE: usize = 2;
/// Synthetic near-copies per real chunk, as in step 6.
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
/// because the dedupe pass needs it.
#[derive(Clone)]
struct Hit {
    score: f32,
    text: String,
    vector: Vec<f32>,
}

/// The token count under the model's own tokenizer.
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

/// True when `v` is within `threshold` cosine of anything already
/// kept. The threshold is deliberately generous: it exists to catch
/// near-copies, not related chunks.
fn is_duplicate(v: &[f32], kept: &[Vec<f32>], threshold: f32) -> bool {
    kept.iter().any(|k| cosine(v, k) > threshold)
}

/// Walk the ranked candidates, keep the first of every distinct chunk,
/// and stop at the first passage that no longer fits the budget.
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

/// The prompt the answer model will respond to: an instruction, the
/// numbered passages, and the question last.
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

/// State shared by every request: the store and its index (steps 3-5),
/// plus what it takes to embed a question on the way in. The SQLite
/// connection is behind a mutex because `Connection` is not `Sync`.
struct App {
    conn: Mutex<Connection>,
    model: BertModel,
    device: Device,
    tokenizer: Tokenizer,
}

/// The request body.
#[derive(Deserialize)]
struct Ask {
    question: String,
}

async fn ask(
    State(app): State<Arc<App>>,
    Json(req): Json<Ask>,
) -> Sse<KeepAliveStream<ReceiverStream<Result<Event, Infallible>>>> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(32);
    tokio::spawn(relay(app, req.question, tx));
    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default())
}

/// The request pipeline, off the request task entirely. Retrieval runs
/// on the blocking pool - it is CPU work against a mutex-guarded
/// connection - and ollama's token stream is relayed from the async
/// side. The client sees the passages first, then the tokens.
async fn relay(
    app: Arc<App>,
    question: String,
    tx: tokio::sync::mpsc::Sender<Result<Event, Infallible>>,
) {
    let embedder = app.clone();
    let work = tokio::task::spawn_blocking(move || -> Result<(Vec<Hit>, usize, String)> {
        let conn = embedder.conn.lock().unwrap();
        let q = embed(
            &embedder.model,
            &embedder.device,
            &embedder.tokenizer,
            &[question.clone()],
        )?
        .remove(0);
        let (_, _, hits) = search_index(&conn, &q, PROBE, CANDIDATES)?;
        let (kept, _, _) = select(&hits, &embedder.tokenizer, DUP_THRESHOLD, CONTEXT_TOKENS);
        let prompt = build_prompt(
            &kept.iter().map(|h| h.text.clone()).collect::<Vec<_>>(),
            &question,
        );
        let tokens = token_count(&embedder.tokenizer, &prompt);
        Ok((kept, tokens, prompt))
    });

    let (kept, prompt_tokens, prompt) = match work.await {
        Ok(Ok(x)) => x,
        Ok(Err(e)) => {
            send_error(&tx, format!("{e}")).await;
            return;
        }
        Err(e) => {
            send_error(&tx, format!("{e}")).await;
            return;
        }
    };

    // The retrieved chunks are named first, one event each, so a client
    // can show its sources while the answer is still being written.
    if !send_passages(&kept, &app.tokenizer, &tx).await {
        return; // client went away
    }

    let body = json!({
        "model": ANSWER_MODEL,
        "prompt": prompt,
        "stream": true,
        // Greedy decoding: a factual question over retrieved passages
        // has one best answer, and sampling at the default temperature
        // makes two identical requests disagree.
        "options": { "num_predict": MAX_ANSWER_TOKENS, "temperature": 0.0 },
    });
    let response = match reqwest::Client::new().post(OLLAMA_URL).json(&body).send().await {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            send_error(&tx, format!("ollama returned {}", r.status())).await;
            return;
        }
        Err(e) => {
            send_error(&tx, format!("ollama unreachable: {e}")).await;
            return;
        }
    };

    // Ollama streams newline-delimited JSON; relay_lines turns each
    // complete line into an SSE event.
    let stream = response.bytes_stream();
    relay_lines(stream, &tx, prompt_tokens).await;
}

/// The retrieved chunks are named first, one event each, so a client
/// can show its sources while the answer is still being written.
/// Returns false when the client has gone away.
async fn send_passages(
    kept: &[Hit],
    tokenizer: &Tokenizer,
    tx: &tokio::sync::mpsc::Sender<Result<Event, Infallible>>,
) -> bool {
    for (i, h) in kept.iter().enumerate() {
        let passage = json!({
            "rank": i + 1,
            "score": h.score,
            "tokens": token_count(tokenizer, &h.text),
            "text": h.text,
        });
        if tx
            .send(Ok(Event::default().event("passage").data(passage.to_string())))
            .await
            .is_err()
        {
            return false;
        }
    }
    true
}

/// Consume ollama's newline-delimited JSON stream and relay each
/// complete line as an SSE event. Each line carries one token in its
/// "response" field; the line with "done": true carries the final
/// counts and ends the answer.
async fn relay_lines(
    mut stream: impl futures_util::Stream<Item = reqwest::Result<bytes::Bytes>> + Unpin,
    tx: &tokio::sync::mpsc::Sender<Result<Event, Infallible>>,
    prompt_tokens: usize,
) {
    let mut buf = String::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => buf.push_str(&String::from_utf8_lossy(&bytes)),
            Err(e) => {
                send_error(tx, format!("{e}")).await;
                return;
            }
        }
        while let Some(pos) = buf.find('\n') {
            let line: String = buf.drain(..=pos).collect();
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(token) = v.get("response").and_then(|r| r.as_str()) {
                if !token.is_empty() {
                    let ev = Event::default().event("token").data(token);
                    if tx.send(Ok(ev)).await.is_err() {
                        return; // client went away
                    }
                }
            }
            if v.get("done").and_then(|d| d.as_bool()) == Some(true) {
                let summary = json!({
                    "prompt_tokens": prompt_tokens,
                    "answer_tokens": v.get("eval_count"),
                });
                let _ = tx
                    .send(Ok(Event::default().event("done").data(summary.to_string())))
                    .await;
                return;
            }
        }
    }
}

async fn send_error(tx: &tokio::sync::mpsc::Sender<Result<Event, Infallible>>, msg: String) {
    let msg = msg.replace('\n', " ");
    let _ = tx
        .send(Ok(Event::default().event("error").data(msg)))
        .await;
}

#[tokio::main]
async fn main() -> Result<()> {
    let device = Device::Cpu;
    let started = std::time::Instant::now();

    let api = Api::new()?;
    let repo = api.model(MODEL_ID.to_string());
    let config: Config = serde_json::from_str(&std::fs::read_to_string(repo.get("config.json")?)?)?;
    let mut tokenizer =
        Tokenizer::from_file(repo.get("tokenizer.json")?).map_err(|e| anyhow::anyhow!("{e}"))?;
    // The published tokenizer.json ships with fixed padding to 128;
    // counting needs it off, as step 6 found.
    tokenizer.with_padding(None);
    let weights = repo.get("model.safetensors")?;
    let vb = unsafe { VarBuilder::from_mmaped_safetensors(&[weights], DTYPE, &device)? };
    let model = BertModel::load(vb, &config)?;
    println!("model loaded in {:.1?} on {device:?}", started.elapsed());

    // Same chunker as steps 4-6: 30-word windows, 6-word overlap.
    let mut chunks: Vec<(usize, String)> = Vec::new();
    for (d, doc) in DOCS.iter().enumerate() {
        for c in chunk(doc, 30, 24) {
            chunks.push((d, c));
        }
    }

    // The store lives in the temp directory; delete any copy from an
    // earlier run so startup always begins from an empty database.
    let db_path = std::env::temp_dir().join("llm-step7-vectors.db");
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

    let texts: Vec<String> = chunks.iter().map(|(_, c)| c.clone()).collect();
    let vectors = embed(&model, &device, &tokenizer, &texts)?;

    let real = chunks.len();
    for (i, v) in vectors.iter().enumerate() {
        conn.execute(
            "INSERT INTO chunks (id, text) VALUES (?1, ?2)",
            params![i as i64, &chunks[i].1],
        )?;
        conn.execute(
            "INSERT INTO vectors (chunk_id, embedding) VALUES (?1, ?2)",
            params![i as i64, to_blob(v)],
        )?;
    }
    // Near-copies of every chunk, as in step 6.
    for (i, v) in vectors.iter().enumerate() {
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

    // The index from step 5, built the same way.
    let mut stmt = conn.prepare("SELECT chunk_id, embedding FROM vectors")?;
    let mut rows = stmt.query([])?;
    let mut ids: Vec<i64> = Vec::new();
    let mut all: Vec<Vec<f32>> = Vec::new();
    while let Some(row) = rows.next()? {
        ids.push(row.get(0)?);
        all.push(from_blob(&row.get::<_, Vec<u8>>(1)?));
    }
    // Both must drop before `conn` is moved into the shared state.
    drop(rows);
    drop(stmt);
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

    let port: u16 = match std::env::args().nth(1) {
        Some(p) => p.parse()?,
        None => DEFAULT_PORT,
    };
    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let app = Arc::new(App {
        conn: Mutex::new(conn),
        model,
        device,
        tokenizer,
    });
    let router = Router::new().route("/ask", post(ask)).with_state(app);
    println!("ready: POST http://{addr}/ask with {{\"question\": \"...\"}}");
    axum::serve(listener, router).await?;
    Ok(())
}
