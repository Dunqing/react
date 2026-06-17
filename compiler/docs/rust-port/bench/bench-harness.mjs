/**
 * Shared warm-loop benchmark harness. Mirrors the Rust `run_bench`
 * (react_compiler_e2e_cli/src/main.rs) methodology EXACTLY so the JS-side
 * numbers are apples-to-apples with the native `--bench`:
 *   - collect all `.js` fixtures recursively, EXCLUDING `*.flow.js`, sorted
 *   - read all sources up front (IO is OUTSIDE the timed region)
 *   - one (or more) warmup pass(es), discarded; also counts compiled-to-code
 *   - N timed passes over the whole corpus; report median/min/max per-iteration
 *   - per-fixture median = median_iteration_total / n
 *   - single-threaded; compilationMode 'all' is the engine's responsibility
 *
 * The only intentional deviation from the Rust harness: more warmup passes,
 * because V8 needs to JIT the Babel/compiler hot paths. Extra warmup makes the
 * JS side look its BEST — the conservative choice that won't inflate the
 * native-vs-JS speedup.
 */
import fs from 'node:fs';
import path from 'node:path';

/** Recursively collect `.js` fixtures (excluding `*.flow.js`), sorted by path. */
export function collectFixtures(dir) {
  const out = [];
  function walk(d) {
    const entries = fs
      .readdirSync(d, {withFileTypes: true})
      .sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
    for (const e of entries) {
      const p = path.join(d, e.name);
      if (e.isDirectory()) walk(p);
      else if (e.name.endsWith('.js') && !e.name.endsWith('.flow.js')) out.push(p);
    }
  }
  walk(dir);
  out.sort();
  return out;
}

/**
 * Run the warm-loop benchmark.
 * @param label    human label for the run
 * @param compileOne (source, filename) => boolean   // true if it produced code
 * @param fixturesDir corpus root
 * @param opts {warmup=3, iters=8}
 */
export function runBench(label, compileOne, fixturesDir, {warmup = 3, iters = 8} = {}) {
  let paths = collectFixtures(fixturesDir);

  // Optional cross-engine intersection filter (env BENCH_FILTER=<listfile>).
  if (process.env.BENCH_FILTER) {
    const allowed = new Set(
      fs
        .readFileSync(process.env.BENCH_FILTER, 'utf8')
        .split('\n')
        .map(l => l.trim())
        .filter(Boolean),
    );
    paths = paths.filter(p => allowed.has(p));
  }

  const fixtures = paths.map(p => ({filename: p, source: fs.readFileSync(p, 'utf8')}));
  const n = fixtures.length;
  if (n === 0) throw new Error(`no fixtures under ${fixturesDir}`);

  // Warmup passes (discarded). Count compiled-to-code on the last warmup pass,
  // and optionally dump the compiled paths (env BENCH_DUMP=<file>).
  let compiledCount = 0;
  const compiledPaths = [];
  for (let w = 0; w < warmup; w++) {
    compiledCount = 0;
    compiledPaths.length = 0;
    for (const f of fixtures) {
      try {
        if (compileOne(f.source, f.filename)) {
          compiledCount++;
          if (process.env.BENCH_DUMP) compiledPaths.push(f.filename);
        }
      } catch {
        /* errors are part of the pipeline cost; counted as not-compiled */
      }
    }
  }
  if (process.env.BENCH_DUMP) {
    fs.writeFileSync(process.env.BENCH_DUMP, compiledPaths.join('\n'));
    console.error(`wrote ${compiledPaths.length} compiled paths to ${process.env.BENCH_DUMP}`);
  }

  // Timed iterations.
  const secs = [];
  for (let i = 0; i < iters; i++) {
    const t0 = process.hrtime.bigint();
    let sink = 0;
    for (const f of fixtures) {
      try {
        if (compileOne(f.source, f.filename)) sink++;
      } catch {
        /* ignore */
      }
    }
    const t1 = process.hrtime.bigint();
    secs.push(Number(t1 - t0) / 1e9);
    globalThis.__benchSink = sink;
  }

  secs.sort((a, b) => a - b);
  const median = secs[secs.length >> 1];
  const min = secs[0];
  const max = secs[secs.length - 1];
  const perFixtureMs = (median / n) * 1000;
  const fps = n / median;

  console.log(`=== ${label} ===`);
  console.log(`fixtures:            ${n}`);
  console.log(`compiled to code:    ${compiledCount}`);
  console.log(`warmup passes:       ${warmup}`);
  console.log(`iterations (timed):  ${iters}`);
  console.log(
    `per-iteration total: median ${median.toFixed(4)}s  min ${min.toFixed(4)}s  max ${max.toFixed(4)}s`,
  );
  console.log(`fixtures/sec:        ${fps.toFixed(1)}`);
  console.log(`per-fixture median:  ${perFixtureMs.toFixed(4)} ms`);
  return {n, compiledCount, median, min, max, perFixtureMs, fps};
}
