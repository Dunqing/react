# Before/after-Oxc apples-to-apples benchmark

Reproduces the three-way compile-time comparison in
`../rust-port-oxc-native-handoff.md`:

| pipeline | what it is |
|---|---|
| **Native-Oxc** | current branch: oxc parse→semantic→passes→native codegen |
| **Pre-Oxc Rust port** | Babel parse→scope→JSON→NAPI→Rust core→Babel codegen (commit `b49e04151e`) |
| **TS/Babel reference** | in-process `babel-plugin-react-compiler` (parse→plugin→codegen) |

All three are measured with the **same methodology**: same 1505-fixture corpus
(non-`*.flow.js` `.js` under `fixtures/compiler/`), all sources read up front
(IO excluded), warmup discarded, median-of-8, single-threaded,
`compilationMode: 'all'`. The native side uses the Rust `--bench` harness
(`react_compiler_e2e_cli`); the two JS pipelines use the shared
`bench-harness.mjs` here, which mirrors the Rust `run_bench` loop exactly.

## Why an intersection run

Native bails *cheaply* on ~370 fixtures gated behind deferred features (fbt,
jsx-outlining, …) — it produces code for 1134/1505 vs 1458/1505 for the other
two. A corpus-average would flatter native by counting those cheap bails. So
the apples-to-apples figure is taken over the **intersection** of fixtures all
three fully compile (1134). The ratios turn out nearly identical either way
(see the handoff doc), confirming the confound is symmetric.

## Native (current tree)

```bash
cd compiler
cargo build -p react_compiler_e2e_cli --release
BIN=target/release/react-compiler-e2e
FX=packages/babel-plugin-react-compiler/src/__tests__/fixtures/compiler

# full pipeline / parse+semantic only / core only (lowering+passes, no print):
$BIN --bench "$FX" --iterations 8
$BIN --bench "$FX" --iterations 8 --bench-parse-only
$BIN --bench "$FX" --iterations 8 --bench-core-only

# dump the set of fixtures that compiled to code (for the intersection):
$BIN --bench "$FX" --iterations 1 --bench-dump-compiled /tmp/native-compiled.txt
# restrict a run to an intersection path-list:
$BIN --bench "$FX" --iterations 8 --bench-filter /tmp/intersection.txt
```

## Pre-Oxc Rust port + TS reference (worktree at `b49e04151e`)

The pre-Oxc Babel/JSON/NAPI port was removed from the branch by #36743;
`b49e04151e` is the last commit with it intact. Build it in an isolated
worktree:

```bash
# 1. worktree at the pre-Oxc baseline
git worktree add --detach /tmp/react-preoxc b49e04151e
ROOT=/tmp/react-preoxc/compiler

# 2. build the napi cdylib and expose it as native/index.node
cd "$ROOT/packages/babel-plugin-react-compiler-rust/native"
cargo build --release
cp "$ROOT/target/release/libreact_compiler_napi.dylib" ./index.node   # .so on Linux

# 3. install deps (TWO yarn roots) + build the TS sides
cd /tmp/react-preoxc && yarn install
cd "$ROOT" && yarn install
yarn workspace babel-plugin-react-compiler-rust build   # tsc -> dist (the pre-Oxc bridge)
yarn workspace babel-plugin-react-compiler run build    # tsup -> dist/index.js (the TS reference plugin)

# 4. run the benches (copy *.mjs here into $ROOT, or set PREOXC_COMPILER_ROOT)
cp "$(git -C /tmp/react-preoxc rev-parse --show-toplevel)"/compiler/docs/rust-port/bench/*.mjs "$ROOT"/ 2>/dev/null || true
cd "$ROOT"
export PREOXC_COMPILER_ROOT="$ROOT"
node bench-rust.mjs              # pre-Oxc Rust port, end-to-end
node bench-rust.mjs --profile    # frontend/boundary/core/codegen split
node bench-ts.mjs                # TS/Babel reference

# 5. intersection: dump each set, intersect, re-run all three with BENCH_FILTER
BENCH_DUMP=/tmp/preoxc-compiled.txt node bench-rust.mjs
BENCH_DUMP=/tmp/ts-compiled.txt     node bench-ts.mjs
# (native set from the native section above)
sort -u /tmp/native-compiled.txt > /tmp/n; sort -u /tmp/preoxc-compiled.txt > /tmp/p; sort -u /tmp/ts-compiled.txt > /tmp/t
comm -12 /tmp/n /tmp/p | comm -12 - /tmp/t > /tmp/intersection.txt
BENCH_FILTER=/tmp/intersection.txt node bench-rust.mjs
BENCH_FILTER=/tmp/intersection.txt node bench-ts.mjs
# native: $BIN --bench "$FX" --iterations 8 --bench-filter /tmp/intersection.txt
#   (the native and worktree fixture dirs are byte-identical at this corpus;
#    intersection paths are stored as the worktree's absolute paths, so point
#    native --bench at the worktree fixtures dir when filtering.)

git worktree remove /tmp/react-preoxc   # cleanup
```

Toolchain used for the recorded numbers: rustc/cargo 1.91.0 (edition 2024),
node v24.16.0, Apple M4 Max, release builds.

## Files
- `bench-harness.mjs` — shared `run_bench`-mirroring loop (`runBench`,
  `collectFixtures`); honours `BENCH_FILTER` / `BENCH_DUMP` env vars.
- `bench-rust.mjs` — pre-Oxc Rust-port engine (full pipeline; `--profile` for the
  sub-phase split). Replicates `BabelPlugin.ts` orchestration against the built
  `dist/` modules.
- `bench-ts.mjs` — in-process TS/Babel reference (bare parse→plugin→codegen).
