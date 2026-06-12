/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */
// One-off: dump TS-vs-oxc line diffs for given fixtures (debug categorization).
import * as babel from '@babel/core';
import generate from '@babel/generator';
import {spawnSync} from 'child_process';
import fs from 'fs';
import path from 'path';
import prettier from 'prettier';
import {parseConfigPragmaForTests} from '../packages/babel-plugin-react-compiler/src/Utils/TestUtils';

const REPO_ROOT = path.resolve(__dirname, '../..');
const TARGET_DIR = path.join(REPO_ROOT, 'compiler/target/debug');
const CLI_BINARY = path.join(TARGET_DIR, 'react-compiler-e2e');
const tsPlugin = require('../packages/babel-plugin-react-compiler/src').default;

async function fmt(code: string, isFlow: boolean): Promise<string> {
  try {
    const ast = babel.parseSync(code, {
      sourceType: 'module',
      parserOpts: {plugins: isFlow ? ['flow', 'jsx'] : ['typescript', 'jsx']},
      configFile: false,
      babelrc: false,
    });
    if (!ast) return code;
    const compact = generate(ast, {compact: true}).code;
    return await prettier.format(compact, {semi: true, parser: isFlow ? 'flow' : 'babel-ts'});
  } catch {
    return code;
  }
}
function ts(p: string, src: string, fl: string) {
  const isFlow = fl.includes('@flow');
  const isScript = fl.includes('@script');
  const opts = parseConfigPragmaForTests(fl, {compilationMode: 'all'});
  try {
    const r = babel.transformSync(src, {
      filename: p,
      sourceType: isScript ? 'script' : 'module',
      parserOpts: {plugins: isFlow ? ['flow', 'jsx'] : ['typescript', 'jsx']},
      plugins: [[tsPlugin, {...opts, compilationMode: 'all', panicThreshold: 'all_errors', logger: {logEvent() {}, debugLogIRs() {}}}]],
      configFile: false,
      babelrc: false,
    });
    return r?.code ?? '';
  } catch {
    return '';
  }
}
function oxc(p: string, src: string, fl: string) {
  const opts = parseConfigPragmaForTests(fl, {compilationMode: 'all'});
  const options = {shouldCompile: true, enableReanimated: false, isDev: false, ...opts, compilationMode: 'all', panicThreshold: 'all_errors', __sourceCode: src};
  const r = spawnSync(CLI_BINARY, ['--frontend', 'oxc', '--filename', p, '--options', JSON.stringify(options), '--json'], {input: src, encoding: 'utf-8', timeout: 30000});
  if (r.stdout) {
    try {
      return JSON.parse(r.stdout).code ?? '';
    } catch {}
  }
  return '';
}

(async () => {
  for (const rel of process.argv.slice(2)) {
    const p = path.join(REPO_ROOT, rel);
    const src = fs.readFileSync(p, 'utf8');
    const fl = src.substring(0, src.indexOf('\n'));
    const isFlow = fl.includes('@flow');
    const a = (await fmt(ts(p, src, fl), isFlow)).split('\n');
    const b = (await fmt(oxc(p, src, fl), isFlow)).split('\n');
    console.log(`\n##### ${path.basename(rel)} #####`);
    const max = Math.max(a.length, b.length);
    for (let i = 0; i < max; i++) {
      if (a[i] !== b[i]) {
        if (a[i] !== undefined) console.log(`-TS  ${a[i]}`);
        if (b[i] !== undefined) console.log(`+OXC ${b[i]}`);
      }
    }
  }
})();
