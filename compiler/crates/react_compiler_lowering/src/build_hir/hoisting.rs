// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the root directory of this source tree.

//! Block-scoped declaration hoisting.
//!
//! Transcribes the `BlockStatement` hoisting logic from
//! `src/HIR/BuildHIR.ts` (the `case 'BlockStatement'` arm). When a hoistable
//! binding is referenced *before* its lexical declaration — and either the
//! reference occurs inside a nested function (TDZ is deferred) or the binding
//! is itself a hoisted function declaration — the compiler emits a
//! `DeclareContext` instruction with a `Hoisted{Const,Let,Function}` kind just
//! before the statement that first references it. This declares the identifier
//! early so downstream passes (EnterSSA, InferMutationAliasingEffects) see a
//! definition before any use.
//!
//! ## TS algorithm (reference)
//! For each statement `s` of the block, the TS pass traverses `s` looking for
//! referenced identifiers whose binding is declared in this block and not yet
//! seen. A reference is hoist-worthy when it is inside an inner function
//! (`fnDepth > 0`) OR the binding kind is `hoisted` (a function declaration).
//! After visiting `s`, any binding *declared* in `s` is removed from the
//! hoistable set.
//!
//! ## Reference-driven port
//! oxc resolves every reference to its binding `SymbolId` with a node span, so
//! instead of an AST traversal we compute, for each hoistable symbol declared
//! in this block, the index of the earliest top-level statement that contains a
//! hoist-worthy reference *before* the symbol's own declaration. We then emit
//! the `DeclareContext` instructions for that statement index right before
//! lowering it.

use std::collections::HashMap;

use oxc_span::GetSpan;
use oxc_span::Span;
use oxc_syntax::scope::ScopeId;
use oxc_syntax::symbol::SymbolId;
use react_compiler_diagnostics::CompilerError;
use react_compiler_diagnostics::CompilerErrorDetail;
use react_compiler_diagnostics::ErrorCategory;
use react_compiler_hir::Effect;
use react_compiler_hir::InstructionKind;
use react_compiler_hir::InstructionValue;
use react_compiler_hir::LValue;
use react_compiler_hir::Place;
use react_compiler_hir::VariableBinding;

use crate::hir_builder::HirBuilder;
use crate::semantic_queries as sq;
use crate::semantic_queries::BindingKind;

use super::lower_value_to_temporary;

/// Compute, per top-level statement index, the set of declarations to hoist
/// before lowering that statement.
///
/// `block_scope` is the scope whose bindings are candidates for hoisting (the
/// nested block's own scope, or — for a function body — the function scope).
/// `statement_spans` are the spans of the block's top-level statements, in
/// source order. The returned map keys are indices into `statement_spans`.
pub(crate) fn compute_block_hoists(
    builder: &HirBuilder,
    block_scope: ScopeId,
    statement_spans: &[Span],
) -> HashMap<usize, Vec<PendingHoistRaw>> {
    let semantic = builder.semantic();
    let mut out: HashMap<usize, Vec<PendingHoistRaw>> = HashMap::new();

    // Top-level statement spans are non-overlapping siblings in source order, so
    // the statement *containing* a span is the one with the greatest `start` not
    // exceeding the query's `start`. Pre-sort `(start, end, index)` by `start`
    // once so each containment lookup is O(log n) (binary search) instead of an
    // O(n) linear scan — turning the per-reference lookup loop from O(n*r) into
    // O(r log n).
    let mut sorted_spans: Vec<(u32, u32, usize)> = statement_spans
        .iter()
        .enumerate()
        .map(|(index, s)| (s.start, s.end, index))
        .collect();
    sorted_spans.sort_by_key(|&(start, _, _)| start);

    // The top-level statement index that contains a given span, by containment.
    let stmt_index_of = |span: Span| -> Option<usize> {
        // Rightmost entry whose `start <= span.start`.
        let pos = sorted_spans.partition_point(|&(start, _, _)| start <= span.start);
        if pos == 0 {
            return None;
        }
        let (start, end, index) = sorted_spans[pos - 1];
        if start <= span.start && span.end <= end {
            Some(index)
        } else {
            None
        }
    };

    // Candidate bindings: every non-param binding declared directly in this
    // block scope (mirrors `stmt.scope.bindings` minus `kind === 'param'`).
    for symbol_id in sq::bindings_in_scope(semantic, block_scope) {
        let kind = sq::binding_kind(semantic, symbol_id);
        if matches!(kind, BindingKind::Param | BindingKind::Module) {
            continue;
        }
        let is_function_decl = matches!(kind, BindingKind::Hoisted);

        let decl_span = sq::declaration_span(semantic, symbol_id);
        // The top-level statement that declares this binding. In the TS pass a
        // binding stays "hoistable" until the statement declaring it has been
        // fully visited, so references in statements at-or-before that index
        // qualify (this is what lets a function declaration's own recursive
        // self-reference trigger a hoist).
        let Some(decl_stmt_index) = stmt_index_of(decl_span) else {
            continue;
        };
        // Enclosing function scope of the binding — references that live in a
        // strictly-nested function cross a function boundary.
        let binding_fn =
            sq::enclosing_function_scope(semantic, semantic.scoping().symbol_scope_id(symbol_id));

        // Find the earliest hoist-worthy reference. A reference qualifies when
        // it is in a statement at or before the declaration's statement *and*
        // either occurs inside an inner function or the binding is a function
        // declaration. We hoist before the statement of the earliest such
        // reference.
        let mut earliest: Option<(usize, Span)> = None;
        for reference in semantic.scoping().get_resolved_references(symbol_id) {
            let ref_node_id = reference.node_id();
            let ref_span = semantic.nodes().get_node(ref_node_id).span();
            let Some(ref_stmt_index) = stmt_index_of(ref_span) else {
                continue;
            };
            // Only references in statements at or before the declaration's own
            // statement are "before declaration" in the TS statement-ordering
            // sense.
            if ref_stmt_index > decl_stmt_index {
                continue;
            }
            let qualifies = if is_function_decl {
                true
            } else {
                let ref_scope = sq::scope_of_node(semantic, ref_node_id);
                let ref_fn = sq::enclosing_function_scope(semantic, ref_scope);
                ref_fn != binding_fn
                    && sq::is_descendant_or_self_scope(semantic, ref_fn, binding_fn)
            };
            if !qualifies {
                continue;
            }
            match earliest {
                Some((idx, span)) if (idx, span.start) <= (ref_stmt_index, ref_span.start) => {}
                _ => earliest = Some((ref_stmt_index, ref_span)),
            }
        }

        let Some((stmt_index, ref_span)) = earliest else {
            continue;
        };

        out.entry(stmt_index).or_default().push(PendingHoistRaw {
            symbol_id,
            binding_kind: kind,
            ref_span,
        });
    }

    // Stable order: declarations hoisted before the same statement are emitted
    // in source order of their triggering reference (matches the TS traversal
    // order, which visits identifiers left-to-right).
    for v in out.values_mut() {
        v.sort_by_key(|p| (p.ref_span.start, p.symbol_id.index()));
    }

    out
}

/// Raw hoist record before the `InstructionKind` is resolved (which needs the
/// builder mutably to record Todo bails for unsupported declaration kinds).
pub(crate) struct PendingHoistRaw {
    pub(crate) symbol_id: SymbolId,
    pub(crate) binding_kind: BindingKind,
    pub(crate) ref_span: Span,
}

/// Emit the `DeclareContext` instructions for the given pending hoists. Mirrors
/// the per-binding emit block in `BuildHIR.ts` (`case 'BlockStatement'`).
pub(crate) fn emit_hoists(
    builder: &mut HirBuilder,
    hoists: &[PendingHoistRaw],
) -> Result<(), CompilerError> {
    for raw in hoists {
        let symbol_id = raw.symbol_id;
        let binding_u32 = symbol_id.index() as u32;
        if builder.environment().is_hoisted_identifier(binding_u32) {
            continue;
        }

        let kind = match raw.binding_kind {
            // const / var are hoisted as HoistedConst.
            BindingKind::Const | BindingKind::Var => InstructionKind::HoistedConst,
            BindingKind::Let => InstructionKind::HoistedLet,
            BindingKind::Hoisted => InstructionKind::HoistedFunction,
            _ => {
                builder.record_error(CompilerErrorDetail {
                    reason: "Unsupported declaration type for hoisting".to_string(),
                    category: ErrorCategory::Todo,
                    loc: Some(builder.loc_of_span(raw.ref_span)),
                    description: None,
                    suggestions: None,
                })?;
                continue;
            }
        };

        let loc = builder.loc_of_span(raw.ref_span);
        let name = builder
            .semantic()
            .scoping()
            .symbol_name(symbol_id)
            .to_string();
        let binding =
            builder.resolve_identifier_symbol(&name, Some(symbol_id), Some(loc))?;
        let identifier = match binding {
            VariableBinding::Identifier { identifier, .. } => identifier,
            _ => {
                // Expected hoisted binding to be a local identifier, not a global.
                builder.record_error(CompilerErrorDetail {
                    reason: "Expected hoisted binding to be a local identifier, not a global"
                        .to_string(),
                    category: ErrorCategory::Invariant,
                    loc: Some(loc),
                    description: None,
                    suggestions: None,
                })?;
                continue;
            }
        };

        let place = Place {
            identifier,
            effect: Effect::Unknown,
            reactive: false,
            loc: Some(loc),
        };
        lower_value_to_temporary(
            builder,
            InstructionValue::DeclareContext {
                lvalue: LValue { kind, place },
                loc: Some(loc),
            },
        )?;
        // A hoisted binding is also a *context* identifier: it is declared via
        // DeclareContext and (when captured by a nested function) loaded/stored
        // through LoadContext/StoreContext. Mirrors `addHoistedIdentifier`
        // adding to BOTH the hoisted and context sets (Environment.ts).
        builder.add_context_identifier(symbol_id);
        builder
            .environment_mut()
            .add_hoisted_identifier(binding_u32);
    }
    Ok(())
}
