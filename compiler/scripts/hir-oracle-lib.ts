/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

/**
 * Shared helpers for the per-pass HIR-diff oracle.
 *
 * Extracted from test-rust-port.ts so that multiple oracle scripts (the
 * NAPI-bridge `test-rust-port.ts` and the OXC-CLI `compare-hir.ts`) can reuse
 * the SAME normalization, fixture discovery, pass ordering, log formatting, and
 * frontier-detection logic without reinventing it.
 *
 * Importing this module has no side effects (no native build, no CLI spawn), so
 * it is safe to import from any script.
 */

import * as babel from '@babel/core';
import hermesParserPlugin from 'babel-plugin-syntax-hermes-parser';
import fs from 'fs';
import path from 'path';

import {parseConfigPragmaForTests} from '../packages/babel-plugin-react-compiler/src/Utils/TestUtils';
import {printDebugHIR} from '../packages/babel-plugin-react-compiler/src/HIR/DebugPrintHIR';
import {printDebugReactiveFunction} from '../packages/babel-plugin-react-compiler/src/HIR/DebugPrintReactiveFunction';
import type {CompilerPipelineValue} from '../packages/babel-plugin-react-compiler/src/Entrypoint/Pipeline';

export const REPO_ROOT = path.resolve(__dirname, '../..');

export const DEFAULT_FIXTURES_DIR = path.join(
  REPO_ROOT,
  'compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler',
);

// --- Log item types (entries = per-pass HIR dumps; events = compile errors) ---
export interface LogEntry {
  kind: 'entry';
  name: string;
  value: string;
}

export interface LogEvent {
  kind: 'event';
  eventKind: string;
  fnName: string | null;
  detail: string;
}

export type LogItem = LogEntry | LogEvent;

export interface CompileOutput {
  log: LogItem[];
  code: string | null;
  error: string | null;
}

// --- Ordered pass list (derived from pipeline.rs DebugLogEntry calls) ---
// Both the TS and Rust pipelines run the same ordered set of passes; the Rust
// pipeline's `DebugLogEntry::new("<Pass>")` calls are the canonical order.
export function derivePassOrder(): string[] {
  const pipelinePath = path.join(
    REPO_ROOT,
    'compiler/crates/react_compiler/src/entrypoint/pipeline.rs',
  );
  const content = fs.readFileSync(pipelinePath, 'utf8');
  const matches = [...content.matchAll(/DebugLogEntry::new\("([^"]+)"/g)];
  return matches.map(m => m[1]);
}

// --- Discover fixtures (single file or recursive directory) ---
export function discoverFixtures(rootPath: string): string[] {
  const stat = fs.statSync(rootPath);
  if (stat.isFile()) {
    return [rootPath];
  }

  const results: string[] = [];
  function walk(dir: string): void {
    for (const entry of fs.readdirSync(dir, {withFileTypes: true})) {
      const fullPath = path.join(dir, entry.name);
      if (entry.isDirectory()) {
        walk(fullPath);
      } else if (
        /\.(js|jsx|ts|tsx)$/.test(entry.name) &&
        !entry.name.endsWith('.expect.md')
      ) {
        results.push(fullPath);
      }
    }
  }
  walk(rootPath);
  results.sort();
  return results;
}

// --- Format a source location for comparison ---
function formatLoc(loc: unknown): string {
  if (loc == null) return '(generated)';
  if (typeof loc === 'symbol') return '(generated)';
  const l = loc as Record<string, unknown>;
  const start = l.start as Record<string, unknown> | undefined;
  const end = l.end as Record<string, unknown> | undefined;
  if (start && end) {
    return `${start.line}:${start.column}-${end.line}:${end.column}`;
  }
  return String(loc);
}

/**
 * Compile a fixture through the TypeScript Babel plugin in-process and capture
 * the per-pass HIR debug log (entries) plus any compile-error events, stopping
 * after `targetPass`.
 *
 * This is the TS side of the oracle. The Rust side is obtained separately (via
 * NAPI bridge or the OXC CLI `--dump-hir`).
 */
export function compileFixtureTS(
  tsPlugin: babel.PluginItem,
  fixturePath: string,
  targetPass: string,
  compilationModeArg: string | null,
): CompileOutput {
  const source = fs.readFileSync(fixturePath, 'utf8');
  const firstLine = source.substring(0, source.indexOf('\n'));

  const pragmaOpts = parseConfigPragmaForTests(firstLine, {
    compilationMode: 'all',
  });

  const log: LogItem[] = [];
  let reachedTarget = false;

  const logger = {
    logEvent(_filename: string | null, event: Record<string, unknown>): void {
      if (reachedTarget) return;
      const kind = event.kind as string;
      if (
        kind === 'CompileError' ||
        kind === 'CompileSkip' ||
        kind === 'PipelineError'
      ) {
        const fnName = (event.fnName as string | null) ?? null;
        let detail: string;
        if (kind === 'CompileError') {
          const d = event.detail as Record<string, unknown> | undefined;
          if (d) {
            const lines = [
              `reason: ${d.reason ?? '(none)'}`,
              `severity: ${d.severity ?? '(none)'}`,
              `category: ${d.category ?? '(none)'}`,
            ];
            if (d.description) {
              lines.push(`description: ${d.description}`);
            }
            const opts = (d as Record<string, unknown>).options as
              | Record<string, unknown>
              | undefined;
            const details = (opts?.details ?? d.details) as
              | Array<Record<string, unknown>>
              | undefined;
            if (details && details.length > 0) {
              for (const item of details) {
                if (item.kind === 'error') {
                  lines.push(
                    `  error: ${formatLoc(item.loc)}${item.message ? ': ' + item.message : ''}`,
                  );
                } else if (item.kind === 'hint') {
                  lines.push(`  hint: ${item.message ?? ''}`);
                }
              }
            }
            if (d.loc && !details) {
              lines.push(`loc: ${formatLoc(d.loc)}`);
            }
            detail = lines.join('\n    ');
          } else {
            detail = '(no detail)';
          }
        } else if (kind === 'CompileSkip') {
          detail = (event.reason as string) ?? '(no reason)';
        } else {
          detail = (event.data as string) ?? '(no data)';
        }
        log.push({kind: 'event', eventKind: kind, fnName, detail});
      }
    },
    debugLogIRs(entry: CompilerPipelineValue): void {
      if (reachedTarget) return;
      if (entry.name === 'EnvironmentConfig') return;
      if (entry.kind === 'hir') {
        log.push({
          kind: 'entry',
          name: entry.name,
          value: printDebugHIR(entry.value),
        });
      } else if (entry.kind === 'debug') {
        log.push({kind: 'entry', name: entry.name, value: entry.value});
      } else if (entry.kind === 'reactive') {
        log.push({
          kind: 'entry',
          name: entry.name,
          value: printDebugReactiveFunction(entry.value),
        });
      } else if (entry.kind === 'ast' && entry.name === targetPass) {
        throw new Error(
          `TODO: HIR oracle does not yet support '${entry.kind}' log entries ` +
            `(pass "${entry.name}"). Extend the debugLogIRs handler to support this kind.`,
        );
      }
      if (entry.name === targetPass) {
        reachedTarget = true;
      }
    },
  };

  const headerBlock = source.substring(0, source.indexOf('*/') + 2 || 200);
  const isFlow = headerBlock.includes('@flow');
  const isScript = firstLine.includes('@script');

  const pluginOptions = {
    ...pragmaOpts,
    ...(compilationModeArg != null
      ? {compilationMode: compilationModeArg}
      : {}),
    panicThreshold: 'all_errors' as const,
    logger,
  };

  const babelPlugins: Array<babel.PluginItem> = isFlow
    ? [hermesParserPlugin, [tsPlugin, pluginOptions]]
    : [[tsPlugin, pluginOptions]];

  let error: string | null = null;
  let code: string | null = null;
  try {
    const result = babel.transformSync(source, {
      filename: fixturePath,
      sourceType: isScript ? 'script' : 'module',
      ...(isFlow ? {} : {parserOpts: {plugins: ['typescript', 'jsx']}}),
      plugins: babelPlugins,
      configFile: false,
      babelrc: false,
    });
    code = result?.code ?? null;
  } catch (e) {
    error = e instanceof Error ? e.message : String(e);
  }

  return {log, code, error};
}

// --- Format a single log item as comparable string ---
export function formatLogItem(item: LogItem): string {
  if (item.kind === 'entry') {
    return `## ${item.name}\n${item.value}`;
  } else {
    return `[${item.eventKind}]${item.fnName ? ' ' + item.fnName : ''}: ${item.detail}`;
  }
}

// --- Format log items as comparable string ---
export function formatLog(log: LogItem[]): string {
  return log.map(formatLogItem).join('\n');
}

/**
 * Normalize opaque IDs so the TS and Rust HIR text can be compared.
 *
 * Type IDs, Identifier IDs, declarationIds, block IDs (bbN), and <generated_N>
 * shape IDs are opaque counters whose absolute values differ between TS and
 * Rust due to allocation order. Each unique ID is remapped to a sequential
 * index. mutableRange is stripped (shares a reference with scope.range in TS
 * but is a copy in Rust; scope.range is validated separately).
 *
 * Maps reset at each `## HIR` boundary because the Rust port creates a fresh
 * Environment per function (so raw IDs can collide across functions) while TS
 * uses a global counter.
 */
export function normalizeIds(text: string): string {
  let typeMap = new Map<string, number>();
  let nextTypeId = 0;
  let idMap = new Map<string, number>();
  let nextIdId = 0;
  let declMap = new Map<string, number>();
  let nextDeclId = 0;
  let generatedMap = new Map<string, number>();
  let nextGeneratedId = 0;
  let blockMap = new Map<string, number>();
  let nextBlockId = 0;
  let isFirstHIR = true;

  const lines = text.split('\n');
  const result = lines.map(line => {
    if (line === '## HIR') {
      if (!isFirstHIR) {
        typeMap = new Map();
        nextTypeId = 0;
        idMap = new Map();
        nextIdId = 0;
        declMap = new Map();
        nextDeclId = 0;
        generatedMap = new Map();
        nextGeneratedId = 0;
        blockMap = new Map();
        nextBlockId = 0;
      }
      isFirstHIR = false;
    }

    return line
      .replace(/\bbb(\d+)\b/g, (_match, num) => {
        const key = `bb:${num}`;
        if (!blockMap.has(key)) {
          blockMap.set(key, nextBlockId++);
        }
        return `bb${blockMap.get(key)}`;
      })
      .replace(/<generated_(\d+)>/g, (_match, num) => {
        const key = `generated:${num}`;
        if (!generatedMap.has(key)) {
          generatedMap.set(key, nextGeneratedId++);
        }
        return `<generated_${generatedMap.get(key)}>`;
      })
      .replace(/Type\(\d+\)/g, match => {
        if (!typeMap.has(match)) {
          typeMap.set(match, nextTypeId++);
        }
        return `Type(${typeMap.get(match)})`;
      })
      .replace(/((?:id|declarationId): )(\d+)/g, (_match, prefix, num) => {
        if (prefix === 'id: ') {
          const key = `id:${num}`;
          if (!idMap.has(key)) {
            idMap.set(key, nextIdId++);
          }
          return `${prefix}${idMap.get(key)}`;
        } else {
          const key = `decl:${num}`;
          if (!declMap.has(key)) {
            declMap.set(key, nextDeclId++);
          }
          return `${prefix}${declMap.get(key)}`;
        }
      })
      .replace(/Identifier\((\d+)\)/g, (_match, num) => {
        const key = `id:${num}`;
        if (!idMap.has(key)) {
          idMap.set(key, nextIdId++);
        }
        return `Identifier(${idMap.get(key)})`;
      })
      .replace(/(\w+)\$(\d+)/g, (_match, name, num) => {
        const key = `id:${num}`;
        if (!idMap.has(key)) {
          idMap.set(key, nextIdId++);
        }
        return `${name}\$${idMap.get(key)}`;
      })
      .replace(/mutableRange: \[\d+:\d+\]/g, 'mutableRange: [_:_]');
  });
  return result.join('\n');
}

/**
 * Find the earliest diverging pass for a fixture given the two per-pass logs.
 *
 * Walks the two logs in lockstep; the first pass whose normalized text differs
 * (or the pass where one log runs out) is the "frontier". `passOrder` is used
 * as a fallback when divergence can't be attributed to a specific entry.
 */
export function findDivergencePass(
  tsLog: LogItem[],
  rustLog: LogItem[],
  passOrder: string[],
): string {
  const maxLen = Math.max(tsLog.length, rustLog.length);
  for (let i = 0; i < maxLen; i++) {
    const tsItem = i < tsLog.length ? tsLog[i] : undefined;
    const rustItem = i < rustLog.length ? rustLog[i] : undefined;

    if (tsItem === undefined || rustItem === undefined) {
      const item = tsItem ?? rustItem;
      if (item && item.kind === 'entry') {
        return item.name;
      }
      for (let j = i - 1; j >= 0; j--) {
        const prev = tsLog[j] ?? rustLog[j];
        if (prev && prev.kind === 'entry') return prev.name;
      }
      return passOrder[0];
    }

    const tsFormatted = normalizeIds(formatLogItem(tsItem));
    const rustFormatted = normalizeIds(formatLogItem(rustItem));
    if (tsFormatted !== rustFormatted) {
      if (tsItem.kind === 'entry') {
        return tsItem.name;
      }
      for (let j = i - 1; j >= 0; j--) {
        if (tsLog[j] && tsLog[j].kind === 'entry') {
          return (tsLog[j] as LogEntry).name;
        }
      }
      return passOrder[0];
    }
  }
  return passOrder[0];
}

/**
 * Parse the OXC CLI `--dump-hir` stdout into LogItem entries.
 *
 * The CLI emits blocks of `## <PassName>\n<hir>` joined by single newlines
 * (matching `formatLog`). Only HIR entries are emitted (no events), so every
 * block becomes a `kind: 'entry'` item.
 */
export function parseDumpHir(stdout: string): LogItem[] {
  const text = stdout.replace(/\n$/, '');
  if (text.length === 0) return [];
  const items: LogItem[] = [];
  // Split on lines that are exactly a `## <Name>` header, keeping the header.
  const lines = text.split('\n');
  let current: {name: string; value: string[]} | null = null;
  const headerRe = /^## (.+)$/;
  for (const line of lines) {
    const m = line.match(headerRe);
    if (m) {
      if (current) {
        items.push({
          kind: 'entry',
          name: current.name,
          value: current.value.join('\n'),
        });
      }
      current = {name: m[1], value: []};
    } else if (current) {
      current.value.push(line);
    }
  }
  if (current) {
    items.push({
      kind: 'entry',
      name: current.name,
      value: current.value.join('\n'),
    });
  }
  return items;
}
