import { useEffect, useRef, useState } from 'react';
import { createEngine } from './engine.js';

export default function App() {
  const [query, setQuery] = useState('');
  const [backend, setBackend] = useState('wasm');
  const [inWorker, setInWorker] = useState(true);
  const [hits, setHits] = useState([]);
  const [stats, setStats] = useState(null);
  const [frames, setFrames] = useState({ worst: 0 });

  // The worker owns the wasm module and the dataset. It starts building its
  // engine as the page loads; keystrokes typed before that queue inside the
  // worker and are answered when the engine is ready.
  const workerRef = useRef(null);
  useEffect(() => {
    const worker = new Worker(new URL('./search.worker.js', import.meta.url), {
      type: 'module',
    });
    worker.onmessage = (event) => {
      const { hits, ms } = event.data;
      setHits(hits);
      setStats({ ms, where: 'worker' });
    };
    workerRef.current = worker;
    return () => worker.terminate();
  }, []);

  // A second engine, on the main thread, built only when a search is asked to
  // run there. That is the mode this step exists to measure against.
  const mainRef = useRef(null);
  const ensureMainEngine = async () => {
    if (!mainRef.current) mainRef.current = await createEngine();
    return mainRef.current;
  };

  // The frame meter. requestAnimationFrame fires once per displayed frame, and
  // the wall clock between two calls is how long a frame really took. The
  // callback's timestamp argument is no good here: headless Chrome hands it out
  // on the display's 16.7 ms grid even when the frame ran late — a 30 ms block
  // on the main thread measured 16.6 on the argument, 30.2 on performance.now().
  // The worst gap in each five-second window is published to the page.
  useEffect(() => {
    let raf;
    let last = performance.now();
    let worst = 0;
    const tick = () => {
      const now = performance.now();
      if (now - last > worst) worst = now - last;
      last = now;
      raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    const publish = setInterval(() => {
      setFrames({ worst });
      worst = 0;
    }, 5000);
    return () => {
      cancelAnimationFrame(raf);
      clearInterval(publish);
    };
  }, []);

  async function onQuery(value) {
    setQuery(value);
    if (!value) {
      setHits([]);
      setStats(null);
      return;
    }
    if (inWorker) {
      workerRef.current?.postMessage({ id: Date.now(), query: value, backend });
    } else {
      const engine = await ensureMainEngine();
      const t0 = performance.now();
      const found = backend === 'js' ? engine.searchJS(value) : engine.searchWasm(value);
      setHits(found);
      setStats({ ms: performance.now() - t0, where: 'main thread' });
    }
  }

  return (
    <main>
      <h1>Fuzzy search</h1>
      <p>100,000 items. Type to search; the frame meter below shows what the typing costs.</p>
      <input
        autoFocus
        aria-label="search the catalogue"
        value={query}
        onChange={(e) => onQuery(e.target.value)}
      />
      <label>
        search runs in{' '}
        <select value={inWorker ? 'worker' : 'main'} onChange={(e) => setInWorker(e.target.value === 'worker')}>
          <option value="worker">a worker</option>
          <option value="main">the main thread</option>
        </select>
      </label>
      <label>
        engine{' '}
        <select value={backend} onChange={(e) => setBackend(e.target.value)}>
          <option value="wasm">Rust / wasm</option>
          <option value="js">JavaScript</option>
        </select>
      </label>
      <p className="meter">
        {stats
          ? `search: ${stats.ms.toFixed(1)} ms on the ${stats.where} · ${hits.length} hits`
          : 'type to search'}
      </p>
      <p className="meter">worst frame gap, last 5 s: {frames.worst.toFixed(1)} ms</p>
      <ol>
        {hits.map((hit) => (
          <li key={hit.title}>
            {hit.title} <span className="score">{hit.score}</span>
          </li>
        ))}
      </ol>
    </main>
  );
}
