// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the root directory of this source tree.

//! Native oxc codegen artifacts (stage N2.1).
//!
//! The compile pipeline (`compile_fn`) produces a `ReactiveFunction` (tree IR)
//! plus its owned `Environment`. To build oxc AST we need an `&'a Allocator`,
//! but the pipeline runs while the input `oxc_ast::Program` + `oxc_semantic`
//! are immutably borrowed. We therefore can't splice compiled oxc nodes into
//! the program from inside the pipeline.
//!
//! The solution is to collect the *owned* (allocator-free) pieces — the
//! `ReactiveFunction` and its `Environment` — into [`NativeArtifact`]s during
//! the pipeline, return them out-of-band from `compile_program`, and then build
//! + splice the oxc AST in `react_compiler_oxc::transform`, AFTER the semantic
//! borrow ends and against a freshly-parsed *owned* program.

use react_compiler_hir::ReactFunctionType;
use react_compiler_hir::environment::Environment;
use react_compiler_hir::reactive::ReactiveFunction;

/// A single compiled function captured for native oxc codegen.
///
/// Holds everything needed to run [`react_compiler_reactive_scopes::codegen_oxc`]
/// after the input program's semantic borrow has been released.
pub struct NativeArtifact {
    /// The fully-lowered, scope-analyzed reactive function tree.
    pub reactive_fn: ReactiveFunction,
    /// The owned environment for this function (scopes, identifiers, config).
    pub env: Environment,
    /// Unique identifier names reserved by `rename_variables`, used by codegen
    /// for collision-safe synthesized names (e.g. the `$` cache variable).
    pub unique_identifiers: std::collections::HashSet<String>,
    /// Source span of the original function node (start, end). Used to locate
    /// and replace the original node in the program body during assembly.
    pub fn_span: (u32, u32),
    /// The function's React classification (Component / Hook / Other).
    pub fn_type: ReactFunctionType,
    /// Whether the original source form was an arrow function.
    pub is_arrow: bool,
    /// The binding name for `const X = ...` / declaration forms, if any.
    pub fn_name: Option<String>,
}
