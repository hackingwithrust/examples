// Compares the wasm search — dataset loaded once into wasm memory, one
// crossing per keystroke — against the all-JavaScript search from step 1,
// and shows the zero-copy view of wasm memory the loaded corpus leaves
// behind. Run from react-wasm/ after `wasm-pack build --target web`:
//
//   node check-search.mjs
//
// The dataset is the step 1 generator, deterministic, so every reader works
// through the same items and the byte offsets below are the same too.
import { readFile } from 'node:fs/promises';

const wasm = await import('./pkg/fuzzy_search.js');
// initSync returns the raw exports, and memory is on that object, not on
// the module namespace.
const instance = wasm.initSync({ module: await readFile('./pkg/fuzzy_search_bg.wasm') });
const memory = instance.memory;

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

// The corpus format load_dataset expects: one line per item, title and
// description separated by a tab.
const corpus = items.map((item) => `${item.title}\t${item.description}`).join('\n');

// A view created before the corpus exists...
const before = memory.buffer.byteLength;
const stale = new Uint8Array(memory.buffer);
// ...does not survive the growth that loading 6 MB causes.
const n = wasm.load_dataset(corpus);
const after = memory.buffer.byteLength;

console.log(`loaded ${n} items, corpus ${corpus.length.toLocaleString('en-US')} bytes`);
console.log(`memory: ${before.toLocaleString('en-US')} -> ${after.toLocaleString('en-US')} bytes`);
console.log(`the view made before loading: length ${stale.length}`);

// A fresh view over the grown buffer. Nothing was copied to make it: it
// points into wasm memory, the same bytes the Rust side scans.
const view = new Uint8Array(memory.buffer);

// The corpus is findable in the view without a single function call. Scan
// for a real description, then decode the line it sits on, straight from
// wasm memory.
const needle = new TextEncoder().encode('nylon pulley for tension');
let at = -1;
outer: for (let i = 0; i <= view.length - needle.length; i++) {
  for (let j = 0; j < needle.length; j++) {
    if (view[i + j] !== needle[j]) continue outer;
  }
  at = i;
  break;
}
let lineEnd = at;
while (view[lineEnd] !== 10) lineEnd++; // the byte after the newline (10) ends the line
console.log(`"nylon pulley for tension" first in wasm memory at byte ${at.toLocaleString('en-US')}`);
console.log(`decoded from the view: "${new TextDecoder().decode(view.subarray(at, lineEnd))}"`);

// The all-JavaScript search from step 1, for comparison.
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

// The wasm search: the query crosses once, packed index/score pairs come
// back, and the titles are looked up in the JavaScript copy for display.
function searchWasm(query) {
  const flat = wasm.search(query, 20);
  const hits = [];
  for (let i = 0; i < flat.length; i += 2) {
    hits.push({ title: items[flat[i]].title, score: flat[i + 1] });
  }
  return hits;
}

const queries = ['bracket', 'steel bracket', 'nylon pulley for tension'];
let mismatches = 0;
for (const q of queries) {
  const js = searchJS(q);
  const ws = searchWasm(q);
  const same =
    js.length === ws.length && js.every((h, i) => h.title === ws[i].title && h.score === ws[i].score);
  if (!same) mismatches++;
  console.log(
    `"${q}": ${ws.length} hits, top "${ws[0].title}" score ${ws[0].score}${same ? '' : ' -- MISMATCH'}`,
  );
}
console.log(mismatches === 0 ? 'all top-20 lists agree' : `${mismatches} mismatches`);
