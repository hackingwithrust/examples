// bench.mjs — the honest version of the benchmark step 1 promised.
//
//   node bench.mjs
//
// Three search variants over the same 100,000-item dataset:
//   original — step 1's all-JavaScript search, unchanged
//   lowered  — the pre-folded JavaScript from step 4's profile
//   wasm     — the step 5 wasm search, dataset loaded once
// Each variant gets one cold, timed run — what a naive benchmark reports —
// then two warm-up runs, then seven timed runs. The median of the seven is
// the number this article quotes. The dataset is generated deterministically,
// so the results are the same on every machine; the times are not, which is
// exactly why they are reported the way they are.

import { readFile } from 'node:fs/promises';

const wasm = await import('./pkg/fuzzy_search.js');
wasm.initSync({ module: await readFile('./pkg/fuzzy_search_bg.wasm') });

// The dataset: 100,000 catalogue items, exactly as step 1 generated them.
const MATERIALS = ['steel', 'brass', 'carbon', 'nylon', 'ceramic', 'alloy'];
const KINDS = ['bracket', 'spacer', 'gear', 'clamp', 'hinge', 'pulley', 'bearing', 'fastener'];
const USES = ['damping', 'mounting', 'rotation', 'sealing', 'support', 'tension'];

const items = [];
for (let i = 0; i < 100_000; i++) {
  const material = MATERIALS[i % 6];
  const kind = KINDS[Math.floor(i / 6) % 8];
  const use = USES[Math.floor(i / 48) % 6];
  items.push({
    title: `${material} ${kind} #${i}`,
    description: `${material} ${kind} for ${use}, rated ${100 + (i % 40)} kg, batch ${9000 + (i % 900)}`,
  });
}

const corpus = items.map((item) => `${item.title}\t${item.description}`).join('\n');
wasm.load_dataset(corpus);

// Fuzzy subsequence score of query against text. -1 means no match. Step 1's
// function, unchanged.
function fuzzyScore(query, text) {
  const q = query.toLowerCase();
  const t = text.toLowerCase();
  let score = 0;
  let qi = 0;
  let streak = 0;
  for (let ti = 0; ti < t.length && qi < q.length; ti++) {
    if (t[ti] === q[qi]) {
      qi++;
      streak++;
      score += 2 + streak; // consecutive matches are worth more
    } else {
      streak = 0;
    }
  }
  if (qi < q.length) return -1; // ran out of text before running out of query
  return score;
}

function searchJS(query) {
  const hits = [];
  for (const item of items) {
    const score = Math.max(fuzzyScore(query, item.title), fuzzyScore(query, item.description));
    if (score >= 0) hits.push({ title: item.title, score });
  }
  hits.sort((a, b) => b.score - a.score);
  return hits.slice(0, 20);
}

// The pre-folded variant from step 4: the dataset's case folding happened
// once, above, and the query folds once per search.
const lowered = items.map((item) => ({
  title: item.title.toLowerCase(),
  description: item.description.toLowerCase(),
}));

function scoreLower(q, text) {
  let score = 0;
  let qi = 0;
  let streak = 0;
  for (let ti = 0; ti < text.length && qi < q.length; ti++) {
    if (text[ti] === q[qi]) {
      qi++;
      streak++;
      score += 2 + streak;
    } else {
      streak = 0;
    }
  }
  if (qi < q.length) return -1;
  return score;
}

function searchLowered(query) {
  const q = query.toLowerCase();
  const hits = [];
  for (const item of lowered) {
    const score = Math.max(scoreLower(q, item.title), scoreLower(q, item.description));
    if (score >= 0) hits.push({ title: item.title, score });
  }
  hits.sort((a, b) => b.score - a.score);
  return hits.slice(0, 20);
}

// The wasm search, the call the real app makes: packed pairs cross once,
// then the twenty hits are unpacked for display.
function searchWasm(query) {
  const flat = wasm.search(query, 20);
  const hits = [];
  for (let i = 0; i < flat.length; i += 2) {
    hits.push({ title: items[flat[i]].title, score: flat[i + 1] });
  }
  return hits;
}

const VARIANTS = [
  ['original JS', searchJS],
  ['lowered  JS', searchLowered],
  ['wasm', searchWasm],
];

const QUERIES = ['bracket', 'steel bracket', 'nylon pulley for tension'];

function timeOne(fn) {
  const t0 = performance.now();
  fn();
  return performance.now() - t0;
}

for (const q of QUERIES) {
  console.log(`query "${q}"`);
  for (const [label, fn] of VARIANTS) {
    const call = () => fn(q);
    const cold = timeOne(call); // timed, and reported as what it is
    timeOne(call); // warm-up: the JIT sees the function, optimizes, tiers up
    timeOne(call);
    const times = [];
    for (let i = 0; i < 7; i++) times.push(timeOne(call));
    times.sort((a, b) => a - b);
    console.log(
      `  ${label}: cold ${cold.toFixed(1)} ms, median ${times[3].toFixed(1)} ms of 7, min ${times[0].toFixed(1)} ms`,
    );
    if (q === 'bracket') {
      console.log(`    all seven: ${times.map((t) => t.toFixed(1)).join(', ')}`);
    }
  }
  // The scan alone, timed inside the module: the query crosses once, then
  // bench_search repeats the exact call a keystroke makes seven times with
  // one Instant around the loop. One warm call first — wasm tiers up too,
  // just less dramatically than JavaScript.
  wasm.bench_search(q, 1);
  const inside = wasm.bench_search(q, 7) / 7;
  console.log(`  wasm scan only, inside the module: ${inside.toFixed(2)} ms per call`);
}
