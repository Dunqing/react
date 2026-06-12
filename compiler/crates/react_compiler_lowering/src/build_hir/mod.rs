
use indexmap::IndexMap;
use react_compiler_ast::scope::BindingKind as AstBindingKind;
use react_compiler_ast::scope::ScopeId;
use react_compiler_ast::scope::ScopeInfo;
use react_compiler_ast::scope::ScopeKind;
use react_compiler_diagnostics::CompilerError;
use react_compiler_hir::environment::Environment;
use react_compiler_hir::*;

use crate::FunctionNode;
use crate::find_context_identifiers::find_context_identifiers;
use crate::hir_builder::HirBuilder;
use crate::hir_builder::reserved_identifier_diagnostic;
use crate::identifier_loc_index::build_identifier_loc_index;

mod expressions;
mod functions;
mod hoisting;
mod jsx;
mod patterns;
mod statements;

pub(crate) use expressions::*;
pub(crate) use functions::*;
pub(crate) use hoisting::*;
pub(crate) use jsx::*;
pub(crate) use patterns::*;
pub(crate) use statements::*;

// =============================================================================
// Source location conversion
// =============================================================================

/// Convert an AST SourceLocation to an HIR SourceLocation.
pub(crate) fn convert_loc(loc: &react_compiler_ast::common::SourceLocation) -> SourceLocation {
    SourceLocation {
        start: Position {
            line: loc.start.line,
            column: loc.start.column,
            index: loc.start.index,
        },
        end: Position {
            line: loc.end.line,
            column: loc.end.column,
            index: loc.end.index,
        },
    }
}

/// Convert an optional AST SourceLocation to an optional HIR SourceLocation.
pub(crate) fn convert_opt_loc(
    loc: &Option<react_compiler_ast::common::SourceLocation>,
) -> Option<SourceLocation> {
    loc.as_ref().map(convert_loc)
}

/// Serialize an expression to a serde_json::Value for UnsupportedNode's original_node.
/// Returns None if serialization fails (should not happen for valid AST nodes).
/// This should ONLY be called on error/bail paths — never eagerly before deciding
/// to create an UnsupportedNode.
pub(crate) fn serialize_expression(
    expr: &react_compiler_ast::expressions::Expression,
) -> Option<serde_json::Value> {
    serde_json::to_value(expr).ok()
}

/// Serialize a statement to a serde_json::Value for UnsupportedNode's original_node.
pub(crate) fn serialize_statement(
    stmt: &react_compiler_ast::statements::Statement,
) -> Option<serde_json::Value> {
    serde_json::to_value(stmt).ok()
}

/// Serialize a pattern to a serde_json::Value for UnsupportedNode's original_node.
pub(crate) fn serialize_pattern(pat: &react_compiler_ast::patterns::PatternLike) -> Option<serde_json::Value> {
    serde_json::to_value(pat).ok()
}

pub(crate) fn pattern_like_loc(
    pattern: &react_compiler_ast::patterns::PatternLike,
) -> Option<react_compiler_ast::common::SourceLocation> {
    use react_compiler_ast::patterns::PatternLike;
    match pattern {
        PatternLike::Identifier(id) => id.base.loc.clone(),
        PatternLike::ObjectPattern(p) => p.base.loc.clone(),
        PatternLike::ArrayPattern(p) => p.base.loc.clone(),
        PatternLike::AssignmentPattern(p) => p.base.loc.clone(),
        PatternLike::RestElement(p) => p.base.loc.clone(),
        PatternLike::MemberExpression(p) => p.base.loc.clone(),
        PatternLike::TSAsExpression(p) => p.base.loc.clone(),
        PatternLike::TSSatisfiesExpression(p) => p.base.loc.clone(),
        PatternLike::TSNonNullExpression(p) => p.base.loc.clone(),
        PatternLike::TSTypeAssertion(p) => p.base.loc.clone(),
        PatternLike::TypeCastExpression(p) => p.base.loc.clone(),
    }
}

/// Extract the HIR SourceLocation from an Expression AST node.
pub(crate) fn expression_loc(expr: &react_compiler_ast::expressions::Expression) -> Option<SourceLocation> {
    use react_compiler_ast::expressions::Expression;
    let loc = match expr {
        Expression::Identifier(e) => e.base.loc.clone(),
        Expression::StringLiteral(e) => e.base.loc.clone(),
        Expression::NumericLiteral(e) => e.base.loc.clone(),
        Expression::BooleanLiteral(e) => e.base.loc.clone(),
        Expression::NullLiteral(e) => e.base.loc.clone(),
        Expression::BigIntLiteral(e) => e.base.loc.clone(),
        Expression::RegExpLiteral(e) => e.base.loc.clone(),
        Expression::CallExpression(e) => e.base.loc.clone(),
        Expression::MemberExpression(e) => e.base.loc.clone(),
        Expression::OptionalCallExpression(e) => e.base.loc.clone(),
        Expression::OptionalMemberExpression(e) => e.base.loc.clone(),
        Expression::BinaryExpression(e) => e.base.loc.clone(),
        Expression::LogicalExpression(e) => e.base.loc.clone(),
        Expression::UnaryExpression(e) => e.base.loc.clone(),
        Expression::UpdateExpression(e) => e.base.loc.clone(),
        Expression::ConditionalExpression(e) => e.base.loc.clone(),
        Expression::AssignmentExpression(e) => e.base.loc.clone(),
        Expression::SequenceExpression(e) => e.base.loc.clone(),
        Expression::ArrowFunctionExpression(e) => e.base.loc.clone(),
        Expression::FunctionExpression(e) => e.base.loc.clone(),
        Expression::ObjectExpression(e) => e.base.loc.clone(),
        Expression::ArrayExpression(e) => e.base.loc.clone(),
        Expression::NewExpression(e) => e.base.loc.clone(),
        Expression::TemplateLiteral(e) => e.base.loc.clone(),
        Expression::TaggedTemplateExpression(e) => e.base.loc.clone(),
        Expression::AwaitExpression(e) => e.base.loc.clone(),
        Expression::YieldExpression(e) => e.base.loc.clone(),
        Expression::SpreadElement(e) => e.base.loc.clone(),
        Expression::MetaProperty(e) => e.base.loc.clone(),
        Expression::ClassExpression(e) => e.base.loc.clone(),
        Expression::PrivateName(e) => e.base.loc.clone(),
        Expression::Super(e) => e.base.loc.clone(),
        Expression::Import(e) => e.base.loc.clone(),
        Expression::ThisExpression(e) => e.base.loc.clone(),
        Expression::ParenthesizedExpression(e) => e.base.loc.clone(),
        Expression::JSXElement(e) => e.base.loc.clone(),
        Expression::JSXFragment(e) => e.base.loc.clone(),
        Expression::AssignmentPattern(e) => e.base.loc.clone(),
        Expression::TSAsExpression(e) => e.base.loc.clone(),
        Expression::TSSatisfiesExpression(e) => e.base.loc.clone(),
        Expression::TSNonNullExpression(e) => e.base.loc.clone(),
        Expression::TSTypeAssertion(e) => e.base.loc.clone(),
        Expression::TSInstantiationExpression(e) => e.base.loc.clone(),
        Expression::TypeCastExpression(e) => e.base.loc.clone(),
    };
    convert_opt_loc(&loc)
}

pub(crate) fn validate_ts_this_parameter(
    scope_info: &ScopeInfo,
    function_scope: ScopeId,
) -> Result<(), CompilerError> {
    let Some(scope) = scope_info.scopes.get(function_scope.0 as usize) else {
        return Ok(());
    };
    let Some(binding_id) = scope.bindings.get("this") else {
        return Ok(());
    };
    let Some(binding) = scope_info.bindings.get(binding_id.0 as usize) else {
        return Ok(());
    };
    if matches!(binding.kind, AstBindingKind::Param) {
        return Err(CompilerError::from(reserved_identifier_diagnostic("this")));
    }
    Ok(())
}

pub(crate) fn is_class_scope_descendant(scope_info: &ScopeInfo, mut scope_id: ScopeId) -> bool {
    while let Some(scope) = scope_info.scopes.get(scope_id.0 as usize) {
        let Some(parent) = scope.parent else {
            return false;
        };
        let Some(parent_scope) = scope_info.scopes.get(parent.0 as usize) else {
            return false;
        };
        if matches!(parent_scope.kind, ScopeKind::Class) {
            return true;
        }
        scope_id = parent;
    }
    false
}

pub(crate) fn validate_ts_this_parameters_in_function_range(
    scope_info: &ScopeInfo,
    start: u32,
    end: u32,
) -> Result<(), CompilerError> {
    if start >= end {
        return Ok(());
    }
    for (node_start, scope_id) in &scope_info.node_to_scope {
        if *node_start < start || *node_start >= end {
            continue;
        }
        let Some(scope) = scope_info.scopes.get(scope_id.0 as usize) else {
            continue;
        };
        if !matches!(scope.kind, ScopeKind::Function)
            || is_class_scope_descendant(scope_info, *scope_id)
        {
            continue;
        }
        validate_ts_this_parameter(scope_info, *scope_id)?;
    }
    Ok(())
}

/// Get the Babel-style type name of an Expression node (e.g. "Identifier", "NumericLiteral").
pub(crate) fn expression_type_name(expr: &react_compiler_ast::expressions::Expression) -> &'static str {
    use react_compiler_ast::expressions::Expression;
    match expr {
        Expression::Identifier(_) => "Identifier",
        Expression::StringLiteral(_) => "StringLiteral",
        Expression::NumericLiteral(_) => "NumericLiteral",
        Expression::BooleanLiteral(_) => "BooleanLiteral",
        Expression::NullLiteral(_) => "NullLiteral",
        Expression::BigIntLiteral(_) => "BigIntLiteral",
        Expression::RegExpLiteral(_) => "RegExpLiteral",
        Expression::CallExpression(_) => "CallExpression",
        Expression::MemberExpression(_) => "MemberExpression",
        Expression::OptionalCallExpression(_) => "OptionalCallExpression",
        Expression::OptionalMemberExpression(_) => "OptionalMemberExpression",
        Expression::BinaryExpression(_) => "BinaryExpression",
        Expression::LogicalExpression(_) => "LogicalExpression",
        Expression::UnaryExpression(_) => "UnaryExpression",
        Expression::UpdateExpression(_) => "UpdateExpression",
        Expression::ConditionalExpression(_) => "ConditionalExpression",
        Expression::AssignmentExpression(_) => "AssignmentExpression",
        Expression::SequenceExpression(_) => "SequenceExpression",
        Expression::ArrowFunctionExpression(_) => "ArrowFunctionExpression",
        Expression::FunctionExpression(_) => "FunctionExpression",
        Expression::ObjectExpression(_) => "ObjectExpression",
        Expression::ArrayExpression(_) => "ArrayExpression",
        Expression::NewExpression(_) => "NewExpression",
        Expression::TemplateLiteral(_) => "TemplateLiteral",
        Expression::TaggedTemplateExpression(_) => "TaggedTemplateExpression",
        Expression::AwaitExpression(_) => "AwaitExpression",
        Expression::YieldExpression(_) => "YieldExpression",
        Expression::SpreadElement(_) => "SpreadElement",
        Expression::MetaProperty(_) => "MetaProperty",
        Expression::ClassExpression(_) => "ClassExpression",
        Expression::PrivateName(_) => "PrivateName",
        Expression::Super(_) => "Super",
        Expression::Import(_) => "Import",
        Expression::ThisExpression(_) => "ThisExpression",
        Expression::ParenthesizedExpression(_) => "ParenthesizedExpression",
        Expression::JSXElement(_) => "JSXElement",
        Expression::JSXFragment(_) => "JSXFragment",
        Expression::AssignmentPattern(_) => "AssignmentPattern",
        Expression::TSAsExpression(_) => "TSAsExpression",
        Expression::TSSatisfiesExpression(_) => "TSSatisfiesExpression",
        Expression::TSNonNullExpression(_) => "TSNonNullExpression",
        Expression::TSTypeAssertion(_) => "TSTypeAssertion",
        Expression::TSInstantiationExpression(_) => "TSInstantiationExpression",
        Expression::TypeCastExpression(_) => "TypeCastExpression",
    }
}

/// Extract the type annotation name from an identifier's typeAnnotation field.
/// The Babel AST stores type annotations as:
/// { "type": "TSTypeAnnotation", "typeAnnotation": { "type": "TSTypeReference", ... } }
/// or { "type": "TypeAnnotation", "typeAnnotation": { "type": "GenericTypeAnnotation", ... } }
/// We extract the inner typeAnnotation's `type` field name.
pub(crate) fn extract_type_annotation_name(
    type_annotation: &Option<react_compiler_ast::common::RawNode>,
) -> Option<String> {
    let val = type_annotation.as_ref()?.parse_value();
    // Navigate: typeAnnotation.typeAnnotation.type
    let inner = val.get("typeAnnotation")?;
    let type_name = inner.get("type")?.as_str()?;
    Some(type_name.to_string())
}

// =============================================================================
// Helper functions
// =============================================================================

pub(crate) fn build_temporary_place(builder: &mut HirBuilder, loc: Option<SourceLocation>) -> Place {
    let id = builder.make_temporary(loc.clone());
    Place {
        identifier: id,
        reactive: false,
        effect: Effect::Unknown,
        loc,
    }
}

/// Promote a temporary identifier to a named identifier (for destructuring).
/// Corresponds to TS `promoteTemporary(identifier)`.
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
    // Optimization: if loading an unnamed temporary, skip creating a new instruction
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

pub(crate) fn lower_expression_to_temporary(
    builder: &mut HirBuilder,
    expr: &react_compiler_ast::expressions::Expression,
) -> Result<Place, CompilerError> {
    let value = lower_expression(builder, expr)?;
    Ok(lower_value_to_temporary(builder, value)?)
}

// =============================================================================
// Operator conversion
// =============================================================================

pub(crate) enum FunctionBody<'a> {
    Block(&'a react_compiler_ast::statements::BlockStatement),
    Expression(&'a react_compiler_ast::expressions::Expression),
}

/// Main entry point: lower a function AST node into HIR.
///
/// Receives a `FunctionNode` (discovered by the entrypoint) and lowers it to HIR.
/// The `id` parameter provides the function name (which may come from the variable
/// declarator rather than the function node itself, e.g. `const Foo = () => {}`).
pub fn lower(
    func: &FunctionNode<'_>,
    _id: Option<&str>,
    scope_info: &ScopeInfo,
    env: &mut Environment,
) -> Result<HirFunction, CompilerError> {
    // Extract params, body, generator, is_async, loc, scope_id, and the AST function's own id
    // Note: `id` param may include inferred names (e.g., from `const Foo = () => {}`),
    // but the HIR function's `id` field should only include the function's own AST id
    // (FunctionDeclaration.id or FunctionExpression.id, NOT arrow functions).
    let (params, body, generator, is_async, loc, start, end, ast_id) = match func {
        FunctionNode::FunctionDeclaration(decl) => (
            &decl.params[..],
            FunctionBody::Block(&decl.body),
            decl.generator,
            decl.is_async,
            convert_opt_loc(&decl.base.loc),
            decl.base.start.unwrap_or(0),
            decl.base.end.unwrap_or(0),
            decl.id.as_ref().map(|id| id.name.as_str()),
        ),
        FunctionNode::FunctionExpression(expr) => (
            &expr.params[..],
            FunctionBody::Block(&expr.body),
            expr.generator,
            expr.is_async,
            convert_opt_loc(&expr.base.loc),
            expr.base.start.unwrap_or(0),
            expr.base.end.unwrap_or(0),
            expr.id.as_ref().map(|id| id.name.as_str()),
        ),
        FunctionNode::ArrowFunctionExpression(arrow) => {
            let body = match arrow.body.as_ref() {
                react_compiler_ast::expressions::ArrowFunctionBody::BlockStatement(block) => {
                    FunctionBody::Block(block)
                }
                react_compiler_ast::expressions::ArrowFunctionBody::Expression(expr) => {
                    FunctionBody::Expression(expr)
                }
            };
            (
                &arrow.params[..],
                body,
                arrow.generator,
                arrow.is_async,
                convert_opt_loc(&arrow.base.loc),
                arrow.base.start.unwrap_or(0),
                arrow.base.end.unwrap_or(0),
                None, // Arrow functions never have an AST id
            )
        }
    };

    let scope_id = scope_info
        .resolve_scope_for_node(func.node_id())
        .unwrap_or(scope_info.program_scope);

    validate_ts_this_parameters_in_function_range(scope_info, start, end)?;

    // Build identifier location index from the AST (replaces serialized referenceLocs/jsxReferencePositions)
    let identifier_locs = build_identifier_loc_index(func, scope_info);

    // Pre-compute context identifiers: variables captured across function boundaries
    let context_identifiers = find_context_identifiers(func, scope_info, env, &identifier_locs)?;

    // For top-level functions, context is empty (no captured refs)
    let context_map: IndexMap<react_compiler_ast::scope::BindingId, Option<SourceLocation>> =
        IndexMap::new();

    let (hir_func, _used_names, _child_bindings) = lower_inner(
        params,
        body,
        ast_id,
        generator,
        is_async,
        loc,
        scope_info,
        env,
        None, // no pre-existing bindings for top-level
        None, // no pre-existing used_names for top-level
        context_map,
        scope_id,
        scope_id, // component_scope = function_scope for top-level
        &context_identifiers,
        true, // is_top_level
        &identifier_locs,
    )?;

    Ok(hir_func)
}

// =============================================================================
// Stubs for future milestones
// =============================================================================
