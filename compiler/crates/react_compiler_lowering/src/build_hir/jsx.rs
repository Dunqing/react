
use react_compiler_diagnostics::CompilerError;
use react_compiler_diagnostics::CompilerErrorDetail;
use react_compiler_diagnostics::ErrorCategory;
use react_compiler_hir::*;

use crate::hir_builder::HirBuilder;

#[allow(unused_imports)]
use super::*;

pub(crate) fn lower_jsx_element_name(
    builder: &mut HirBuilder,
    name: &react_compiler_ast::jsx::JSXElementName,
) -> Result<JsxTag, CompilerError> {
    use react_compiler_ast::jsx::JSXElementName;
    match name {
        JSXElementName::JSXIdentifier(id) => {
            let tag = &id.name;
            let loc = convert_opt_loc(&id.base.loc);
            let start = id.base.start.unwrap_or(0);
            if tag.starts_with(|c: char| c.is_ascii_uppercase()) {
                // Component tag: resolve as identifier and load
                let place = lower_identifier(builder, tag, start, loc.clone(), id.base.node_id)?;
                let load_value = if builder.is_context_identifier(tag, start, id.base.node_id) {
                    InstructionValue::LoadContext { place, loc }
                } else {
                    InstructionValue::LoadLocal { place, loc }
                };
                let temp = lower_value_to_temporary(builder, load_value)?;
                Ok(JsxTag::Place(temp))
            } else {
                // Builtin HTML tag
                Ok(JsxTag::Builtin(BuiltinTag {
                    name: tag.clone(),
                    loc,
                }))
            }
        }
        JSXElementName::JSXMemberExpression(member) => {
            let place = lower_jsx_member_expression(builder, member)?;
            Ok(JsxTag::Place(place))
        }
        JSXElementName::JSXNamespacedName(ns) => {
            let namespace = &ns.namespace.name;
            let name = &ns.name.name;
            let tag = format!("{}:{}", namespace, name);
            let loc = convert_opt_loc(&ns.base.loc);
            if namespace.contains(':') || name.contains(':') {
                builder.record_error(CompilerErrorDetail {
                    category: ErrorCategory::Syntax,
                    reason: "Expected JSXNamespacedName to have no colons in the namespace or name"
                        .to_string(),
                    description: Some(format!("Got `{}` : `{}`", namespace, name)),
                    loc: loc.clone(),
                    suggestions: None,
                })?;
            }
            let place = lower_value_to_temporary(
                builder,
                InstructionValue::Primitive {
                    value: PrimitiveValue::String(tag),
                    loc: loc.clone(),
                },
            )?;
            Ok(JsxTag::Place(place))
        }
    }
}

pub(crate) fn lower_jsx_member_expression(
    builder: &mut HirBuilder,
    expr: &react_compiler_ast::jsx::JSXMemberExpression,
) -> Result<Place, CompilerError> {
    use react_compiler_ast::jsx::JSXMemberExprObject;
    // Use the full member expression's loc for instruction locs (matching TS: exprPath.node.loc)
    let expr_loc = convert_opt_loc(&expr.base.loc);
    let object = match &*expr.object {
        JSXMemberExprObject::JSXIdentifier(id) => {
            let id_loc = convert_opt_loc(&id.base.loc);
            let start = id.base.start.unwrap_or(0);
            // Use identifier's own loc for the place, but member expression's loc for the instruction
            let place = lower_identifier(builder, &id.name, start, id_loc, id.base.node_id)?;
            let load_value = if builder.is_context_identifier(&id.name, start, id.base.node_id) {
                InstructionValue::LoadContext {
                    place,
                    loc: expr_loc.clone(),
                }
            } else {
                InstructionValue::LoadLocal {
                    place,
                    loc: expr_loc.clone(),
                }
            };
            lower_value_to_temporary(builder, load_value)?
        }
        JSXMemberExprObject::JSXMemberExpression(inner) => {
            lower_jsx_member_expression(builder, inner)?
        }
    };
    let prop_name = &expr.property.name;
    let value = InstructionValue::PropertyLoad {
        object,
        property: PropertyLiteral::String(prop_name.clone()),
        loc: expr_loc,
    };
    Ok(lower_value_to_temporary(builder, value)?)
}

pub(crate) fn lower_jsx_element(
    builder: &mut HirBuilder,
    child: &react_compiler_ast::jsx::JSXChild,
) -> Result<Option<Place>, CompilerError> {
    use react_compiler_ast::jsx::JSXChild;
    use react_compiler_ast::jsx::JSXExpressionContainerExpr;
    match child {
        JSXChild::JSXText(text) => {
            // FBT whitespace normalization differs from standard JSX.
            // Since the fbt transform runs after, preserve all whitespace
            // in FBT subtrees as is.
            let value = if builder.fbt_depth > 0 {
                Some(text.value.clone())
            } else {
                trim_jsx_text(&text.value)
            };
            match value {
                None => Ok(None),
                Some(value) => {
                    let loc = convert_opt_loc(&text.base.loc);
                    let place = lower_value_to_temporary(
                        builder,
                        InstructionValue::JSXText { value, loc },
                    )?;
                    Ok(Some(place))
                }
            }
        }
        JSXChild::JSXElement(element) => {
            let value = lower_expression(
                builder,
                &react_compiler_ast::expressions::Expression::JSXElement(element.clone()),
            )?;
            Ok(Some(lower_value_to_temporary(builder, value)?))
        }
        JSXChild::JSXFragment(fragment) => {
            let value = lower_expression(
                builder,
                &react_compiler_ast::expressions::Expression::JSXFragment(fragment.clone()),
            )?;
            Ok(Some(lower_value_to_temporary(builder, value)?))
        }
        JSXChild::JSXExpressionContainer(container) => match &container.expression {
            JSXExpressionContainerExpr::JSXEmptyExpression(_) => Ok(None),
            JSXExpressionContainerExpr::Expression(expr) => {
                Ok(Some(lower_expression_to_temporary(builder, expr)?))
            }
        },
        JSXChild::JSXSpreadChild(spread) => Ok(Some(lower_expression_to_temporary(
            builder,
            &spread.expression,
        )?)),
    }
}

/// Split a string on line endings, handling \r\n, \n, and \r.
pub(crate) fn split_line_endings(s: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' {
            lines.push(&s[start..i]);
            if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                i += 2;
            } else {
                i += 1;
            }
            start = i;
        } else if bytes[i] == b'\n' {
            lines.push(&s[start..i]);
            i += 1;
            start = i;
        } else {
            i += 1;
        }
    }
    lines.push(&s[start..]);
    lines
}

/// Trims whitespace according to the JSX spec.
/// Implementation ported from Babel's cleanJSXElementLiteralChild.
pub(crate) fn trim_jsx_text(original: &str) -> Option<String> {
    // Split on \r\n, \n, or \r to handle all line ending styles (matching TS split(/\r\n|\n|\r/))
    let lines: Vec<&str> = split_line_endings(original);

    // NOTE: when builder.fbt_depth > 0, the TS skips whitespace trimming entirely.
    // That check is handled by the caller (lower_jsx_element) before calling this function.

    let mut last_non_empty_line = 0;
    for (i, line) in lines.iter().enumerate() {
        if line.contains(|c: char| c != ' ' && c != '\t') {
            last_non_empty_line = i;
        }
    }

    let mut str = String::new();

    for (i, line) in lines.iter().enumerate() {
        let is_first_line = i == 0;
        let is_last_line = i == lines.len() - 1;
        let is_last_non_empty_line = i == last_non_empty_line;

        // Replace rendered whitespace tabs with spaces
        let mut trimmed_line = line.replace('\t', " ");

        // Trim whitespace touching a newline (leading whitespace on non-first lines)
        if !is_first_line {
            trimmed_line = trimmed_line.trim_start_matches(' ').to_string();
        }

        // Trim whitespace touching an endline (trailing whitespace on non-last lines)
        if !is_last_line {
            trimmed_line = trimmed_line.trim_end_matches(' ').to_string();
        }

        if !trimmed_line.is_empty() {
            if !is_last_non_empty_line {
                trimmed_line.push(' ');
            }
            str.push_str(&trimmed_line);
        }
    }

    if str.is_empty() { None } else { Some(str) }
}

/// Collect locations of fbt:enum, fbt:plural, fbt:pronoun sub-tags
/// within the children of an fbt/fbs JSX element.
pub(crate) fn collect_fbt_sub_tags(
    children: &[react_compiler_ast::jsx::JSXChild],
    tag_name: &str,
    enum_locs: &mut Vec<Option<SourceLocation>>,
    plural_locs: &mut Vec<Option<SourceLocation>>,
    pronoun_locs: &mut Vec<Option<SourceLocation>>,
) {
    use react_compiler_ast::jsx::JSXChild;
    for child in children {
        match child {
            JSXChild::JSXElement(el) => {
                collect_fbt_sub_tags_from_element(
                    el,
                    tag_name,
                    enum_locs,
                    plural_locs,
                    pronoun_locs,
                );
            }
            JSXChild::JSXFragment(frag) => {
                collect_fbt_sub_tags(
                    &frag.children,
                    tag_name,
                    enum_locs,
                    plural_locs,
                    pronoun_locs,
                );
            }
            JSXChild::JSXExpressionContainer(container) => {
                if let react_compiler_ast::jsx::JSXExpressionContainerExpr::Expression(expr) =
                    &container.expression
                {
                    collect_fbt_sub_tags_from_expr(
                        expr,
                        tag_name,
                        enum_locs,
                        plural_locs,
                        pronoun_locs,
                    );
                }
            }
            _ => {}
        }
    }
}

pub(crate) fn collect_fbt_sub_tags_from_element(
    el: &react_compiler_ast::jsx::JSXElement,
    tag_name: &str,
    enum_locs: &mut Vec<Option<SourceLocation>>,
    plural_locs: &mut Vec<Option<SourceLocation>>,
    pronoun_locs: &mut Vec<Option<SourceLocation>>,
) {
    use react_compiler_ast::jsx::JSXElementName;
    if let JSXElementName::JSXNamespacedName(ns) = &el.opening_element.name {
        if ns.namespace.name == tag_name {
            let loc = convert_opt_loc(&ns.base.loc);
            match ns.name.name.as_str() {
                "enum" => enum_locs.push(loc),
                "plural" => plural_locs.push(loc),
                "pronoun" => pronoun_locs.push(loc),
                _ => {}
            }
        }
    }
    collect_fbt_sub_tags(&el.children, tag_name, enum_locs, plural_locs, pronoun_locs);
    // Also traverse JSX attributes (matching TS expr.traverse which visits all nodes)
    for attr in &el.opening_element.attributes {
        if let react_compiler_ast::jsx::JSXAttributeItem::JSXAttribute(a) = attr {
            if let Some(val) = &a.value {
                if let react_compiler_ast::jsx::JSXAttributeValue::JSXExpressionContainer(
                    container,
                ) = val
                {
                    if let react_compiler_ast::jsx::JSXExpressionContainerExpr::Expression(expr) =
                        &container.expression
                    {
                        collect_fbt_sub_tags_from_expr(
                            expr,
                            tag_name,
                            enum_locs,
                            plural_locs,
                            pronoun_locs,
                        );
                    }
                } else if let react_compiler_ast::jsx::JSXAttributeValue::JSXElement(nested) = val {
                    collect_fbt_sub_tags_from_element(
                        nested,
                        tag_name,
                        enum_locs,
                        plural_locs,
                        pronoun_locs,
                    );
                }
            }
        }
    }
}

pub(crate) fn collect_fbt_sub_tags_from_expr(
    expr: &react_compiler_ast::expressions::Expression,
    tag_name: &str,
    enum_locs: &mut Vec<Option<SourceLocation>>,
    plural_locs: &mut Vec<Option<SourceLocation>>,
    pronoun_locs: &mut Vec<Option<SourceLocation>>,
) {
    use react_compiler_ast::expressions::Expression;
    match expr {
        Expression::JSXElement(el) => {
            collect_fbt_sub_tags_from_element(el, tag_name, enum_locs, plural_locs, pronoun_locs);
        }
        Expression::JSXFragment(frag) => {
            collect_fbt_sub_tags(
                &frag.children,
                tag_name,
                enum_locs,
                plural_locs,
                pronoun_locs,
            );
        }
        Expression::ConditionalExpression(cond) => {
            collect_fbt_sub_tags_from_expr(
                &cond.consequent,
                tag_name,
                enum_locs,
                plural_locs,
                pronoun_locs,
            );
            collect_fbt_sub_tags_from_expr(
                &cond.alternate,
                tag_name,
                enum_locs,
                plural_locs,
                pronoun_locs,
            );
        }
        Expression::LogicalExpression(log) => {
            collect_fbt_sub_tags_from_expr(
                &log.left,
                tag_name,
                enum_locs,
                plural_locs,
                pronoun_locs,
            );
            collect_fbt_sub_tags_from_expr(
                &log.right,
                tag_name,
                enum_locs,
                plural_locs,
                pronoun_locs,
            );
        }
        Expression::ParenthesizedExpression(paren) => {
            collect_fbt_sub_tags_from_expr(
                &paren.expression,
                tag_name,
                enum_locs,
                plural_locs,
                pronoun_locs,
            );
        }
        Expression::ArrowFunctionExpression(arrow) => match arrow.body.as_ref() {
            react_compiler_ast::expressions::ArrowFunctionBody::Expression(body_expr) => {
                collect_fbt_sub_tags_from_expr(
                    body_expr,
                    tag_name,
                    enum_locs,
                    plural_locs,
                    pronoun_locs,
                );
            }
            react_compiler_ast::expressions::ArrowFunctionBody::BlockStatement(block) => {
                collect_fbt_sub_tags_from_stmts(
                    &block.body,
                    tag_name,
                    enum_locs,
                    plural_locs,
                    pronoun_locs,
                );
            }
        },
        Expression::CallExpression(call) => {
            for arg in &call.arguments {
                collect_fbt_sub_tags_from_expr(arg, tag_name, enum_locs, plural_locs, pronoun_locs);
            }
        }
        _ => {}
    }
}

pub(crate) fn collect_fbt_sub_tags_from_stmts(
    stmts: &[react_compiler_ast::statements::Statement],
    tag_name: &str,
    enum_locs: &mut Vec<Option<SourceLocation>>,
    plural_locs: &mut Vec<Option<SourceLocation>>,
    pronoun_locs: &mut Vec<Option<SourceLocation>>,
) {
    for stmt in stmts {
        if let react_compiler_ast::statements::Statement::ReturnStatement(ret) = stmt {
            if let Some(arg) = &ret.argument {
                collect_fbt_sub_tags_from_expr(arg, tag_name, enum_locs, plural_locs, pronoun_locs);
            }
        } else if let react_compiler_ast::statements::Statement::ExpressionStatement(expr_stmt) =
            stmt
        {
            collect_fbt_sub_tags_from_expr(
                &expr_stmt.expression,
                tag_name,
                enum_locs,
                plural_locs,
                pronoun_locs,
            );
        }
    }
}
