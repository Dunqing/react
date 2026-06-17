/**
 * TS/Babel reference benchmark — the in-process historical baseline.
 *
 * Drives the SAME shared harness as bench-rust.mjs (bench-harness.mjs runBench),
 * so the per-fixture median is apples-to-apples with the pre-Oxc Rust driver and
 * the native engine. methodology fidelity is the whole point: we DO NOT
 * reimplement the timing loop.
 *
 * compileOne is the BARE in-process TS pipeline ONLY:
 *   Babel parse (plugins ['typescript','jsx'], sourceType 'module')
 *   -> babel-plugin-react-compiler (compilationMode 'all', panicThreshold 'none')
 *   -> Babel codegen.
 * No prettier, no sprout/shared-runtime type provider, no @expect-error
 * handling, no hermes-parser, no HIR re-validation, no fbt/idx plugins,
 * no evaluator presets. Just parse -> plugin -> generate, the raw compile cost.
 *
 * The plugin is loaded from packages/babel-plugin-react-compiler/dist/index.js
 * (built with `yarn workspace babel-plugin-react-compiler run build`, = tsup) —
 * the EXACT artifact `yarn snap` loads (snap constants.ts BABEL_PLUGIN_SRC).
 *
 * Run: cd compiler && node bench-ts.mjs
 */
import {createRequire} from 'node:module';
import path from 'node:path';
import * as BabelParser from '@babel/parser';
import {transformFromAstSync} from '@babel/core';
import {runBench} from './bench-harness.mjs';

const require = createRequire(import.meta.url);

// Point this at a worktree checked out at the pre-Oxc baseline commit
// b49e04151e (see README.md). Default matches the doc's measurement worktree.
const COMPILER_ROOT =
  process.env.PREOXC_COMPILER_ROOT ?? '/Users/qing/p/github/react-preoxc-bench/compiler';
const PLUGIN_DIST = path.join(
  COMPILER_ROOT,
  'packages/babel-plugin-react-compiler/dist/index.js',
);
const FIXTURES_DIR = path.join(
  COMPILER_ROOT,
  'packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler',
);

// default export = BabelPluginReactCompiler (src/index.ts line 62-63).
const BabelPluginReactCompiler = require(PLUGIN_DIST).default;

// Bare plugin options: compile every function, never throw on diagnostics.
const PLUGIN_OPTIONS = {
  compilationMode: 'all',
  panicThreshold: 'none',
};

/**
 * Bare in-process TS pipeline. Returns true if it produced code.
 * Mirrors snap/compiler.ts parseInput + transformFromAstSync, stripped to the
 * compiler plugin only.
 */
function compileOne(source, filename) {
  const ast = BabelParser.parse(source, {
    sourceFilename: filename,
    plugins: ['typescript', 'jsx'],
    sourceType: 'module',
  });

  const result = transformFromAstSync(ast, source, {
    filename,
    highlightCode: false,
    retainLines: true,
    compact: true,
    plugins: [[BabelPluginReactCompiler, PLUGIN_OPTIONS]],
    sourceType: 'module',
    ast: false,
    cloneInputAst: false,
    configFile: false,
    babelrc: false,
  });

  return result?.code != null;
}

runBench('TS/Babel reference (in-process)', compileOne, FIXTURES_DIR, {
  warmup: 3,
  iters: 8,
});
