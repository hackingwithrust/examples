// tradeoffs.mjs — the three costs the closing article puts numbers on.
//
//   node tradeoffs.mjs boundary       — the fixed toll of one wasm call
//   node tradeoffs.mjs small <N>      — JS vs wasm over an N-item dataset
//   node tradeoffs.mjs peritem        — the toll paid once per item, the
//                                       design wasm cannot save
//
// "small" takes the size as an argument because load_dataset can only be
// called once per process (the DATASET static is a OnceLock), so each
// dataset size gets a fresh process. Like bench.mjs: one cold run, two
// warm-ups, then seven timed runs, median of the seven reported.

import { readFile } from 'node:fs/promises';

const wasm = await import('./pkg/fuzzy_search.js');
wasm.initSync({ module: await readFile('./pkg/fuzzy_search_bg.wasm') });

const MATERIALS = ['steel', 'brass', 'carbon', 'nylon', 'ceramic', 'alloy'];
const KINDS = ['bracket', 'spacer', 'gear', 'clamp', 'hinge', 'pulley', 'bearing', 'fastener'];
const USES = ['damping', 'mounting', 'rotation', 'sealing', 'support', 'tension'];

// The same generator every step used, cut off at N items.
function buildItems(n) {
  const items = [];
  for (let i = 0; i < n; i++) {
    const material = MATERIALS[i % 6];
    const kind = KINDS[Math.floor(i / 6) % 8];
    const use = USES[Math.floor(i / 48) % 6];
    items.push({
      title: `${material} ${kind} #${i}`,
      description: `${material} ${kind} for ${use}, rated ${100 + (i % 40)} kg, batch ${9000 + (i % 900)}`,
    });
  }
  return items;
}

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
      score += 2 + streak;
    } else {
      streak = 0;
    }
  }
  if (qi < q.length) return -1;
  return score;
}

function timeOne(fn) {
  const t0 = performance.now();
  fn();
  return performance.now() - t0;
}

function medianOf7(fn) {
  timeOne(fn); // cold
  timeOne(fn); // warm-up
  timeOne(fn);
  const times = [];
  for (let i = 0; i < 7; i++) times.push(timeOne(fn));
  times.sort((a, b) => a - b);
  return times[3];
}

if (process.argv[2] === 'boundary') {
  // One million calls of the smallest possible export, against the same
  // function written in JavaScript. The difference is the toll: argument
  // conversion, the call into the instance, the result crossing back.
  const addJS = (a, b) => a + b;
  const addWasm = wasm.add;
  const N = 1_000_000;
  let sink = 0;
  const run = (fn) => {
    const t0 = performance.now();
    for (let i = 0; i < N; i++) sink += fn(2, 3);
    return performance.now() - t0;
  };
  run(addJS); // tier up first; only the second run of each is reported
  const js = run(addJS);
  run(addWasm);
  const wasmMs = run(addWasm);
  console.log(`${N} calls of add(2, 3):`);
  console.log(`  JavaScript: ${js.toFixed(1)} ms total, ${(js * 1e6 / N).toFixed(1)} ns per call`);
  console.log(`  wasm:       ${wasmMs.toFixed(1)} ms total, ${(wasmMs * 1e6 / N).toFixed(1)} ns per call`);
  if (sink === -1) console.log('unreachable');
} else if (process.argv[2] === 'small') {
  const n = Number(process.argv[3]);
  if (!Number.isInteger(n) || n <= 0) {
    console.error('usage: node tradeoffs.mjs small <N>');
    process.exit(1);
  }
  const items = buildItems(n);
  const corpus = items.map((item) => `${item.title}\t${item.description}`).join('\n');
  wasm.load_dataset(corpus);

  function searchJS(query) {
    const hits = [];
    for (const item of items) {
      const score = Math.max(fuzzyScore(query, item.title), fuzzyScore(query, item.description));
      if (score >= 0) hits.push({ title: item.title, score });
    }
    hits.sort((a, b) => b.score - a.score);
    return hits.slice(0, 20);
  }

  function searchWasm(query) {
    const flat = wasm.search(query, 20);
    const hits = [];
    for (let i = 0; i < flat.length; i += 2) {
      hits.push({ title: items[flat[i]].title, score: flat[i + 1] });
    }
    return hits;
  }

  const query = 'bracket';
  const jsHits = searchJS(query);
  const wasmHits = searchWasm(query);
  const sameTop = jsHits[0]?.title === wasmHits[0]?.title;
  const jsMedian = medianOf7(() => searchJS(query));
  const wasmMedian = medianOf7(() => searchWasm(query));
  const scanOnly = wasm.bench_search(query, 200) / 200; // in-module: no boundary
  console.log(`dataset ${n} items, query "${query}", top hit ${wasmHits[0]?.title} (matches JS: ${sameTop})`);
  console.log(`  JavaScript: median ${jsMedian.toFixed(2)} ms of 7`);
  console.log(`  wasm:       median ${wasmMedian.toFixed(2)} ms of 7`);
  console.log(`  wasm scan only, inside the module: ${scanOnly.toFixed(3)} ms per call`);
} else if (process.argv[2] === 'peritem') {
  // The anti-pattern: calling wasm once per item. Same 100,000-item dataset,
  // same scoring, but the boundary is crossed 200,000 times per search —
  // every string copied in and a score copied back, item by item.
  const items = buildItems(100_000);
  const query = 'bracket';
  const searchPerItem = (score) => {
    const hits = [];
    for (const item of items) {
      const s = Math.max(score(query, item.title), score(query, item.description));
      if (s >= 0) hits.push(s);
    }
    hits.sort((a, b) => b - a);
    return hits.length;
  };
  const jsHits = searchPerItem(fuzzyScore);
  const wasmHits = searchPerItem(wasm.fuzzy_score);
  if (jsHits !== wasmHits) {
    console.error(`parity broken: JS ${jsHits} hits, wasm ${wasmHits}`);
    process.exit(1);
  }
  const jsMedian = medianOf7(() => searchPerItem(fuzzyScore));
  const wasmMedian = medianOf7(() => searchPerItem(wasm.fuzzy_score));
  console.log(`the same scan, one wasm call per item (${jsHits} hits, query "${query}"):`);
  console.log(`  JavaScript, no boundary: median ${jsMedian.toFixed(1)} ms of 7`);
  console.log(`  wasm, 200,000 calls:     median ${wasmMedian.toFixed(1)} ms of 7`);
} else {
  console.error('usage: node tradeoffs.mjs boundary | small <N> | peritem');
  process.exit(1);
}
