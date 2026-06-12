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
- **Status**: Complete (commit 93ca32a473). oxc pinned 0.121.0; needs rustc 1.92.0 (`rustup run 1.92.0 cargo …`; machine default 1.91.0). Source entry already exists: `transform_source`/`lint_source`. No convert_scope tests, no fixtures. No shared-file drift (#36743 edits were pure rustfmt).

### Stage 2: Recover the e2e harness (oxc-only) + smoke test
- **Goal**: Recover `react_compiler_e2e_cli` and `compiler/scripts/test-e2e.{sh,ts}` from `d3da200820^`. The CLI reads source from stdin and compiles via `--frontend oxc` (it also had `--json` and `--dump-scope`). SWC is dropped — strip SWC from the recovered CLI/harness so it's oxc-only and builds. Smoke-test: pipe one fixture through `--frontend oxc`.
- **Depends on**: Stage 1
- **Parallel**: no
- **Success criteria**: `cargo build -p react_compiler_e2e_cli` (rustc 1.92.0) succeeds; piping a simple component fixture through `--frontend oxc` emits compiled code.
- **Status**: Complete (commit e8394befba). CLI oxc-only, builds; smoke test emits memoized output. Oracle: `bash compiler/scripts/test-e2e.sh --variant oxc` compares oxc output vs in-process TS Babel-plugin baseline over ~1803 fixtures (Flow fixtures auto-skip — oxc has no Flow parser). Caveat: harness shells out to `~/.cargo/bin/cargo` (1.91.0) — pre-build with `rustup run 1.92.0` or pin that line.

### Stage 3: Baseline e2e parity across the fixture corpus (oxc path)
- **Goal**: Run the recovered `test-e2e` harness over the fixture corpus via the OXC frontend, comparing compiled output against the reference. This validates the recovered bridge (convert_scope + convert_ast + unchanged pipeline) end-to-end.
- **Depends on**: Stage 2
- **Parallel**: no
- **Success criteria**: report baseline `X/total` passing + the failing fixtures and any common failure signature (use `--dump-scope` to localize scope vs ast vs codegen divergences).
- **Status**: Not Started

> Per-pass HIR-diff oracle wiring (`test-rust-port.ts` via the oxc path) is deferred to **M2**, where it localizes divergences pass-by-pass during the lowering rewrite. For M1 the e2e compiled-code comparison is the decisive oracle.

## Verification (M1 done)
`cargo build` workspace green · `cargo test -p react_compiler_oxc` green · scope parity green ·
`bash compiler/scripts/test-rust-port.sh` via oxc path at target parity · `/compiler-verify` clean.
