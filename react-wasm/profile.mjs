// profile.mjs — the step 1 benchmark, in a form a profiler can attach to.
//
//   node profile.mjs                    times both search variants
//   node --prof profile.mjs original    profiles only the step 1 original
//
// "original" is step 1's code, unchanged. "lowered" is the same search with
// the dataset's case folding done once, at build time, instead of on every
// call. The dataset is generated deterministically, so the best hits are the
// same on every machine; the times are not, which is why the script reports
// a median of 7 runs.

const WHICH = process.argv[2] ?? "both";
if (WHICH !== "both" && WHICH !== "original" && WHICH !== "lowered") {
  console.error(`profile.mjs: unknown variant "${WHICH}" (use original, lowered, or nothing)`);
  process.exit(1);
}

// The dataset: 100,000 catalogue items. Generated deterministically, so every
// reader works through the same list.
const MATERIALS = ["steel", "brass", "carbon", "nylon", "ceramic", "alloy"];
const KINDS = ["bracket", "spacer", "gear", "clamp", "hinge", "pulley", "bearing", "fastener"];
const USES = ["damping", "mounting", "rotation", "sealing", "support", "tension"];

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

// Fuzzy subsequence score of query against text. -1 means no match.
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

// Runs once per keystroke in the real app.
function search(items, query) {
  const hits = [];
  for (const item of items) {
    const score = Math.max(fuzzyScore(query, item.title), fuzzyScore(query, item.description));
    if (score >= 0) hits.push({ title: item.title, score });
  }
  hits.sort((a, b) => b.score - a.score);
  return hits.slice(0, 20);
}

// The same scan, but the dataset's case folding happened once, above, and the
// query folds once per search instead of twice per call.
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

function bench(label, fn) {
  fn(); // warm-up: step 6 explains why this matters
  const times = [];
  let best = null;
  for (let i = 0; i < 7; i++) {
    const t0 = performance.now();
    const hits = fn();
    times.push(performance.now() - t0);
    best = hits[0];
  }
  times.sort((a, b) => a - b);
  console.log(`${label}: ${times[3].toFixed(1)} ms (median of 7), best "${best.title}"`);
}

const queries = ["bracket", "steel bracket", "nylon pulley for tension"];

if (WHICH === "both" || WHICH === "original") {
  for (const q of queries) bench(`original, query "${q}"`, () => search(items, q));
}
if (WHICH === "both" || WHICH === "lowered") {
  for (const q of queries) bench(`lowered,  query "${q}"`, () => searchLowered(q));
}
