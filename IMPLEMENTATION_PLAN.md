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

### Stage N1.1: Scope queries direct from oxc_semantic
- **Goal**: Introduce a scope-query module that reads `oxc_semantic` directly (get_binding,
  binding kind Var/Let/Const/Param/Module/Hoisted/Local, import classification, declaration
  node, scope kind incl. For-vs-Block, parent walk, reference→symbol). Port the classification
  logic from the reference `convert_scope.rs`. NO `ScopeInfo` struct — functions over
  `&Semantic`. (Coupled with N1.2 since binding lookups are keyed by oxc AST nodes.)
- **Depends on**: N1.0
- **Success criteria**: module compiles; unit-resolves bindings for sample sources matching
  the recovered convert_scope semantics.
- **Status**: Not Started

### Stage N1.2: Lowering reads oxc_ast directly (the core rewrite)
- **Goal**: Rewrite `react_compiler_lowering` (`build_hir.rs`, `hir_builder.rs`) to dispatch on
  `oxc_ast` enums (statements/expressions/patterns/JSX; oxc separates `Declaration`; TS
  annotation nodes largely ignored) and use N1.1 scope queries. Input becomes
  `&'a oxc_ast::Program<'a>`; HIR stays owned (no lifetime leak). Delete `convert_ast`.
- **Depends on**: N1.1
- **Success criteria**: drive to HIR-diff parity with the bridged baseline, fixture-by-fixture (`X/1803`).
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
