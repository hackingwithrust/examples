//! Step 3: real embeddings, and the distance between them.
//!
//! Loads `all-MiniLM-L6-v2` (a 384-dimension sentence encoder), embeds three
//! sentences, and prints the cosine similarity between every pair - the
//! retrieval primitive the whole track is built on.

use anyhow::Result;
use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config, DTYPE};
use hf_hub::api::sync::Api;
use tokenizers::Tokenizer;

const MODEL_ID: &str = "sentence-transformers/all-MiniLM-L6-v2";

const SENTENCES: [&str; 3] = [
    "How do I run a language model on my own machine?",
    "What is the fastest way to host a model locally?",
    "Add a new column to an existing SQLite table.",
];

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

    // Tokenize all three sentences into one padded batch.
    let encodings = tokenizer
        .encode_batch(SENTENCES.to_vec(), true)
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
    let types = vec![vec![0u32; len]; SENTENCES.len()];

    let ids = Tensor::new(token_ids, &device)?;
    let mask = Tensor::new(attention.clone(), &device)?;
    let types = Tensor::new(types, &device)?;

    // Forward pass: one embedding vector per token, [batch, seq, 384].
    let hidden = model.forward(&ids, &types, Some(&mask))?.to_vec3::<f32>()?;

    // Mean-pool the token vectors over real tokens (padding contributes
    // nothing): one plain Vec<f32> per sentence.
    fn sentence_vector(tokens: &[Vec<f32>], mask: &[u32]) -> Vec<f32> {
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
    }
    let vectors: Vec<Vec<f32>> = hidden
        .iter()
        .zip(attention.iter())
        .map(|(tokens, mask)| sentence_vector(tokens, mask))
        .collect();

    let (rows, dim) = (vectors.len(), vectors[0].len());
    println!("each sentence is now {dim} numbers; sentence 0 starts with:");
    for v in vectors[0].iter().take(4) {
        println!("  {v:.6}");
    }

    // Cosine similarity: dot product divided by both lengths. The vectors
    // stay ordinary floats from here on - no more tensors.
    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
        let len_a = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let len_b = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        dot / (len_a * len_b)
    }
    for i in 0..rows {
        for j in (i + 1)..rows {
            println!("s{i} vs s{j}: {:.4}", cosine(&vectors[i], &vectors[j]));
        }
    }

    Ok(())
}
