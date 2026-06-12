// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the root directory of this source tree.

//! Scope and binding queries answered DIRECTLY from `oxc_semantic::Semantic`.
//!
//! Unlike the old `react_compiler_ast::scope::ScopeInfo`, this module does NOT
//! mirror oxc's scope/symbol tables into a parallel data structure. Each query
//! is a free function over `&Semantic` (plus `&Program`/AST nodes where the
//! declaration must be inspected), delegating to oxc's `Scoping`, `AstNodes`,
//! and resolved `Reference`s.
//!
//! The enums mirror the old `react_compiler_ast::scope` enums so that the
//! lowering rewrite (stage N1.2) can keep its existing `match` arms unchanged.
//!
//! ## oxc 0.121 API notes
//! - In 0.121 there is a single unified `Scoping` struct (accessed via
//!   `semantic.scoping()`), not separate `SymbolTable` / `ScopeTree`.
//! - References are resolved to symbols during `SemanticBuilder`. An
//!   `IdentifierReference` carries a `Cell<Option<ReferenceId>>`; the resolved
//!   `Reference::symbol_id()` is the binding's `SymbolId`. No offset maps.
//! - `AstNode::scope_id()` gives the enclosing scope of any node directly.

use oxc_ast::AstKind;
use oxc_semantic::NodeId;
use oxc_semantic::Semantic;
use oxc_span::GetSpan;
use oxc_span::Span;
use oxc_syntax::reference::ReferenceId;
use oxc_syntax::scope::ScopeFlags;
use oxc_syntax::scope::ScopeId;
use oxc_syntax::symbol::SymbolFlags;
use oxc_syntax::symbol::SymbolId;

// ---------------------------------------------------------------------------
// Enums (mirror `react_compiler_ast::scope` so downstream match arms are stable)
// ---------------------------------------------------------------------------

/// The kind of a binding (variable/declaration). Mirrors
/// `react_compiler_ast::scope::BindingKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingKind {
    Var,
    Let,
    Const,
    Param,
    /// Import bindings (import declarations).
    Module,
    /// Function declarations (hoisted).
    Hoisted,
    /// Other local bindings (class declarations, type aliases, etc.).
    Local,
    /// Binding kind not recognized.
    Unknown,
}

/// The kind of a scope. Mirrors `react_compiler_ast::scope::ScopeKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeKind {
    Program,
    Function,
    Block,
    For,
    Class,
    Switch,
    Catch,
}

/// The kind of an import binding. Mirrors
/// `react_compiler_ast::scope::ImportBindingKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportBindingKind {
    Default,
    Named,
    Namespace,
}

/// Information about an import binding, derived from its declaring
/// `ImportDeclaration`. Mirrors `react_compiler_ast::scope::ImportBindingData`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportInfo {
    /// The module specifier string (e.g. "react" in `import {useState} from 'react'`).
    pub source: String,
    pub kind: ImportBindingKind,
    /// For named imports: the imported name (e.g. "bar" in `import {bar as baz} from 'foo'`).
    /// `None` for default and namespace imports.
    pub imported: Option<String>,
}

// ---------------------------------------------------------------------------
// Reference resolution
// ---------------------------------------------------------------------------

/// Resolve a `ReferenceId` to the `SymbolId` of the binding it refers to.
///
/// References are resolved by `SemanticBuilder`; unresolved references (globals)
/// return `None`. Backed by `Scoping::get_reference` + `Reference::symbol_id`.
pub fn resolve_reference_id(semantic: &Semantic, reference_id: ReferenceId) -> Option<SymbolId> {
    semantic.scoping().get_reference(reference_id).symbol_id()
}

/// Resolve an `IdentifierReference` AST node to the `SymbolId` of its binding.
///
/// Reads the `reference_id` populated on the node by `SemanticBuilder`, then
/// delegates to [`resolve_reference_id`]. Returns `None` for globals or if the
/// node was never assigned a reference id.
pub fn resolve_identifier_reference(
    semantic: &Semantic,
    ident: &oxc_ast::ast::IdentifierReference,
) -> Option<SymbolId> {
    let reference_id = ident.reference_id.get()?;
    resolve_reference_id(semantic, reference_id)
}

// ---------------------------------------------------------------------------
// Node -> scope
// ---------------------------------------------------------------------------

/// The enclosing `ScopeId` for an AST node, by its `NodeId`.
///
/// Backed by `AstNode::scope_id()` (every node records its enclosing scope).
pub fn scope_of_node(semantic: &Semantic, node_id: NodeId) -> ScopeId {
    semantic.nodes().get_node(node_id).scope_id()
}

/// The root (program/module) `ScopeId`. Backed by `Scoping::root_scope_id`.
pub fn program_scope(semantic: &Semantic) -> ScopeId {
    semantic.scoping().root_scope_id()
}

/// The parent of a scope, or `None` for the root scope.
/// Backed by `Scoping::scope_parent_id`.
pub fn scope_parent(semantic: &Semantic, scope_id: ScopeId) -> Option<ScopeId> {
    semantic.scoping().scope_parent_id(scope_id)
}

// ---------------------------------------------------------------------------
// Symbol -> binding kind
// ---------------------------------------------------------------------------

/// Classify a symbol's `BindingKind`.
///
/// Combines `SymbolFlags` with inspection of the declaration AST node. The node
/// inspection runs *before* the `FunctionScopedVariable` flag check because oxc
/// marks function parameters and catch parameters with `FunctionScopedVariable`;
/// we must split `Param` from `Var` by looking at the declaration node:
/// `FormalParameter`/`FormalParameterRest` -> `Param`. `Import` -> `Module`,
/// `FunctionDeclaration` -> `Hoisted`, class -> `Local`.
pub fn binding_kind(semantic: &Semantic, symbol_id: SymbolId) -> BindingKind {
    let flags = semantic.scoping().symbol_flags(symbol_id);

    if flags.contains(SymbolFlags::Import) {
        return BindingKind::Module;
    }

    // Inspect the declaration node first: parameters and catch params carry the
    // FunctionScopedVariable flag, so we must distinguish them by AST shape
    // before falling through to the flag-based var/let/const classification.
    let decl_node = semantic.symbol_declaration(symbol_id);
    match decl_node.kind() {
        AstKind::FormalParameter(_) => return BindingKind::Param,
        AstKind::FormalParameterRest(_) => return BindingKind::Param,
        AstKind::CatchParameter(_) => return BindingKind::Let,
        AstKind::TSTypeAliasDeclaration(_) => return BindingKind::Local,
        AstKind::TSEnumDeclaration(_) => return BindingKind::Local,
        AstKind::TSModuleDeclaration(_) => return BindingKind::Local,
        AstKind::Function(_) => {
            if flags.contains(SymbolFlags::Function) {
                return BindingKind::Hoisted;
            }
            return BindingKind::Local;
        }
        AstKind::Class(_) => return BindingKind::Local,
        _ => {}
    }

    if flags.contains(SymbolFlags::FunctionScopedVariable) {
        return BindingKind::Var;
    }

    if flags.contains(SymbolFlags::BlockScopedVariable) {
        if flags.contains(SymbolFlags::ConstVariable) {
            return BindingKind::Const;
        }
        return BindingKind::Let;
    }

    if flags.contains(SymbolFlags::Function) {
        BindingKind::Hoisted
    } else if flags.contains(SymbolFlags::Class) {
        BindingKind::Local
    } else {
        BindingKind::Unknown
    }
}

// ---------------------------------------------------------------------------
// Symbol -> declaration node / span
// ---------------------------------------------------------------------------

/// The `NodeId` of the AST node that declares a symbol.
/// Backed by `Scoping::symbol_declaration`.
pub fn declaration_node_id(semantic: &Semantic, symbol_id: SymbolId) -> NodeId {
    semantic.scoping().symbol_declaration(symbol_id)
}

/// The `Span` of the AST node that declares a symbol.
pub fn declaration_span(semantic: &Semantic, symbol_id: SymbolId) -> Span {
    semantic.symbol_declaration(symbol_id).kind().span()
}

// ---------------------------------------------------------------------------
// Symbol -> import info
// ---------------------------------------------------------------------------

/// Extract [`ImportInfo`] for a symbol whose declaration is an import specifier.
///
/// Returns `None` for non-import symbols. The module source is read from the
/// enclosing `ImportDeclaration` found by walking the parent chain from the
/// specifier node.
pub fn import_info(semantic: &Semantic, symbol_id: SymbolId) -> Option<ImportInfo> {
    let decl_node = semantic.symbol_declaration(symbol_id);

    match decl_node.kind() {
        AstKind::ImportDefaultSpecifier(_) => {
            let import_decl = find_import_declaration(semantic, decl_node.id())?;
            Some(ImportInfo {
                source: import_decl.source.value.to_string(),
                kind: ImportBindingKind::Default,
                imported: None,
            })
        }
        AstKind::ImportNamespaceSpecifier(_) => {
            let import_decl = find_import_declaration(semantic, decl_node.id())?;
            Some(ImportInfo {
                source: import_decl.source.value.to_string(),
                kind: ImportBindingKind::Namespace,
                imported: None,
            })
        }
        AstKind::ImportSpecifier(spec) => {
            let import_decl = find_import_declaration(semantic, decl_node.id())?;
            let imported_name = match &spec.imported {
                oxc_ast::ast::ModuleExportName::IdentifierName(ident) => ident.name.to_string(),
                oxc_ast::ast::ModuleExportName::IdentifierReference(ident) => {
                    ident.name.to_string()
                }
                oxc_ast::ast::ModuleExportName::StringLiteral(lit) => lit.value.to_string(),
            };
            Some(ImportInfo {
                source: import_decl.source.value.to_string(),
                kind: ImportBindingKind::Named,
                imported: Some(imported_name),
            })
        }
        _ => None,
    }
}

/// Walk up the parent chain from an import specifier node to its enclosing
/// `ImportDeclaration`. Backed by `AstNodes::parent_id`.
fn find_import_declaration<'a>(
    semantic: &'a Semantic,
    specifier_node_id: NodeId,
) -> Option<&'a oxc_ast::ast::ImportDeclaration<'a>> {
    let mut current_id = specifier_node_id;
    // Bound the walk to avoid an infinite loop on a malformed tree.
    for _ in 0..10 {
        let parent_id = semantic.nodes().parent_id(current_id);
        if parent_id == current_id {
            return None;
        }
        let parent_node = semantic.nodes().get_node(parent_id);
        if let AstKind::ImportDeclaration(decl) = parent_node.kind() {
            return Some(decl);
        }
        current_id = parent_id;
    }
    None
}

// ---------------------------------------------------------------------------
// Scope -> kind
// ---------------------------------------------------------------------------

/// Classify a scope's [`ScopeKind`].
///
/// Combines `ScopeFlags` with the scope-creating AST node. `ScopeFlags` cannot
/// distinguish a `for` loop scope from a plain block scope (both are non-function
/// block scopes), so we inspect the node returned by `Scoping::get_node_id`:
/// `For*Statement` -> `For`, otherwise `Block`. Class/switch are likewise read
/// from the node.
pub fn scope_kind(semantic: &Semantic, scope_id: ScopeId) -> ScopeKind {
    let flags = semantic.scoping().scope_flags(scope_id);

    if flags.contains(ScopeFlags::Top) {
        return ScopeKind::Program;
    }
    if flags.intersects(ScopeFlags::Function) {
        return ScopeKind::Function;
    }
    if flags.contains(ScopeFlags::CatchClause) {
        return ScopeKind::Catch;
    }
    if flags.contains(ScopeFlags::ClassStaticBlock) {
        return ScopeKind::Class;
    }

    // Distinguish For from Block (and detect Class/Switch) via the scope node.
    let node_id = semantic.scoping().get_node_id(scope_id);
    match semantic.nodes().get_node(node_id).kind() {
        AstKind::ForStatement(_) | AstKind::ForInStatement(_) | AstKind::ForOfStatement(_) => {
            ScopeKind::For
        }
        AstKind::Class(_) => ScopeKind::Class,
        AstKind::SwitchStatement(_) => ScopeKind::Switch,
        _ => ScopeKind::Block,
    }
}

// ---------------------------------------------------------------------------
// Binding lookup (scope + parent chain)
// ---------------------------------------------------------------------------

/// Look up a binding by name starting at `scope_id`, walking up the parent
/// chain. Returns the binding's `SymbolId`, or `None` for globals.
///
/// Implemented over `Scoping::iter_bindings_in` + `symbol_name` + `scope_parent_id`
/// rather than `Scoping::find_binding` to avoid threading an allocator (the oxc
/// lookup takes an arena-allocated `Ident`). Walking the chain returns the
/// nearest (innermost) declaration, so shadowed names resolve correctly.
pub fn get_binding(semantic: &Semantic, scope_id: ScopeId, name: &str) -> Option<SymbolId> {
    let scoping = semantic.scoping();
    let mut current = Some(scope_id);
    while let Some(id) = current {
        for symbol_id in scoping.iter_bindings_in(id) {
            if scoping.symbol_name(symbol_id) == name {
                return Some(symbol_id);
            }
        }
        current = scoping.scope_parent_id(id);
    }
    None
}

/// Look up a binding by name declared *directly* in `scope_id` (no parent walk).
pub fn get_binding_in_scope(
    semantic: &Semantic,
    scope_id: ScopeId,
    name: &str,
) -> Option<SymbolId> {
    let scoping = semantic.scoping();
    scoping
        .iter_bindings_in(scope_id)
        .find(|&symbol_id| scoping.symbol_name(symbol_id) == name)
}

// ---------------------------------------------------------------------------
// Bindings declared in a scope
// ---------------------------------------------------------------------------

/// All symbols declared directly in a scope. Backed by `Scoping::iter_bindings_in`.
pub fn bindings_in_scope(semantic: &Semantic, scope_id: ScopeId) -> Vec<SymbolId> {
    semantic.scoping().iter_bindings_in(scope_id).collect()
}

/// Direct child scope ids of `scope_id`.
/// Backed by scanning all scopes and comparing `scope_parent_id`.
pub fn child_scopes(semantic: &Semantic, scope_id: ScopeId) -> Vec<ScopeId> {
    let scoping = semantic.scoping();
    scoping
        .scope_descendants_from_root()
        .filter(|&sid| scoping.scope_parent_id(sid) == Some(scope_id))
        .collect()
}

/// Symbols declared in a scope plus those in its direct child *block* scopes.
///
/// In Babel a function body's block shares the function scope, so var/let/const
/// all live in one scope. oxc may split them (params/var in the function scope,
/// let/const in a child block). This merges them to match the Babel-shaped view
/// the compiler expects (mirrors `ScopeInfo::scope_bindings_with_children`).
pub fn bindings_in_scope_with_children(semantic: &Semantic, scope_id: ScopeId) -> Vec<SymbolId> {
    let scoping = semantic.scoping();
    let mut out: Vec<SymbolId> = scoping.iter_bindings_in(scope_id).collect();
    for child in child_scopes(semantic, scope_id) {
        if scope_kind(semantic, child) == ScopeKind::Block {
            out.extend(scoping.iter_bindings_in(child));
        }
    }
    out
}

/// Find a block scope in the descendants of `ancestor` that declares *all* of
/// `names`. Skips function scopes and any scope rejected by `is_claimed`.
///
/// Used for synthetic blocks where position-based lookup fails. Mirrors
/// `ScopeInfo::find_block_scope_by_bindings`.
pub fn find_block_scope_by_bindings(
    semantic: &Semantic,
    names: &[&str],
    ancestor: ScopeId,
    is_claimed: impl Fn(ScopeId) -> bool,
) -> Option<ScopeId> {
    let scoping = semantic.scoping();
    // scope_descendants_from_root yields scopes in document order; restrict to
    // those whose ancestor chain includes `ancestor`.
    for sid in scoping.scope_descendants_from_root() {
        if !is_descendant_or_self(semantic, sid, ancestor) {
            continue;
        }
        if scope_kind(semantic, sid) == ScopeKind::Function {
            continue;
        }
        if is_claimed(sid) {
            continue;
        }
        let declares_all = names.iter().all(|name| {
            scoping
                .iter_bindings_in(sid)
                .any(|symbol_id| scoping.symbol_name(symbol_id) == *name)
        });
        if declares_all {
            return Some(sid);
        }
    }
    None
}

/// True if `scope` is `ancestor` or a (transitive) descendant of it.
fn is_descendant_or_self(semantic: &Semantic, scope: ScopeId, ancestor: ScopeId) -> bool {
    let scoping = semantic.scoping();
    let mut current = Some(scope);
    while let Some(id) = current {
        if id == ancestor {
            return true;
        }
        current = scoping.scope_parent_id(id);
    }
    false
}

// ---------------------------------------------------------------------------
// Function-boundary scope queries (used by FindContextIdentifiers and the
// nested-function captured-context analysis)
// ---------------------------------------------------------------------------

/// Innermost Function-kind scope enclosing `scope_id` (inclusive), or the
/// program scope if none. Used to detect references that cross a function
/// boundary relative to their binding.
pub fn enclosing_function_scope(semantic: &Semantic, scope_id: ScopeId) -> ScopeId {
    let mut current = Some(scope_id);
    while let Some(id) = current {
        if scope_kind(semantic, id) == ScopeKind::Function
            || scope_kind(semantic, id) == ScopeKind::Program
        {
            return id;
        }
        current = scope_parent(semantic, id);
    }
    program_scope(semantic)
}

/// True if `scope` is a (transitive) descendant of `ancestor`, or equal.
pub fn is_descendant_or_self_scope(
    semantic: &Semantic,
    scope: ScopeId,
    ancestor: ScopeId,
) -> bool {
    let mut current = Some(scope);
    while let Some(id) = current {
        if id == ancestor {
            return true;
        }
        current = scope_parent(semantic, id);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_allocator::Allocator;
    use oxc_ast::ast::Program;
    use oxc_parser::Parser;
    use oxc_semantic::SemanticBuilder;
    use oxc_span::SourceType;

    /// Run `f` with a freshly built `Semantic` for `source`.
    fn with_semantic<R>(source: &str, f: impl FnOnce(&Semantic, &Program) -> R) -> R {
        let allocator = Allocator::default();
        let source_type = SourceType::tsx();
        let ret = Parser::new(&allocator, source, source_type).parse();
        assert!(ret.errors.is_empty(), "parse errors: {:?}", ret.errors);
        let program = ret.program;
        let semantic_ret = SemanticBuilder::new().build(&program);
        assert!(
            semantic_ret.errors.is_empty(),
            "semantic errors: {:?}",
            semantic_ret.errors
        );
        f(&semantic_ret.semantic, &program)
    }

    /// Look up a symbol by name anywhere in the program (first match).
    fn symbol_named(semantic: &Semantic, name: &str) -> SymbolId {
        let scoping = semantic.scoping();
        scoping
            .symbol_ids()
            .find(|&id| scoping.symbol_name(id) == name)
            .unwrap_or_else(|| panic!("no symbol named {name}"))
    }

    #[test]
    fn classifies_const_let_var_param_function() {
        let src = "
            function outer(p) {
                var v = 1;
                let l = 2;
                const c = 3;
                function inner() {}
                return p + v + l + c;
            }
        ";
        with_semantic(src, |semantic, _program| {
            assert_eq!(
                binding_kind(semantic, symbol_named(semantic, "v")),
                BindingKind::Var
            );
            assert_eq!(
                binding_kind(semantic, symbol_named(semantic, "l")),
                BindingKind::Let
            );
            assert_eq!(
                binding_kind(semantic, symbol_named(semantic, "c")),
                BindingKind::Const
            );
            assert_eq!(
                binding_kind(semantic, symbol_named(semantic, "p")),
                BindingKind::Param
            );
            assert_eq!(
                binding_kind(semantic, symbol_named(semantic, "inner")),
                BindingKind::Hoisted
            );
            assert_eq!(
                binding_kind(semantic, symbol_named(semantic, "outer")),
                BindingKind::Hoisted
            );
        });
    }

    #[test]
    fn classifies_class_as_local() {
        let src = "class Foo {}";
        with_semantic(src, |semantic, _program| {
            assert_eq!(
                binding_kind(semantic, symbol_named(semantic, "Foo")),
                BindingKind::Local
            );
        });
    }

    #[test]
    fn classifies_imports() {
        let src = "
            import Default from 'mod-a';
            import {bar as baz} from 'mod-b';
            import * as ns from 'mod-c';
        ";
        with_semantic(src, |semantic, _program| {
            // default import
            let default_sym = symbol_named(semantic, "Default");
            assert_eq!(binding_kind(semantic, default_sym), BindingKind::Module);
            assert_eq!(
                import_info(semantic, default_sym),
                Some(ImportInfo {
                    source: "mod-a".to_string(),
                    kind: ImportBindingKind::Default,
                    imported: None,
                })
            );

            // named import with alias
            let named_sym = symbol_named(semantic, "baz");
            assert_eq!(binding_kind(semantic, named_sym), BindingKind::Module);
            assert_eq!(
                import_info(semantic, named_sym),
                Some(ImportInfo {
                    source: "mod-b".to_string(),
                    kind: ImportBindingKind::Named,
                    imported: Some("bar".to_string()),
                })
            );

            // namespace import
            let ns_sym = symbol_named(semantic, "ns");
            assert_eq!(binding_kind(semantic, ns_sym), BindingKind::Module);
            assert_eq!(
                import_info(semantic, ns_sym),
                Some(ImportInfo {
                    source: "mod-c".to_string(),
                    kind: ImportBindingKind::Namespace,
                    imported: None,
                })
            );
        });
    }

    #[test]
    fn for_loop_scope_is_for_not_block() {
        let src = "
            for (let i = 0; i < 10; i++) {
                const x = i;
            }
            { const plain = 1; }
        ";
        with_semantic(src, |semantic, _program| {
            // The scope declaring `i` is the for-loop scope.
            let i_sym = symbol_named(semantic, "i");
            let for_scope = semantic.scoping().symbol_scope_id(i_sym);
            assert_eq!(
                scope_kind(semantic, for_scope),
                ScopeKind::For,
                "for-loop header scope must classify as For"
            );

            // The scope declaring `plain` is a plain block.
            let plain_sym = symbol_named(semantic, "plain");
            let block_scope = semantic.scoping().symbol_scope_id(plain_sym);
            assert_eq!(
                scope_kind(semantic, block_scope),
                ScopeKind::Block,
                "plain block scope must classify as Block"
            );

            // Program scope.
            assert_eq!(
                scope_kind(semantic, program_scope(semantic)),
                ScopeKind::Program
            );
        });
    }

    #[test]
    fn shadowed_binding_resolves_to_nearest() {
        let src = "
            const x = 1;
            function f() {
                const x = 2;
                {
                    const x = 3;
                    return x;
                }
            }
        ";
        with_semantic(src, |semantic, _program| {
            let scoping = semantic.scoping();
            // There are three distinct `x` symbols.
            let x_symbols: Vec<SymbolId> = scoping
                .symbol_ids()
                .filter(|&id| scoping.symbol_name(id) == "x")
                .collect();
            assert_eq!(x_symbols.len(), 3, "expected three shadowed x bindings");

            // From the innermost block scope, get_binding resolves to the
            // innermost `x` (the one declared in that block), not the outer ones.
            let inner_x = x_symbols
                .iter()
                .copied()
                .max_by_key(|&id| declaration_span(semantic, id).start)
                .unwrap();
            let inner_scope = scoping.symbol_scope_id(inner_x);
            let resolved = get_binding(semantic, inner_scope, "x").unwrap();
            assert_eq!(
                resolved, inner_x,
                "get_binding from innermost scope must resolve to the nearest x"
            );

            // From the program scope, get_binding resolves to the outermost `x`.
            let outer_x = x_symbols
                .iter()
                .copied()
                .min_by_key(|&id| declaration_span(semantic, id).start)
                .unwrap();
            let resolved_outer = get_binding(semantic, program_scope(semantic), "x").unwrap();
            assert_eq!(resolved_outer, outer_x);
        });
    }

    #[test]
    fn reference_resolves_to_right_symbol() {
        let src = "
            const value = 1;
            function use() {
                return value;
            }
        ";
        with_semantic(src, |semantic, _program| {
            let value_sym = symbol_named(semantic, "value");
            let scoping = semantic.scoping();

            // There must be at least one resolved reference to `value`.
            let resolved_refs = scoping.get_resolved_reference_ids(value_sym);
            assert!(
                !resolved_refs.is_empty(),
                "expected a resolved reference to `value`"
            );

            // Each resolved reference id maps back to the same symbol via our
            // free function.
            for &ref_id in resolved_refs {
                assert_eq!(
                    resolve_reference_id(semantic, ref_id),
                    Some(value_sym),
                    "reference must resolve to the `value` symbol"
                );
            }
        });
    }

    #[test]
    fn declaration_node_and_span() {
        let src = "function f() { return 1; }";
        with_semantic(src, |semantic, _program| {
            let f_sym = symbol_named(semantic, "f");
            // declaration node + span are consistent (function declaration node).
            let decl_id = declaration_node_id(semantic, f_sym);
            let span = declaration_span(semantic, f_sym);
            assert!(span.end > span.start);
            // scope_of_node on the declaration node yields the enclosing scope.
            let enclosing = scope_of_node(semantic, decl_id);
            assert_eq!(scope_kind(semantic, enclosing), ScopeKind::Program);
        });
    }

    #[test]
    fn bindings_and_block_scope_lookup() {
        // `a`/`b` live directly in the function scope; `inner1`/`inner2` live in
        // a genuine nested block scope (the case find_block_scope_by_bindings is
        // designed for: locate a block by the set of names it declares).
        let src = "
            function f() {
                const a = 1;
                const b = 2;
                {
                    const inner1 = 3;
                    const inner2 = 4;
                    return a + b + inner1 + inner2;
                }
            }
        ";
        with_semantic(src, |semantic, _program| {
            let scoping = semantic.scoping();

            // The function scope declares a and b directly.
            let f_scope = function_scope(semantic, symbol_named(semantic, "f"));
            let direct: Vec<&str> = bindings_in_scope(semantic, f_scope)
                .iter()
                .map(|&s| scoping.symbol_name(s))
                .collect();
            assert!(direct.contains(&"a"), "function scope declares a");
            assert!(direct.contains(&"b"), "function scope declares b");

            // bindings_in_scope_with_children also pulls in the nested block's
            // inner1 / inner2 from the direct child Block scope.
            let merged: Vec<&str> = bindings_in_scope_with_children(semantic, f_scope)
                .iter()
                .map(|&s| scoping.symbol_name(s))
                .collect();
            assert!(merged.contains(&"a"));
            assert!(merged.contains(&"b"));
            assert!(
                merged.contains(&"inner1"),
                "child block bindings should be merged in"
            );
            assert!(merged.contains(&"inner2"));

            // find_block_scope_by_bindings locates the nested block by its names.
            let found = find_block_scope_by_bindings(
                semantic,
                &["inner1", "inner2"],
                program_scope(semantic),
                |_| false,
            )
            .expect("should find the nested block declaring inner1 & inner2");
            assert_eq!(scope_kind(semantic, found), ScopeKind::Block);
            // The found scope is the same one declaring inner1.
            let inner1_scope = scoping.symbol_scope_id(symbol_named(semantic, "inner1"));
            assert_eq!(found, inner1_scope);

            // is_claimed predicate can veto a candidate.
            let vetoed = find_block_scope_by_bindings(
                semantic,
                &["inner1", "inner2"],
                program_scope(semantic),
                |sid| sid == inner1_scope,
            );
            assert!(vetoed.is_none(), "claimed scope must be skipped");
        });
    }

    #[test]
    fn enclosing_function_scope_crosses_arrow_and_block() {
        // `x` is declared in `outer`'s function scope; the arrow `() => x`
        // introduces a nested function scope. The arrow's body references `x`,
        // and enclosing_function_scope on that reference's scope must walk up to
        // the arrow's function scope (NOT `outer`'s), while `x`'s binding scope
        // resolves to `outer`'s function scope — establishing the boundary.
        let src = "
            function outer() {
                let x = 1;
                const fn = () => {
                    {
                        return x;
                    }
                };
                return fn;
            }
        ";
        with_semantic(src, |semantic, _program| {
            let scoping = semantic.scoping();

            // The function scope that declares `x`.
            let x_sym = symbol_named(semantic, "x");
            let x_scope = scoping.symbol_scope_id(x_sym);
            let outer_fn = enclosing_function_scope(semantic, x_scope);
            assert_eq!(
                scope_kind(semantic, outer_fn),
                ScopeKind::Function,
                "x's enclosing function scope is outer's function scope"
            );

            // The arrow has its own Function-kind scope (declares `fn`? no — fn is
            // in outer; locate the arrow scope as the Function child of outer that
            // is not outer itself). Use the reference to `x` inside the arrow body.
            let refs: Vec<_> = scoping.get_resolved_references(x_sym).collect();
            assert!(!refs.is_empty(), "expected a reference to x");
            let ref_node = refs[0].node_id();
            let ref_scope = scope_of_node(semantic, ref_node);
            let ref_fn = enclosing_function_scope(semantic, ref_scope);
            assert_eq!(
                scope_kind(semantic, ref_fn),
                ScopeKind::Function,
                "the reference is inside a function scope (the arrow)"
            );
            assert_ne!(
                ref_fn, outer_fn,
                "the arrow's function scope differs from x's binding function scope (crosses a function boundary)"
            );

            // is_descendant_or_self_scope: the arrow's scope is a descendant of
            // outer's function scope.
            assert!(
                is_descendant_or_self_scope(semantic, ref_fn, outer_fn),
                "arrow scope is a descendant of outer's function scope"
            );
            assert!(
                is_descendant_or_self_scope(semantic, outer_fn, outer_fn),
                "a scope is a descendant-or-self of itself"
            );
            assert!(
                !is_descendant_or_self_scope(semantic, outer_fn, ref_fn),
                "outer's function scope is NOT a descendant of the arrow scope"
            );
        });
    }

    /// Helper: the function scope (a child of the symbol's declaring scope) for
    /// a function symbol. Function declarations bind into the *outer* scope; the
    /// function's own scope is a Function-kind child.
    fn function_scope(semantic: &Semantic, func_sym: SymbolId) -> ScopeId {
        let outer = semantic.scoping().symbol_scope_id(func_sym);
        for child in child_scopes(semantic, outer) {
            if scope_kind(semantic, child) == ScopeKind::Function {
                return child;
            }
        }
        outer
    }
}
