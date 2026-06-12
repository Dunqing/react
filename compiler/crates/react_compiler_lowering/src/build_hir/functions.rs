//! Nested function / arrow / object-method lowering — reads `oxc_ast` directly
//! and produces HIR.
//!
//! Stage N1.2.5 transcribes `lower_function*` + the captured-context hoisting
//! analysis from the pre-flip reference, translating `react_compiler_ast` /
//! `ScopeInfo` access to `oxc_ast` / `oxc_semantic`.
//!
//! A nested function captures variables declared in the enclosing function(s):
//! those become the function's `context`, and references / reassignments that
//! cross the function boundary use `LoadContext`/`StoreContext`. The captured
//! set is computed reference-driven (no AST walk) — see [`gather_captured_context`].
//!
//! Constructs that can't be faithfully transcribed yet (destructuring/rest
//! params, generators, bodyless TS overloads, get/set object methods) keep
//! bailing with a graceful `Todo` so the crate stays green.

use indexmap::IndexMap;
use indexmap::IndexSet;
use oxc_ast::ast as oxc;
use oxc_semantic::Semantic;
use oxc_span::GetSpan;
use oxc_syntax::scope::ScopeId;
use oxc_syntax::symbol::SymbolId;
use react_compiler_diagnostics::CompilerError;
use react_compiler_diagnostics::CompilerErrorDetail;
use react_compiler_diagnostics::ErrorCategory;
use react_compiler_hir::*;

use crate::FunctionForm;
use crate::hir_builder::HirBuilder;
use crate::hir_builder::todo_diagnostic;
use crate::semantic_queries as sq;

use super::FunctionBody;
use super::lower_inner;
use super::lower_value_to_temporary;
use super::span_to_location;

// =============================================================================
// Public entry points
// =============================================================================

/// Lower a function/arrow *expression* to a `FunctionExpression` InstructionValue.
pub(crate) fn lower_function_to_value(
    builder: &mut HirBuilder,
    form: &FunctionForm<'_>,
    expr_type: FunctionExpressionType,
) -> Result<InstructionValue, CompilerError> {
    let loc = Some(builder.loc_of_span(form.span()));
    let name = match form {
        FunctionForm::Function(f) => f.id.as_ref().map(|id| id.name.to_string()),
        FunctionForm::Arrow(_) => None,
    };
    let lowered_func = lower_function(builder, form)?;
    Ok(InstructionValue::FunctionExpression {
        name,
        name_hint: None,
        lowered_func,
        expr_type,
        loc,
    })
}

/// Lower a function/arrow node into a `LoweredFunction` (a FunctionId in the env).
pub(crate) fn lower_function(
    builder: &mut HirBuilder,
    form: &FunctionForm<'_>,
) -> Result<LoweredFunction, CompilerError> {
    // The function's own scope. oxc records it on the node; fall back to the
    // program scope for degraded scope info.
    let function_scope = function_scope_of(form).unwrap_or_else(|| sq::program_scope(builder.semantic()));

    // Extract params, body, id, generator/async, and loc from the form.
    let loc = Some(builder.loc_of_span(form.span()));
    match form {
        FunctionForm::Function(f) => {
            let body = match &f.body {
                Some(b) => FunctionBody::Block(b),
                None => {
                    // Bodyless TS overload / declare — cannot lower.
                    return Err(CompilerError::from(todo_diagnostic(
                        "nested function without a body (TS overload/declare)",
                        loc,
                    )));
                }
            };
            let id = f.id.as_ref().map(|id| id.name.as_str());
            lower_function_parts(
                builder,
                &f.params.items,
                body,
                id,
                id,
                f.generator,
                f.r#async,
                loc,
                function_scope,
            )
        }
        FunctionForm::Arrow(a) => {
            // `() => expr` is represented as a FunctionBody with a single
            // ExpressionStatement when `expression == true`.
            let body = if a.expression {
                match a.body.statements.first() {
                    Some(oxc::Statement::ExpressionStatement(es)) => {
                        FunctionBody::Expression(&es.expression)
                    }
                    _ => FunctionBody::Block(&a.body),
                }
            } else {
                FunctionBody::Block(&a.body)
            };
            lower_function_parts(
                builder,
                &a.params.items,
                body,
                None,
                None,
                false,
                a.r#async,
                loc,
                function_scope,
            )
        }
    }
}

/// Lower a function *declaration* statement: emit a `FunctionExpression` and
/// store it into the (hoisted) function-name binding.
pub(crate) fn lower_function_declaration(
    builder: &mut HirBuilder,
    func: &oxc::Function,
) -> Result<(), CompilerError> {
    let loc = Some(builder.loc_of_span(func.span()));
    let func_name = func.id.as_ref().map(|id| id.name.to_string());

    let function_scope =
        func.scope_id.get().unwrap_or_else(|| sq::program_scope(builder.semantic()));

    let body = match &func.body {
        Some(b) => FunctionBody::Block(b),
        None => {
            // Bodyless TS overload / declare — nothing to lower.
            return Err(CompilerError::from(todo_diagnostic(
                "function declaration without a body (TS overload/declare)",
                loc,
            )));
        }
    };
    let id = func.id.as_ref().map(|id| id.name.as_str());

    let lowered_func = lower_function_parts(
        builder,
        &func.params.items,
        body,
        id,
        id,
        func.generator,
        func.r#async,
        loc.clone(),
        function_scope,
    )?;

    // Emit the FunctionExpression value into a temporary.
    let fn_value = InstructionValue::FunctionExpression {
        name: func_name.clone(),
        name_hint: None,
        lowered_func,
        expr_type: FunctionExpressionType::FunctionDeclaration,
        loc: loc.clone(),
    };
    let fn_place = lower_value_to_temporary(builder, fn_value)?;

    // Store into the function-name binding (StoreLocal / StoreContext with
    // InstructionKind::Function), mirroring statements::store_to_identifier.
    if let Some(id_node) = &func.id {
        let name = id_node.name.as_str();
        let ident_loc = Some(builder.loc_of_span(id_node.span));
        let symbol_id = id_node.symbol_id.get();
        let binding = builder.resolve_identifier_symbol(name, symbol_id, ident_loc)?;
        match binding {
            VariableBinding::Identifier { identifier, .. } => {
                // Use the full function declaration loc for the Place (matches
                // the reference, where lowerAssignment uses stmt.node.loc).
                let place = Place {
                    identifier,
                    reactive: false,
                    effect: Effect::Unknown,
                    loc: loc.clone(),
                };
                if builder.is_context_symbol(symbol_id) {
                    lower_value_to_temporary(
                        builder,
                        InstructionValue::StoreContext {
                            lvalue: LValue {
                                kind: InstructionKind::Function,
                                place,
                            },
                            value: fn_place,
                            loc,
                        },
                    )?;
                } else {
                    lower_value_to_temporary(
                        builder,
                        InstructionValue::StoreLocal {
                            lvalue: LValue {
                                kind: InstructionKind::Function,
                                place,
                            },
                            value: fn_place,
                            type_annotation: None,
                            loc,
                        },
                    )?;
                }
            }
            _ => {
                builder.record_error(CompilerErrorDetail {
                    category: ErrorCategory::Invariant,
                    reason: format!(
                        "Could not find binding for function declaration `{}`",
                        name
                    ),
                    description: None,
                    loc,
                    suggestions: None,
                })?;
            }
        }
    }
    Ok(())
}

/// Lower an object method (`{ foo() {} }`) into an `ObjectProperty`.
///
/// oxc represents object methods as `ObjectProperty { method: true, value:
/// Expression::FunctionExpression(func) }`. get/set accessors (`kind != Init`)
/// keep bailing with a `Todo`.
pub(crate) fn lower_object_method(
    builder: &mut HirBuilder,
    p: &oxc::ObjectProperty,
) -> Result<Option<ObjectProperty>, CompilerError> {
    let loc = Some(builder.loc_of_span(p.span));

    if p.kind != oxc::PropertyKind::Init {
        let kind_str = match p.kind {
            oxc::PropertyKind::Get => "get",
            oxc::PropertyKind::Set => "set",
            oxc::PropertyKind::Init => "method",
        };
        builder.record_error(CompilerErrorDetail {
            category: ErrorCategory::Todo,
            reason: format!(
                "(BuildHIR::lowerExpression) Handle {} functions in ObjectExpression",
                kind_str
            ),
            description: None,
            loc,
            suggestions: None,
        })?;
        return Ok(None);
    }

    let key = match super::expressions::lower_object_property_key(builder, &p.key, p.computed)? {
        Some(k) => k,
        None => return Ok(None),
    };

    let form = match &p.value {
        oxc::Expression::FunctionExpression(func) => FunctionForm::Function(func),
        oxc::Expression::ArrowFunctionExpression(arrow) => FunctionForm::Arrow(arrow),
        _ => {
            builder.record_diagnostic(todo_diagnostic(
                "object method with non-function value",
                loc,
            ));
            return Ok(None);
        }
    };

    let lowered_func = lower_function(builder, &form)?;

    let method_value = InstructionValue::ObjectMethod {
        loc: loc.clone(),
        lowered_func,
    };
    let method_place = lower_value_to_temporary(builder, method_value)?;

    Ok(Some(ObjectProperty {
        key,
        property_type: ObjectPropertyType::Method,
        place: method_place,
    }))
}

// =============================================================================
// Shared internals
// =============================================================================

/// Resolve the oxc scope id introduced by a function/arrow node.
fn function_scope_of(form: &FunctionForm<'_>) -> Option<ScopeId> {
    match form {
        FunctionForm::Function(f) => f.scope_id.get(),
        FunctionForm::Arrow(a) => a.scope_id.get(),
    }
}

/// Shared lowering for a nested function given its extracted parts: gather the
/// captured context, recurse via `lower_inner`, merge child bindings / used
/// names back into the parent, and register the resulting `HirFunction`.
#[allow(clippy::too_many_arguments)]
fn lower_function_parts<'a>(
    builder: &mut HirBuilder,
    params: &'a [oxc::FormalParameter<'a>],
    body: FunctionBody<'a>,
    id: Option<&str>,
    ast_id: Option<&str>,
    generator: bool,
    is_async: bool,
    loc: Option<SourceLocation>,
    function_scope: ScopeId,
) -> Result<LoweredFunction, CompilerError> {
    // Read-only data captured before we take `&mut env`.
    let semantic = builder.semantic();
    let source_text = builder.source_text();
    let component_scope = builder.component_scope();

    let parent_bindings = builder.bindings().clone();
    let parent_used_names = builder.used_names().clone();
    let context_ids = builder.context_identifiers().clone();

    // The captured-context bindings (free variables of this nested function).
    let captured = gather_captured_context(semantic, function_scope, component_scope, source_text);

    // Merged context: parent context + this function's captures (captures win).
    let mut merged_context: IndexMap<SymbolId, Option<SourceLocation>> = builder.context().clone();
    for (sym, ctx_loc) in captured {
        merged_context.insert(sym, ctx_loc);
    }

    let env = builder.environment_mut();
    let (hir_func, child_used_names, child_bindings) = lower_inner(
        params,
        body,
        id,
        ast_id,
        generator,
        is_async,
        loc,
        semantic,
        source_text,
        env,
        Some(parent_bindings),
        Some(parent_used_names),
        merged_context,
        function_scope,
        component_scope,
        &context_ids,
        false, // nested function
    )?;

    builder.merge_used_names(child_used_names);
    builder.merge_bindings(child_bindings);

    let func_id = builder.environment_mut().add_function(hir_func);
    Ok(LoweredFunction { func: func_id })
}

/// Gather the captured-context bindings for a nested function: variables
/// declared in the scope chain between the function's parent scope and the
/// component scope (inclusive) that are referenced from inside the function.
///
/// The result is keyed by `SymbolId` and ordered by the earliest (lowest
/// source position) reference, matching the position-ordered traversal the TS
/// compiler gets from Babel.
fn gather_captured_context(
    semantic: &Semantic,
    function_scope: ScopeId,
    component_scope: ScopeId,
    source_text: &str,
) -> IndexMap<SymbolId, Option<SourceLocation>> {
    let scoping = semantic.scoping();

    // The "pure" scopes are the function's parent scope up to and including the
    // component scope. A captured binding must be declared in one of these.
    let pure_scopes: IndexSet<ScopeId> = match sq::scope_parent(semantic, function_scope) {
        Some(parent) => capture_scopes(semantic, parent, component_scope),
        None => IndexSet::new(),
    };

    // (min source position, loc) per captured binding.
    let mut captured: std::collections::HashMap<SymbolId, (u32, Option<SourceLocation>)> =
        std::collections::HashMap::new();

    for sym in scoping.symbol_ids() {
        let decl_scope = scoping.symbol_scope_id(sym);
        if !pure_scopes.contains(&decl_scope) {
            continue;
        }

        // Skip type-only bindings (their declaration node is a TS type decl).
        use oxc_ast::AstKind;
        let decl_node = semantic.symbol_declaration(sym);
        if matches!(
            decl_node.kind(),
            AstKind::TSTypeAliasDeclaration(_)
                | AstKind::TSInterfaceDeclaration(_)
                | AstKind::TSEnumDeclaration(_)
                | AstKind::TSModuleDeclaration(_)
        ) {
            continue;
        }

        let decl_start = sq::declaration_span(semantic, sym).start;

        for reference in scoping.get_resolved_references(sym) {
            let ref_scope = sq::scope_of_node(semantic, reference.node_id());
            // The reference must be inside the nested function's scope subtree.
            if !sq::is_descendant_or_self_scope(semantic, ref_scope, function_scope) {
                continue;
            }
            let ref_span = semantic.nodes().get_node(reference.node_id()).kind().span();
            let ref_start = ref_span.start;
            // Skip a "reference" that is actually the binding's declaration site.
            if ref_start == decl_start {
                continue;
            }
            let loc = Some(span_to_location(source_text, ref_span));
            captured
                .entry(sym)
                .and_modify(|(min_pos, existing_loc)| {
                    if ref_start < *min_pos {
                        *min_pos = ref_start;
                        *existing_loc = loc.clone();
                    }
                })
                .or_insert((ref_start, loc));
        }
    }

    // Sort by earliest reference position so context declarations appear in
    // source order.
    let mut sorted: Vec<_> = captured.into_iter().collect();
    sorted.sort_by_key(|(_, (pos, _))| *pos);

    sorted
        .into_iter()
        .map(|(sym, (_, loc))| (sym, loc))
        .collect()
}

/// Walk the scope parent chain from `from` up to and including `to`.
fn capture_scopes(semantic: &Semantic, from: ScopeId, to: ScopeId) -> IndexSet<ScopeId> {
    let mut result = IndexSet::new();
    let mut current = Some(from);
    while let Some(sid) = current {
        result.insert(sid);
        if sid == to {
            break;
        }
        current = sq::scope_parent(semantic, sid);
    }
    result
}
