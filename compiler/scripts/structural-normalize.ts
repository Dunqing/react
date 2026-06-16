/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */

/**
 * structuralNormalize(code): returns a canonical form of a formatted compiler
 * output such that two outputs differing ONLY by the following known-equivalent
 * dimensions normalize to the SAME string:
 *
 *   1. JSX self-closing vs explicit close: `<X />` === `<X></X>`.
 *   2. Leading / inner comments and `'use ...'` directive prologues (oxc drops,
 *      TS preserves). All comments / directives are stripped.
 *   3. Compiler temporary-name divergence (`h` vs `t0`, etc.), the order in
 *      which hoisted `let x;` scaffolding is declared, AND `$[N]` cache-slot
 *      index ordering. Handled via: dropping bare uninitialized declarations,
 *      alpha-renaming temp-like locals to a canonical first-use sequence
 *      (v0, v1, ...), masking `$[N]`/`_c(N)` indices, and sorting the
 *      known-commutative cache-store runs (`$[#] = e;`) and cache-compare guard
 *      chains (`$[#] !== x || ...`).
 *   4. Declaration kind (`let`/`const`/`var`) and declaration-vs-bare-assignment
 *      (`let x = e` vs `x = e`) — both collapsed to a bare assignment.
 *   5. Quote style, trailing semicolons, `x["a"]` vs `x.a`, whitespace —
 *      regenerated compact with consistent options + a textual cleanup pass.
 *
 * Approach is AST-based via @babel/parser + traverse + generator. On ANY
 * failure we fall back to a purely textual normalization. Always returns a
 * string.
 *
 * NOTE on slot masking: masking `$[N]` / `_c(N)` indices and sorting the
 * commutative cache-store / guard runs collapses cache-slot ORDERING
 * permutations (known-equivalent here), but also hides a genuine slot-COUNT
 * bug if oxc allocated a different number of slots, and hides a guard/store
 * that references the WRONG variable if it happens to sort to the same place.
 * That is an accepted tradeoff for the COSMETIC/OTHER decision: the goal is to
 * keep OTHER limited to genuine wrong-code, and cache-slot ordering is the
 * dominant false-positive.
 */

import {parse} from '@babel/parser';
import _traverse from '@babel/traverse';
import _generate from '@babel/generator';
import * as t from '@babel/types';

const traverse: typeof import('@babel/traverse').default =
  (_traverse as any).default ?? _traverse;
const generate: typeof import('@babel/generator').default =
  (_generate as any).default ?? _generate;

/**
 * Purely textual canonicalization. Used both as the AST fallback and as a final
 * cleanup pass over generated output.
 */
function textualNormalize(code: string): string {
  let c = code;
  // Strip block comments and line comments (heuristic; safe for the fallback).
  c = c.replace(/\/\*[\s\S]*?\*\//g, '');
  c = c.replace(/(^|[^:])\/\/[^\n]*/g, '$1');
  // Drop directive prologues like 'use memo' / "use no memo".
  c = c.replace(/^\s*['"]use [^'"]*['"];?\s*$/gm, '');
  // Member access:  x["a"]  =>  x.a   for identifier-like keys.
  c = c.replace(/\[(['"])([A-Za-z_$][A-Za-z0-9_$]*)\1\]/g, '.$2');
  // Mask cache-slot indices and _c() count.
  c = c.replace(/\$\[\s*\d+\s*\]/g, '$[#]');
  c = c.replace(/_c\(\s*\d+\s*\)/g, '_c(#)');
  // Normalize declaration kind.
  c = c.replace(/\b(const|var)\b/g, 'let');
  // Collapse whitespace.
  c = c
    .split('\n')
    .map(l => l.replace(/\s+/g, ' ').trim())
    .filter(l => l.length > 0)
    .join('\n');
  // <Tag ...></Tag>  =>  <Tag .../>   (no children between).
  c = c.replace(/<([A-Za-z_$][\w.$]*)((?:\s+[^<>]*?)?)>\s*<\/\1>/g, '<$1$2/>');
  // Normalize self-closing spacing.
  c = c.replace(/\s*\/>/g, '/>');
  // Drop trailing semicolons and unify quotes.
  c = c.replace(/;+/g, ';');
  c = c.replace(/;\s*$/gm, '');
  c = c.replace(/'/g, '"');
  c = c.replace(/[ \t]+/g, ' ').trim();
  return c;
}

const TEMP_NAME = /^(?:t\d+|_temp\d*|[a-z])$/;

function isCacheMember(node: t.Node | null | undefined): boolean {
  return (
    t.isMemberExpression(node) &&
    t.isIdentifier(node.object) &&
    node.object.name === '$'
  );
}

/**
 * True for a commutative cache access statement: either a cache STORE
 * (`$[#] = e;`) or a cache LOAD (`x = $[#];`). Both write/read independent cache
 * slots and are order-independent within a contiguous run.
 */
function isCacheAccess(node: t.Node): boolean {
  if (
    !t.isExpressionStatement(node) ||
    !t.isAssignmentExpression(node.expression) ||
    node.expression.operator !== '='
  ) {
    return false;
  }
  const {left, right} = node.expression;
  // Store: `$[#] = <anything>`.
  if (isCacheMember(left)) return true;
  // Load: `<id> = $[#]`.
  if (t.isIdentifier(left) && isCacheMember(right)) return true;
  return false;
}

/**
 * Move all `FunctionDeclaration` statements in a body to the end, sorted by
 * printed form. Function declarations are hoisted in JS, so their textual
 * position relative to other statements is semantically irrelevant; TS and oxc
 * emit hoisted helper functions in different positions (e.g. a helper before
 * vs after an `export const ...`). Canonicalizing to "sorted, at the end" makes
 * those equivalent.
 */
function hoistFunctionDeclarations(body: t.Statement[]): t.Statement[] {
  const fns: t.Statement[] = [];
  const rest: t.Statement[] = [];
  for (const stmt of body) {
    if (t.isFunctionDeclaration(stmt)) fns.push(stmt);
    else rest.push(stmt);
  }
  if (fns.length === 0) return body;
  fns.sort((a, b) =>
    generate(a, {compact: true}).code < generate(b, {compact: true}).code
      ? -1
      : 1,
  );
  return [...rest, ...fns];
}

/**
 * Sort maximal runs of consecutive cache-access statements (`$[#] = e;` stores
 * and `x = $[#];` loads). These access independent cache slots and are
 * commutative, so sorting by their printed form canonicalizes ordering
 * permutations. We only reorder within a contiguous run so we never move a
 * statement across an intervening non-cache statement.
 */
function sortCacheStoreRuns(body: t.Statement[]): t.Statement[] {
  const out: t.Statement[] = [];
  let i = 0;
  while (i < body.length) {
    if (isCacheAccess(body[i])) {
      let j = i;
      while (j < body.length && isCacheAccess(body[j])) j++;
      const run = body.slice(i, j);
      run.sort((a, b) =>
        generate(a, {compact: true}).code <
        generate(b, {compact: true}).code
          ? -1
          : 1,
      );
      out.push(...run);
      i = j;
    } else {
      out.push(body[i]);
      i++;
    }
  }
  return out;
}

/**
 * Canonicalize a commutative chain of `$[#] !== x` comparisons joined by a
 * single logical operator (`||` or `&&`). The operands are independent cache
 * freshness checks, so sorting them by printed form canonicalizes ordering.
 */
function sortGuardChain(node: t.LogicalExpression): void {
  const op = node.operator;
  // Flatten the chain.
  const parts: t.Expression[] = [];
  function flatten(e: t.Expression): void {
    if (t.isLogicalExpression(e) && e.operator === op) {
      flatten(e.left);
      flatten(e.right);
    } else {
      parts.push(e);
    }
  }
  flatten(node);
  // Only treat as a sortable guard chain if every leaf is a `$[#] !== x`
  // (or `===`) comparison against the cache. Otherwise leave order alone.
  const allCacheCompares = parts.every(
    p =>
      t.isBinaryExpression(p) &&
      (p.operator === '!==' || p.operator === '===') &&
      t.isMemberExpression(p.left) &&
      t.isIdentifier(p.left.object) &&
      p.left.object.name === '$',
  );
  if (!allCacheCompares || parts.length < 2) return;
  parts.sort((a, b) =>
    generate(a, {compact: true}).code < generate(b, {compact: true}).code
      ? -1
      : 1,
  );
  // Rebuild a left-leaning chain in sorted order.
  let rebuilt: t.Expression = parts[0];
  for (let i = 1; i < parts.length; i++) {
    rebuilt = t.logicalExpression(op, rebuilt, parts[i]);
  }
  node.operator = (rebuilt as t.LogicalExpression).operator;
  node.left = (rebuilt as t.LogicalExpression).left;
  node.right = (rebuilt as t.LogicalExpression).right;
}

function structuralNormalizeAst(code: string): string {
  const ast = parse(code, {
    sourceType: 'unambiguous',
    plugins: ['typescript', 'jsx'],
    errorRecovery: true,
    attachComment: false,
  });

  // --- Pass 0: strip directives and canonicalize literal escaping. ---
  // Directives (`'use strict'`, `'worklet'`, etc.) are preserved by TS but
  // dropped by oxc; remove them everywhere. Re-encode string literals and
  // template raw text by clearing `.extra.raw` so the generator emits a
  // canonical escaping (e.g. TS `"ŧ"` vs oxc `"ŧ"` are the same value).
  traverse(ast, {
    Directive(path: any) {
      path.remove();
    },
    StringLiteral(path: any) {
      // Drop cached raw text so the generator re-encodes from `.value`,
      // canonicalizing escaping differences (e.g. `ŧ` vs the literal char).
      if (path.node.extra != null) {
        delete path.node.extra.raw;
        delete path.node.extra.rawValue;
      }
    },
    NumericLiteral(path: any) {
      // Drop cached raw text so the generator re-encodes from `.value`,
      // canonicalizing equivalent numeric spellings of the SAME IEEE-754
      // value (e.g. babel's `1000` vs oxc's minified `1e3`, or `2.18e22` vs
      // `218e8`). These are the identical value with identical runtime
      // semantics; the difference is purely the printer's shortest-form
      // choice, which oxc's codegen applies unconditionally. Mirrors the
      // StringLiteral canonicalization above.
      if (path.node.extra != null) {
        delete path.node.extra.raw;
        delete path.node.extra.rawValue;
      }
    },
  });

  // --- Pass 1: mask cache-slot indices in the AST. ---
  // Replace every `$[<int>]` index and `_c(<int>)` arg with a constant `0`, so
  // that downstream commutative-run sorting keys depend on the stored VALUE
  // rather than the (permutation-varying) slot index.
  traverse(ast, {
    MemberExpression(path: any) {
      if (
        t.isIdentifier(path.node.object) &&
        path.node.object.name === '$' &&
        path.node.computed &&
        t.isNumericLiteral(path.node.property)
      ) {
        path.node.property = t.numericLiteral(0);
      }
    },
    CallExpression(path: any) {
      if (
        t.isIdentifier(path.node.callee) &&
        path.node.callee.name === '_c' &&
        path.node.arguments.length === 1 &&
        t.isNumericLiteral(path.node.arguments[0])
      ) {
        path.node.arguments[0] = t.numericLiteral(0);
      }
    },
  });

  // --- Pass 2: alpha-rename temp-like names by first-appearance order. ---
  // We rename by NAME (not by binding identity). Compiler temporaries have
  // unique names per function, so a name-based mapping correctly unifies all
  // occurrences regardless of whether one side emits a block-scoped `const x`
  // (a fresh binding) where the other reuses a function-scoped `let x` (the
  // binding-identity approach diverges in exactly that case). An identifier is
  // renameable only when it appears in a "real" identifier position (a value
  // reference or a binding site), never as a member-property name, a
  // non-computed object/class key, a JSX attribute name, or an import/export
  // external specifier name. We skip bare `let x;` declaration sites so the
  // canonical numbering follows first real use (declaration order differs
  // between TS and oxc).
  const nameMap = new Map<string, string>();
  let counter = 0;
  function canonicalFor(name: string): string {
    let c = nameMap.get(name);
    if (c == null) {
      c = `__v${counter++}__`;
      nameMap.set(name, c);
    }
    return c;
  }
  // An Identifier path is in a renameable (value/binding) position.
  function isRenameablePosition(path: any): boolean {
    const {parent, node} = path;
    if (parent == null) return false;
    // Member property: `a.b` (non-computed) -> skip `b`.
    if (t.isMemberExpression(parent) && parent.property === node && !parent.computed) {
      return false;
    }
    if (
      t.isOptionalMemberExpression(parent) &&
      parent.property === node &&
      !parent.computed
    ) {
      return false;
    }
    // Object property key (non-computed) -> skip.
    if (
      (t.isObjectProperty(parent) || t.isObjectMethod(parent)) &&
      parent.key === node &&
      !parent.computed
    ) {
      return false;
    }
    // Class property/method key (non-computed) -> skip.
    if (
      (t.isClassProperty(parent) ||
        t.isClassMethod(parent) ||
        t.isClassPrivateProperty(parent)) &&
      parent.key === node &&
      !(parent as any).computed
    ) {
      return false;
    }
    // JSX attribute name -> skip.
    if (t.isJSXAttribute(parent) && parent.name === node) return false;
    // import/export specifier external (`imported`/`exported`) names -> skip.
    if (
      (t.isImportSpecifier(parent) ||
        t.isExportSpecifier(parent)) &&
      (parent as any).imported === node
    ) {
      return false;
    }
    if (t.isExportSpecifier(parent) && parent.exported === node) return false;
    // Label identifiers (`break foo`) -> skip (rare; keep as-is).
    if (
      t.isLabeledStatement(parent) ||
      t.isBreakStatement(parent) ||
      t.isContinueStatement(parent)
    ) {
      return false;
    }
    return true;
  }
  // First pass: assign canonical names in first-appearance order.
  traverse(ast, {
    Identifier(path: any) {
      const name = path.node.name;
      if (name === '$' || name === '_c') return;
      if (!TEMP_NAME.test(name)) return;
      if (!isRenameablePosition(path)) return;
      // Skip bare uninitialized `let x;` declarator id positions for ordering.
      const parent = path.parent;
      if (
        t.isVariableDeclarator(parent) &&
        parent.id === path.node &&
        parent.init == null
      ) {
        return;
      }
      canonicalFor(name);
    },
  });
  // Second pass: rewrite every renameable occurrence using the map.
  traverse(ast, {
    Identifier(path: any) {
      const name = path.node.name;
      if (!nameMap.has(name)) return;
      if (!isRenameablePosition(path)) return;
      path.node.name = nameMap.get(name)!;
    },
  });

  // --- Pass 2b: drop bare uninitialized `let x;` hoisted declarations. ---
  // Pure SSA scaffolding; emitted in inconsistent order. Removing them (after
  // renaming, so bindings still resolved above) neutralizes declaration order.
  // Never touch declarations that are a for-loop / for-in / for-of head, since
  // those legitimately have no initializer and are structurally required.
  traverse(ast, {
    VariableDeclaration(path: any) {
      const parent = path.parent;
      if (
        t.isForStatement(parent) && parent.init === path.node
      ) {
        return;
      }
      if (
        (t.isForOfStatement(parent) || t.isForInStatement(parent)) &&
        parent.left === path.node
      ) {
        return;
      }
      const kept = path.node.declarations.filter(
        (d: t.VariableDeclarator) => d.init != null,
      );
      if (kept.length === 0) {
        path.remove();
      } else if (kept.length !== path.node.declarations.length) {
        path.node.declarations = kept;
      }
    },
  });

  // --- Pass 3: structural canonicalizations. ---
  traverse(ast, {
    // JSX self-closing: empty element -> self-closing.
    JSXElement(path: any) {
      const children = path.node.children.filter((ch: any) => {
        if (t.isJSXText(ch)) return ch.value.trim() !== '';
        return true;
      });
      if (children.length === 0) {
        path.node.children = [];
        path.node.openingElement.selfClosing = true;
        path.node.closingElement = null;
      }
    },
    // Declaration -> bare assignment (collapses `let x = e`, `const x = e`,
    // and `x = e`). Only for initialized single declarators; multi-declarator
    // and uninitialized forms are left (uninitialized were already dropped).
    VariableDeclaration(path: any) {
      if (
        path.node.declarations.length === 1 &&
        path.node.declarations[0].init != null &&
        t.isIdentifier(path.node.declarations[0].id) &&
        // Don't rewrite for-loop heads etc.
        (path.parent == null ||
          t.isBlockStatement(path.parent) ||
          t.isProgram(path.parent))
      ) {
        const d = path.node.declarations[0];
        path.replaceWith(
          t.expressionStatement(
            t.assignmentExpression('=', d.id as t.LValue, d.init!),
          ),
        );
      } else if (path.node.kind !== 'let') {
        path.node.kind = 'let';
      }
    },
    // Sort commutative cache-compare guard chains.
    LogicalExpression(path: any) {
      sortGuardChain(path.node);
    },
    // Drop empty `else {}` blocks (no-op; one side emits them, the other not).
    IfStatement(path: any) {
      if (
        path.node.alternate != null &&
        t.isBlockStatement(path.node.alternate) &&
        path.node.alternate.body.length === 0
      ) {
        path.node.alternate = null;
      }
    },
  });

  // --- Pass 4: sort commutative cache-store statement runs + hoist function
  // declarations to a canonical position in every block. ---
  traverse(ast, {
    BlockStatement(path: any) {
      path.node.body = hoistFunctionDeclarations(
        sortCacheStoreRuns(path.node.body),
      );
    },
    Program(path: any) {
      path.node.body = hoistFunctionDeclarations(
        sortCacheStoreRuns(path.node.body),
      );
    },
  });

  // --- Pass 5: merge + sort the leading import-declaration run. ---
  // Import order is benign here (all imports are side-effect-free module
  // bindings), and TS may merge several specifiers from the same source into
  // one declaration where oxc emits separate declarations (or vice versa). We
  // merge same-source imports, sort each declaration's specifiers, then sort
  // the declarations — collapsing both ordering and split/merge differences.
  {
    const body = (ast.program as t.Program).body;
    let end = 0;
    while (end < body.length && t.isImportDeclaration(body[end])) end++;
    if (end > 0) {
      const imports = body.slice(0, end) as t.ImportDeclaration[];
      // Merge by source value. Preserve first-seen order of sources.
      const bySource = new Map<string, t.ImportDeclaration>();
      for (const imp of imports) {
        const key = imp.source.value;
        const existing = bySource.get(key);
        if (existing == null) {
          bySource.set(key, imp);
        } else {
          existing.specifiers.push(...imp.specifiers);
        }
      }
      const merged = [...bySource.values()];
      // Sort specifiers within each declaration.
      for (const imp of merged) {
        imp.specifiers.sort((a, b) =>
          generate(a, {compact: true}).code <
          generate(b, {compact: true}).code
            ? -1
            : 1,
        );
      }
      // Sort declarations by printed form.
      merged.sort((a, b) =>
        generate(a, {compact: true}).code < generate(b, {compact: true}).code
          ? -1
          : 1,
      );
      (ast.program as t.Program).body = [...merged, ...body.slice(end)];
    }
  }

  let out = generate(ast, {
    compact: true,
    comments: false,
    jsescOption: {quotes: 'double'},
  }).code;

  out = textualNormalize(out);
  return out;
}

export function structuralNormalize(code: string): string {
  if (code == null || code.trim() === '') return '';
  try {
    return structuralNormalizeAst(code);
  } catch {
    return textualNormalize(code);
  }
}
