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

/// Resolved plan for emitting a `@gating`-gated function during assembly.
///
/// When a function has gating configured (static `gating` option or a dynamic
/// `'use memo if(...)'` directive), the compiler emits BOTH the compiled and the
/// original function and selects between them at runtime via an imported gating
/// flag. All collision-sensitive names (the gating import local name, and — for
/// the use-before-declaration dispatcher form — the `_result` / `_optimized` /
/// `_unoptimized` names) are resolved during the pipeline (where the
/// `ProgramContext` import/uid state lives) and carried here for assembly.
///
/// Mirrors `insertGatedFunctionDeclaration` in `Entrypoint/Gating.ts`.
#[derive(Debug, Clone)]
pub struct GatingPlan {
    /// Resolved local binding name for the gating import (collision-aware).
    pub gating_local_name: String,
    /// Module the gating function is imported from.
    pub gating_source: String,
    /// The imported specifier name (`importSpecifierName`).
    pub gating_imported: String,
    /// Whether the function is referenced before its declaration at top level
    /// (requires the hoistable dispatcher form rather than a simple `const`).
    pub referenced_before_declaration: bool,
    /// `<gating>_result` — the gating-call result binding (dispatcher form only).
    pub result_name: Option<String>,
    /// `<origName>_optimized` — the compiled function name (dispatcher form only).
    pub optimized_name: Option<String>,
    /// `<origName>_unoptimized` — the original function name (dispatcher form only).
    pub unoptimized_name: Option<String>,
}

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
    /// When set, the function is emitted in a `@gating`-gated form selecting
    /// between the compiled and original function at runtime. `None` for the
    /// common (non-gated) case and for outlined functions.
    pub gating: Option<GatingPlan>,
    /// For outlined functions (which have a sentinel `fn_span` of `(0, 0)` and
    /// no source location), the source span of the PARENT function the outlined
    /// fn was extracted from. Assembly inserts the outlined declaration directly
    /// AFTER the parent's spliced statement, mirroring TS
    /// `insertNewOutlinedFunctionNode` (which inserts adjacent to the parent)
    /// rather than appending all outlined fns at the end of the program. `None`
    /// for ordinary (spanned) functions.
    pub insert_after_span: Option<(u32, u32)>,
}
