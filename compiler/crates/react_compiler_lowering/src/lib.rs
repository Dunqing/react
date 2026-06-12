pub mod build_hir;
pub mod find_context_identifiers;
pub mod hir_builder;
pub mod semantic_queries;

use react_compiler_hir::BindingKind;

use crate::semantic_queries::BindingKind as SemBindingKind;

/// Convert a semantic-query binding kind to an HIR binding kind.
pub fn convert_binding_kind(kind: &SemBindingKind) -> BindingKind {
    match kind {
        SemBindingKind::Var => BindingKind::Var,
        SemBindingKind::Let => BindingKind::Let,
        SemBindingKind::Const => BindingKind::Const,
        SemBindingKind::Param => BindingKind::Param,
        SemBindingKind::Module => BindingKind::Module,
        SemBindingKind::Hoisted => BindingKind::Hoisted,
        SemBindingKind::Local => BindingKind::Local,
        SemBindingKind::Unknown => BindingKind::Unknown,
    }
}

/// A function to lower, in oxc form. Analogous to TS's `NodePath<t.Function>`.
///
/// The discovery phase (in the entrypoint) resolves a top-level function and
/// hands one of these to [`lower`]. Both variants carry the oxc allocator
/// lifetime `'a` so the lowering can borrow the AST directly (no bridge).
pub enum FunctionForm<'a> {
    /// `function Foo() {}` / `function () {}` (declaration or expression share
    /// the same oxc `Function` node type).
    Function(&'a oxc_ast::ast::Function<'a>),
    /// `() => {}` / `() => expr`.
    Arrow(&'a oxc_ast::ast::ArrowFunctionExpression<'a>),
}

impl<'a> FunctionForm<'a> {
    /// The byte span of the function node.
    pub fn span(&self) -> oxc_span::Span {
        use oxc_span::GetSpan;
        match self {
            FunctionForm::Function(f) => f.span(),
            FunctionForm::Arrow(a) => a.span(),
        }
    }

    /// Whether the function is a generator (`function*`). Arrows are never generators.
    pub fn is_generator(&self) -> bool {
        match self {
            FunctionForm::Function(f) => f.generator,
            FunctionForm::Arrow(_) => false,
        }
    }

    /// Whether the function is `async`.
    pub fn is_async(&self) -> bool {
        match self {
            FunctionForm::Function(f) => f.r#async,
            FunctionForm::Arrow(a) => a.r#async,
        }
    }

    /// The function's own AST id (for `function Foo`/named function expressions).
    /// Arrows never have one.
    pub fn ast_id_name(&self) -> Option<&'a str> {
        match self {
            FunctionForm::Function(f) => f.id.as_ref().map(|id| id.name.as_str()),
            FunctionForm::Arrow(_) => None,
        }
    }
}

// The main lower() function - delegates to build_hir
pub use build_hir::lower;
// Re-export post-build helper functions used by optimization passes
pub use hir_builder::{
    create_temporary_place, get_reverse_postordered_blocks, mark_instruction_ids,
    mark_predecessors, remove_dead_do_while_statements, remove_unnecessary_try_catch,
    remove_unreachable_for_updates,
};
pub use react_compiler_hir::visitors::each_terminal_successor;
pub use react_compiler_hir::visitors::terminal_fallthrough;
