---
name: compiler-verify
description: Use when you need to run all compiler checks (tests, lint, format) before committing. Detects whether TS or Rust code changed and runs the appropriate checks.
---

# Compiler Verify

Run all verification steps for compiler changes.

Arguments:
- $ARGUMENTS: Optional test pattern for `yarn snap -p <pattern>`

## Instructions

1. **Detect what changed** by running `git diff --name-only HEAD` (or vs the base branch).
   Categorize changes:
   - **TS changes**: files in `compiler/packages/`
   - **Rust changes**: files in `compiler/crates/`
   - **Both**: run all checks

2. **If TS changed**, run these sequentially (stop on failure):
   - `yarn snap` (or `yarn snap -p <pattern>` if a pattern was provided) — compiler tests
   - `yarn test` — test full compiler
   - `yarn workspace babel-plugin-react-compiler lint` — lint compiler source

3. **If Rust changed**, run these sequentially (stop on failure):
   - `rustup run 1.94.0 cargo test` (from `compiler/`) — Rust unit tests; must pass
   - `npx tsx compiler/scripts/compare-code.ts --limit 0` — OXC semantic-parity oracle (compares native Rust-on-OXC output vs the TS compiler across all fixtures; SEMANTIC-pass must not regress)
   - `npx tsx compiler/scripts/test-e2e.ts --variant oxc` — end-to-end OXC oracle (compares the OXC CLI's compiled output and logger events against the TS baseline)

4. **Always run** (from the repo root):
   - `yarn prettier-all` — format all changed files

5. **If implementing a plan doc**, check:
   - Plan doc has no unaddressed "Remaining Work" items
   - Plan doc status is updated to reflect current state

6. Report results: list each step as passed/failed. On failure, stop and show the error with suggested fixes.

## Common Mistakes

- **Running `yarn snap` without `-p`** is fine for full verification, but slow. Use `-p` for focused checks.
- **Running prettier from compiler/** — must run from the repo root.
- **Forgetting Rust tests** — if you touched `.rs` files, always run the round-trip test and `yarn snap --rust`.
