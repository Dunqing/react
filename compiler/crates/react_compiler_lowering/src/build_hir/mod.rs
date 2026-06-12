//! BuildHIR — lowers an oxc function node directly into HIR.
//!
//! Stage N1.2.1 flips the input of lowering from `react_compiler_ast` +
//! `ScopeInfo` to `oxc_ast` + `oxc_semantic`. This module owns the function
//! *shell* (params, body block walk, return) and a per-construct dispatch.
//!
//! Only the shell and a small set of trivial constructs lower for REAL; every
//! other construct records a graceful `Todo` (via [`crate::hir_builder::todo_diagnostic`])
//! so the crate stays green and trivial fixtures still produce HIR. The full
//! per-construct transcription (statements / expressions / jsx / patterns) lands
//! in later N1.2.x / N1.3 stages.

use std::collections::HashSet;

use indexmap::IndexMap;
use oxc_ast::ast as oxc;
use oxc_semantic::Semantic;
use oxc_span::GetSpan;
use oxc_span::Span;
use oxc_syntax::scope::ScopeId;
use oxc_syntax::symbol::SymbolId;
use react_compiler_diagnostics::CompilerError;
use react_compiler_hir::environment::Environment;
use react_compiler_hir::*;

use crate::FunctionForm;
use crate::find_context_identifiers::find_context_identifiers;
use crate::hir_builder::HirBuilder;
use crate::hir_builder::is_always_reserved_word;
use crate::hir_builder::reserved_identifier_diagnostic;
use crate::hir_builder::todo_diagnostic;
use crate::semantic_queries as sq;

mod expressions;
mod functions;
mod hoisting;
mod jsx;
mod patterns;
mod statements;

#[allow(unused_imports)]
pub(crate) use expressions::lower_expression;
pub(crate) use expressions::lower_expression_to_temporary;
#[allow(unused_imports)]
pub(crate) use functions::{
    lower_function, lower_function_declaration, lower_function_to_value, lower_object_method,
};
#[allow(unused_imports)]
pub(crate) use patterns::{
    AssignmentStyle, lower_assignment, lower_assignment_target, lower_identifier_for_assignment,
};
// The per-construct lowering (statements / jsx / patterns) is transcribed
// incrementally in later N1.2.x / N1.3 stages. Expression lowering (N1.2.3)
// lives in `expressions.rs`. The dispatch in this module handles the function
// shell + statement constructs and bails (graceful Todo) on the rest.

// =============================================================================
// Source location conversion (oxc Span -> HIR SourceLocation)
// =============================================================================

/// Convert a byte offset within `source` to a 1-based line / 0-based column.
fn position_of_offset(source: &str, offset: u32) -> Position {
    let off = offset as usize;
    let mut line: u32 = 1;
    let mut line_start: usize = 0;
    for (i, b) in source.as_bytes().iter().enumerate() {
        if i >= off {
            break;
        }
        if *b == b'\n' {
            line += 1;
            line_start = i + 1;
        }
    }
    Position {
        line,
        column: (off.saturating_sub(line_start)) as u32,
        index: Some(offset),
    }
}

/// Convert an oxc [`Span`] to an HIR [`SourceLocation`] using the source text.
pub(crate) fn span_to_location(source: &str, span: Span) -> SourceLocation {
    SourceLocation {
        start: position_of_offset(source, span.start),
        end: position_of_offset(source, span.end),
    }
}

// =============================================================================
// Helper functions
// =============================================================================

pub(crate) fn build_temporary_place(
    builder: &mut HirBuilder,
    loc: Option<SourceLocation>,
) -> Place {
    let id = builder.make_temporary(loc.clone());
    Place {
        identifier: id,
        reactive: false,
        effect: Effect::Unknown,
        loc,
    }
}

/// Promote a temporary identifier to a named identifier (for destructuring /
/// catch bindings). Corresponds to TS `promoteTemporary(identifier)`.
pub(crate) fn promote_temporary(builder: &mut HirBuilder, identifier_id: IdentifierId) {
    let env = builder.environment_mut();
    let decl_id = env.identifiers[identifier_id.0 as usize].declaration_id;
    env.identifiers[identifier_id.0 as usize].name =
        Some(IdentifierName::Promoted(format!("#t{}", decl_id.0)));
}

pub(crate) fn lower_value_to_temporary(
    builder: &mut HirBuilder,
    value: InstructionValue,
) -> Result<Place, CompilerError> {
    if let InstructionValue::LoadLocal { ref place, .. } = value {
        let ident = &builder.environment().identifiers[place.identifier.0 as usize];
        if ident.name.is_none() {
            return Ok(place.clone());
        }
    }
    let loc = value.loc().cloned();
    let place = build_temporary_place(builder, loc.clone());
    builder.push(Instruction {
        id: EvaluationOrder(0),
        lvalue: place.clone(),
        value,
        loc,
        effects: None,
    });
    Ok(place)
}

// =============================================================================
// Function body abstraction
// =============================================================================

/// The body of a function, as an oxc node.
pub(crate) enum FunctionBody<'a> {
    /// A `{ ... }` block body (function declarations, expressions, block arrows).
    Block(&'a oxc::FunctionBody<'a>),
    /// An expression-bodied arrow `() => expr`.
    Expression(&'a oxc::Expression<'a>),
}

// =============================================================================
// Entry point: lower a function AST node into HIR
// =============================================================================

/// Lower an oxc function node into an owned [`HirFunction`].
///
/// - `func`: the discovered function (declaration/expression/arrow), in oxc form.
/// - `id`: the inferred name (may come from a `const Foo = () => {}` declarator).
/// - `semantic`: the oxc semantic model — direct source of scope/binding info.
/// - `source_text`: the full source (for span -> location conversion).
/// - `env`: the shared compilation environment.
pub fn lower(
    func: &FunctionForm<'_>,
    id: Option<&str>,
    semantic: &Semantic,
    source_text: &str,
    env: &mut Environment,
) -> Result<HirFunction, CompilerError> {
    // Resolve the function's own scope (oxc records it directly on the node).
    let function_scope = function_scope_of(func).unwrap_or_else(|| sq::program_scope(semantic));

    // Pre-compute context identifiers (captured across function boundaries).
    // For top-level functions this is empty; the full analysis is deferred to N1.3.
    let context_identifiers: HashSet<SymbolId> =
        find_context_identifiers(func, semantic, function_scope);

    let context_map: IndexMap<SymbolId, Option<SourceLocation>> = IndexMap::new();

    let (params, rest_param, body): (
        &[oxc::FormalParameter],
        Option<&oxc::FormalParameterRest>,
        FunctionBody,
    ) = match func {
        FunctionForm::Function(f) => {
            let body = match &f.body {
                Some(b) => FunctionBody::Block(b),
                // A bodyless function (TS overload/declare) — nothing to lower.
                None => {
                    return Err(CompilerError::from(todo_diagnostic(
                        "function without a body (TS overload/declare)",
                        Some(span_to_location(source_text, func.span())),
                    )));
                }
            };
            (&f.params.items, f.params.rest.as_deref(), body)
        }
        FunctionForm::Arrow(a) => {
            // oxc represents `() => expr` as a FunctionBody with a single
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
            (&a.params.items, a.params.rest.as_deref(), body)
        }
    };

    let loc = Some(span_to_location(source_text, func.span()));
    let ast_id = func.ast_id_name();
    let id = id.or(ast_id);

    let (f, _, _) = lower_inner(
        params,
        rest_param,
        body,
        id,
        ast_id,
        func.is_generator(),
        func.is_async(),
        loc,
        semantic,
        source_text,
        env,
        None,
        None,
        context_map,
        function_scope,
        function_scope, // component_scope == function_scope for top-level
        &context_identifiers,
        true,
    )?;
    Ok(f)
}

/// Resolve the oxc scope id introduced by a function node.
fn function_scope_of(func: &FunctionForm<'_>) -> Option<ScopeId> {
    match func {
        FunctionForm::Function(f) => f.scope_id.get(),
        FunctionForm::Arrow(a) => a.scope_id.get(),
    }
}

// =============================================================================
// lower_inner — builds the function shell and walks the body
// =============================================================================

#[allow(clippy::too_many_arguments)]
pub(crate) fn lower_inner(
    params: &[oxc::FormalParameter],
    rest_param: Option<&oxc::FormalParameterRest>,
    body: FunctionBody<'_>,
    id: Option<&str>,
    ast_id: Option<&str>,
    generator: bool,
    is_async: bool,
    loc: Option<SourceLocation>,
    semantic: &Semantic,
    source_text: &str,
    env: &mut Environment,
    parent_bindings: Option<IndexMap<SymbolId, IdentifierId>>,
    parent_used_names: Option<IndexMap<String, SymbolId>>,
    context_map: IndexMap<SymbolId, Option<SourceLocation>>,
    function_scope: ScopeId,
    component_scope: ScopeId,
    context_identifiers: &HashSet<SymbolId>,
    is_top_level: bool,
) -> Result<
    (
        HirFunction,
        IndexMap<String, SymbolId>,
        IndexMap<SymbolId, IdentifierId>,
    ),
    CompilerError,
> {
    let _ = ast_id; // reserved for HIR id parity; arrows have none

    let mut builder = HirBuilder::new(
        env,
        semantic,
        source_text,
        function_scope,
        component_scope,
        context_identifiers.clone(),
        parent_bindings,
        Some(context_map.clone()),
        None,
        parent_used_names,
    );

    // Context places from the captured refs.
    let mut context: Vec<Place> = Vec::new();
    for (&symbol_id, ctx_loc) in &context_map {
        let name = semantic.scoping().symbol_name(symbol_id).to_string();
        let identifier = builder.resolve_binding(&name, symbol_id)?;
        context.push(Place {
            identifier,
            effect: Effect::Unknown,
            reactive: false,
            loc: ctx_loc.clone(),
        });
    }

    // Lower parameters. Identifier and destructuring params (incl. defaults)
    // lower for real; the rest param is threaded separately below.
    let mut hir_params: Vec<ParamPattern> = Vec::new();
    for param in params {
        lower_param(&mut builder, param, &mut hir_params)?;
    }
    if let Some(rest) = rest_param {
        lower_rest_param(&mut builder, rest, &mut hir_params)?;
    }

    // Lower the body.
    let mut directives: Vec<String> = Vec::new();
    match body {
        FunctionBody::Expression(expr) => {
            let fallthrough = builder.reserve(BlockKind::Block);
            let value = lower_expression_to_temporary(&mut builder, expr)?;
            builder.terminate_with_continuation(
                Terminal::Return {
                    value,
                    return_variant: ReturnVariant::Implicit,
                    id: EvaluationOrder(0),
                    loc: None,
                    effects: None,
                },
                fallthrough,
            );
        }
        FunctionBody::Block(block) => {
            directives = block
                .directives
                .iter()
                .map(|d| d.directive.to_string())
                .collect();
            // A function body shares the function scope (Babel-shaped view), so
            // hoist declarations referenced before their lexical position.
            let fn_scope = builder.function_scope();
            statements::lower_block_statements(&mut builder, Some(fn_scope), &block.statements)?;
        }
    }

    // Emit final Return(Void, undefined).
    let undefined_value = InstructionValue::Primitive {
        value: PrimitiveValue::Undefined,
        loc: None,
    };
    let return_value = lower_value_to_temporary(&mut builder, undefined_value)?;
    builder.terminate(
        Terminal::Return {
            value: return_value,
            return_variant: ReturnVariant::Void,
            id: EvaluationOrder(0),
            loc: None,
            effects: None,
        },
        None,
    );

    let (hir_body, instructions, used_names, child_bindings) = builder.build()?;

    let returns = crate::hir_builder::create_temporary_place(env, loc.clone());

    Ok((
        HirFunction {
            loc,
            id: id.map(|s| s.to_string()),
            name_hint: None,
            fn_type: if is_top_level {
                env.fn_type
            } else {
                ReactFunctionType::Other
            },
            params: hir_params,
            return_type_annotation: None,
            returns,
            context,
            body: hir_body,
            instructions,
            generator,
            is_async,
            directives,
            aliasing_effects: None,
        },
        used_names,
        child_bindings,
    ))
}

// =============================================================================
// Parameter lowering (shell: identifier params real, rest bail)
// =============================================================================

fn lower_param(
    builder: &mut HirBuilder,
    param: &oxc::FormalParameter,
    hir_params: &mut Vec<ParamPattern>,
) -> Result<(), CompilerError> {
    let pattern = &param.pattern;

    // Defaulted params (`function f(a = 1)`) carry the default in `initializer`,
    // separate from the binding pattern. Lower as a promoted temporary param,
    // resolve `value === undefined ? default : value`, then assign into the
    // pattern — mirroring how the reference treats an AssignmentPattern param.
    if let Some(initializer) = &param.initializer {
        let param_loc = Some(builder.loc_of_span(pattern.span()));
        let place = build_temporary_place(builder, param_loc.clone());
        promote_temporary(builder, place.identifier);
        hir_params.push(ParamPattern::Place(place.clone()));
        let resolved = patterns::lower_default(builder, param_loc.clone(), initializer, place)?;
        patterns::lower_assignment(
            builder,
            param_loc,
            InstructionKind::Let,
            pattern,
            resolved,
            patterns::AssignmentStyle::Assignment,
        )?;
        return Ok(());
    }

    match pattern {
        oxc::BindingPattern::BindingIdentifier(ident) => {
            if is_always_reserved_word(&ident.name) {
                return Err(CompilerError::from(reserved_identifier_diagnostic(
                    &ident.name,
                )));
            }
            let param_loc = Some(builder.loc_of_span(ident.span));
            let symbol_id = ident.symbol_id.get();
            let binding =
                builder.resolve_identifier_symbol(&ident.name, symbol_id, param_loc.clone())?;
            match binding {
                VariableBinding::Identifier { identifier, .. } => {
                    builder.set_identifier_declaration_loc(identifier, &param_loc);
                    hir_params.push(ParamPattern::Place(Place {
                        identifier,
                        effect: Effect::Unknown,
                        reactive: false,
                        loc: param_loc,
                    }));
                }
                _ => {
                    // Param did not resolve to a local binding (degraded scope
                    // info). Bail gracefully rather than emitting a broken param.
                    builder.record_diagnostic(todo_diagnostic(
                        &format!("parameter `{}` without local binding", ident.name),
                        param_loc,
                    ));
                }
            }
        }
        // Destructuring params (`function f({a}, [b]) {}`): create a promoted
        // temporary param and destructure it into the pattern.
        oxc::BindingPattern::ObjectPattern(_) | oxc::BindingPattern::ArrayPattern(_) => {
            let param_loc = Some(builder.loc_of_span(pattern.span()));
            let place = build_temporary_place(builder, param_loc.clone());
            promote_temporary(builder, place.identifier);
            hir_params.push(ParamPattern::Place(place.clone()));
            patterns::lower_assignment(
                builder,
                param_loc,
                InstructionKind::Let,
                pattern,
                place,
                patterns::AssignmentStyle::Assignment,
            )?;
        }
        oxc::BindingPattern::AssignmentPattern(_) => {
            // An AssignmentPattern at the top of a FormalParameter is unusual
            // (defaults come via `initializer`); lower it via lower_assignment.
            let param_loc = Some(builder.loc_of_span(pattern.span()));
            let place = build_temporary_place(builder, param_loc.clone());
            promote_temporary(builder, place.identifier);
            hir_params.push(ParamPattern::Place(place.clone()));
            patterns::lower_assignment(
                builder,
                param_loc,
                InstructionKind::Let,
                pattern,
                place,
                patterns::AssignmentStyle::Assignment,
            )?;
        }
    }
    Ok(())
}

/// Lower a rest parameter (`function f(...rest) {}`). The rest binding is a
/// spread param; its argument pattern is then assigned from a temporary.
fn lower_rest_param(
    builder: &mut HirBuilder,
    rest: &oxc::FormalParameterRest,
    hir_params: &mut Vec<ParamPattern>,
) -> Result<(), CompilerError> {
    let rest_loc = Some(builder.loc_of_span(rest.span));
    let place = build_temporary_place(builder, rest_loc.clone());
    hir_params.push(ParamPattern::Spread(SpreadPattern {
        place: place.clone(),
    }));
    patterns::lower_assignment(
        builder,
        rest_loc,
        InstructionKind::Let,
        &rest.rest.argument,
        place,
        patterns::AssignmentStyle::Assignment,
    )?;
    Ok(())
}
