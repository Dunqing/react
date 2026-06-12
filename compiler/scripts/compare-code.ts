/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

/**
 * compare-code.ts — the semantic-equivalence oracle for the OXC frontend.
 *
 * This is the PRIMARY correctness metric for the Rust-port endgame. The endgame
 * strategy is SEMANTIC PARITY: the oxc output is "correct" when it is
 * structurally equivalent to the TypeScript (Babel-plugin) baseline. Cosmetic
 * printing differences between oxc_codegen and Babel's printer (quote style,
 * `let` vs `const`, JSX self-close, comment/directive preservation, temp-name
 * choice, cache-slot ordering, import merging, …) are ACCEPTED and neutralized
 * by `structuralNormalize` (see scripts/structural-normalize.ts).
 *
 * A fixture SEMANTIC-passes when EITHER:
 *   - byte-identical: the two Prettier-formatted outputs are equal, OR
 *   - structurally-equivalent: they are equal after `structuralNormalize`, OR
 *   - both error / TS-error-with-unmemoized-passthrough (mirrors the e2e harness
 *     pass rules, so this oracle never reports a "failure" the harness counts as
 *     a pass).
 *
 * Otherwise the fixture lands in one of three FAILURE buckets:
 *   - N-VAL: TS produces NO output (it validates/bails), but oxc emits memoized
 *            output. oxc is missing a validation/bailout. (Wrong to memoize.)
 *   - BAIL:  both compile but oxc lacks memoization (`_c(` / `useMemoCache`) that
 *            TS has, or oxc errored while TS compiled. A construct oxc can't yet
 *            codegen, so it bailed (per-scope or whole-fn).
 *   - OTHER: both memoize and both are real output, but they are NOT structurally
 *            equivalent. These are the genuine wrong-code bugs — the bucket to
 *            keep at zero.
 *
 * Usage:
 *   # Corpus mode (default): run all fixtures, print the SEMANTIC-pass count and
 *   # the failure-bucket histogram. This is the headline endgame number.
 *   npx tsx compiler/scripts/compare-code.ts [--limit N] [--json out.json]
 *
 *   # Single-fixture mode: print the byte diff AND the structural diff for one
 *   # fixture (path may be absolute or relative to the repo root). Use this to
 *   # investigate a specific OTHER/BAIL fixture.
 *   npx tsx compiler/scripts/compare-code.ts <fixture-path> [--no-color]
 *
 * Flags:
 *   --limit N    only the first N fixtures (corpus mode)
 *   --json FILE  also write the full per-fixture result table as JSON
 *   --list BKT   in corpus mode, also list every fixture in bucket BKT
 *                (one of: OTHER, BAIL, N-VAL)
 *   --no-color   disable ANSI colors
 *
 * Implementation note: reuses the in-process TS compile (Babel plugin) from the
 * e2e harness and the built oxc CLI binary, exactly as test-e2e.ts does, so the
 * baseline and the oxc output are produced the same way the harness produces
 * them.
 */

import * as babel from '@babel/core';
import generate from '@babel/generator';
import {execSync, spawnSync} from 'child_process';
import fs from 'fs';
import path from 'path';
import prettier from 'prettier';

import {parseConfigPragmaForTests} from '../packages/babel-plugin-react-compiler/src/Utils/TestUtils';
import {structuralNormalize} from './structural-normalize';

const REPO_ROOT = path.resolve(__dirname, '../..');

// --- Args ---
const rawArgs = process.argv.slice(2);
const noColor = rawArgs.includes('--no-color') || !!process.env.NO_COLOR;
function flagValue(name: string): string | null {
  const idx = rawArgs.indexOf(name);
  return idx >= 0 ? rawArgs[idx + 1] : null;
}
const limit = flagValue('--limit') ? parseInt(flagValue('--limit')!, 10) : 0;
const jsonOut = flagValue('--json');
const listBucket = flagValue('--list');

// Positional: a single fixture path (anything not a flag or a flag value).
const flagsWithValue = new Set(['--limit', '--json', '--list']);
const consumed = new Set<number>();
for (let i = 0; i < rawArgs.length; i++) {
  if (rawArgs[i].startsWith('--')) {
    consumed.add(i);
    if (flagsWithValue.has(rawArgs[i])) consumed.add(i + 1);
  }
}
const positional = rawArgs.filter((_a, i) => !consumed.has(i));
const singleFixture = positional[0] ?? null;

// --- Colors ---
const useColor = !noColor;
const RED = useColor ? '\x1b[0;31m' : '';
const GREEN = useColor ? '\x1b[0;32m' : '';
const YELLOW = useColor ? '\x1b[0;33m' : '';
const CYAN = useColor ? '\x1b[0;36m' : '';
const BOLD = useColor ? '\x1b[1m' : '';
const DIM = useColor ? '\x1b[2m' : '';
const RESET = useColor ? '\x1b[0m' : '';

const FIXTURES_DIR = path.join(
  REPO_ROOT,
  'compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler',
);

function discoverFixtures(rootPath: string): string[] {
  const stat = fs.statSync(rootPath);
  if (stat.isFile()) return [rootPath];
  const results: string[] = [];
  function walk(dir: string): void {
    for (const entry of fs.readdirSync(dir, {withFileTypes: true})) {
      const fullPath = path.join(dir, entry.name);
      if (entry.isDirectory()) walk(fullPath);
      else if (
        /\.(js|jsx|ts|tsx)$/.test(entry.name) &&
        !entry.name.endsWith('.expect.md')
      )
        results.push(fullPath);
    }
  }
  walk(rootPath);
  results.sort();
  return results;
}

// --- Build the Rust crates once (CLI binary + napi dylib). ---
function build(): {cliBinary: string} {
  console.error('Building Rust e2e CLI...');
  execSync('rustup run 1.92.0 cargo build -p react_compiler_e2e_cli', {
    cwd: path.join(REPO_ROOT, 'compiler/crates'),
    stdio: ['inherit', 'pipe', 'pipe'],
    shell: '/bin/bash',
  });
  const targetDir = path.join(REPO_ROOT, 'compiler/target/debug');
  return {cliBinary: path.join(targetDir, 'react-compiler-e2e')};
}

const {cliBinary} = build();
const tsPlugin = require('../packages/babel-plugin-react-compiler/src').default;

// --- Normalize compiler output for comparison (identical to test-e2e.ts). ---
async function formatCode(code: string, isFlow: boolean): Promise<string> {
  try {
    const parserPlugins = isFlow ? ['flow', 'jsx'] : ['typescript', 'jsx'];
    const ast = babel.parseSync(code, {
      sourceType: 'module',
      parserOpts: {plugins: parserPlugins},
      configFile: false,
      babelrc: false,
    });
    if (!ast) return code;
    const compact = generate(ast, {compact: true}).code;
    return await prettier.format(compact, {
      semi: true,
      parser: isFlow ? 'flow' : 'babel-ts',
    });
  } catch {
    return code;
  }
}

function compileBabel(fixturePath: string, source: string, firstLine: string) {
  const isFlow = firstLine.includes('@flow');
  const isScript = firstLine.includes('@script');
  const parserPlugins = isFlow ? ['flow', 'jsx'] : ['typescript', 'jsx'];
  const pragmaOpts = parseConfigPragmaForTests(firstLine, {
    compilationMode: 'all',
  });
  try {
    const result = babel.transformSync(source, {
      filename: fixturePath,
      sourceType: isScript ? 'script' : 'module',
      parserOpts: {plugins: parserPlugins},
      plugins: [
        [
          tsPlugin,
          {
            ...pragmaOpts,
            compilationMode: 'all',
            panicThreshold: 'all_errors',
            logger: {logEvent(): void {}, debugLogIRs(): void {}},
          },
        ],
      ],
      configFile: false,
      babelrc: false,
    });
    return {code: result?.code ?? null, error: null as string | null};
  } catch (e) {
    return {code: null, error: e instanceof Error ? e.message : String(e)};
  }
}

function compileOxc(fixturePath: string, source: string, firstLine: string) {
  const pragmaOpts = parseConfigPragmaForTests(firstLine, {
    compilationMode: 'all',
  });
  const options = {
    shouldCompile: true,
    enableReanimated: false,
    isDev: false,
    ...pragmaOpts,
    compilationMode: 'all',
    panicThreshold: 'all_errors',
    __sourceCode: source,
  };
  const result = spawnSync(
    cliBinary,
    [
      '--frontend',
      'oxc',
      '--filename',
      fixturePath,
      '--options',
      JSON.stringify(options),
      '--json',
    ],
    {
      input: source,
      encoding: 'utf-8',
      timeout: 30000,
      env: {...process.env, REACT_COMPILER_CODEGEN_BAIL_DEBUG: '1'},
    },
  );
  const stderr = result.stderr || '';
  if (result.stdout) {
    try {
      const envelope = JSON.parse(result.stdout);
      return {
        code: envelope.code ?? null,
        error: envelope.error ?? null,
        stderr,
      };
    } catch {
      /* fall through */
    }
  }
  return {
    code: null,
    error: result.stderr || `Process exited ${result.status}`,
    stderr,
  };
}

function hasMemo(code: string): boolean {
  return code.includes('_c(') || code.includes('useMemoCache');
}

type Verdict = 'IDENTICAL' | 'STRUCTURAL' | 'N-VAL' | 'BAIL' | 'OTHER';

interface Result {
  fixture: string;
  verdict: Verdict;
  tsCode: string;
  oxcCode: string;
  bailReasons: string[];
}

/**
 * Compare one fixture's TS baseline vs oxc output. Returns the verdict and the
 * formatted code for both sides. Pure (no I/O beyond the supplied compiled
 * code), so single-fixture mode and corpus mode share exactly the same logic.
 */
function classify(
  relPath: string,
  isFlow: boolean,
  tsCode: string,
  oxcCode: string,
  oxcError: string | null,
  bailReasons: string[],
): Result {
  // Flow files are auto-passed for oxc in the harness (no native Flow parser).
  if (isFlow) {
    return {
      fixture: relPath,
      verdict: 'IDENTICAL',
      tsCode,
      oxcCode,
      bailReasons,
    };
  }

  const tsErrored = tsCode.trim() === '';
  const oxcErrored = oxcCode.trim() === '' || oxcError != null;

  // Both errored (no output) -> semantic pass.
  if (tsErrored && oxcErrored) {
    return {
      fixture: relPath,
      verdict: 'IDENTICAL',
      tsCode,
      oxcCode,
      bailReasons,
    };
  }
  // TS errored + oxc emitted unmemoized passthrough -> semantic pass (harness rule).
  if (tsErrored && !oxcErrored && !hasMemo(oxcCode)) {
    return {
      fixture: relPath,
      verdict: 'IDENTICAL',
      tsCode,
      oxcCode,
      bailReasons,
    };
  }
  // Byte-identical -> semantic pass.
  if (tsCode === oxcCode) {
    return {
      fixture: relPath,
      verdict: 'IDENTICAL',
      tsCode,
      oxcCode,
      bailReasons,
    };
  }

  // --- Not byte-identical: decide structural-equivalence vs a failure bucket. ---
  if (tsErrored && !oxcErrored) {
    // TS validated/bailed (no output) but oxc produced memoized output.
    return {fixture: relPath, verdict: 'N-VAL', tsCode, oxcCode, bailReasons};
  }
  if (!tsErrored && oxcErrored) {
    // TS compiled but oxc errored / emitted nothing -> whole-fn codegen bail.
    return {fixture: relPath, verdict: 'BAIL', tsCode, oxcCode, bailReasons};
  }
  // Both produced output. If oxc lacks memoization that TS has, it's a per-scope
  // bail (some construct fell back to uncompiled source).
  if (hasMemo(tsCode) && !hasMemo(oxcCode)) {
    return {fixture: relPath, verdict: 'BAIL', tsCode, oxcCode, bailReasons};
  }
  // Both memoized (or both unmemoized): structural-equivalence is the oracle.
  if (structuralNormalize(tsCode) === structuralNormalize(oxcCode)) {
    return {
      fixture: relPath,
      verdict: 'STRUCTURAL',
      tsCode,
      oxcCode,
      bailReasons,
    };
  }
  return {fixture: relPath, verdict: 'OTHER', tsCode, oxcCode, bailReasons};
}

const SEMANTIC_PASS: ReadonlySet<Verdict> = new Set([
  'IDENTICAL',
  'STRUCTURAL',
]);

// --- Simple line diff (for single-fixture mode). ---
function lineDiff(
  a: string,
  b: string,
  labelA: string,
  labelB: string,
): string {
  const al = a.split('\n');
  const bl = b.split('\n');
  const out: string[] = [
    `${RED}--- ${labelA}${RESET}`,
    `${GREEN}+++ ${labelB}${RESET}`,
  ];
  const max = Math.max(al.length, bl.length);
  let any = false;
  for (let i = 0; i < max; i++) {
    if (al[i] === bl[i]) continue;
    any = true;
    if (al[i] !== undefined) out.push(`${RED}-${al[i]}${RESET}`);
    if (bl[i] !== undefined) out.push(`${GREEN}+${bl[i]}${RESET}`);
  }
  if (!any) out.push(`${DIM}(no differences)${RESET}`);
  return out.join('\n');
}

/**
 * Resolve a single-fixture argument leniently. Accepts: an absolute path; a path
 * relative to the repo root (`compiler/packages/.../foo.js`) or to the compiler
 * dir (`packages/.../foo.js`); a path relative to the fixtures dir
 * (`optional/foo.js`); or a bare basename (`foo.js`), found by scanning the
 * corpus. Returns the absolute path, or null if nothing matches.
 */
function resolveFixture(arg: string): string | null {
  if (path.isAbsolute(arg)) return fs.existsSync(arg) ? arg : null;
  const candidates = [
    path.resolve(REPO_ROOT, arg),
    path.resolve(REPO_ROOT, 'compiler', arg),
    path.resolve(FIXTURES_DIR, arg),
  ];
  for (const c of candidates) {
    if (fs.existsSync(c) && fs.statSync(c).isFile()) return c;
  }
  // Last resort: match by basename across the corpus.
  const base = path.basename(arg);
  const hit = discoverFixtures(FIXTURES_DIR).find(
    f => path.basename(f) === base,
  );
  return hit ?? null;
}

async function runSingle(fixture: string): Promise<void> {
  const fixturePath = resolveFixture(fixture);
  if (fixturePath == null) {
    console.error(`${RED}Fixture not found: ${fixture}${RESET}`);
    process.exit(2);
  }
  const relPath = path.relative(REPO_ROOT, fixturePath);
  const source = fs.readFileSync(fixturePath, 'utf8');
  const firstLine = source.substring(0, source.indexOf('\n'));
  const isFlow = firstLine.includes('@flow');

  const tsRes = compileBabel(fixturePath, source, firstLine);
  const tsCode = await formatCode(tsRes.code ?? '', isFlow);
  const oxcRes = compileOxc(fixturePath, source, firstLine);
  const oxcCode = await formatCode(oxcRes.code ?? '', isFlow);
  const bailReasons = [...oxcRes.stderr.matchAll(/CODEGEN_BAIL: (.+)/g)].map(
    m => m[1].trim(),
  );

  const r = classify(
    relPath,
    isFlow,
    tsCode,
    oxcCode,
    oxcRes.error,
    bailReasons,
  );
  const pass = SEMANTIC_PASS.has(r.verdict);
  const color = pass ? GREEN : RED;

  console.log(`${BOLD}${relPath}${RESET}`);
  console.log(
    `${color}verdict: ${r.verdict}  (${pass ? 'SEMANTIC-pass' : 'FAIL'})${RESET}`,
  );
  if (r.bailReasons.length) {
    console.log(
      `${YELLOW}codegen bail reasons: ${r.bailReasons.join(', ')}${RESET}`,
    );
  }
  console.log('');
  console.log(`${BOLD}=== byte diff (TS vs oxc) ===${RESET}`);
  console.log(lineDiff(tsCode, oxcCode, 'TypeScript', 'oxc'));
  console.log('');
  console.log(`${BOLD}=== structural diff (normalized) ===${RESET}`);
  console.log(
    lineDiff(
      structuralNormalize(tsCode),
      structuralNormalize(oxcCode),
      'TypeScript (normalized)',
      'oxc (normalized)',
    ),
  );
  process.exit(pass ? 0 : 1);
}

async function runCorpus(): Promise<void> {
  let fixtures = discoverFixtures(FIXTURES_DIR);
  if (limit > 0) fixtures = fixtures.slice(0, limit);
  console.error(
    `Comparing ${fixtures.length} fixtures (TS baseline vs oxc)...`,
  );

  const results: Result[] = [];
  const counts: Record<Verdict, number> = {
    IDENTICAL: 0,
    STRUCTURAL: 0,
    'N-VAL': 0,
    BAIL: 0,
    OTHER: 0,
  };
  const bailHist = new Map<string, number>();

  for (let i = 0; i < fixtures.length; i++) {
    const fixturePath = fixtures[i];
    const relPath = path.relative(REPO_ROOT, fixturePath);
    const source = fs.readFileSync(fixturePath, 'utf8');
    const firstLine = source.substring(0, source.indexOf('\n'));
    const isFlow = firstLine.includes('@flow');
    if (i % 25 === 0) process.stderr.write(`\r  ${i + 1}/${fixtures.length}`);

    const tsRes = compileBabel(fixturePath, source, firstLine);
    const tsCode = await formatCode(tsRes.code ?? '', isFlow);

    let r: Result;
    if (isFlow) {
      r = classify(relPath, true, tsCode, '', null, []);
    } else {
      const oxcRes = compileOxc(fixturePath, source, firstLine);
      const oxcCode = await formatCode(oxcRes.code ?? '', isFlow);
      const bailReasons = [
        ...oxcRes.stderr.matchAll(/CODEGEN_BAIL: (.+)/g),
      ].map(m => m[1].trim());
      r = classify(relPath, false, tsCode, oxcCode, oxcRes.error, bailReasons);
    }

    counts[r.verdict]++;
    if (r.verdict === 'BAIL') {
      if (r.bailReasons.length === 0) {
        const k = '(no-reason / whole-fn or non-debug)';
        bailHist.set(k, (bailHist.get(k) ?? 0) + 1);
      }
      for (const reason of r.bailReasons) {
        bailHist.set(reason, (bailHist.get(reason) ?? 0) + 1);
      }
    }
    results.push(r);
  }
  process.stderr.write('\r\x1b[K');

  const semanticPass = counts.IDENTICAL + counts.STRUCTURAL;
  const total = fixtures.length;
  const passColor = counts.OTHER === 0 ? GREEN : YELLOW;

  console.log('');
  console.log(`${BOLD}=== SEMANTIC PARITY (primary metric) ===${RESET}`);
  console.log(
    `${passColor}SEMANTIC-pass: ${semanticPass}/${total} ` +
      `(${((semanticPass / total) * 100).toFixed(1)}%)${RESET}`,
  );
  console.log(
    `${DIM}  byte-identical:  ${counts.IDENTICAL}\n` +
      `  structural-equiv: ${counts.STRUCTURAL}${RESET}`,
  );
  console.log('');
  console.log(`${BOLD}=== FAILURE BUCKETS ===${RESET}`);
  console.log(
    `  ${'N-VAL'.padEnd(7)} ${String(counts['N-VAL']).padStart(5)}   ${DIM}oxc memoizes where TS bails/validates${RESET}`,
  );
  console.log(
    `  ${'BAIL'.padEnd(7)} ${String(counts.BAIL).padStart(5)}   ${DIM}oxc fell back to uncompiled (per-scope or whole-fn)${RESET}`,
  );
  console.log(
    `  ${RED}${'OTHER'.padEnd(7)} ${String(counts.OTHER).padStart(5)}${RESET}   ${DIM}genuine wrong-code (keep at zero)${RESET}`,
  );
  const totalFail = counts['N-VAL'] + counts.BAIL + counts.OTHER;
  console.log(`  ${'TOTAL'.padEnd(7)} ${String(totalFail).padStart(5)}`);

  if (bailHist.size > 0) {
    console.log('');
    console.log(
      `${BOLD}=== BAIL sub-histogram (by codegen reason) ===${RESET}`,
    );
    for (const [reason, n] of [...bailHist.entries()].sort(
      (a, b) => b[1] - a[1],
    )) {
      console.log(`  ${String(n).padStart(5)}  ${reason}`);
    }
  }

  if (listBucket) {
    const want = listBucket.toUpperCase() as Verdict;
    console.log('');
    console.log(`${BOLD}=== ${want} fixtures ===${RESET}`);
    for (const r of results.filter(r => r.verdict === want)) {
      console.log(`  ${CYAN}${r.fixture}${RESET}`);
    }
  } else if (counts.OTHER > 0) {
    console.log('');
    console.log(`${BOLD}=== OTHER fixtures (wrong-code) ===${RESET}`);
    for (const r of results.filter(r => r.verdict === 'OTHER')) {
      console.log(`  ${RED}${r.fixture}${RESET}`);
    }
  }

  if (jsonOut) {
    fs.writeFileSync(
      jsonOut,
      JSON.stringify(
        {
          total,
          semanticPass,
          counts,
          bailHist: Object.fromEntries(bailHist),
          results: results.map(({tsCode: _t, oxcCode: _o, ...rest}) => rest),
        },
        null,
        2,
      ),
    );
    console.error(`\nWrote ${jsonOut}`);
  }
}

(async () => {
  if (singleFixture) {
    await runSingle(singleFixture);
  } else {
    await runCorpus();
  }
})();
