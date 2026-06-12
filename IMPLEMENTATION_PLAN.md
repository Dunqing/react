# Task: Make Oxc the React Compiler (Rust port) internals — fully native, no bridge

Branch: `oxc-migration`. Approved plan: `~/.claude/plans/async-singing-wombat.md`.
Deliberate fork (diverges from upstream #36743 on purpose).

**Direction (user-corrected):** Replace ALL internals with Oxc. Lowering + program
discovery consume `oxc_ast` + `oxc_semantic` DIRECTLY. Codegen builds `oxc_ast`
directly, printed by `oxc_codegen`. **Delete `react_compiler_ast` AND `ScopeInfo`** —
scope/binding is queried straight from `oxc_semantic` (no intermediate scope type, no
oxc↔babel-AST conversion).

## Foundation already in place (keep)
- **F1** (commit 93ca32a473): recovered `react_compiler_oxc` — gives the oxc parse +
  `SemanticBuilder` plumbing and `transform_source`/`lint_source` entry. Its
  `convert_ast.rs` / `convert_scope.rs` are now **reference only** (port their
  classification logic into direct oxc_semantic queries, then delete).
- **F2** (commit e8394befba): recovered oxc-only e2e harness — the oracle.
- **F3** (commit 85b64b2d27): baseline **1713/1803 (95%)** via `test-e2e.sh --variant oxc`.
  Proves oxc parse+semantic produce correct compilation. Failure buckets known
  (mostly oxc_codegen-vs-Babel printer diffs, not correctness).

## Native milestones
- **N1 — Native lowering + discovery** ← current focus. `oxc_ast` + `oxc_semantic` in,
  owned HIR out. Delete `convert_ast` + `ScopeInfo` + `convert_scope`. Codegen *temporarily*
  still emits `react_compiler_ast` for output only (deleted in N2). Invariant: HIR must stay
  identical to the current (bridged) pipeline — same source → same HIR.
- **N2 — Native codegen.** HIR → `oxc_ast` via `AstBuilder`, printed by `oxc_codegen`.
  Output contract becomes oxc. Delete `convert_ast_reverse`.
- **N3 — Finalize.** Delete `react_compiler_ast` + Babel NAPI/JSON bridge; add the
  `oxc_linter::Rule` + build-time `transform` API.

## Oracles
- **Primary (printer-independent): per-pass HIR diff.** Same source → same HIR regardless of
  frontend. This is the precise oracle for the N1 rewrite (localizes the first diverging pass).
- **Secondary: e2e compiled-code** (`test-e2e.sh --variant oxc`) — end-to-end, but
  overcounts vs Babel (two printers). Re-baselined against oxc_codegen in N2.

## N1 stages

### Stage N1.0: HIR-dump on the oxc path (debugging oracle)
- **Goal**: Add a `--dump-hir` mode to `react_compiler_e2e_cli` (oxc frontend) that runs the
  pipeline with debug enabled and emits the per-pass HIR log (`debug_print::debug_hir`,
  already gated on `context.debug_enabled`). Confirm it matches the TS compiler's per-pass HIR
  on a few fixtures (reuse normalization from `scripts/test-rust-port.ts`).
- **Depends on**: F1, F2
- **Success criteria**: `--dump-hir` emits per-pass HIR for a fixture; spot-matches TS HIR.
- **Status**: Complete (commit a8b5fbbdd8). `--dump-hir` on the oxc CLI emits per-pass HIR in test-rust-port.ts format; post-lowering `HIR` matches TS on `useMemo-simple.js` + `simple-alias.js`. `__debug` (PluginOptions.debug) drives `context.debug_enabled`; oxc `transform()` now carries `ordered_log` through.

### Stage N1.1: Scope queries direct from oxc_semantic — COMPLETE (commit 62fbbd046f)
`react_compiler_lowering/src/semantic_queries.rs`: free functions over `&Semantic` (no ScopeInfo
struct), enums mirror the old ones, 8/8 tests. oxc 0.121 = unified `Scoping` via `semantic.scoping()`.

### Stage N1.2: Lowering reads oxc_ast directly (the core rewrite — IN-PLACE, red build)
Strategy (user-chosen): in-place retarget; build is RED during transcription, then green + HIR parity.
Sub-stages:
- **N1.2.0** (stays GREEN): split `build_hir.rs` (7358L) into a `build_hir/` module dir by concern
  (`mod.rs` = `lower()` + driver; `statements.rs`, `expressions.rs`, `jsx.rs`, `patterns.rs`,
  `hoisting.rs`/context). Pure mechanical extraction — `cargo test` + `--dump-hir` parity unchanged.
- **N1.2.1** (boundary flip; kept GREEN via temporary bailouts): flip the input type — `lower()` +
  HIRBuilder take `&'a oxc_ast` + `&Semantic` (+ `semantic_queries`); `compile_program` →
  `(&Program, &Semantic, …)`; minimal oxc discovery to find functions; update `pipeline.rs:61/1218`
  and `react_compiler_oxc::transform()` to pass oxc directly; **delete `convert_ast.rs`**. Un-transcribed
  constructs bail with a graceful `Todo` so the crate COMPILES; function shell/params/return lower for
  real so trivial fixtures produce HIR. HIR stays owned (no lifetime leak past lowering). In-place — NO
  parallel module; the original react_compiler_ast logic is preserved in git (commit 1a02788) as the
  transcription reference.
- **N1.2.2…k** (fill bailouts; GREEN each, testable via --dump-hir): transcribe each `build_hir/` module
  to oxc_ast + semantic_queries, replacing bailouts with real logic (read original from git). Order:
  statements → expressions → patterns/lvalue → jsx → hoisting/context. ~15 real structural diffs
  (Declaration vs Statement, JSX/property/optional-chaining shapes, TS nodes). Each stage raises the
  set of fixtures whose native HIR matches TS HIR.
- **N1.2.final** (GREEN + parity): resolve remaining errors; `--dump-hir` (native) vs TS HIR; drive
  fixture parity up. Delete `ScopeInfo` (`react_compiler_ast/src/scope.rs`) + `convert_scope.rs`.
- **Depends on**: N1.1
- **Success criteria**: `cargo build` green; native-lowering HIR matches TS HIR across fixtures
  (target ≈ the 95% baseline minus codegen-only failures); no `react_compiler_ast`/`ScopeInfo` on input path.
- **Status**: N1.2.0 ✅ (1a02788) · N1.2.1 ✅ (9bae312eb0 — input path native, green, trivial fixtures
  lower; codegen returns ast:None; old build_hir submodules removed, logic ref'd from git 1a02788;
  `HIRBuilder<'a>{semantic,source_text}`, `FunctionForm{Function|Arrow}`). Next: **N1.2.2 oracle**, then
  fill bailouts. ⚠ Oracle gap: TS-side HIR tooling (`yarn snap -d`/`test-rust-port.ts`) didn't build in
  the agent session — must be made reliably runnable before the transcription stages.

### Stage N1.2.2: Reliable per-pass HIR-diff oracle (oxc CLI ↔ TS)
- **Goal**: Make `scripts/test-rust-port.ts` (or a sibling) source the Rust-side HIR from the oxc CLI
  `--dump-hir` (not NAPI) and diff per-pass against the in-process TS compiler's HIR (`printDebugHIR` +
  `normalizeIds`) for any fixture, plus a corpus run reporting per-fixture frontier. Ensure deps install
  (corepack yarn shim worked in Stage 3) and the TS plugin builds.
- **Depends on**: N1.2.1
- **Success criteria**: one command diffs native-HIR vs TS-HIR for a fixture and across the corpus;
  report current baseline (expected LOW now — most constructs bail) + that the tool works + the frontier.
- **Status**: Not Started

### Stage N1.3: Discovery + context-identifiers native
- **Goal**: `program.rs` `AstWalker` discovery, `find_context_identifiers.rs`,
  `identifier_loc_index.rs` walk `oxc_ast` + oxc_semantic. Remove last `react_compiler_ast`
  uses on the input side; delete `ScopeInfo`/`convert_scope`.
- **Depends on**: N1.2
- **Success criteria**: full `test-e2e.sh --variant oxc` ≥ 95% baseline (codegen still react_compiler_ast); no react_compiler_ast on input path; `cargo build` green.
- **Status**: Not Started

## Verification (N1 done)
HIR-diff parity vs baseline · `test-e2e.sh --variant oxc` ≥ 1713/1803 · `grep -rl react_compiler_ast compiler/crates/react_compiler_lowering` empty · `/compiler-verify` clean.
