
use indexmap::IndexMap;
use indexmap::IndexSet;
use react_compiler_ast::scope::ScopeInfo;
use react_compiler_hir::*;

use crate::identifier_loc_index::IdentifierLocIndex;

#[allow(unused_imports)]
use super::*;

/// Gather captured context variables for a nested function.
///
/// Walks through all identifier references (via `reference_to_binding`) and checks
/// which ones resolve to bindings declared in scopes between the function's parent scope
/// and the component scope. These are "free variables" that become the function's `context`.
pub(crate) fn gather_captured_context(
    scope_info: &ScopeInfo,
    function_scope: react_compiler_ast::scope::ScopeId,
    component_scope: react_compiler_ast::scope::ScopeId,
    func_start: u32,
    func_end: u32,
    identifier_locs: &IdentifierLocIndex,
    ref_node_ids_override: Option<&IndexSet<u32>>,
) -> IndexMap<react_compiler_ast::scope::BindingId, Option<SourceLocation>> {
    let parent_scope = scope_info.scopes[function_scope.0 as usize].parent;
    let pure_scopes = match parent_scope {
        Some(parent) => capture_scopes(scope_info, parent, component_scope),
        None => IndexSet::new(),
    };

    // Collect the earliest (lowest source position) reference location for each
    // captured binding. Using the minimum position makes the result independent of
    // ref_node_id_to_binding iteration order, matching the behavior the TS compiler
    // gets from Babel's position-ordered traversal.
    let mut captured: std::collections::HashMap<
        react_compiler_ast::scope::BindingId,
        (u32, Option<SourceLocation>), // (min_position, loc)
    > = std::collections::HashMap::new();

    for (&ref_nid, &binding_id) in &scope_info.ref_node_id_to_binding {
        if let Some(allowed) = ref_node_ids_override {
            if !allowed.contains(&ref_nid) {
                continue;
            }
        } else {
            // Range check: use the position stored in identifier_locs
            let ref_start = identifier_locs.get(&ref_nid).map(|e| e.start).unwrap_or(0);
            if ref_start < func_start || ref_start >= func_end {
                continue;
            }
        }
        let binding = &scope_info.bindings[binding_id.0 as usize];
        // Skip references that are actually the binding's own declaration site
        if binding.declaration_node_id == Some(ref_nid) {
            continue;
        }
        // Skip function/class declaration names that are not expression references.
        // Skip type-annotation references: TS's gatherCapturedContext traverse
        // skips TypeAnnotation/TSTypeAnnotation/TypeAlias/TSTypeAliasDeclaration
        // subtrees, so identifiers there never become captures (they DO still
        // feed FindContextIdentifiers and the hoisting analysis, which have no
        // such skip in TS).
        if let Some(entry) = identifier_locs.get(&ref_nid) {
            if entry.is_declaration_name || entry.in_type_annotation {
                continue;
            }
        }
        // Skip type-only bindings
        if binding.declaration_type == "TypeAlias"
            || binding.declaration_type == "OpaqueType"
            || binding.declaration_type == "InterfaceDeclaration"
            || binding.declaration_type == "TSTypeAliasDeclaration"
            || binding.declaration_type == "TSInterfaceDeclaration"
            || binding.declaration_type == "TSEnumDeclaration"
        {
            continue;
        }
        if pure_scopes.contains(&binding.scope) {
            let ref_start = identifier_locs.get(&ref_nid).map(|e| e.start).unwrap_or(0);
            // Skip references whose start offset aliases the binding's own
            // declaration offset. Hermes desugars (component syntax) reuse the
            // original source offsets for generated nodes, so a sibling
            // reference structurally OUTSIDE this function (e.g. the forwardRef
            // argument naming the desugared inner function) can fall inside the
            // function's position range and alias the declaration position. In
            // real source a non-declaration reference can never share its
            // declaration's offset, so this only filters desugared aliases.
            if binding.declaration_start == Some(ref_start) {
                continue;
            }
            let loc = identifier_locs.get(&ref_nid).map(|entry| {
                if let Some(oe_loc) = &entry.opening_element_loc {
                    oe_loc.clone()
                } else {
                    entry.loc.clone()
                }
            });
            captured
                .entry(binding.id)
                .and_modify(|(min_pos, existing_loc)| {
                    if ref_start < *min_pos {
                        *min_pos = ref_start;
                        *existing_loc = loc.clone();
                    }
                })
                .or_insert((ref_start, loc));
        }
    }

    // Sort captured entries by source position so context declarations appear
    // in source order, matching the TS compiler's position-ordered traversal.
    let mut sorted: Vec<_> = captured.into_iter().collect();
    sorted.sort_by_key(|(_, (pos, _))| *pos);

    sorted
        .into_iter()
        .map(|(bid, (_, loc))| (bid, loc))
        .collect()
}

pub(crate) fn capture_scopes(
    scope_info: &ScopeInfo,
    from: react_compiler_ast::scope::ScopeId,
    to: react_compiler_ast::scope::ScopeId,
) -> IndexSet<react_compiler_ast::scope::ScopeId> {
    let mut result = IndexSet::new();
    let mut current = Some(from);
    while let Some(scope_id) = current {
        result.insert(scope_id);
        if scope_id == to {
            break;
        }
        current = scope_info.scopes[scope_id.0 as usize].parent;
    }
    result
}

pub(crate) fn collect_identifier_node_ids_from_body(body: &FunctionBody) -> IndexSet<u32> {
    let mut positions = IndexSet::new();
    match body {
        FunctionBody::Block(block) => {
            for stmt in &block.body {
                collect_identifier_node_ids_from_stmt(stmt, &mut positions);
            }
        }
        FunctionBody::Expression(expr) => {
            collect_identifier_node_ids_from_expr(expr, &mut positions);
        }
    }
    positions
}

pub(crate) fn collect_identifier_node_ids_from_stmt(
    stmt: &react_compiler_ast::statements::Statement,
    positions: &mut IndexSet<u32>,
) {
    use react_compiler_ast::statements::Statement;
    match stmt {
        Statement::ExpressionStatement(s) => {
            collect_identifier_node_ids_from_expr(&s.expression, positions)
        }
        Statement::ReturnStatement(s) => {
            if let Some(arg) = &s.argument {
                collect_identifier_node_ids_from_expr(arg, positions);
            }
        }
        Statement::ThrowStatement(s) => {
            collect_identifier_node_ids_from_expr(&s.argument, positions)
        }
        Statement::BlockStatement(s) => {
            for stmt in &s.body {
                collect_identifier_node_ids_from_stmt(stmt, positions);
            }
        }
        Statement::IfStatement(s) => {
            collect_identifier_node_ids_from_expr(&s.test, positions);
            collect_identifier_node_ids_from_stmt(&s.consequent, positions);
            if let Some(alt) = &s.alternate {
                collect_identifier_node_ids_from_stmt(alt, positions);
            }
        }
        Statement::VariableDeclaration(s) => {
            for decl in &s.declarations {
                if let Some(init) = &decl.init {
                    collect_identifier_node_ids_from_expr(init, positions);
                }
            }
        }
        _ => {}
    }
}

pub(crate) fn collect_identifier_node_ids_from_expr(
    expr: &react_compiler_ast::expressions::Expression,
    positions: &mut IndexSet<u32>,
) {
    use react_compiler_ast::expressions::Expression;
    match expr {
        Expression::Identifier(id) => {
            if let Some(nid) = id.base.node_id {
                positions.insert(nid);
            }
        }
        Expression::CallExpression(call) => {
            collect_identifier_node_ids_from_expr(&call.callee, positions);
            for arg in &call.arguments {
                collect_identifier_node_ids_from_expr(arg, positions);
            }
        }
        Expression::BinaryExpression(e) => {
            collect_identifier_node_ids_from_expr(&e.left, positions);
            collect_identifier_node_ids_from_expr(&e.right, positions);
        }
        Expression::ConditionalExpression(e) => {
            collect_identifier_node_ids_from_expr(&e.test, positions);
            collect_identifier_node_ids_from_expr(&e.consequent, positions);
            collect_identifier_node_ids_from_expr(&e.alternate, positions);
        }
        Expression::LogicalExpression(e) => {
            collect_identifier_node_ids_from_expr(&e.left, positions);
            collect_identifier_node_ids_from_expr(&e.right, positions);
        }
        Expression::MemberExpression(e) => {
            collect_identifier_node_ids_from_expr(&e.object, positions);
        }
        Expression::OptionalMemberExpression(e) => {
            collect_identifier_node_ids_from_expr(&e.object, positions);
        }
        Expression::OptionalCallExpression(e) => {
            collect_identifier_node_ids_from_expr(&e.callee, positions);
            for arg in &e.arguments {
                collect_identifier_node_ids_from_expr(arg, positions);
            }
        }
        Expression::UpdateExpression(e) => {
            collect_identifier_node_ids_from_expr(&e.argument, positions);
        }
        Expression::FunctionExpression(func) => {
            for stmt in &func.body.body {
                collect_identifier_node_ids_from_stmt(stmt, positions);
            }
        }
        Expression::UnaryExpression(e) => {
            collect_identifier_node_ids_from_expr(&e.argument, positions);
        }
        Expression::ParenthesizedExpression(e) => {
            collect_identifier_node_ids_from_expr(&e.expression, positions);
        }
        Expression::TypeCastExpression(e) => {
            collect_identifier_node_ids_from_expr(&e.expression, positions);
        }
        Expression::ArrowFunctionExpression(arrow) => match arrow.body.as_ref() {
            react_compiler_ast::expressions::ArrowFunctionBody::BlockStatement(block) => {
                for stmt in &block.body {
                    collect_identifier_node_ids_from_stmt(stmt, positions);
                }
            }
            react_compiler_ast::expressions::ArrowFunctionBody::Expression(e) => {
                collect_identifier_node_ids_from_expr(e, positions);
            }
        },
        Expression::JSXElement(el) => {
            if let react_compiler_ast::jsx::JSXElementName::JSXIdentifier(id) =
                &el.opening_element.name
            {
                if let Some(nid) = id.base.node_id {
                    positions.insert(nid);
                }
            }
            for attr in &el.opening_element.attributes {
                match attr {
                    react_compiler_ast::jsx::JSXAttributeItem::JSXAttribute(a) => {
                        if let Some(val) = &a.value {
                            match val {
                                react_compiler_ast::jsx::JSXAttributeValue::JSXExpressionContainer(c) => {
                                    if let react_compiler_ast::jsx::JSXExpressionContainerExpr::Expression(e) = &c.expression {
                                        collect_identifier_node_ids_from_expr(e, positions);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    react_compiler_ast::jsx::JSXAttributeItem::JSXSpreadAttribute(a) => {
                        collect_identifier_node_ids_from_expr(&a.argument, positions);
                    }
                }
            }
            for child in &el.children {
                match child {
                    react_compiler_ast::jsx::JSXChild::JSXExpressionContainer(c) => {
                        if let react_compiler_ast::jsx::JSXExpressionContainerExpr::Expression(e) =
                            &c.expression
                        {
                            collect_identifier_node_ids_from_expr(e, positions);
                        }
                    }
                    react_compiler_ast::jsx::JSXChild::JSXElement(child_el) => {
                        collect_identifier_node_ids_from_expr(
                            &Expression::JSXElement(child_el.clone()),
                            positions,
                        );
                    }
                    react_compiler_ast::jsx::JSXChild::JSXSpreadChild(s) => {
                        collect_identifier_node_ids_from_expr(&s.expression, positions);
                    }
                    _ => {}
                }
            }
        }
        Expression::JSXFragment(frag) => {
            for child in &frag.children {
                match child {
                    react_compiler_ast::jsx::JSXChild::JSXExpressionContainer(c) => {
                        if let react_compiler_ast::jsx::JSXExpressionContainerExpr::Expression(e) =
                            &c.expression
                        {
                            collect_identifier_node_ids_from_expr(e, positions);
                        }
                    }
                    react_compiler_ast::jsx::JSXChild::JSXElement(child_el) => {
                        collect_identifier_node_ids_from_expr(
                            &Expression::JSXElement(child_el.clone()),
                            positions,
                        );
                    }
                    _ => {}
                }
            }
        }
        Expression::ArrayExpression(arr) => {
            for elem in &arr.elements {
                if let Some(e) = elem {
                    collect_identifier_node_ids_from_expr(e, positions);
                }
            }
        }
        Expression::ObjectExpression(obj) => {
            for prop in &obj.properties {
                match prop {
                    react_compiler_ast::expressions::ObjectExpressionProperty::ObjectProperty(
                        p,
                    ) => {
                        collect_identifier_node_ids_from_expr(&p.value, positions);
                    }
                    react_compiler_ast::expressions::ObjectExpressionProperty::SpreadElement(s) => {
                        collect_identifier_node_ids_from_expr(&s.argument, positions);
                    }
                    _ => {}
                }
            }
        }
        Expression::NewExpression(e) => {
            collect_identifier_node_ids_from_expr(&e.callee, positions);
            for arg in &e.arguments {
                collect_identifier_node_ids_from_expr(arg, positions);
            }
        }
        Expression::AssignmentExpression(e) => {
            collect_identifier_node_ids_from_expr(&e.right, positions);
        }
        Expression::TemplateLiteral(e) => {
            for expr in &e.expressions {
                collect_identifier_node_ids_from_expr(expr, positions);
            }
        }
        Expression::SpreadElement(e) => {
            collect_identifier_node_ids_from_expr(&e.argument, positions);
        }
        Expression::SequenceExpression(e) => {
            for expr in &e.expressions {
                collect_identifier_node_ids_from_expr(expr, positions);
            }
        }
        _ => {}
    }
}
