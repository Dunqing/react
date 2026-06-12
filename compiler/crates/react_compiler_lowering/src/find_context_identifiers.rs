//! FindContextIdentifiers — over `oxc_semantic` (reference-driven).
//!
//! In the TS compiler this pass computes the set of variables that must use
//! `LoadContext`/`StoreContext` rather than `LoadLocal`/`StoreLocal`: variables
//! that are *reassigned* and *referenced/reassigned* from inside a nested
//! function (i.e. across a function boundary relative to where they are bound).
//!
//! oxc resolves every reference to its binding `SymbolId` directly, and every
//! node records its enclosing scope. We therefore do not walk the AST: for each
//! symbol declared inside the compiled function we inspect its resolved
//! references and classify them by whether they cross a function boundary
//! (relative to the binding's own enclosing function scope).

use std::collections::HashSet;

use oxc_semantic::Semantic;
use oxc_syntax::scope::ScopeId;
use oxc_syntax::symbol::SymbolId;

use crate::FunctionForm;
use crate::semantic_queries as sq;

/// Compute the set of captured context identifiers for `func`.
///
/// A binding is a context identifier if:
/// - It is reassigned from inside a nested function (`reassigned_by_inner`), OR
/// - It is reassigned AND referenced from inside a nested function
///   (`reassigned && referenced_by_inner`).
pub fn find_context_identifiers(
    _func: &FunctionForm<'_>,
    semantic: &Semantic,
    function_scope: ScopeId,
) -> HashSet<SymbolId> {
    let scoping = semantic.scoping();
    let mut result: HashSet<SymbolId> = HashSet::new();

    for sym in scoping.symbol_ids() {
        let decl_scope = scoping.symbol_scope_id(sym);
        // Only consider symbols declared at or inside the compiled function.
        // This skips program-scope bindings (module locals / imports) and any
        // symbols belonging to a sibling/outer function.
        if !sq::is_descendant_or_self_scope(semantic, decl_scope, function_scope) {
            continue;
        }

        let binding_fn = sq::enclosing_function_scope(semantic, decl_scope);

        let mut reassigned = false;
        let mut reassigned_by_inner = false;
        let mut referenced_by_inner = false;

        for reference in scoping.get_resolved_references(sym) {
            let is_write = reference.is_write();
            if is_write {
                reassigned = true;
            }

            let ref_scope = sq::scope_of_node(semantic, reference.node_id());
            let ref_fn = sq::enclosing_function_scope(semantic, ref_scope);

            // The reference crosses a function boundary if it is used from a
            // function nested strictly below the binding's enclosing function.
            if ref_fn != binding_fn
                && sq::is_descendant_or_self_scope(semantic, ref_fn, binding_fn)
            {
                referenced_by_inner = true;
                if is_write {
                    reassigned_by_inner = true;
                }
            }
        }

        if reassigned_by_inner || (reassigned && referenced_by_inner) {
            result.insert(sym);
        }
    }

    result
}
