# llm-apps

A local RAG service in Rust, built one step at a time. Each article in the
[LLM apps in Rust](https://hackingwithrust.com/projects/llm-apps/) track ends
at a tag (`llm-ch02`, `llm-ch03`, ...), and each tag is a project that builds
and runs.

- `llm-ch02` — the candle runtime on CPU: a tensor in, a tensor out, no weights yet
- `llm-ch03` — all-MiniLM-L6-v2 loaded on CPU: 384-number sentence vectors and cosine similarity
