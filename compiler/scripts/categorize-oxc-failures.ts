/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

/**
 * Categorize oxc Code-FAIL fixtures into buckets for the Rust port endgame.
 *
 * For every fixture where `--variant oxc` Code FAILS (vs the TS baseline),
 * classify into:
 *   - N-VAL:    TS produces NO output (validates/bails), oxc produces output.
 *   - BAIL:     both compile, but oxc lacks memoization (`_c(`) where TS has it.
 *   - COSMETIC: both memoize, only formatting/printing differs.
 *   - OTHER:    real wrong-code differences. The important bugs.
 *
 * Also histograms BAIL fixtures by codegen bail-reason (via the
 * REACT_COMPILER_CODEGEN_BAIL_DEBUG=1 stderr trace).
 *
 * Usage:
 *   tsx compiler/scripts/categorize-oxc-failures.ts [--limit N] [--json out.json]
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
const rawArgs = process.argv.slice(2);
const limitIdx = rawArgs.indexOf('--limit');
const limit = limitIdx >= 0 ? parseInt(rawArgs[limitIdx + 1], 10) : 0;
const jsonIdx = rawArgs.indexOf('--json');
const jsonOut = jsonIdx >= 0 ? rawArgs[jsonIdx + 1] : null;

const FIXTURES_DIR = path.join(
  REPO_ROOT,
  'compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler',
);

function discoverFixtures(rootPath: string): string[] {
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

// --- Build ---
console.error('Building Rust native module and e2e CLI...');
execSync(
  'rustup run 1.92.0 cargo build -p react_compiler_napi -p react_compiler_e2e_cli',
  {cwd: path.join(REPO_ROOT, 'compiler/crates'), stdio: ['inherit', 'pipe', 'pipe'], shell: true},
);
const TARGET_DIR = path.join(REPO_ROOT, 'compiler/target/debug');
const NATIVE_NODE_PATH = path.join(
  REPO_ROOT,
  'compiler/packages/babel-plugin-react-compiler-rust/native/index.node',
);
const dylib = fs.existsSync(path.join(TARGET_DIR, 'libreact_compiler_napi.dylib'))
  ? path.join(TARGET_DIR, 'libreact_compiler_napi.dylib')
  : path.join(TARGET_DIR, 'libreact_compiler_napi.so');
fs.copyFileSync(dylib, NATIVE_NODE_PATH);
const CLI_BINARY = path.join(TARGET_DIR, 'react-compiler-e2e');

const tsPlugin = require('../packages/babel-plugin-react-compiler/src').default;

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

function compileBabel(plugin: any, fixturePath: string, source: string, firstLine: string) {
  const isFlow = firstLine.includes('@flow');
  const isScript = firstLine.includes('@script');
  const parserPlugins = isFlow ? ['flow', 'jsx'] : ['typescript', 'jsx'];
  const pragmaOpts = parseConfigPragmaForTests(firstLine, {compilationMode: 'all'});
  const pluginOptions = {
    ...pragmaOpts,
    compilationMode: 'all' as const,
    panicThreshold: 'all_errors' as const,
    logger: {logEvent(): void {}, debugLogIRs(): void {}},
  };
  try {
    const result = babel.transformSync(source, {
      filename: fixturePath,
      sourceType: isScript ? 'script' : 'module',
      parserOpts: {plugins: parserPlugins},
      plugins: [[plugin, pluginOptions]],
      configFile: false,
      babelrc: false,
    });
    return {code: result?.code ?? null, error: null as string | null, stderr: ''};
  } catch (e) {
    return {code: null, error: e instanceof Error ? e.message : String(e), stderr: ''};
  }
}

function compileCli(fixturePath: string, source: string, firstLine: string) {
  const pragmaOpts = parseConfigPragmaForTests(firstLine, {compilationMode: 'all'});
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
    CLI_BINARY,
    ['--frontend', 'oxc', '--filename', fixturePath, '--options', JSON.stringify(options), '--json'],
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
      return {code: envelope.code ?? null, error: envelope.error ?? null, stderr};
    } catch {
      /* fall through */
    }
  }
  return {code: null, error: result.stderr || `Process exited ${result.status}`, stderr};
}

function hasMemo(code: string): boolean {
  return code.includes('_c(') || code.includes('useMemoCache');
}

// Robust structural normalizer for the COSMETIC-vs-OTHER decision. Neutralizes
// known-equivalent printing differences (comments/directives, JSX self-closing,
// compiler temp-name divergence, cache-slot index/ordering permutations,
// declaration kind, quotes/semicolons/whitespace) via an AST pass with a
// textual fallback. See compiler/scripts/structural-normalize.ts.
function cosmeticNormalize(code: string): string {
  return structuralNormalize(code);
}

type Bucket = 'N-VAL' | 'BAIL' | 'COSMETIC' | 'OTHER';

interface Row {
  fixture: string;
  bucket: Bucket;
  bailReasons: string[];
}

(async () => {
  let fixtures = discoverFixtures(FIXTURES_DIR);
  if (limit > 0) fixtures = fixtures.slice(0, limit);
  console.error(`Categorizing ${fixtures.length} fixtures...`);

  const rows: Row[] = [];
  const bucketCounts: Record<Bucket, number> = {'N-VAL': 0, BAIL: 0, COSMETIC: 0, OTHER: 0};
  const bailHist = new Map<string, number>();
  const examples: Record<Bucket, string[]> = {'N-VAL': [], BAIL: [], COSMETIC: [], OTHER: []};
  let codePassed = 0;
  const codePassedFixtures: string[] = [];

  for (let i = 0; i < fixtures.length; i++) {
    const fixturePath = fixtures[i];
    const relPath = path.relative(REPO_ROOT, fixturePath);
    const source = fs.readFileSync(fixturePath, 'utf8');
    const firstLine = source.substring(0, source.indexOf('\n'));
    const isFlow = firstLine.includes('@flow');
    if (i % 25 === 0) process.stderr.write(`\r  ${i + 1}/${fixtures.length}`);

    const tsRes = compileBabel(tsPlugin, fixturePath, source, firstLine);
    const tsCode = await formatCode(tsRes.code ?? '', isFlow);

    // Flow files are auto-passed for oxc in the harness; mirror that.
    if (isFlow) {
      codePassed++;
      codePassedFixtures.push(relPath);
      continue;
    }

    const oxcRes = compileCli(fixturePath, source, firstLine);
    const oxcCode = await formatCode(oxcRes.code ?? '', isFlow);

    const tsErrored = tsCode.trim() === '';
    const oxcErrored = oxcCode.trim() === '' || oxcRes.error != null;
    const codeMatch = tsCode === oxcCode || (tsErrored && oxcErrored);

    // passthrough rule from harness: TS errored + oxc emitted but unmemoized
    let codePassthrough = false;
    if (!codeMatch && tsErrored && oxcCode.trim() !== '' && !hasMemo(oxcCode)) {
      codePassthrough = true;
    }
    const codeOk = codeMatch || codePassthrough;
    if (codeOk) {
      codePassed++;
      codePassedFixtures.push(relPath);
      continue;
    }

    // --- CODE FAILED: classify ---
    const bailReasons = [...oxcRes.stderr.matchAll(/CODEGEN_BAIL: (.+)/g)].map(m => m[1].trim());
    let bucket: Bucket;
    if (tsErrored && !oxcErrored) {
      // TS validated/bailed (no output) but oxc produced (memoized) output.
      bucket = 'N-VAL';
    } else if (!tsErrored && !oxcErrored && hasMemo(tsCode) && !hasMemo(oxcCode)) {
      // Both compiled, but oxc lacks memoization -> un-codegen'd construct.
      bucket = 'BAIL';
    } else if (!tsErrored && !oxcErrored && hasMemo(tsCode) && hasMemo(oxcCode)) {
      // Both memoized; check if difference is purely cosmetic.
      bucket = cosmeticNormalize(tsCode) === cosmeticNormalize(oxcCode) ? 'COSMETIC' : 'OTHER';
    } else if (!tsErrored && !oxcErrored && !hasMemo(tsCode) && !hasMemo(oxcCode)) {
      // Neither memoized (both passthrough-ish) but still differ -> cosmetic or other.
      bucket = cosmeticNormalize(tsCode) === cosmeticNormalize(oxcCode) ? 'COSMETIC' : 'OTHER';
    } else if (!tsErrored && oxcErrored) {
      // TS compiled but oxc produced nothing / errored -> codegen bail (whole fn).
      bucket = 'BAIL';
    } else {
      bucket = 'OTHER';
    }

    bucketCounts[bucket]++;
    if (bucket === 'BAIL') {
      if (bailReasons.length === 0) bailHist.set('(no-reason / whole-fn or non-debug)', (bailHist.get('(no-reason / whole-fn or non-debug)') ?? 0) + 1);
      for (const r of bailReasons) bailHist.set(r, (bailHist.get(r) ?? 0) + 1);
    }
    if (examples[bucket].length < 6) examples[bucket].push(relPath);
    rows.push({fixture: relPath, bucket, bailReasons});
  }
  process.stderr.write('\r\x1b[K');

  // --- Report ---
  console.log('');
  console.log('=== CODE-PASS ===');
  console.log(`Code passed: ${codePassed}/${fixtures.length}`);
  console.log('');
  console.log('=== FAILURE CATEGORIZATION ===');
  for (const b of ['N-VAL', 'BAIL', 'COSMETIC', 'OTHER'] as Bucket[]) {
    console.log(`${b.padEnd(9)} ${String(bucketCounts[b]).padStart(5)}   e.g. ${examples[b].slice(0, 3).map(f => path.basename(f)).join(', ')}`);
  }
  const totalFail = bucketCounts['N-VAL'] + bucketCounts.BAIL + bucketCounts.COSMETIC + bucketCounts.OTHER;
  console.log(`${'TOTAL'.padEnd(9)} ${String(totalFail).padStart(5)}`);
  console.log('');
  console.log('=== BAIL SUB-HISTOGRAM (by codegen reason) ===');
  const sortedBail = [...bailHist.entries()].sort((a, b) => b[1] - a[1]);
  for (const [reason, n] of sortedBail) {
    console.log(`${String(n).padStart(5)}  ${reason}`);
  }
  console.log('');
  console.log('=== OTHER (potential wrong-code bugs) ===');
  for (const r of rows.filter(r => r.bucket === 'OTHER')) {
    console.log(`  ${r.fixture}`);
  }

  if (jsonOut) {
    fs.writeFileSync(
      jsonOut,
      JSON.stringify(
        {codePassed, total: fixtures.length, bucketCounts, bailHist: Object.fromEntries(sortedBail), examples, rows, codePassedFixtures},
        null,
        2,
      ),
    );
    console.error(`\nWrote ${jsonOut}`);
  }
})();
