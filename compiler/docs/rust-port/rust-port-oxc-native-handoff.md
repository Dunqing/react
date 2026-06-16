# React Compiler — Native Oxc Migration: Status & Handoff

**Branch:** `oxc-migration` (a deliberate fork; diverges from upstream #36743, which moved OXC/SWC
integration out of this repo — see [[react-compiler-oxc-migration]] memory).

## What this migration did

Replaced the React Compiler Rust port's hand-written, Babel-shaped AST + scope representation with
**Oxc as the native AST + semantic foundation**. The entire compile pipeline now runs on Oxc with no
Babel and no intermediate AST:

```
source → oxc_parser → oxc_semantic → native lowering (build_hir) → analysis/reactive-scope passes
       → native codegen (codegen_oxc, AstBuilder) → oxc_codegen → output source
```

- `react_compiler_ast` (the ~4,400-line hand-written Babel AST) and the hand-rolled `ScopeInfo`:
  **DELETED**. Scope/binding is queried directly from `oxc_semantic` via
  `react_compiler_lowering/src/semantic_queries.rs` (free functions, no intermediate scope struct).
- The legacy Babel NAPI bridge (`packages/babel-plugin-react-compiler-rust`), `bridge.ts`,
  `babel-ast-to-json.mjs`, `test-babel-ast.sh`: **DELETED** (Babel is no longer in any path).

## Current correctness (oracles)

Two oracles, both comparing the native Oxc compiler against the in-process TS compiler over the
~1,803-fixture corpus (`compiler/packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler/`):

- **`compiler/scripts/compare-code.ts`** — SEMANTIC equivalence of compiled output (structural: alpha-
  renames temporaries, masks `$[N]`/`_c(N)` slot indices, normalizes JSX self-close / decl-kind /
  comments). This is the PRIMARY metric. `tsx compiler/scripts/compare-code.ts --limit 0` (corpus),
  `tsx compiler/scripts/compare-code.ts <fixture>` (single, prints diff), `--list BAIL|OTHER|N-VAL`.
  **SEMANTIC-pass ≈ 1673/1803 (92.8%).**
- **`compiler/scripts/compare-hir.ts`** — per-pass HIR diff (printer-independent). **HIR-MATCH ≈ 1445/1803**
  byte-identical to the TS compiler. (Many semantically-correct fixtures differ only in HIR temp/block
  ID *numbering*, which compiles identically — so HIR-MATCH < SEMANTIC-pass by design.)

Toolchain: requires **rustc 1.92.0** (oxc 0.121); `rustup run 1.92.0 cargo …` from `compiler/`. Node
scripts run via `tsx`. The CLI is `react_compiler_e2e_cli` (`--frontend oxc`, `--dump-hir`, `--json`).

## Architecture map (key files)

| Concern | File |
|---|---|
| Scope/binding queries (oxc_semantic) | `react_compiler_lowering/src/semantic_queries.rs` |
| Lowering (oxc_ast → HIR) | `react_compiler_lowering/src/build_hir/{mod,statements,expressions,jsx,patterns,functions,hoisting}.rs` |
| Discovery + Component/Hook classification | `react_compiler/src/entrypoint/program.rs` (`getComponentOrHookLike` port) |
| Native codegen (HIR → oxc_ast) | `react_compiler_reactive_scopes/src/codegen_oxc.rs` |
| Output assembly (splice + print) | `react_compiler_oxc/src/codegen_assembly.rs` (re-parse source into fresh Allocator, splice compiled fns by span, inject `_c` import, `oxc_codegen`) |
| Memo-stat counting | `react_compiler/src/entrypoint/...count_memo_blocks` |
| Pipeline / entry | `react_compiler/src/entrypoint/{pipeline.rs,program.rs}`; `compile_program(&Program, &Semantic, source, options)` |

The analysis/inference/reactive-scope passes (~60k lines) were NOT rewritten — they operate on HIR and
were already AST-agnostic. Only lowering, discovery, and codegen were retargeted to Oxc.

## Deferred / remaining work (~130 fixtures to reach full corpus parity)

This was scoped to **semantic parity excluding big opt-in features** (user decision). Remaining buckets
(regenerate exact lists with `compare-code.ts --list {BAIL|OTHER|N-VAL}`):

### Deferred opt-in/pragma features (~50)
- **`@gating` dynamic gating** (~16) — emit the gated-export wrapper. `gating.rs` was removed in cleanup; re-add the codegen.
- **`enableJsxOutlining`** (~10) — outline JSX subtrees into separate components (distinct from the
  function-outlining already implemented in N2.8).
- **fbt / fbs** (~22 BAIL + ~2 OTHER) — `<fbt>`/`<fbs>` JSX is its own transform subsystem; the TS test
  path even runs `babel-plugin-fbt` preprocessing. Native codegen bails on fbt JSX tags.
- **`@enableEmitInstrumentForget` instrumentation** (~2), **optimizeForSSR / SSR mode** (~1).

### Lowering Todos / inference invariants (8 N-VAL + a few BAIL)
- `lowerReorderableExpression` BuildHIR path (default-param-accesses-local, object get/set in expr position, eval-unsupported).
- `ValidateSourceLocations` — needs codegen source-location tracking (currently skipped).
- `ValidateContextVariableLValues` / `InferMutationAliasingEffects` invariants on a couple repro fixtures
  (forcing them risks false-positives on the 1600+ passing fixtures — deferred deliberately).
- **TypeCastExpression codegen** (~3) — needs a JSON→oxc-TSType annotation converter.
- Destructuring of context variables in lowering (~2, explicit Todo).

### Linting / oxlint integration (capability exists; oxlint Rule deferred)
`react_compiler_oxc` exposes `lint(program, semantic, source_text, options) -> Vec<OxcDiagnostic>` and
`lint_source(...)` — native linting on the oxc AST + semantic, no Babel. Wiring this as an actual
`oxc_linter::Rule` was deferred: **`oxc_linter` is not published to crates.io** (the `Rule` trait is
internal to the oxc monorepo + a generated registry), and oxc maintainers recommend JS plugins for
external custom rules. The oxlint Rule registration therefore belongs in the oxc project (consistent
with upstream #36743). See `rust-port-oxc.md` for the deferral note.

### Scattered single-cause OTHER (~26)
Each a distinct small fix, no common root: `-0` constant-propagation formatting, arrow concise-body
passthrough printing, `enableNameAnonymousFunctions` naming, lone-surrogate string round-trip
(`lone-surrogate-string-values.js` — an oxc_codegen printer-level issue), `jsx-preserve-whitespace.tsx`
multi-line text line-join, etc.

## Working notes for whoever continues

- The original react_compiler_ast lowering/codegen LOGIC (deleted) is the transcription reference in git:
  lowering at commit **`1a02788`**; the dead HIR→react_compiler_ast codegen + `convert_ast_reverse`
  (oxc AstBuilder construction patterns) at the commit **before `bc9d4ea244`** (N2.final). The TS source
  under `compiler/packages/babel-plugin-react-compiler/src/` is the ultimate reference.
- Driving metric for any change: keep `compare-code.ts` SEMANTIC-pass from regressing; a lowering/inference
  fix should hold or raise `compare-hir.ts` HIR-MATCH. Use `--list` + single-fixture diff to localize.
- oxc 0.121 specifics that bit repeatedly: `BindingPattern` IS the enum (no `.kind`); `MemberExpression`
  split into Static/Computed/PrivateField; optional chaining is one `ChainExpression` over plain members
  (NOT Babel per-link nesting); `symbol_id`/`reference_id` are `Cell`s; `oxc_ast::Expression` isn't `Clone`.
