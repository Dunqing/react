//! FindContextIdentifiers — over `oxc_semantic` (stage N1.2.1 minimal stub).
//!
//! In the TS compiler this pass computes the set of variables declared between
//! the component/hook scope and any nested function scope that are referenced
//! from within those nested functions — the captured "context" identifiers that
//! must use `LoadContext`/`StoreContext` rather than `LoadLocal`/`StoreLocal`.
//!
//! Full transcription to the oxc-direct model is deferred to N1.3. For now this
//! returns an EMPTY set: trivial top-level fixtures (no nested function capture)
//! lower correctly, and any construct relying on captured context bails with a
//! graceful `Todo` from the dispatch in `build_hir`.

use std::collections::HashSet;

use oxc_semantic::Semantic;
use oxc_syntax::scope::ScopeId;
use oxc_syntax::symbol::SymbolId;

use crate::FunctionForm;

/// Compute the set of captured context identifiers for `func`.
///
/// N1.2.1: minimal stub returning an empty set (see module docs).
pub fn find_context_identifiers(
    _func: &FunctionForm<'_>,
    _semantic: &Semantic,
    _function_scope: ScopeId,
) -> HashSet<SymbolId> {
    // TODO(N1.3): traverse nested function scopes and collect symbols declared
    // in the compiled function that are referenced from inner functions.
    HashSet::new()
}
