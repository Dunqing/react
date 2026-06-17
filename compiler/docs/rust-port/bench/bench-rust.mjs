/**
 * Pre-Oxc Rust-port benchmark engine. Drives the FULL pre-Oxc pipeline
 *   Babel parse -> scope.ts -> JSON serialize -> NAPI -> Rust core -> JSON
 *   parse-back -> Babel codegen
 * over the fixture corpus via the shared run_bench-mirroring harness.
 *
 * Per-fixture work is exactly BabelPlugin.ts's orchestration (replicated),
 * using the package's own COMPILED dist modules. The headline run uses the
 * non-profiled compileWithRust (what BabelPlugin.ts actually calls). Pass
 * `--profile` to instead accumulate the frontend/boundary/core/codegen split
 * via compileWithRustProfiled.
 *
 * Run: cd compiler && node bench-rust.mjs [--profile]
 */
import {createRequire} from 'node:module';
import * as BabelParser from '@babel/parser';
import generate from '@babel/generator';
import traverseModule from '@babel/traverse';
import {runBench, collectFixtures} from './bench-harness.mjs';
import fs from 'node:fs';

const require = createRequire(import.meta.url);
// Point this at a worktree checked out at the pre-Oxc baseline commit
// b49e04151e (see README.md). Default matches the doc's measurement worktree.
const ROOT = process.env.PREOXC_COMPILER_ROOT ?? '/Users/qing/p/github/react-preoxc-bench/compiler';
const PKG = `${ROOT}/packages/babel-plugin-react-compiler-rust`;
const FX = `${ROOT}/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler`;

const {extractScopeInfo} = require(`${PKG}/dist/scope.js`);
const {resolveOptions} = require(`${PKG}/dist/options.js`);
const {compileWithRust, compileWithRustProfiled} = require(`${PKG}/dist/bridge.js`);
const generateCode = generate.default ?? generate;
const traverse = traverseModule.default ?? traverseModule;

// ---- comment-dedup + applyRenames, copied verbatim from BabelPlugin.ts logic ----
function deduplicateComments(node) {
  const canonical = new Map();
  const dedup = comments =>
    comments.map(c => {
      const key = `${c.start}:${c.end}`;
      const existing = canonical.get(key);
      if (existing != null) return existing;
      canonical.set(key, c);
      return c;
    });
  function visit(n) {
    if (n == null || typeof n !== 'object') return;
    if (Array.isArray(n)) {
      for (const item of n) visit(item);
      return;
    }
    if (n.leadingComments) n.leadingComments = dedup(n.leadingComments);
    if (n.trailingComments) n.trailingComments = dedup(n.trailingComments);
    if (n.innerComments) n.innerComments = dedup(n.innerComments);
    for (const key of Object.keys(n)) {
      if (
        key === 'leadingComments' ||
        key === 'trailingComments' ||
        key === 'innerComments' ||
        key === 'start' ||
        key === 'end' ||
        key === 'loc'
      )
        continue;
      visit(n[key]);
    }
  }
  visit(node);
}

function applyRenames(progPath, renames) {
  const renamesByPos = new Map();
  for (const r of renames) renamesByPos.set(r.declarationStart, r);
  progPath.traverse({
    Scope(path) {
      const scope = path.scope;
      for (const [name, binding] of Object.entries(scope.bindings)) {
        const start = binding.identifier.start;
        if (start != null) {
          const rename = renamesByPos.get(start);
          if (rename != null && name === rename.original) {
            scope.rename(rename.original, rename.renamed);
            renamesByPos.delete(start);
          }
        }
      }
    },
  });
}

function prep(source, filename) {
  const ast = BabelParser.parse(source, {
    sourceFilename: filename,
    plugins: ['typescript', 'jsx'],
    sourceType: 'module',
  });
  let programPath = null;
  const file = {ast, code: source, opts: {filename, plugins: []}};
  traverse(ast, {
    Program(path) {
      programPath = path;
      path.stop();
    },
  });
  if (programPath == null) throw new Error('no Program path');
  const opts = resolveOptions(
    {compilationMode: 'all', panicThreshold: 'none'},
    file,
    filename,
    ast,
  );
  const scopeInfo = extractScopeInfo(programPath);
  return {ast, programPath, opts, scopeInfo};
}

function finalize(ast, programPath, result) {
  if (result.kind === 'error') return false;
  if (result.ast != null) {
    const newProgram = result.ast.program ?? result.ast;
    deduplicateComments(newProgram);
    ast.comments = [];
    programPath.replaceWith(newProgram);
  }
  if (result.renames != null && result.renames.length > 0) {
    applyRenames(programPath, result.renames);
  }
  const {code} = generateCode(ast, {});
  return code != null;
}

// Headline end-to-end unit (non-profiled — matches BabelPlugin.ts).
function compileOne(source, filename) {
  const {ast, programPath, opts, scopeInfo} = prep(source, filename);
  const result = compileWithRust(ast, scopeInfo, opts, source);
  return finalize(ast, programPath, result);
}

if (process.argv.includes('--profile')) {
  // Sub-phase decomposition (single pass, accumulate microseconds).
  const paths = collectFixtures(FX);
  let frontend = 0,
    jsSerialize = 0,
    jsParseBack = 0,
    rustDeser = 0,
    rustSer = 0,
    rustCore = 0,
    codegen = 0,
    ok = 0;
  // warm up first
  for (const p of paths.slice(0, 200)) {
    try {
      compileOne(fs.readFileSync(p, 'utf8'), p);
    } catch {}
  }
  for (const p of paths) {
    const source = fs.readFileSync(p, 'utf8');
    try {
      const tf0 = process.hrtime.bigint();
      const {ast, programPath, opts, scopeInfo} = prep(source, p);
      const tf1 = process.hrtime.bigint();
      frontend += Number(tf1 - tf0) / 1000; // parse + traverse + resolveOptions + scope
      const {result, bridgeTiming, rustTiming} = compileWithRustProfiled(
        ast,
        scopeInfo,
        opts,
        source,
      );
      jsSerialize +=
        bridgeTiming.jsStringifyAst_us +
        bridgeTiming.jsStringifyScope_us +
        bridgeTiming.jsStringifyOptions_us;
      jsParseBack += bridgeTiming.jsParseResult_us;
      for (const e of rustTiming) {
        if (e.name.includes('deserialize')) rustDeser += e.duration_us;
        else if (e.name.includes('serialize')) rustSer += e.duration_us;
        else rustCore += e.duration_us;
      }
      const tc0 = process.hrtime.bigint();
      if (finalize(ast, programPath, result)) ok++;
      const tc1 = process.hrtime.bigint();
      codegen += Number(tc1 - tc0) / 1000;
    } catch {}
  }
  const boundary = jsSerialize + jsParseBack + rustDeser + rustSer;
  const total = frontend + boundary + rustCore + codegen;
  const pct = x => ((x / total) * 100).toFixed(1) + '%';
  console.log('=== Pre-Oxc sub-phase decomposition (sum over corpus, us) ===');
  console.log(`compiled ok:          ${ok}/${paths.length}`);
  console.log(`frontend (parse+scope):   ${frontend.toFixed(0)}us  ${pct(frontend)}`);
  console.log(
    `JSON/NAPI boundary:       ${boundary.toFixed(0)}us  ${pct(boundary)}   (js-ser ${jsSerialize.toFixed(0)} + js-parse ${jsParseBack.toFixed(0)} + rust-deser ${rustDeser.toFixed(0)} + rust-ser ${rustSer.toFixed(0)})`,
  );
  console.log(`Rust compiler core:       ${rustCore.toFixed(0)}us  ${pct(rustCore)}`);
  console.log(`Babel codegen:            ${codegen.toFixed(0)}us  ${pct(codegen)}`);
  console.log(`total:                    ${total.toFixed(0)}us`);
} else {
  runBench(
    'Pre-Oxc Rust port (Babel parse->scope->JSON->NAPI->Rust->Babel codegen)',
    compileOne,
    FX,
    {warmup: 3, iters: 8},
  );
}
