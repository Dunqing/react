/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

/**
 * Per-pass HIR-diff oracle: OXC-native compiler path vs the TypeScript compiler.
 *
 * For each fixture this compares the per-pass HIR of:
 *   - TS side:  the TypeScript Babel plugin, run in-process (printDebugHIR).
 *   - OXC side: the prebuilt e2e CLI binary, via `--frontend oxc --dump-hir`.
 *
 * Both sides go through the SAME normalization (normalizeIds) before diffing,
 * so opaque IDs / mutableRange noise is removed. Reuses the shared helpers in
 * hir-oracle-lib.ts (no logic is reinvented here).
 *
 * We do NOT expect high parity yet: most constructs bail with a graceful Todo
 * during lowering, so the HIR they produce diverges from TS at the very first
 * pass (`## HIR`). The deliverable is a working oracle + a baseline report of
 * where each fixture's HIR first diverges (the "frontier").
 *
 * Usage:
 *   tsx compiler/scripts/compare-hir.ts <fixture>            # single fixture, prints diff
 *   tsx compiler/scripts/compare-hir.ts                      # corpus run
 *   tsx compiler/scripts/compare-hir.ts <dir>                # corpus run over a subtree
 *
 * Flags:
 *   --no-color    Disable ANSI color codes (also respects NO_COLOR)
 *   --out <path>  Write per-fixture results (corpus mode) to this file
 *   --no-build    Skip the cargo build step (binary must already exist)
 *   --limit N     Max diffs to print in corpus mode (default 20, 0 = all)
 */

import {execSync, spawnSync} from 'child_process';
import fs from 'fs';
import path from 'path';

import {
  REPO_ROOT,
  DEFAULT_FIXTURES_DIR,
  type LogItem,
  derivePassOrder,
  discoverFixtures,
  compileFixtureTS,
  formatLog,
  formatLogItem,
  normalizeIds,
  findDivergencePass,
  parseDumpHir,
} from './hir-oracle-lib';
import {parseConfigPragmaForTests} from '../packages/babel-plugin-react-compiler/src/Utils/TestUtils';

// --- Parse flags ---
const rawArgs = process.argv.slice(2);
const noColor = rawArgs.includes('--no-color') || !!process.env.NO_COLOR;
const noBuild = rawArgs.includes('--no-build');
const outIdx = rawArgs.indexOf('--out');
const outPath = outIdx >= 0 ? rawArgs[outIdx + 1] : null;
const limitIdx = rawArgs.indexOf('--limit');
const limitArg = limitIdx >= 0 ? parseInt(rawArgs[limitIdx + 1], 10) : 20;

const flagValueIndices = new Set<number>();
if (outIdx >= 0) flagValueIndices.add(outIdx + 1);
if (limitIdx >= 0) flagValueIndices.add(limitIdx + 1);
const positional = rawArgs.filter(
  (a, i) => !a.startsWith('--') && !flagValueIndices.has(i),
);

// --- ANSI colors ---
const useColor = !noColor;
const RED = useColor ? '\x1b[0;31m' : '';
const GREEN = useColor ? '\x1b[0;32m' : '';
const YELLOW = useColor ? '\x1b[0;33m' : '';
const BOLD = useColor ? '\x1b[1m' : '';
const DIM = useColor ? '\x1b[2m' : '';
const RESET = useColor ? '\x1b[0m' : '';

const PASS_ORDER = derivePassOrder();
const TARGET_PASS = PASS_ORDER[PASS_ORDER.length - 1];

const fixturesPath = positional[0]
  ? path.resolve(positional[0])
  : DEFAULT_FIXTURES_DIR;
const fixtures = discoverFixtures(fixturesPath);
const singleMode = fixtures.length === 1;

if (fixtures.length === 0) {
  console.error('No fixtures found at', fixturesPath);
  process.exit(1);
}

// --- Build the e2e CLI binary once up front ---
const TARGET_DIR = path.join(REPO_ROOT, 'compiler/target/debug');
const CLI_BINARY = path.join(TARGET_DIR, 'react-compiler-e2e');

if (!noBuild) {
  if (!singleMode || !fs.existsSync(CLI_BINARY)) {
    console.error('Building react_compiler_e2e_cli (rustc 1.94.0)...');
  }
  try {
    execSync('rustup run 1.94.0 cargo build -p react_compiler_e2e_cli', {
      cwd: path.join(REPO_ROOT, 'compiler/crates'),
      stdio: ['inherit', 'pipe', 'pipe'],
      shell: true,
    });
  } catch (e: any) {
    if (e.stderr) process.stderr.write(e.stderr);
    console.error(
      `${RED}ERROR: Failed to build react_compiler_e2e_cli.${RESET}`,
    );
    process.exit(1);
  }
}
if (!fs.existsSync(CLI_BINARY)) {
  console.error(`${RED}ERROR: CLI binary not found at ${CLI_BINARY}.${RESET}`);
  console.error('Run without --no-build, or build it first.');
  process.exit(1);
}

// --- Load the TS plugin (in-process) ---
const tsPlugin = require('../packages/babel-plugin-react-compiler/src').default;

// --- Get the OXC side per-pass HIR via the CLI `--dump-hir` ---
function compileOxcDumpHir(fixturePath: string): {
  log: LogItem[];
  error: string | null;
} {
  const source = fs.readFileSync(fixturePath, 'utf8');
  const firstLine = source.substring(0, source.indexOf('\n'));
  const pragmaOpts = parseConfigPragmaForTests(firstLine, {
    compilationMode: 'all',
  });
  // --dump-hir already forces compilationMode:all + __debug:true on the Rust
  // side; we still forward pragma options so per-fixture flags are honored.
  const options = {
    ...pragmaOpts,
    compilationMode: 'all',
    panicThreshold: 'all_errors',
  };

  const result = spawnSync(
    CLI_BINARY,
    [
      '--frontend',
      'oxc',
      '--filename',
      fixturePath,
      '--options',
      JSON.stringify(options),
      '--dump-hir',
    ],
    {
      input: source,
      encoding: 'utf-8',
      timeout: 60000,
      maxBuffer: 64 * 1024 * 1024,
    },
  );

  if (result.status !== 0 || result.error) {
    // Non-zero exit / spawn error => the OXC path crashed (NOT a graceful bail).
    const stderr = (result.stderr ?? '').toString().trim();
    return {
      log: [],
      error:
        result.error?.message ??
        (stderr || `oxc CLI exited with status ${result.status}`),
    };
  }
  return {log: parseDumpHir(result.stdout ?? ''), error: null};
}

// --- Classify a single fixture ---
type Outcome =
  | {kind: 'match'; passes: number}
  | {kind: 'frontier'; pass: string}
  | {kind: 'oxc-crash'; detail: string}
  | {kind: 'ts-error-oxc-bail'};

interface Result {
  fixture: string;
  outcome: Outcome;
  diff?: string;
}

// --- Simple line-by-line diff (TS vs OXC) ---
function unifiedDiff(expected: string, actual: string): string {
  const expectedLines = expected.split('\n');
  const actualLines = actual.split('\n');
  const lines: string[] = [];
  lines.push(`${RED}--- TypeScript${RESET}`);
  lines.push(`${GREEN}+++ OXC${RESET}`);
  const maxLen = Math.max(expectedLines.length, actualLines.length);
  for (let i = 0; i < maxLen; i++) {
    const eLine = i < expectedLines.length ? expectedLines[i] : undefined;
    const aLine = i < actualLines.length ? actualLines[i] : undefined;
    if (eLine === aLine) continue;
    lines.push(`${YELLOW}@@ line ${i + 1} @@${RESET}`);
    if (eLine !== undefined) lines.push(`${RED}-${eLine}${RESET}`);
    if (aLine !== undefined) lines.push(`${GREEN}+${aLine}${RESET}`);
  }
  return lines.join('\n');
}

function classify(fixturePath: string): Result {
  const relPath = path.relative(REPO_ROOT, fixturePath);
  const ts = compileFixtureTS(tsPlugin, fixturePath, TARGET_PASS, 'all');
  const oxc = compileOxcDumpHir(fixturePath);

  // OXC crashed (non-zero exit) — this is a real failure, not a graceful bail.
  if (oxc.error !== null) {
    return {
      fixture: relPath,
      outcome: {kind: 'oxc-crash', detail: oxc.error.split('\n')[0]},
    };
  }

  // The TS log can include compile-error events; the OXC --dump-hir log only
  // contains HIR entries. To compare apples-to-apples we diff per-pass HIR.
  // We restrict the TS log to its entries (HIR dumps), matching the OXC dump.
  const tsEntries = ts.log.filter(i => i.kind === 'entry');
  const oxcEntries = oxc.log; // already entries-only

  // If TS emitted nothing (errored before HIR) and OXC also emitted nothing,
  // treat as the known "ts-error-oxc-bail" frontier rather than a clean match.
  if (tsEntries.length === 0 && oxcEntries.length === 0) {
    return {fixture: relPath, outcome: {kind: 'ts-error-oxc-bail'}};
  }

  // Compare per-pass over the COMMON prefix length. Trailing passes that only
  // one side emits are an emission-coverage difference (e.g. the config-gated
  // ValidatePreservedManualMemoization "ok" entry, whose default differs
  // between the two frontends) — NOT a meaningful HIR divergence. We only flag
  // a frontier when a pass that BOTH sides emit has differing content, or when
  // the shorter log diverges before it runs out. This keeps the signal on
  // real HIR differences (e.g. lowering bails at `## HIR`).
  const commonLen = Math.min(tsEntries.length, oxcEntries.length);
  const tsCommon = tsEntries.slice(0, commonLen);
  const oxcCommon = oxcEntries.slice(0, commonLen);

  const tsFormatted = normalizeIds(formatLog(tsCommon));
  const oxcFormatted = normalizeIds(formatLog(oxcCommon));

  if (tsFormatted === oxcFormatted) {
    return {fixture: relPath, outcome: {kind: 'match', passes: commonLen}};
  }

  // Diverged within the common prefix — find the first diverging pass.
  const pass = findDivergencePass(tsCommon, oxcCommon, PASS_ORDER);
  return {
    fixture: relPath,
    outcome: {kind: 'frontier', pass},
    diff: unifiedDiff(tsFormatted, oxcFormatted),
  };
}

// --- Single-fixture mode: print the diff ---
if (singleMode) {
  const r = classify(fixtures[0]);
  console.log('');
  switch (r.outcome.kind) {
    case 'match':
      console.log(
        `${GREEN}MATCH${RESET} ${r.fixture} — HIR identical for all ${r.outcome.passes} shared passes`,
      );
      break;
    case 'frontier':
      console.log(
        `${YELLOW}FRONTIER${RESET} ${r.fixture} — first diverges at pass ${BOLD}${r.outcome.pass}${RESET}`,
      );
      console.log('');
      console.log(r.diff);
      break;
    case 'ts-error-oxc-bail':
      console.log(
        `${YELLOW}TS-ERROR + OXC-BAIL${RESET} ${r.fixture} — both produced no HIR (known frontier, not a crash)`,
      );
      break;
    case 'oxc-crash':
      console.log(`${RED}OXC-CRASH${RESET} ${r.fixture} — ${r.outcome.detail}`);
      break;
  }
  process.exit(r.outcome.kind === 'oxc-crash' ? 1 : 0);
}

// --- Corpus mode ---
console.error(
  `Comparing ${BOLD}${fixtures.length}${RESET} fixtures (OXC CLI vs TS, per-pass HIR)...`,
);

let matches = 0;
let crashes = 0;
let tsErrorBail = 0;
// frontier pass -> count
const frontierHistogram = new Map<string, number>();
const results: Result[] = [];
const diffsToShow: Result[] = [];

for (const fixturePath of fixtures) {
  // Skip Flow fixtures (.flow.js) per the task spec.
  if (fixturePath.endsWith('.flow.js')) continue;
  const r = classify(fixturePath);
  results.push(r);
  switch (r.outcome.kind) {
    case 'match':
      matches++;
      break;
    case 'frontier': {
      const c = frontierHistogram.get(r.outcome.pass) ?? 0;
      frontierHistogram.set(r.outcome.pass, c + 1);
      if (limitArg === 0 || diffsToShow.length < limitArg) diffsToShow.push(r);
      break;
    }
    case 'ts-error-oxc-bail':
      tsErrorBail++;
      break;
    case 'oxc-crash':
      crashes++;
      break;
  }
}

const total = results.length;

// --- Per-fixture results file ---
function outcomeLabel(o: Outcome): string {
  switch (o.kind) {
    case 'match':
      return 'MATCH';
    case 'frontier':
      return `FRONTIER\t${o.pass}`;
    case 'ts-error-oxc-bail':
      return 'TS-ERROR+OXC-BAIL';
    case 'oxc-crash':
      return `OXC-CRASH\t${o.detail}`;
  }
}

if (outPath) {
  const lines = results.map(r => `${outcomeLabel(r.outcome)}\t${r.fixture}`);
  fs.writeFileSync(path.resolve(outPath), lines.join('\n') + '\n');
  console.error(`Wrote per-fixture results to ${outPath}`);
}

function frontierCount(): number {
  let n = 0;
  for (const c of frontierHistogram.values()) n += c;
  return n;
}

// --- Print diffs (bounded) ---
for (const r of diffsToShow) {
  if (r.outcome.kind !== 'frontier') continue;
  console.log(
    `${YELLOW}FRONTIER${RESET} ${r.fixture} @ ${BOLD}${r.outcome.pass}${RESET}`,
  );
}
if (diffsToShow.length < frontierCount()) {
  console.log(
    `${DIM}  (... and ${frontierCount() - diffsToShow.length} more frontier fixtures; see --out file)${RESET}`,
  );
}

// --- Frontier histogram (ordered by pass order) ---
console.log('');
console.log(`${BOLD}=== Frontier histogram (first diverging pass) ===${RESET}`);
const orderedFrontiers = PASS_ORDER.filter(p => frontierHistogram.has(p));
for (const pass of orderedFrontiers) {
  console.log(`  ${pass}: ${frontierHistogram.get(pass)}`);
}
// Any frontier pass not in PASS_ORDER (shouldn't happen, but be safe)
for (const [pass, count] of frontierHistogram) {
  if (!PASS_ORDER.includes(pass))
    console.log(`  ${pass} (unordered): ${count}`);
}

// --- Summary ---
console.log('');
const hirFrontier = frontierHistogram.get('HIR') ?? 0;
const laterFrontier = frontierCount() - hirFrontier;
console.log(`${BOLD}=== Baseline summary ===${RESET}`);
console.log(`  total compared:        ${total}`);
console.log(`  ${GREEN}fully MATCH:${RESET}           ${matches}`);
console.log(`  frontier @ HIR (bail): ${hirFrontier}`);
console.log(`  frontier @ later pass: ${laterFrontier}`);
console.log(`  TS-error + OXC-bail:   ${tsErrorBail}`);
console.log(`  ${RED}OXC crashes:${RESET}           ${crashes}`);

process.exit(0);
