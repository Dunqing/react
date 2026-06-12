use std::collections::HashSet;

use indexmap::IndexMap;
use react_compiler_ast::scope::ScopeInfo;
use react_compiler_ast::scope::ScopeKind;
use react_compiler_diagnostics::CompilerDiagnostic;
use react_compiler_diagnostics::CompilerDiagnosticDetail;
use react_compiler_diagnostics::CompilerError;
use react_compiler_diagnostics::CompilerErrorDetail;
use react_compiler_diagnostics::ErrorCategory;
use react_compiler_hir::environment::Environment;
use react_compiler_hir::*;

use crate::hir_builder::HirBuilder;
use crate::hir_builder::is_always_reserved_word;
use crate::hir_builder::reserved_identifier_diagnostic;
use crate::identifier_loc_index::IdentifierLocIndex;

#[allow(unused_imports)]
use super::*;

pub(crate) fn lower_function_to_value(
    builder: &mut HirBuilder,
    expr: &react_compiler_ast::expressions::Expression,
    expr_type: FunctionExpressionType,
) -> Result<InstructionValue, CompilerDiagnostic> {
    use react_compiler_ast::expressions::Expression;
    let loc = match expr {
        Expression::ArrowFunctionExpression(arrow) => convert_opt_loc(&arrow.base.loc),
        Expression::FunctionExpression(func) => convert_opt_loc(&func.base.loc),
        _ => None,
    };
    let name = match expr {
        Expression::FunctionExpression(func) => func.id.as_ref().map(|id| id.name.clone()),
        _ => None,
    };
    let lowered_func = lower_function(builder, expr)?;
    Ok(InstructionValue::FunctionExpression {
        name,
        name_hint: None,
        lowered_func,
        expr_type,
        loc,
    })
}

pub(crate) fn lower_function(
    builder: &mut HirBuilder,
    expr: &react_compiler_ast::expressions::Expression,
) -> Result<LoweredFunction, CompilerDiagnostic> {
    use react_compiler_ast::expressions::Expression;

    // Extract function parts from the AST node
    let (params, body, id, generator, is_async, func_start, func_end, func_loc, func_node_id) =
        match expr {
            Expression::ArrowFunctionExpression(arrow) => {
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
                    None::<&str>,
                    arrow.generator,
                    arrow.is_async,
                    arrow.base.start.unwrap_or(0),
                    arrow.base.end.unwrap_or(0),
                    convert_opt_loc(&arrow.base.loc),
                    arrow.base.node_id,
                )
            }
            Expression::FunctionExpression(func) => (
                &func.params[..],
                FunctionBody::Block(&func.body),
                func.id.as_ref().map(|id| id.name.as_str()),
                func.generator,
                func.is_async,
                func.base.start.unwrap_or(0),
                func.base.end.unwrap_or(0),
                convert_opt_loc(&func.base.loc),
                func.base.node_id,
            ),
            _ => {
                return Err(CompilerDiagnostic::new(
                    ErrorCategory::Invariant,
                    "lower_function called with non-function expression",
                    None,
                ));
            }
        };

    // Find the function's scope. For synthetic zero-width functions (e.g., desugared
    // match IIFEs from Hermes with start=end=0), node_id_to_scope won't have an entry.
    let function_scope =
        if let Some(scope) = builder.scope_info().resolve_scope_for_node(func_node_id) {
            scope
        } else if func_start < func_end {
            builder.scope_info().program_scope
        } else {
            let parent = builder.function_scope();
            let scope_info = builder.scope_info();
            let mapped: std::collections::HashSet<react_compiler_ast::scope::ScopeId> =
                scope_info.node_id_to_scope.values().copied().collect();
            let param_names: Vec<String> = params
                .iter()
                .filter_map(|p| {
                    if let react_compiler_ast::patterns::PatternLike::Identifier(id) = p {
                        Some(id.name.clone())
                    } else {
                        None
                    }
                })
                .collect();
            let mut descendants = std::collections::HashSet::new();
            descendants.insert(parent);
            let mut changed = true;
            while changed {
                changed = false;
                for (i, scope) in scope_info.scopes.iter().enumerate() {
                    let sid = react_compiler_ast::scope::ScopeId(i as u32);
                    if let Some(p) = scope.parent {
                        if descendants.contains(&p) && !descendants.contains(&sid) {
                            descendants.insert(sid);
                            changed = true;
                        }
                    }
                }
            }
            let mut found = scope_info.program_scope;
            for (i, scope) in scope_info.scopes.iter().enumerate() {
                let sid = react_compiler_ast::scope::ScopeId(i as u32);
                if let Some(p) = scope.parent {
                    if descendants.contains(&p)
                        && matches!(scope.kind, ScopeKind::Function)
                        && !mapped.contains(&sid)
                        && !builder.is_synthetic_scope_claimed(sid)
                    {
                        if !param_names.is_empty() {
                            let all_match = param_names
                                .iter()
                                .all(|name| scope.bindings.contains_key(name));
                            if !all_match {
                                continue;
                            }
                        }
                        found = sid;
                        break;
                    }
                }
            }
            builder.claim_synthetic_scope(found);
            found
        };

    let component_scope = builder.component_scope();
    let scope_info = builder.scope_info();

    let parent_bindings = builder.bindings().clone();
    let parent_used_names = builder.used_names().clone();
    let context_ids = builder.context_identifiers().clone();
    let ident_locs = builder.identifier_locs();

    // For synthetic functions with zero-width position ranges, position-based
    // reference filtering fails. Walk the body AST to collect actual positions.
    let ref_override = if func_start >= func_end {
        Some(collect_identifier_node_ids_from_body(&body))
    } else {
        None
    };

    // Gather captured context
    let captured_context = gather_captured_context(
        scope_info,
        function_scope,
        component_scope,
        func_start,
        func_end,
        ident_locs,
        ref_override.as_ref(),
    );
    let merged_context: IndexMap<react_compiler_ast::scope::BindingId, Option<SourceLocation>> = {
        let parent_context = builder.context().clone();
        let mut merged = parent_context;
        for (k, v) in captured_context {
            merged.insert(k, v);
        }
        merged
    };

    // Use scope_info_and_env_mut to avoid conflicting borrows
    let (scope_info, env) = builder.scope_info_and_env_mut();
    let (hir_func, child_used_names, child_bindings) = lower_inner(
        params,
        body,
        id,
        generator,
        is_async,
        func_loc,
        scope_info,
        env,
        Some(parent_bindings),
        Some(parent_used_names),
        merged_context,
        function_scope,
        component_scope,
        &context_ids,
        false, // nested function
        ident_locs,
    )?;

    builder.merge_used_names(child_used_names);
    builder.merge_bindings(child_bindings);

    let func_id = builder.environment_mut().add_function(hir_func);
    Ok(LoweredFunction { func: func_id })
}

/// Lower a function declaration statement to a FunctionExpression + StoreLocal.
pub(crate) fn lower_function_declaration(
    builder: &mut HirBuilder,
    func_decl: &react_compiler_ast::statements::FunctionDeclaration,
) -> Result<(), CompilerError> {
    let loc = convert_opt_loc(&func_decl.base.loc);
    let func_start = func_decl.base.start.unwrap_or(0);
    let func_end = func_decl.base.end.unwrap_or(0);

    let func_name = func_decl.id.as_ref().map(|id| id.name.clone());

    // Find the function's scope
    let function_scope = builder
        .scope_info()
        .resolve_scope_for_node(func_decl.base.node_id)
        .unwrap_or(builder.scope_info().program_scope);

    let component_scope = builder.component_scope();
    let scope_info = builder.scope_info();

    let parent_bindings = builder.bindings().clone();
    let parent_used_names = builder.used_names().clone();
    let context_ids = builder.context_identifiers().clone();
    let ident_locs = builder.identifier_locs();

    // Gather captured context
    let captured_context = gather_captured_context(
        scope_info,
        function_scope,
        component_scope,
        func_start,
        func_end,
        ident_locs,
        None,
    );
    let merged_context: IndexMap<react_compiler_ast::scope::BindingId, Option<SourceLocation>> = {
        let parent_context = builder.context().clone();
        let mut merged = parent_context;
        for (k, v) in captured_context {
            merged.insert(k, v);
        }
        merged
    };

    let (scope_info, env) = builder.scope_info_and_env_mut();
    let (hir_func, child_used_names, child_bindings) = lower_inner(
        &func_decl.params,
        FunctionBody::Block(&func_decl.body),
        func_decl.id.as_ref().map(|id| id.name.as_str()),
        func_decl.generator,
        func_decl.is_async,
        loc.clone(),
        scope_info,
        env,
        Some(parent_bindings),
        Some(parent_used_names),
        merged_context,
        function_scope,
        component_scope,
        &context_ids,
        false, // nested function
        ident_locs,
    )?;

    builder.merge_used_names(child_used_names);
    builder.merge_bindings(child_bindings);

    let func_id = builder.environment_mut().add_function(hir_func);
    let lowered_func = LoweredFunction { func: func_id };

    // Emit FunctionExpression instruction
    let fn_value = InstructionValue::FunctionExpression {
        name: func_name.clone(),
        name_hint: None,
        lowered_func,
        expr_type: FunctionExpressionType::FunctionDeclaration,
        loc: loc.clone(),
    };
    let fn_place = lower_value_to_temporary(builder, fn_value)?;

    // Resolve the binding for the function name and store. TS resolves the id
    // via Babel's `path.scope.getBinding(name)`, which starts at the function's
    // OWN scope: a body-level local that shadows the function's name resolves
    // to that inner binding — storing the function into the shadow while
    // references elsewhere resolve to the hoisted binding in the parent scope.
    // This is a known TS quirk that we reproduce for parity (see
    // todo-repro-named-function-with-shadowed-local-same-name). Fall back to
    // node-based resolution when the scope walk fails (degraded scope info,
    // e.g. synthetic scopes, or backends that split function-body scopes).
    if let Some(ref name) = func_name {
        if let Some(id_node) = &func_decl.id {
            let start = id_node.base.start.unwrap_or(0);
            let ident_loc = convert_opt_loc(&id_node.base.loc);
            let scope_binding = builder.get_function_declaration_binding(function_scope, name);
            let mut is_context = false;
            let binding = match scope_binding {
                Some(binding_id) => {
                    is_context = builder.is_context_binding(binding_id);
                    let binding_kind = crate::convert_binding_kind(
                        &builder.scope_info().bindings[binding_id.0 as usize].kind,
                    );
                    let identifier =
                        builder.resolve_binding_with_loc(name, binding_id, ident_loc.clone())?;
                    VariableBinding::Identifier {
                        identifier,
                        binding_kind,
                    }
                }
                None => {
                    let mut binding = builder.resolve_identifier(
                        name,
                        start,
                        ident_loc.clone(),
                        id_node.base.node_id,
                    )?;
                    if matches!(&binding, VariableBinding::Global { .. }) {
                        // For function redeclarations (e.g., `function x() {} function x() {}`),
                        // the redeclaration's identifier may not be in ref_node_id_to_binding
                        // (OXC/SWC don't map constant violations). Retry using the first
                        // declaration's node_id from the scope chain.
                        let fallback = {
                            let si = builder.scope_info();
                            let scope_id = si
                                .resolve_scope_for_node(func_decl.base.node_id)
                                .unwrap_or(si.program_scope);
                            si.get_binding(scope_id, name).map(|bid| {
                                let b = &si.bindings[bid.0 as usize];
                                (b.declaration_start.unwrap_or(0), b.declaration_node_id)
                            })
                        };
                        if let Some((ds, ds_node_id)) = fallback {
                            binding = builder.resolve_identifier(
                                name,
                                ds,
                                ident_loc.clone(),
                                ds_node_id,
                            )?;
                        }
                    }
                    if matches!(&binding, VariableBinding::Identifier { .. }) {
                        is_context =
                            builder.is_context_identifier(name, start, id_node.base.node_id);
                    }
                    binding
                }
            };
            match binding {
                VariableBinding::Identifier { identifier, .. } => {
                    // Don't override the identifier's declaration loc here.
                    // For function redeclarations (e.g., `function x() {} function x() {}`),
                    // the identifier's loc should remain the first declaration's loc,
                    // which was already set during define_binding.
                    // Use the full function declaration loc for the Place,
                    // matching the TS behavior where lowerAssignment uses stmt.node.loc
                    let place = Place {
                        identifier,
                        reactive: false,
                        effect: Effect::Unknown,
                        loc: loc.clone(),
                    };
                    if is_context {
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
    }
    Ok(())
}

/// Lower a function expression used as an object method.
pub(crate) fn lower_function_for_object_method(
    builder: &mut HirBuilder,
    method: &react_compiler_ast::expressions::ObjectMethod,
) -> Result<LoweredFunction, CompilerError> {
    let func_start = method.base.start.unwrap_or(0);
    let func_end = method.base.end.unwrap_or(0);
    let func_loc = convert_opt_loc(&method.base.loc);

    let function_scope = builder
        .scope_info()
        .resolve_scope_for_node(method.base.node_id)
        .unwrap_or(builder.scope_info().program_scope);

    let component_scope = builder.component_scope();
    let scope_info = builder.scope_info();

    let parent_bindings = builder.bindings().clone();
    let parent_used_names = builder.used_names().clone();
    let context_ids = builder.context_identifiers().clone();
    let ident_locs = builder.identifier_locs();

    let captured_context = gather_captured_context(
        scope_info,
        function_scope,
        component_scope,
        func_start,
        func_end,
        ident_locs,
        None,
    );
    let merged_context: IndexMap<react_compiler_ast::scope::BindingId, Option<SourceLocation>> = {
        let parent_context = builder.context().clone();
        let mut merged = parent_context;
        for (k, v) in captured_context {
            merged.insert(k, v);
        }
        merged
    };

    let (scope_info, env) = builder.scope_info_and_env_mut();
    let (hir_func, child_used_names, child_bindings) = lower_inner(
        &method.params,
        FunctionBody::Block(&method.body),
        None,
        method.generator,
        method.is_async,
        func_loc,
        scope_info,
        env,
        Some(parent_bindings),
        Some(parent_used_names),
        merged_context,
        function_scope,
        component_scope,
        &context_ids,
        false, // nested function
        ident_locs,
    )?;

    builder.merge_used_names(child_used_names);
    builder.merge_bindings(child_bindings);

    let func_id = builder.environment_mut().add_function(hir_func);
    Ok(LoweredFunction { func: func_id })
}

/// Internal helper: lower a function given its extracted parts.
/// Used by both the top-level `lower()` and nested `lower_function()`.
pub(crate) fn lower_inner(
    params: &[react_compiler_ast::patterns::PatternLike],
    body: FunctionBody<'_>,
    id: Option<&str>,
    generator: bool,
    is_async: bool,
    loc: Option<SourceLocation>,
    scope_info: &ScopeInfo,
    env: &mut Environment,
    parent_bindings: Option<IndexMap<react_compiler_ast::scope::BindingId, IdentifierId>>,
    parent_used_names: Option<IndexMap<String, react_compiler_ast::scope::BindingId>>,
    context_map: IndexMap<react_compiler_ast::scope::BindingId, Option<SourceLocation>>,
    function_scope: react_compiler_ast::scope::ScopeId,
    component_scope: react_compiler_ast::scope::ScopeId,
    context_identifiers: &HashSet<react_compiler_ast::scope::BindingId>,
    is_top_level: bool,
    identifier_locs: &IdentifierLocIndex,
) -> Result<
    (
        HirFunction,
        IndexMap<String, react_compiler_ast::scope::BindingId>,
        IndexMap<react_compiler_ast::scope::BindingId, IdentifierId>,
    ),
    CompilerError,
> {
    validate_ts_this_parameter(scope_info, function_scope)?;

    let mut builder = HirBuilder::new(
        env,
        scope_info,
        function_scope,
        component_scope,
        context_identifiers.clone(),
        parent_bindings,
        Some(context_map.clone()),
        None,
        parent_used_names,
        identifier_locs,
    );

    // Build context places from the captured refs
    let mut context: Vec<Place> = Vec::new();
    for (&binding_id, ctx_loc) in &context_map {
        let binding = &scope_info.bindings[binding_id.0 as usize];
        let identifier = builder.resolve_binding(&binding.name, binding_id)?;
        context.push(Place {
            identifier,
            effect: Effect::Unknown,
            reactive: false,
            loc: ctx_loc.clone(),
        });
    }

    // Process parameters
    let mut hir_params: Vec<ParamPattern> = Vec::new();
    for param in params {
        match param {
            react_compiler_ast::patterns::PatternLike::Identifier(ident) => {
                if is_always_reserved_word(&ident.name) {
                    return Err(CompilerError::from(reserved_identifier_diagnostic(
                        &ident.name,
                    )));
                }
                let start = ident.base.start.unwrap_or(0);
                let param_loc = convert_opt_loc(&ident.base.loc);
                let mut binding = builder.resolve_identifier(
                    &ident.name,
                    start,
                    param_loc.clone(),
                    ident.base.node_id,
                )?;
                if !matches!(binding, VariableBinding::Identifier { .. }) {
                    // Position-based resolution failed (common for synthetic params
                    // like $$gen$m0 at position 0). Try lookup in function scope
                    // and descendants.
                    if let Some((binding_id, binding_data)) = builder
                        .scope_info()
                        .find_binding_id_in_descendants(&ident.name, builder.function_scope())
                    {
                        let binding_kind = crate::convert_binding_kind(&binding_data.kind);
                        let identifier = builder.resolve_binding_with_loc(
                            &ident.name,
                            binding_id,
                            param_loc.clone(),
                        )?;
                        binding = VariableBinding::Identifier {
                            identifier,
                            binding_kind,
                        };
                    }
                }
                match binding {
                    VariableBinding::Identifier { identifier, .. } => {
                        builder.set_identifier_declaration_loc(identifier, &param_loc);
                        let place = Place {
                            identifier,
                            effect: Effect::Unknown,
                            reactive: false,
                            loc: param_loc,
                        };
                        hir_params.push(ParamPattern::Place(place));
                    }
                    _ => {
                        builder.record_diagnostic(
                            CompilerDiagnostic::new(
                                ErrorCategory::Invariant,
                                "Could not find binding",
                                Some(format!(
                                    "[BuildHIR] Could not find binding for param `{}`",
                                    ident.name
                                )),
                            )
                            .with_detail(
                                CompilerDiagnosticDetail::Error {
                                    loc: convert_opt_loc(&ident.base.loc),
                                    message: Some("Could not find binding".to_string()),
                                    identifier_name: None,
                                },
                            ),
                        );
                    }
                }
            }
            react_compiler_ast::patterns::PatternLike::RestElement(rest) => {
                let rest_loc = convert_opt_loc(&rest.base.loc);
                // Create a temporary place for the spread param
                let place = build_temporary_place(&mut builder, rest_loc.clone());
                hir_params.push(ParamPattern::Spread(SpreadPattern {
                    place: place.clone(),
                }));
                // Delegate the assignment of the rest argument
                lower_assignment(
                    &mut builder,
                    rest_loc,
                    InstructionKind::Let,
                    &rest.argument,
                    place,
                    AssignmentStyle::Assignment,
                )?;
            }
            react_compiler_ast::patterns::PatternLike::ObjectPattern(_)
            | react_compiler_ast::patterns::PatternLike::ArrayPattern(_)
            | react_compiler_ast::patterns::PatternLike::AssignmentPattern(_) => {
                let param_loc = convert_opt_loc(&pattern_like_loc(param));
                let place = build_temporary_place(&mut builder, param_loc.clone());
                promote_temporary(&mut builder, place.identifier);
                hir_params.push(ParamPattern::Place(place.clone()));
                lower_assignment(
                    &mut builder,
                    param_loc,
                    InstructionKind::Let,
                    param,
                    place,
                    AssignmentStyle::Assignment,
                )?;
            }
            react_compiler_ast::patterns::PatternLike::MemberExpression(member) => {
                builder.record_diagnostic(
                    CompilerDiagnostic::new(
                        ErrorCategory::Todo,
                        "Handle MemberExpression parameters",
                        Some("[BuildHIR] Add support for MemberExpression parameters".to_string()),
                    )
                    .with_detail(CompilerDiagnosticDetail::Error {
                        loc: convert_opt_loc(&member.base.loc),
                        message: Some("Unsupported parameter type".to_string()),
                        identifier_name: None,
                    }),
                );
            }
            react_compiler_ast::patterns::PatternLike::TSAsExpression(_)
            | react_compiler_ast::patterns::PatternLike::TSSatisfiesExpression(_)
            | react_compiler_ast::patterns::PatternLike::TSNonNullExpression(_)
            | react_compiler_ast::patterns::PatternLike::TSTypeAssertion(_)
            | react_compiler_ast::patterns::PatternLike::TypeCastExpression(_) => {}
        }
    }

    // Lower the body
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
                .map(|d| d.value.value.clone())
                .collect();
            // Use lower_block_statement_with_scope to get hoisting support for the function body.
            // Pass the function scope since in Babel, a function body BlockStatement shares
            // the function's scope (node_to_scope maps the function node, not the block).
            lower_block_statement_with_scope(&mut builder, block, function_scope)?;
        }
    }

    // Emit final Return(Void, undefined)
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

    // Build the HIR
    let (hir_body, instructions, used_names, child_bindings) = builder.build()?;

    // Create the returns place
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

pub(crate) fn lower_object_method(
    builder: &mut HirBuilder,
    method: &react_compiler_ast::expressions::ObjectMethod,
) -> Result<Option<ObjectProperty>, CompilerError> {
    use react_compiler_ast::expressions::ObjectMethodKind;
    if !matches!(method.kind, ObjectMethodKind::Method) {
        let kind_str = match method.kind {
            ObjectMethodKind::Get => "get",
            ObjectMethodKind::Set => "set",
            ObjectMethodKind::Method => "method",
        };
        builder.record_error(CompilerErrorDetail {
            reason: format!(
                "(BuildHIR::lowerExpression) Handle {} functions in ObjectExpression",
                kind_str
            ),
            category: ErrorCategory::Todo,
            loc: convert_opt_loc(&method.base.loc),
            description: None,
            suggestions: None,
        })?;
        return Ok(None);
    }
    let key = lower_object_property_key(builder, &method.key, method.computed)?.unwrap_or(
        ObjectPropertyKey::String {
            name: String::new(),
        },
    );

    let lowered_func = lower_function_for_object_method(builder, method)?;

    let loc = convert_opt_loc(&method.base.loc);
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

pub(crate) fn lower_object_property_key(
    builder: &mut HirBuilder,
    key: &react_compiler_ast::expressions::Expression,
    computed: bool,
) -> Result<Option<ObjectPropertyKey>, CompilerError> {
    use react_compiler_ast::expressions::Expression;
    match key {
        Expression::StringLiteral(lit) => Ok(Some(ObjectPropertyKey::String {
            name: lit.value.clone(),
        })),
        Expression::Identifier(ident) if !computed => Ok(Some(ObjectPropertyKey::Identifier {
            name: ident.name.clone(),
        })),
        Expression::NumericLiteral(lit) if !computed => Ok(Some(ObjectPropertyKey::Identifier {
            name: lit.value.to_string(),
        })),
        _ if computed => {
            let place = lower_expression_to_temporary(builder, key)?;
            Ok(Some(ObjectPropertyKey::Computed { name: place }))
        }
        _ => {
            let loc = match key {
                Expression::Identifier(i) => convert_opt_loc(&i.base.loc),
                _ => None,
            };
            builder.record_error(CompilerErrorDetail {
                category: ErrorCategory::Todo,
                reason: "Unsupported key type in ObjectExpression".to_string(),
                description: None,
                loc,
                suggestions: None,
            })?;
            Ok(None)
        }
    }
}
