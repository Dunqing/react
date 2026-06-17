# React Compiler — Native Oxc Migration: Status & Handoff

**TL;DR.** The React Compiler's Rust port now runs **fully natively on Oxc** (no Babel, no hand-written
AST): **96.4% semantic parity** (1738/1803 fixtures) with the TypeScript compiler, **~8× faster than
TS/Babel** and **~6× faster than the prior Babel-AST Rust port**, **~22% smaller binary**, on
**oxc 0.136 / rustc 1.94**, clippy-clean, and synced with upstream `main`. The remaining ~65 fixtures are
deferred opt-in features (fbt, SSR, JSX-outlining, instrumentation) plus a scattered single-cause tail.

**Branch:** `oxc-migration` — a **deliberate fork** that diverges from upstream React #36743 (which removed
in-repo OXC/SWC integration, intending those to live in the OXC project and consume `react_compiler` as a
crate). This branch instead makes Oxc the compiler's *native* AST + scope core.

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
  comments / numeric-literal form). This is the PRIMARY metric. `tsx compiler/scripts/compare-code.ts --limit 0` (corpus),
  `tsx compiler/scripts/compare-code.ts <fixture>` (single, prints diff), `--list BAIL|OTHER|N-VAL`.
  **SEMANTIC-pass ≈ 1738/1803 (96.4%)** (after @gating + scattered-tail + jsx-outlining + React.memo/forwardRef
  discovery; was 1673 at migration-complete). Remaining 65: N-VAL 9, BAIL 36 (~23 fbt), OTHER 20.
- **`compiler/scripts/compare-hir.ts`** — per-pass HIR diff (printer-independent). **HIR-MATCH ≈ 1447/1803**
  byte-identical to the TS compiler. (Many semantically-correct fixtures differ only in HIR temp/block
  ID *numbering*, which compiles identically — so HIR-MATCH < SEMANTIC-pass by design.)

Toolchain: requires **rustc 1.94.0** (oxc 0.136); `cargo …` from `compiler/` uses it automatically
via `rust-toolchain.toml` (or `rustup run 1.94.0 cargo …`). Node
scripts run via `tsx`. The CLI is `react_compiler_e2e_cli` (`--frontend oxc`, `--dump-hir`, `--json`).

## Performance (Apple M4 Max, release, single-threaded, warm, median-of-8, 1505-fixture corpus)

End-to-end compile (full source → compiled output), per-fixture median:

| | per-fixture | fixtures/sec | vs native |
|---|---|---|---|
| **Native-Oxc** (oxc parse→semantic→passes→native codegen) | **~0.23 ms** | ~4,300 | 1× |
| **Pre-Oxc Rust port** (Babel parse→scope→JSON→NAPI Rust→Babel codegen) | 1.38 ms | ~720 | **6.1× slower** |
| **TS/Babel reference** (in-process) | 1.895 ms | ~528 | **~8.1× slower** |
| Native parse+semantic only | 0.0029 ms | ~350k | — |

**Binary size** (release, fat-LTO, stripped): native-Oxc **4.64 MB** (oxc 0.136; was 5.1 MB at 0.121 — the upgrade +
dead-code polish shrank it ~9%) vs pre-Oxc NAPI cdylib **5.92 MB** → native is **~22% smaller** despite bundling a full
JS parser+semantic (deleting `react_compiler_ast` + serde + the NAPI glue more than offsets oxc). CLI-vs-cdylib caveat applies.

**Where the migration's win comes from** (pre-Oxc sub-phase breakdown): JS frontend (Babel parse+scope) ~24%,
JSON/NAPI boundary (serialize+deserialize round-trip) ~36%, **shared Rust compiler core ~36%**, output codegen ~4%.
The Rust `compile_program` core is *shared* between the two states (pre-Oxc core alone = 0.335 ms/fix, slightly MORE
than the native full pipeline) — so the ~6.1× is overwhelmingly from **eliminating the JS frontend + the JSON/NAPI
boundary**, exactly the "pure-Rust pipeline / no JSON boundary" motivation. oxc parse+semantic is ~80× cheaper than
Babel parse+scope and ~1.2% of the native pipeline. Bench harness: `react_compiler_e2e_cli --bench <dir> [--iterations N] [--bench-parse-only]`.

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

### ✅ Done after migration-complete (climbing the tail)
- **`@gating`** — gated-export codegen implemented natively (commit 2c1107bc09). 14/16 fixtures pass; 2 remain
  (one needs `@enableEmitInstrumentForget`, one needs nested-object-property arrow discovery).
- **Scattered single-cause** (7 fixes, commits 57e3b1e6..325fefb9): `@script` require-import, `-0` folding,
  TS `as`/`satisfies` codegen (+ cleared 3 TypeCast BAILs), fn-expression naming ×2, top-level arrow block-body,
  `enableNameAnonymousFunctions` wrapping.

### Deferred opt-in/pragma features
- **`enableJsxOutlining`** (~9) — the native `outline_jsx` pass produces structurally-wrong output (wrong outlined-fn
  shape/fragments) — a pass-correctness bug, not a single-cause fix. Distinct from the function-outlining done in N2.8.
- **Deep recursive function-discovery** (several fixtures across buckets) — TS `program.traverse` discovers/compiles
  EVERY nested function (IIFEs, array/object-nested arrows); native discovery only checks specific syntactic positions.
  Matching TS's full recursive traversal + `skip()` is one coherent (but large, regression-prone) change that would
  unlock several fixtures at once.
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

## Path to oxc integration (#36743)

Upstream's stated direction is for the OXC integration to live in the OXC project, consuming
`react_compiler` as a crate. Assessment of what that takes:

- **Mechanical (low risk):** edition 2021→2024 + toolchain 1.94→1.96 (oxc's); switch oxc deps from
  crates.io `"0.136.0"` to oxc's workspace `path` deps — oxc HEAD == 0.136.0 today, so **no API
  reconciliation right now** (in-tree thereafter means tracking oxc HEAD as it moves). Strip the
  JS/NAPI-oriented surface (serde `CompileResult`, `ordered_log` JSON, the e2e CLI) to a clean Rust API.
- **The lint rule *unblocks* in-tree:** the `oxc_linter::Rule` we deferred was blocked only by
  `oxc_linter` not being on crates.io — in-tree the `Rule` trait + `declare_oxc_lint!` are available.
- **Real blockers:**
  1. **No home for a heavyweight transform.** The compiler *rewrites* code (memoization); oxc lint rules
     emit diagnostics and oxc transformers do syntax-lowering — neither fits a ~74k-line optimizing
     transform. It'd be its own crate(s) that oxc apps consume; oxc has no existing "userland optimizing
     transform" product to host it.
  2. **The oracle can't follow.** Correctness is validated against the in-process **TypeScript** React
     compiler; oxc has no React/Node toolchain to run it. Testing would have to become self-contained
     snapshots.
  3. **Scope + governance.** ~74k lines / 12 crates, and this is a **fork** of Meta's compiler diverging
     from React upstream. Publish-as-crate (ownership?) vs vendor-into-oxc, and who keeps it in sync with
     React's evolving compiler semantics — a cross-project decision, not a code task.

## Working notes for whoever continues

- The original react_compiler_ast lowering/codegen LOGIC (deleted) is the transcription reference in git:
  lowering at commit **`1a02788`**; the dead HIR→react_compiler_ast codegen + `convert_ast_reverse`
  (oxc AstBuilder construction patterns) at the commit **before `bc9d4ea244`** (N2.final). The TS source
  under `compiler/packages/babel-plugin-react-compiler/src/` is the ultimate reference.
- Driving metric for any change: keep `compare-code.ts` SEMANTIC-pass from regressing; a lowering/inference
  fix should hold or raise `compare-hir.ts` HIR-MATCH. Use `--list` + single-fixture diff to localize.
- oxc API specifics that bit repeatedly (held 0.121→0.136): `BindingPattern` IS the enum (no `.kind`);
  `MemberExpression` split into Static/Computed/PrivateField; optional chaining is one `ChainExpression`
  over plain members (NOT Babel per-link nesting); `symbol_id`/`reference_id` are `Cell`s;
  `oxc_ast::Expression` isn't `Clone`. (0.136 deltas: `Atom`→`oxc_ast::ast::Str`, `AstBuilder::atom`→`str`,
  `SemanticBuilder` needs `.with_build_nodes(true)`, `ParserReturn/SemanticBuilderReturn.errors`→`.diagnostics`.)
