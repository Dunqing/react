/**
 * Verify the three before/after-Oxc benchmark pipelines (native-Oxc, pre-Oxc
 * Rust port, TS/Babel) produce SEMANTICALLY EQUIVALENT output on the
 * intersection fixtures, so the timing comparison is over equal work. Uses the
 * project's own structuralNormalize oracle (alpha-renames temporaries, masks
 * $[N]/_c(N) slot indices, etc.) — the same normalization compare-code.ts uses.
 *
 * Reads pre-Oxc + TS outputs pre-dumped by docs/rust-port/bench/dump-outputs.mjs
 * (/tmp/out-preoxc/<idx>.js, /tmp/out-ts/<idx>.js, /tmp/out-map.tsv), spawns the
 * native CLI per fixture (same `all_errors` config the oracle uses), normalizes
 * all three, and reports pairwise semantic-match rates + memo presence. See
 * docs/rust-port/bench/README.md for the full recipe.
 *
 * Run: npx tsx compiler/scripts/compare-three-pipelines.ts
 */
import {spawnSync} from 'child_process';
import fs from 'fs';
import path from 'path';
import {structuralNormalize} from './structural-normalize';

const REPO_ROOT = path.resolve(__dirname, '../..');
const CLI = path.join(REPO_ROOT, 'compiler/target/release/react-compiler-e2e');

function nativeCode(fixturePath: string, source: string): string | null {
  const firstLine = source.split('\n')[0] ?? '';
  const isScript = firstLine.includes('@script');
  const options = {
    shouldCompile: true,
    enableReanimated: false,
    isDev: false,
    compilationMode: 'all',
    panicThreshold: 'all_errors',
    __sourceCode: source,
  };
  const r = spawnSync(
    CLI,
    ['--frontend', 'oxc', '--filename', fixturePath, '--options', JSON.stringify(options), '--json'],
    {input: source, encoding: 'utf-8', timeout: 30000, maxBuffer: 64 * 1024 * 1024},
  );
  if (!r.stdout) return null;
  try {
    const env = JSON.parse(r.stdout);
    return env.code ?? null;
  } catch {
    return null;
  }
}

function hasMemo(code: string | null): boolean {
  return code != null && (code.includes('_c(') || code.includes('useMemoCache'));
}
function norm(code: string | null): string | null {
  if (code == null || code.trim() === '') return null;
  try {
    return structuralNormalize(code);
  } catch {
    return code;
  }
}

const map = fs
  .readFileSync('/tmp/out-map.tsv', 'utf-8')
  .split('\n')
  .filter(Boolean)
  .map(l => {
    const [idx, p] = l.split('\t');
    return {idx: Number(idx), p};
  });

let nT = 0, // native vs ts comparable (both non-null)
  nTmatch = 0,
  pT = 0, // preoxc vs ts
  pTmatch = 0,
  nP = 0, // native vs preoxc
  nPmatch = 0;
let nativeNull = 0, memoNative = 0, memoPre = 0, memoTs = 0, total = 0;
const ntMismatch: string[] = [];
const ptMismatch: string[] = [];

for (const {idx, p} of map) {
  total++;
  const source = fs.readFileSync(p, 'utf-8');
  const nc = nativeCode(p, source);
  if (nc == null) nativeNull++;
  const pc = fs.existsSync(`/tmp/out-preoxc/${idx}.js`)
    ? fs.readFileSync(`/tmp/out-preoxc/${idx}.js`, 'utf-8')
    : null;
  const tc = fs.existsSync(`/tmp/out-ts/${idx}.js`)
    ? fs.readFileSync(`/tmp/out-ts/${idx}.js`, 'utf-8')
    : null;
  if (hasMemo(nc)) memoNative++;
  if (hasMemo(pc)) memoPre++;
  if (hasMemo(tc)) memoTs++;

  const nn = norm(nc),
    np = norm(pc),
    nt = norm(tc);
  if (nn != null && nt != null) {
    nT++;
    if (nn === nt) nTmatch++;
    else if (ntMismatch.length < 12) ntMismatch.push(path.basename(p));
  }
  if (np != null && nt != null) {
    pT++;
    if (np === nt) pTmatch++;
    else if (ptMismatch.length < 12) ptMismatch.push(path.basename(p));
  }
  if (nn != null && np != null) {
    nP++;
    if (nn === np) nPmatch++;
  }
  if (total % 200 === 0) process.stderr.write(`  ${total}/${map.length}\r`);
}

const pct = (a: number, b: number) => (b === 0 ? 'n/a' : ((a / b) * 100).toFixed(2) + '%');
console.log(`\n=== Semantic-equivalence over ${total} intersection fixtures (structuralNormalize) ===`);
console.log(`native produced no code (unexpected): ${nativeNull}`);
console.log(`memoized output present:  native ${memoNative}  pre-Oxc ${memoPre}  TS ${memoTs}  (of ${total})`);
console.log(`native  ≡ TS:      ${nTmatch}/${nT}  ${pct(nTmatch, nT)}`);
console.log(`pre-Oxc ≡ TS:      ${pTmatch}/${pT}  ${pct(pTmatch, pT)}`);
console.log(`native  ≡ pre-Oxc: ${nPmatch}/${nP}  ${pct(nPmatch, nP)}`);
console.log(`native≠TS sample:  ${ntMismatch.join(', ') || '(none)'}`);
console.log(`preoxc≠TS sample:  ${ptMismatch.join(', ') || '(none)'}`);
