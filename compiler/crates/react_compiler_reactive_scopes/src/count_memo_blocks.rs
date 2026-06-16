// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the root directory of this source tree.

//! Counts memoized and pruned-memo scope statistics over a `ReactiveFunction`.
//!
//! These counts feed the `CompileSuccess` logger event (`memoBlocks`,
//! `memoValues`, `prunedMemoBlocks`, `prunedMemoValues`). They are derived
//! purely from the reactive-scope structure and are independent of code
//! generation, so the native oxc codegen path can source them without running
//! the (now-deleted) tree-IR codegen.
//!
//! TS: `CountMemoBlockVisitor` in `src/ReactiveScopes/CodegenReactiveFunction.ts`.

use react_compiler_hir::environment::Environment;
use react_compiler_hir::{PrunedReactiveScopeBlock, ReactiveFunction, ReactiveScopeBlock};

use crate::visitors::{ReactiveFunctionVisitor, visit_reactive_function};

/// Counts memo blocks and pruned memo blocks in a reactive function.
struct CountMemoBlockVisitor<'a> {
    env: &'a Environment,
}

struct CountMemoBlockState {
    memo_blocks: u32,
    memo_values: u32,
    pruned_memo_blocks: u32,
    pruned_memo_values: u32,
}

impl<'a> ReactiveFunctionVisitor for CountMemoBlockVisitor<'a> {
    type State = CountMemoBlockState;

    fn env(&self) -> &Environment {
        self.env
    }

    fn visit_scope(&self, scope_block: &ReactiveScopeBlock, state: &mut CountMemoBlockState) {
        state.memo_blocks += 1;
        let scope = &self.env.scopes[scope_block.scope.0 as usize];
        state.memo_values += scope.declarations.len() as u32;
        self.traverse_scope(scope_block, state);
    }

    fn visit_pruned_scope(
        &self,
        scope_block: &PrunedReactiveScopeBlock,
        state: &mut CountMemoBlockState,
    ) {
        state.pruned_memo_blocks += 1;
        let scope = &self.env.scopes[scope_block.scope.0 as usize];
        state.pruned_memo_values += scope.declarations.len() as u32;
        self.traverse_pruned_scope(scope_block, state);
    }
}

/// Returns `(memo_blocks, memo_values, pruned_memo_blocks, pruned_memo_values)`.
pub fn count_memo_blocks(func: &ReactiveFunction, env: &Environment) -> (u32, u32, u32, u32) {
    let visitor = CountMemoBlockVisitor { env };
    let mut state = CountMemoBlockState {
        memo_blocks: 0,
        memo_values: 0,
        pruned_memo_blocks: 0,
        pruned_memo_values: 0,
    };
    visit_reactive_function(func, &visitor, &mut state);
    (
        state.memo_blocks,
        state.memo_values,
        state.pruned_memo_blocks,
        state.pruned_memo_values,
    )
}
