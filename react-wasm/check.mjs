// Compares the JavaScript fuzzyScore from step 1 with the Rust fuzzy_score
// in pkg/. Run from react-wasm/ after `wasm-pack build --target web`:
//
//   node check.mjs
//
// Every case is a real item from the step 1 dataset, which is generated
// deterministically, so these strings are the same on every machine.
import { readFile } from 'node:fs/promises';

const wasm = await import('./pkg/fuzzy_search.js');
wasm.initSync({ module: await readFile('./pkg/fuzzy_search_bg.wasm') });

// The simple functions first — watch the types come back. Note the `n` on
// expand's argument: u64 arrives as BigInt, and a plain number is rejected.
console.log('add(2, 3)       ->', wasm.add(2, 3));
console.log('expand(12_345n) ->', wasm.expand(12_345n));
console.log('shout("hello")  ->', wasm.shout('hello'));

// The JavaScript original, copied from step 1.
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

const cases = [
  ['bracket', 'steel bracket #0'],
  ['steel bracket', 'steel bracket #0'],
  ['nylon pulley for tension', 'nylon pulley for tension, rated 133 kg, batch 9273'],
  ['pulley', 'brass pulley #31'],
  ['gear', 'steel bracket #0'],
  ['brak', 'steel bracket #0'],
];

let mismatches = 0;
for (const [query, text] of cases) {
  const js = fuzzyScore(query, text);
  const rust = wasm.fuzzy_score(query, text);
  if (js !== rust) mismatches++;
  console.log(
    `${JSON.stringify(query).padEnd(28)} js ${String(js).padStart(3)}  rust ${String(rust).padStart(3)}${js === rust ? '' : '   <-- MISMATCH'}`,
  );
}
console.log(mismatches === 0 ? 'all scores agree' : `${mismatches} mismatches`);
