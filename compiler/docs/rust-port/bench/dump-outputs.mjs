/**
 * Dump the pre-Oxc Rust-port and TS/Babel compiled output for each fixture in
 * an intersection list, so a separate comparator (with structuralNormalize) can
 * check semantic equivalence. Uses the SAME engines + config (all_errors) as the
 * benchmark. Output goes to /tmp/out-preoxc/<idx>.js and /tmp/out-ts/<idx>.js,
 * idx = line number in the intersection list (1-based). Writes index map too.
 *
 * Run: cd compiler && node dump-outputs.mjs /tmp/intersection.txt
 */
import {createRequire} from 'node:module';
import fs from 'node:fs';
import path from 'node:path';
import * as BabelParser from '@babel/parser';
import {transformFromAstSync} from '@babel/core';
import generate from '@babel/generator';
import traverseModule from '@babel/traverse';

const require = createRequire(import.meta.url);
// Worktree checked out at the pre-Oxc baseline b49e04151e (see README.md).
const ROOT =
  process.env.PREOXC_COMPILER_ROOT ?? '/Users/qing/p/github/react-preoxc-bench/compiler';
const PKG = `${ROOT}/packages/babel-plugin-react-compiler-rust`;
const TS_PLUGIN = `${ROOT}/packages/babel-plugin-react-compiler/dist/index.js`;

const {extractScopeInfo} = require(`${PKG}/dist/scope.js`);
const {resolveOptions} = require(`${PKG}/dist/options.js`);
const {compileWithRust} = require(`${PKG}/dist/bridge.js`);
const BabelPluginReactCompiler = require(TS_PLUGIN).default;
const generateCode = generate.default ?? generate;
const traverse = traverseModule.default ?? traverseModule;

function deduplicateComments(node) {
  const canonical = new Map();
  const dedup = comments =>
    comments.map(c => {
      const key = `${c.start}:${c.end}`;
      const e = canonical.get(key);
      if (e != null) return e;
      canonical.set(key, c);
      return c;
    });
  (function visit(n) {
    if (n == null || typeof n !== 'object') return;
    if (Array.isArray(n)) return n.forEach(visit);
    if (n.leadingComments) n.leadingComments = dedup(n.leadingComments);
    if (n.trailingComments) n.trailingComments = dedup(n.trailingComments);
    if (n.innerComments) n.innerComments = dedup(n.innerComments);
    for (const k of Object.keys(n)) {
      if (['leadingComments', 'trailingComments', 'innerComments', 'start', 'end', 'loc'].includes(k)) continue;
      visit(n[k]);
    }
  })(node);
}
function applyRenames(progPath, renames) {
  const byPos = new Map();
  for (const r of renames) byPos.set(r.declarationStart, r);
  progPath.traverse({
    Scope(p) {
      for (const [name, b] of Object.entries(p.scope.bindings)) {
        const start = b.identifier.start;
        const r = start != null ? byPos.get(start) : null;
        if (r != null && name === r.original) {
          p.scope.rename(r.original, r.renamed);
          byPos.delete(start);
        }
      }
    },
  });
}

function preoxcCode(source, filename) {
  const ast = BabelParser.parse(source, {sourceFilename: filename, plugins: ['typescript', 'jsx'], sourceType: 'module'});
  let programPath = null;
  const file = {ast, code: source, opts: {filename, plugins: []}};
  traverse(ast, {Program(p) { programPath = p; p.stop(); }});
  const opts = resolveOptions({compilationMode: 'all', panicThreshold: 'all_errors'}, file, filename, ast);
  const scopeInfo = extractScopeInfo(programPath);
  const result = compileWithRust(ast, scopeInfo, opts, source);
  if (result.kind === 'error') return null;
  if (result.ast != null) {
    const np = result.ast.program ?? result.ast;
    deduplicateComments(np);
    ast.comments = [];
    programPath.replaceWith(np);
  }
  if (result.renames?.length) applyRenames(programPath, result.renames);
  return generateCode(ast, {}).code;
}

function tsCode(source, filename) {
  const ast = BabelParser.parse(source, {sourceFilename: filename, plugins: ['typescript', 'jsx'], sourceType: 'module'});
  const result = transformFromAstSync(ast, source, {
    filename,
    plugins: [[BabelPluginReactCompiler, {compilationMode: 'all', panicThreshold: 'all_errors'}]],
    sourceType: 'module',
    ast: false,
    cloneInputAst: false,
    configFile: false,
    babelrc: false,
  });
  return result?.code ?? null;
}

const listFile = process.argv[2] ?? '/tmp/intersection.txt';
const paths = fs.readFileSync(listFile, 'utf8').split('\n').map(s => s.trim()).filter(Boolean);
fs.mkdirSync('/tmp/out-preoxc', {recursive: true});
fs.mkdirSync('/tmp/out-ts', {recursive: true});
const map = [];
let pOk = 0, tOk = 0;
paths.forEach((p, i) => {
  const idx = i + 1;
  const source = fs.readFileSync(p, 'utf8');
  map.push(`${idx}\t${p}`);
  let pc = null, tc = null;
  try { pc = preoxcCode(source, p); } catch {}
  try { tc = tsCode(source, p); } catch {}
  if (pc != null) { fs.writeFileSync(`/tmp/out-preoxc/${idx}.js`, pc); pOk++; }
  if (tc != null) { fs.writeFileSync(`/tmp/out-ts/${idx}.js`, tc); tOk++; }
});
fs.writeFileSync('/tmp/out-map.tsv', map.join('\n'));
console.log(`dumped ${paths.length} fixtures: preoxc ok ${pOk}, ts ok ${tOk}`);
