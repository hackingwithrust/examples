//! Step 4: chunking documents for retrieval.
//!
//! Same model and same cosine arithmetic as step 3, but the input is a
//! whole document instead of three sentences. The document is cut into
//! fixed 30-word windows - once with no overlap, once with a 6-word
//! overlap - every chunk is embedded, and one question is scored against
//! each set. The chunk that wins changes, and that difference is what
//! overlap buys.

use anyhow::Result;
use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config, DTYPE};
use hf_hub::api::sync::Api;
use tokenizers::Tokenizer;

const MODEL_ID: &str = "sentence-transformers/all-MiniLM-L6-v2";

const DOC: &str = "Running a sentence encoder on your own machine is mostly an exercise in memory budgeting. The runtime in this project is candle, which loads the safetensors weights through memory mapping, so nothing is copied until a layer actually runs. Startup cost is dominated entirely by paging the model in on the first forward pass. Its peak memory footprint is about 340 megabytes. That is with three sentences in the batch; a long document pushes the number up. Tokenization itself is cheap, microseconds per sentence, and it happens before anything reaches the model. Embedding a whole document at once is the mistake this step fixes: the vector averages every topic together and retrieval degrades quietly.";

const QUESTION: &str = "How much memory does inference use at peak?";

/// Cut `doc` into word windows of `window` words, advancing `stride`
/// words each time. `stride == window` means adjacent chunks share
/// nothing; `stride < window` means each chunk keeps `window - stride`
/// words of the previous chunk as overlap.
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

/// The first 10 words of a chunk, for the printed table.
fn preview(s: &str) -> String {
    let words: Vec<&str> = s.split_whitespace().take(10).collect();
    let mut out = words.join(" ");
    if s.split_whitespace().count() > 10 {
        out.push_str(" ...");
    }
    out
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

    // The model's tokenizer pads every input to 128 tokens, so the real
    // length is the number of positions the attention mask marks as
    // actual tokens - not the number of ids in the encoding.
    let real_tokens = |e: &tokenizers::Encoding| e.get_attention_mask().iter().sum::<u32>();
    let doc_tokens = real_tokens(&tokenizer.encode(DOC, true).map_err(|e| anyhow::anyhow!("{e}"))?);
    println!(
        "document: {} words, {doc_tokens} tokens",
        DOC.split_whitespace().count()
    );

    for (label, stride) in [("no overlap", 30), ("6-word overlap", 24)] {
        let chunks = chunk(DOC, 30, stride);
        println!(
            "\n{label}: {} chunks of 30 words, stride {stride}",
            chunks.len()
        );
        for (i, c) in chunks.iter().enumerate() {
            let tokens = real_tokens(
                &tokenizer.encode(c.as_str(), true).map_err(|e| anyhow::anyhow!("{e}"))?,
            );
            println!("  chunk {i}: {tokens} tokens, starts {:?}", preview(c));
        }
        let mut texts = chunks.clone();
        texts.push(QUESTION.to_string());
        let vectors = embed(&model, &device, &tokenizer, &texts)?;
        let q = vectors.last().unwrap();
        let mut scored: Vec<(usize, f32)> = vectors[..vectors.len() - 1]
            .iter()
            .enumerate()
            .map(|(i, v)| (i, cosine(v, q)))
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        println!("  question: {QUESTION:?}");
        println!(
            "    best chunk {}: {:.4}, then chunk {}: {:.4}",
            scored[0].0, scored[0].1, scored[1].0, scored[1].1
        );
    }

    let whole = vec![DOC.to_string(), QUESTION.to_string()];
    let vectors = embed(&model, &device, &tokenizer, &whole)?;
    println!(
        "\nwhole document as one chunk: {:.4}",
        cosine(&vectors[0], &vectors[1])
    );

    Ok(())
}
