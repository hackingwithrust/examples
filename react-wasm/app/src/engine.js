// One engine per JavaScript context: the worker holds one, and the main thread
// builds one only if a search is asked to run there. Each engine owns its own
// wasm module instance and its own copy of the corpus inside that instance's
// memory — two engines means the 7.2 MB corpus crosses the boundary twice.
import init, { load_dataset, search } from '../../pkg/fuzzy_search.js';
import { buildItems, fuzzyScore } from './dataset.mjs';

export async function createEngine() {
  await init();
  const items = buildItems();
  const corpus = items.map((item) => `${item.title}\t${item.description}`).join('\n');
  load_dataset(corpus);

  // The step 1 search, for the JavaScript column of the comparison.
  function searchJS(query) {
    const hits = [];
    for (const item of items) {
      const score = Math.max(fuzzyScore(query, item.title), fuzzyScore(query, item.description));
      if (score >= 0) hits.push({ title: item.title, score });
    }
    hits.sort((a, b) => b.score - a.score);
    return hits.slice(0, 20);
  }

  // The wasm search: packed index/score pairs come back, the titles never
  // cross the boundary — this engine's items array is the display copy.
  function searchWasm(query) {
    const flat = search(query, 20);
    const hits = [];
    for (let i = 0; i < flat.length; i += 2) {
      hits.push({ title: items[flat[i]].title, score: flat[i + 1] });
    }
    return hits;
  }

  return { items, searchJS, searchWasm };
}
