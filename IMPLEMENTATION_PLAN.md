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
- **N2 — Native codegen** (user-chosen: no-bridge-ever; full native, no interim convert_ast_reverse).
  HIR → `oxc_ast` via `AstBuilder`, spliced into the oxc Program, printed by `oxc_codegen`. The CODE oracle
  (`test-e2e --variant oxc`, the F3 harness) becomes the truth — should resolve most of the 742 (benign HIR deltas
  that compile identically). References (read-only, deleted at end of N2): existing `codegen_reactive_function.rs`
  (~4300L, HIR→react_compiler_ast — the LOGIC) + `convert_ast_reverse.rs` (1942L, react_compiler_ast→oxc — the
  AstBuilder CONSTRUCTION patterns); fuse them so codegen builds oxc directly.
  - **N2.1**: vertical slice — native codegen for core constructs (fn + memoization cache emission `_c(n)`/`$[…]`
    + scope wrapping + return + var-decls + calls + members + JSX) + program assembly (splice compiled fns into the
    oxc Program) + print via oxc_codegen; wire `test-e2e --variant oxc`. Green via bailouts. Report code-oracle pass count.
  - **N2.2+**: broaden codegen coverage; drive `test-e2e --variant oxc` up toward/above the 95% bridge baseline;
    fix real lowering+codegen bugs the CODE oracle localizes (this is where the 742 get settled).
  - **N2.final**: delete `convert_ast_reverse` + the old react_compiler_ast codegen path.

  **N2.1 ✅ (e5f0606)** native codegen works end-to-end. New: `codegen_oxc.rs` (~920L, ReactiveFunction→oxc via
  AstBuilder), `codegen_assembly.rs` (~230L, re-parse source into fresh Allocator + splice compiled fns by span +
  inject `_c` import + oxc_codegen print), `native_codegen.rs` (~45L, `NativeArtifact` carrier — env moved out
  after the dead react_compiler_ast codegen reads it; compile_program returns `CompileProgramResult{result, native_artifacts}`).
  **CODE oracle test-e2e --variant oxc: Code 584/1803, Events 1325/1803, both 240/1803** (from 0). Real: fn shell+ident
  params, `_c(n)` + reactive-scope if/else cache wrapping, const/let/reassign, call/member/object/array/binary/unary/
  logical/conditional/primitive/LoadGlobal, JSX. Bail (un-memoized): early-return scopes, destructuring/function/catch
  stores, loops/switch/try/break/continue/label terminals, sequence/optional values, object methods, template, fn-expr.
  ~918 fns bail (dominant lever). Cosmetic: `x=x+b` vs `+=`, `let v;` vs `let v=0;`, JSX `/>`, `x["a"]` vs `x.a`, comments.
  Next: N2.2 broaden codegen (terminals + early-return), N2.3 (remaining values/stores), N2.4 cosmetic/codegen-choice polish.

  **N2.2 ✅ (13d738e6d8)** codegen_oxc.rs 2382L. **Code-pass 584→756.** Added all control-flow terminals, Destructure
  stores, PropertyStore/ComputedStore/StoreGlobal, TemplateLiteral/TaggedTemplate, NewExpression, Await, Sequence,
  OptionalChaining, FunctionExpression/arrow (recursive). 219 newly pass; 47 "regressions" are NOT codegen bugs —
  they're @validate*/@outputMode:lint/error.todo-* fixtures where TS errors with no output but the Rust pipeline LACKS
  those validation passes (→ separate workstream: **port missing validation passes** [tracked as N-VAL]). One real bug
  fixed (overlapping-scopes trailing `return undefined`).
  - **N2.3 (next)**: temporary promotion/naming — port/wire `promote_used_temporaries` so the ~423 unnamed-identifier
    (264) + compound-temporary (159) bails resolve. Biggest remaining codegen lever.
  - Then tail: early-return scopes (36), object methods (21), PostfixUpdate (22), NextPropertyOf, spread params, etc.

  **N2.3 ✅ (53a14efd5c + a21d2fdb8e)** Code-pass 756→788. Diagnosis: PromoteUsedTemporaries already ported+wired;
  bails were codegen mishandling — fixed compound-reactive-value temp table (store ReactiveValue, rebuild via
  codegen_value since oxc Expression isn't Clone) + member-expr JSX tags. 13 flips all N-VAL artifacts. Per-stage gains
  shrinking (multiple bails per fixture). Next bail #1: unnamed-id via optional-chain member loads / scope-dep naming.
  - **N2.4 (next)**: categorize the ~1015 code-failures → 3 buckets [codegen-bail / cosmetic-printer / N-VAL], size them,
    then attack the biggest actionable (codegen) bucket. Endgame target = the 95% bridge baseline (~1713/1803) on the CODE oracle.

  **N2.4 ✅ (18c223e0b9)** Code 788→797. Implemented PostfixUpdate/PrefixUpdate/PropertyDelete/ComputedDelete/RegExp/
  MetaProperty. **KEY: categorized the 1015 code-failures via a structural-equivalence normalizer:**
  - **COSMETIC 510** — structurally equivalent, only printing differs (JSX self-close, comments, temp-naming, slot-order,
    decl-kind). FUNCTIONALLY CORRECT. → byte-match-vs-Babel oracle UNDERSTATES correctness (two printers can't match).
  - **OTHER 263** — real gaps: JSX-outlining ~111, codegen feature-gaps (gating/instrument/DCE/SSA) ~87, memo-mismatch ~43,
    arrow-vs-fn ~20, **+2 real `?.`-dropping correctness BUGS** (optional-call-chained.js, optional-member-expression-chain.js).
  - **BAIL 201** — un-codegen'd construct. **N-VAL 16** — needs validation passes.
  - **Functionally-correct ≈ 797 + 510 = ~1298/1803 (72%).** True remaining work = BAIL 201 + OTHER 263 + N-VAL 16.
  → Strategic fork (semantic vs byte parity; feature-gap scope) raised with user before the endgame grind. Fix the 2 bugs regardless.

  **ENDGAME = SEMANTIC PARITY (user-chosen).** Oracle = structural equivalence (accept cosmetic oxc_codegen-vs-Babel
  printer diffs; the 510 are functionally correct). Grind real correctness; defer big features.
  - **N2.5**: fix the 2 `?.`-dropping bugs (optional-call-chained.js, optional-member-expression-chain.js) + promote the
    N2.4 structural-equivalence normalizer to a committed reusable oracle (semantic-pass = byte-pass + cosmetic, ~1298+).
  - **N2.6**: clear BAIL 201 (remaining un-codegen'd constructs).

  **N2.5 ✅ (49bff15d02)** Fixed both `?.` bugs — root cause in LOWERING (oxc 0.121 chains = one ChainExpression over
  plain members vs Babel per-link nesting; lowering collapsed chains, lost inner `?.`). Deep fix in expressions.rs
  (`chain_subtree_has_optional` recursion) + codegen_oxc (`to_optional`/`unchain`/`chain` flatten). Semantic oracle
  `compiler/scripts/compare-code.ts` committed (PRIMARY metric). **SEMANTIC-pass 1326→1337/1803 (74.2%)** = byte 801 +
  structural 536. Buckets: N-VAL 15, BAIL 202, OTHER 249. test-babel-ast.sh still 1787/1787.
  - **N2.7**: diagnose+fix the ~75 upstream-PIPELINE errors (EnterSSA/Refs/destructure invariants on native HIR — real
    lowering-fidelity or ported-pass bugs), then achievable OTHER gaps (memo opt-out ~43, DCE/SSA); DEFER JSX-outlining
    (~111) + full SSR/gating + fbt (~22) + TypeCast (3).

  **N2.6 ✅ (3114c4737f)** codegen_oxc +150L. BAIL 202→127, **SEMANTIC-pass 1337→1422 (78.9%)**, 0 regressions.
  Implemented: early-return scope guard, object methods, optional dep paths, reassign-as-expr StoreLocal, spread params,
  NextPropertyOf, Debugger. Of BAIL: **75 are upstream-PIPELINE errors (NOT codegen)** — EnterSSA hoisting / Refs
  validation / destructure invariants tripping on native HIR; +26 Family-B optional/logical inlining; +22 fbt (deferred);
  +3 TypeCast (deferred). OTHER 249→239, N-VAL 15.
  - **N-VAL**: port the 16 missing validation passes (+ any others surfaced) so Rust rejects what TS rejects.

  **N2.7 ✅ (54e4075881)** Real LOWERING bug found+fixed: native lowering never ported BuildHIR BlockStatement hoisting
  (DeclareContext for Hoisted Const/Let/Function + mark as context idents). Cleared EnterSSA/InferMutationAliasing
  invariants. Also fixed `'use no memo'`/`'use no forget'` opt-out (log+skip per-fn, not fatal-bail whole file).
  **SEMANTIC-pass 1422→1446 (80.2%), HIR-MATCH 978→1009 (+31), BAIL 127→92**, 0 regressions, all tests + test-babel-ast.sh green.
  - **N2.8 (next)**: re-categorize OTHER (~239), attack achievable correctness sub-buckets (memo-mismatch ~43, arrow-vs-fn
    ~20); DEFER JSX-outlining (~111) + SSR/gating/instrumentation. Then N-VAL (15), Family-B inlining (BAIL ~26).
  - Then **N2.final** (delete convert_ast_reverse + dead react_compiler_ast codegen) + **N3** (delete react_compiler_ast +
    Babel NAPI/JSON; add oxc_linter Rule + transform API). Deferred features tracked in a handoff note.

  **N-VAL (parallelizable later)**: port missing TS validation passes (validateNoSetStateInEffects,
  validateNoJSXInTryStatements, rules-of-hooks, etc.) so the Rust pipeline rejects what TS rejects. Orthogonal to codegen.
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
- **Status**: Complete (commit 060d035454). `compiler/scripts/compare-hir.ts` (+ `hir-oracle-lib.ts`):
  single-fixture diff + corpus run. **Baseline: 70 MATCH / 1662 frontier@HIR / 51 TS-error+oxc-bail / 0 crashes**
  (1783 fixtures). TS side: `yarn install` (corepack shim) once; runs `printDebugHIR` in-process via tsx (no dist build).
  Single: `tsx compiler/scripts/compare-hir.ts <fixture> --no-build`; corpus: `tsx compiler/scripts/compare-hir.ts --out … --limit 0`.

### Stages N1.2.3+: fill lowering bailouts (transcribe from git 1a02788)
Re-create each `build_hir/*` concern reading oxc_ast (port logic from git 1a02788's react_compiler_ast version),
wire into mod.rs dispatch, keep GREEN, measure with compare-hir.ts (MATCH count ↑ / fewer Todo bails).
Order by leverage: **expressions** (N1.2.3) → statements/control-flow → patterns/destructuring → JSX →
function-expressions/hoisting/context-capture. MATCH count is a LAGGING indicator (a fixture only flips to
MATCH once ALL its constructs are done) — also track "fixtures with zero Todo bailouts" as the leading signal.
- **Status**: N1.2.3 ✅ (384d59d568) expressions.rs 1889L, MATCH 70→90, 0 regressions. Remaining expr
  bails (arrow/func-expr, JSX, class, this/super, yield, destructuring-assign, logical-assign) need later infra.
  Next: N1.2.4 statements/control-flow.
- **Status**: N1.2.4 ✅ (5b852e0bf3) statements.rs 1413L, MATCH 90→241, 0 regressions. Frontier: nested
  functions/arrows 99%, JSX ~42%, destructuring ~14%, + orthogonal fn_type(Component vs Other) discovery gap.
  Next: N1.2.5 nested functions/arrows + context-capture (coupled: functions.rs + hoisting.rs + find_context_identifiers.rs).
- **Status**: N1.2.5 ✅ (3269510639) functions.rs 465L + find_context_identifiers.rs 79L (context capture is
  reference-driven via oxc resolved-references, not AST-walked — much simpler than the old position-based code).
  MATCH 241→384, 0 regressions, context-capture HIR identical. Frontier: JSX (~575) + destructuring (~656, overlap);
  destructuring bails also cause downstream `InferMutationAliasingEffects` invariant (uninitialized binding).
  Next: N1.2.6 patterns/destructuring (params + declarations + assignment targets), then N1.2.7 JSX.
- **Status**: N1.2.6 ✅ (316c2fb628) patterns.rs 1114L, MATCH 384→487, 0 regressions, InferMutationAliasing
  invariant resolved. oxc binding-family vs assignment-target-family are SEPARATE trees (two entry points).
  Frontier: JSX 589 (~47%); remaining ~655 are downstream semantic divergences (mutable-range/reactive-scope) —
  re-evaluate after JSX. catch-destructuring still bails (TS aborts there too). Next: N1.2.7 JSX.
- **Status**: N1.2.7 ✅ (b136174be8) jsx.rs (~30KB), **MATCH 487→978/1783**, 0 regression (spot-checked + baseline
  saved to compiler/oxc-hir-match-baseline.txt). **Frontier @ later-pass = 0** → once lowering matches, the whole
  pipeline matches faithfully. KEY INSIGHT: the HIR oracle is STRICTER than correctness — some frontiers (e.g.
  while-logical.js) are pure identifier/block-ID renumbering (isomorphic HIR, different alloc order) that compile to
  IDENTICAL code. So 978 is a strict LOWER BOUND on correctness; the true measure is the e2e CODE oracle (returns in N2).
  Remaining 752 = real bails (un-transcribed; block codegen → must finish) + cosmetic order-diffs (compile fine → N2 settles).
  Plan: finish the real bails, then move to N2 (native codegen) and use the code oracle as truth — do NOT chase cosmetic HIR diffs.
  Next: N1.2.8 categorize the 752 + finish remaining real bails.
- **Status**: Frontier analysis (752): only **~10 real bails** left (rare: update-expr ×3, TSEnumDeclaration ×2,
  tagged-template, reassignment, Yield, MetaProperty, ClassExpression) → lowering transcription is essentially DONE.
  **742 lower fully** but HIR differs; sample (51): ~7 pure-renumber, ~44 have a real HIR delta. The HIR oracle
  CANNOT determine if these compile identically (downstream DCE/const-prop/etc. normalize benign lowering deltas
  before codegen). → The HIR oracle has reached its useful limit; the CODE oracle (test-e2e --variant oxc) is now
  the right measure. **DECISION POINT (N2 approach):** re-enable code output to measure true correctness.
  - Option A (validate-first): reuse existing HIR→react_compiler_ast codegen + kept `convert_ast_reverse`
    (react_compiler_ast→oxc) + oxc_codegen to print; run test-e2e --variant oxc NOW to see true pass rate.
    Fast, reuses code, but transiently uses convert_ast_reverse (an output-side bridge, deleted in full native codegen).
  - Option B (no-bridge-ever): go straight to full native codegen (HIR→oxc_ast via AstBuilder); validate only after.
  Real bails (~10) finished opportunistically either way.

### Stage N1.3: Discovery + context-identifiers native
- **Goal**: `program.rs` `AstWalker` discovery, `find_context_identifiers.rs`,
  `identifier_loc_index.rs` walk `oxc_ast` + oxc_semantic. Remove last `react_compiler_ast`
  uses on the input side; delete `ScopeInfo`/`convert_scope`.
- **Depends on**: N1.2
- **Success criteria**: full `test-e2e.sh --variant oxc` ≥ 95% baseline (codegen still react_compiler_ast); no react_compiler_ast on input path; `cargo build` green.
- **Status**: Not Started

## Verification (N1 done)
HIR-diff parity vs baseline · `test-e2e.sh --variant oxc` ≥ 1713/1803 · `grep -rl react_compiler_ast compiler/crates/react_compiler_lowering` empty · `/compiler-verify` clean.
