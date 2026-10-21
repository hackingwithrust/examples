// The worker owns the wasm module and the dataset. The main thread never
// touches either: it posts { id, query, backend } and gets { id, hits, ms }.
// Messages posted while the engine is still starting queue until module
// evaluation finishes, so the first keystroke never gets lost.
import { createEngine } from './engine.js';

const engine = await createEngine();

self.onmessage = (event) => {
  const { id, query, backend } = event.data;
  const t0 = performance.now();
  const hits = backend === 'js' ? engine.searchJS(query) : engine.searchWasm(query);
  const ms = performance.now() - t0;
  self.postMessage({ id, hits, ms });
};
