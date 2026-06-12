# Task: Migrate React Compiler (Rust port) to Oxc as the AST + scope core

Branch: `oxc-migration`. Approved plan: `~/.claude/plans/async-singing-wombat.md`.
Deliberate fork (diverges from upstream #36743 on purpose). Wholesale replacement:
oxc_ast/oxc_semantic become the core; `react_compiler_ast` deleted at the end.

**Accelerant:** the adapter we need for Phase 1 was deleted yesterday (#36743) and is
fully recoverable from `d3da200820^` (≈zero API drift). Reuse it, don't rewrite it.

## Milestones (approved-plan phases)
- **M1 — Oxc front + scope behind a temporary bridge** ← current focus (Stages 1–4 below)
- M2 — Lowering & discovery consume `oxc_ast` directly; delete `convert_ast`
- M3 — Codegen emits `oxc_ast`, printed by `oxc_codegen`; drop JSON output
- M4 — Delete `react_compiler_ast` + Babel bridge; add `oxc_linter` Rule
- M5 (optional) — thin `ScopeInfo` to direct `oxc_semantic` queries

## Stages (M1)

### Stage 1: Recover & build `react_compiler_oxc`
- **Goal**: Restore the deleted crate from `d3da200820^` (convert_scope, convert_ast, convert_ast_reverse, apply_renames, prefilter, diagnostics, lib). Re-add oxc deps (was 0.135.0) to workspace + crate Cargo.toml. Fix any drift vs current `react_compiler` (post #36729 by-value AST, #36730 raw-JSON subtrees) so it builds.
- **Depends on**: none
- **Parallel**: no (foundational)
- **Success criteria**: `cargo build -p react_compiler_oxc` succeeds; report what modules/tests the recovered crate already contains.
- **Status**: Not Started

### Stage 2: `compile_source` entrypoint + end-to-end smoke test
- **Goal**: Add `compile_source(source, source_type, options)` (oxc_parser → oxc_semantic → `transform()`), if not already present in recovered lib.rs. Add a Rust test compiling a simple component + a hook end-to-end through the oxc path and asserting it produces a compiled `File`.
- **Depends on**: Stage 1
- **Parallel**: with Stage 3 (different files)
- **Success criteria**: `cargo test -p react_compiler_oxc` smoke test green.
- **Status**: Not Started

### Stage 3: Scope parity validation vs Babel goldens
- **Goal**: Re-establish convert_scope correctness — oxc-derived `ScopeInfo` resolves every identifier reference to the same binding (by name + declaration position) as the Babel `fixture.scope.json` goldens (`scripts/babel-ast-to-json.mjs`). Recover/adapt any deleted convert_scope tests.
- **Depends on**: Stage 1
- **Parallel**: with Stage 2
- **Success criteria**: scope resolution-equivalence passes across the fixture corpus; report pass/total.
- **Status**: Not Started

### Stage 4: Drive the HIR-diff oracle through the oxc path
- **Goal**: Update `scripts/test-rust-port.ts` so the Rust port is fed via `compile_source(fixtureSource)` (oxc parse+semantic) instead of Babel-serialized JSON. Keep the Babel NAPI path as a temporary fallback. Run the per-pass HIR-diff oracle.
- **Depends on**: Stage 2, Stage 3
- **Parallel**: no
- **Success criteria**: establish baseline `X/1724`; goal `1724/1724`. Report the frontier (first diverging pass) for any failures.
- **Status**: Not Started

## Verification (M1 done)
`cargo build` workspace green · `cargo test -p react_compiler_oxc` green · scope parity green ·
`bash compiler/scripts/test-rust-port.sh` via oxc path at target parity · `/compiler-verify` clean.
