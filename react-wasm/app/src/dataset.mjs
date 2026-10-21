// The dataset: 100,000 catalogue items. Generated deterministically, so every
// reader works through the same list — the same generator every step used.
const MATERIALS = ['steel', 'brass', 'carbon', 'nylon', 'ceramic', 'alloy'];
const KINDS = ['bracket', 'spacer', 'gear', 'clamp', 'hinge', 'pulley', 'bearing', 'fastener'];
const USES = ['damping', 'mounting', 'rotation', 'sealing', 'support', 'tension'];

export function buildItems() {
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
  return items;
}

// Fuzzy subsequence score of query against text. -1 means no match. Step 1's
// function, unchanged.
export function fuzzyScore(query, text) {
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
