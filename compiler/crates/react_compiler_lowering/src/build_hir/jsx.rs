//! JSX lowering — reads `oxc_ast` directly and produces HIR.
//!
//! Stage N1.2.7 transcribes JSX lowering from the pre-flip
//! `react_compiler_ast` reference (`git show 1a02788:…/build_hir/jsx.rs` plus
//! the `JSXElement`/`JSXFragment` arms of the reference `expressions.rs`) to
//! read `oxc_ast` enums and resolve scope/bindings via `semantic_queries`.
//!
//! The *algorithm* and the HIR it produces are unchanged from the reference;
//! only the AST access changes. In particular, oxc parses a component tag
//! (`<Foo>`) as `JSXElementName::IdentifierReference` (carrying a real
//! `reference_id`), and a host tag (`<div>`) as `JSXElementName::Identifier`
//! (a bare `JSXIdentifier` with no reference). This is the key classification
//! the reference performed by inspecting the first character of the name.

use oxc_ast::ast as oxc;
use react_compiler_diagnostics::CompilerDiagnostic;
use react_compiler_diagnostics::CompilerDiagnosticDetail;
use react_compiler_diagnostics::CompilerError;
use react_compiler_diagnostics::CompilerErrorDetail;
use react_compiler_diagnostics::ErrorCategory;
use react_compiler_hir::*;

use crate::hir_builder::HirBuilder;

use super::expressions::lower_expression_to_temporary;
use super::expressions::lower_identifier_value;
use super::lower_value_to_temporary;

// =============================================================================
// JSX element name (the tag)
// =============================================================================

/// Lower a JSX opening-element name into a [`JsxTag`].
///
/// - host tag (`<div>`): oxc parses as `JSXElementName::Identifier`; lowers to a
///   `JsxTag::Builtin` string.
/// - component tag (`<Foo>`): oxc parses as `JSXElementName::IdentifierReference`
///   (carries a `reference_id`); lowers to a `JsxTag::Place` via a LoadLocal /
///   LoadContext / LoadGlobal, mirroring identifier lowering.
/// - member tag (`<Foo.Bar>`): `JSXElementName::MemberExpression`.
/// - namespaced tag (`<svg:path>`): `JSXElementName::NamespacedName`.
pub(crate) fn lower_jsx_element_name(
    builder: &mut HirBuilder,
    name: &oxc::JSXElementName,
) -> Result<JsxTag, CompilerError> {
    match name {
        // Host/builtin tag: `<div>`, `<my-element>`.
        oxc::JSXElementName::Identifier(id) => {
            let loc = Some(builder.loc_of_span(id.span));
            Ok(JsxTag::Builtin(BuiltinTag {
                name: id.name.to_string(),
                loc,
            }))
        }
        // Component tag: `<Foo>`. oxc gives a real IdentifierReference, so reuse
        // identifier lowering (LoadLocal / LoadContext / LoadGlobal).
        oxc::JSXElementName::IdentifierReference(id) => {
            let load_value = lower_identifier_value(builder, id)?;
            let temp = lower_value_to_temporary(builder, load_value)?;
            Ok(JsxTag::Place(temp))
        }
        oxc::JSXElementName::MemberExpression(member) => {
            let place = lower_jsx_member_expression(builder, member)?;
            Ok(JsxTag::Place(place))
        }
        oxc::JSXElementName::NamespacedName(ns) => {
            let namespace = ns.namespace.name.as_str();
            let local = ns.name.name.as_str();
            let tag = format!("{}:{}", namespace, local);
            let loc = Some(builder.loc_of_span(ns.span));
            if namespace.contains(':') || local.contains(':') {
                builder.record_error(CompilerErrorDetail {
                    category: ErrorCategory::Syntax,
                    reason: "Expected JSXNamespacedName to have no colons in the namespace or name"
                        .to_string(),
                    description: Some(format!("Got `{}` : `{}`", namespace, local)),
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
        // `<this>` — rare. Bail gracefully to a placeholder.
        oxc::JSXElementName::ThisExpression(this_expr) => {
            let loc = Some(builder.loc_of_span(this_expr.span));
            builder.record_diagnostic(CompilerDiagnostic::todo(
                "(BuildHIR::lowerJsxElementName) Handle ThisExpression tags",
                loc.clone(),
            ));
            let place = lower_value_to_temporary(
                builder,
                InstructionValue::Primitive {
                    value: PrimitiveValue::Undefined,
                    loc,
                },
            )?;
            Ok(JsxTag::Place(place))
        }
    }
}

/// Lower a JSX member-expression tag (`<Foo.Bar>` / `<Foo.Bar.Baz>`).
///
/// Mirrors the reference: the full member expression's loc is used for the
/// instruction locs (matching TS `exprPath.node.loc`), while the leaf
/// identifier's own loc is used for its place.
pub(crate) fn lower_jsx_member_expression(
    builder: &mut HirBuilder,
    expr: &oxc::JSXMemberExpression,
) -> Result<Place, CompilerError> {
    let expr_loc = Some(builder.loc_of_span(expr.span));
    let object = match &expr.object {
        oxc::JSXMemberExpressionObject::IdentifierReference(id) => {
            // Reuse identifier lowering to resolve the binding, but emit the
            // load with the member expression's loc (matching the reference).
            let load_value = lower_identifier_value(builder, id)?;
            let load_value = match load_value {
                InstructionValue::LoadLocal { place, .. } => InstructionValue::LoadLocal {
                    place,
                    loc: expr_loc.clone(),
                },
                InstructionValue::LoadContext { place, .. } => InstructionValue::LoadContext {
                    place,
                    loc: expr_loc.clone(),
                },
                InstructionValue::LoadGlobal { binding, .. } => InstructionValue::LoadGlobal {
                    binding,
                    loc: expr_loc.clone(),
                },
                other => other,
            };
            lower_value_to_temporary(builder, load_value)?
        }
        oxc::JSXMemberExpressionObject::MemberExpression(inner) => {
            lower_jsx_member_expression(builder, inner)?
        }
        oxc::JSXMemberExpressionObject::ThisExpression(this_expr) => {
            let loc = Some(builder.loc_of_span(this_expr.span));
            builder.record_diagnostic(CompilerDiagnostic::todo(
                "(BuildHIR::lowerJsxMemberExpression) Handle ThisExpression objects",
                loc.clone(),
            ));
            lower_value_to_temporary(
                builder,
                InstructionValue::Primitive {
                    value: PrimitiveValue::Undefined,
                    loc,
                },
            )?
        }
    };
    let prop_name = expr.property.name.to_string();
    let value = InstructionValue::PropertyLoad {
        object,
        property: PropertyLiteral::String(prop_name),
        loc: expr_loc,
    };
    lower_value_to_temporary(builder, value)
}

// =============================================================================
// JSX children
// =============================================================================

/// Lower a single JSX child into an optional place. Returns `None` for children
/// that produce no value (whitespace-only text, empty expression containers).
pub(crate) fn lower_jsx_child(
    builder: &mut HirBuilder,
    child: &oxc::JSXChild,
) -> Result<Option<Place>, CompilerError> {
    match child {
        oxc::JSXChild::Text(text) => {
            // FBT whitespace normalization differs from standard JSX.
            // Since the fbt transform runs after, preserve all whitespace
            // in FBT subtrees as is.
            let value = if builder.fbt_depth > 0 {
                Some(text.value.to_string())
            } else {
                trim_jsx_text(text.value.as_str())
            };
            match value {
                None => Ok(None),
                Some(value) => {
                    let loc = Some(builder.loc_of_span(text.span));
                    let place = lower_value_to_temporary(
                        builder,
                        InstructionValue::JSXText { value, loc },
                    )?;
                    Ok(Some(place))
                }
            }
        }
        oxc::JSXChild::Element(element) => {
            let value = lower_jsx_element_value(builder, element)?;
            Ok(Some(lower_value_to_temporary(builder, value)?))
        }
        oxc::JSXChild::Fragment(fragment) => {
            let value = lower_jsx_fragment_value(builder, fragment)?;
            Ok(Some(lower_value_to_temporary(builder, value)?))
        }
        oxc::JSXChild::ExpressionContainer(container) => match &container.expression {
            oxc::JSXExpression::EmptyExpression(_) => Ok(None),
            expr => {
                // `JSXExpression` inherits the `Expression` variants; the
                // non-empty arms are all real expressions.
                let inner = expr
                    .as_expression()
                    .expect("non-empty JSXExpression is an Expression");
                Ok(Some(lower_expression_to_temporary(builder, inner)?))
            }
        },
        oxc::JSXChild::Spread(spread) => Ok(Some(lower_expression_to_temporary(
            builder,
            &spread.expression,
        )?)),
    }
}

// =============================================================================
// JSXElement / JSXFragment value lowering (the dispatch entry points)
// =============================================================================

/// Lower a `JSXElement` node into a `JsxExpression` InstructionValue.
pub(crate) fn lower_jsx_element_value(
    builder: &mut HirBuilder,
    jsx_element: &oxc::JSXElement,
) -> Result<InstructionValue, CompilerError> {
    let loc = Some(builder.loc_of_span(jsx_element.span));
    let opening_loc = Some(builder.loc_of_span(jsx_element.opening_element.span));
    let closing_loc = jsx_element
        .closing_element
        .as_ref()
        .map(|c| builder.loc_of_span(c.span));

    // Lower the tag name.
    let tag = lower_jsx_element_name(builder, &jsx_element.opening_element.name)?;

    // Lower attributes (props).
    let mut props: Vec<JsxAttribute> = Vec::new();
    for attr_item in &jsx_element.opening_element.attributes {
        match attr_item {
            oxc::JSXAttributeItem::SpreadAttribute(spread) => {
                let argument = lower_expression_to_temporary(builder, &spread.argument)?;
                props.push(JsxAttribute::SpreadAttribute { argument });
            }
            oxc::JSXAttributeItem::Attribute(attr) => {
                // Get the attribute name.
                let prop_name = match &attr.name {
                    oxc::JSXAttributeName::Identifier(id) => {
                        let name = id.name.as_str();
                        if name.contains(':') {
                            builder.record_error(CompilerErrorDetail {
                                category: ErrorCategory::Todo,
                                reason: format!(
                                    "(BuildHIR::lowerExpression) Unexpected colon in attribute name `{}`",
                                    name
                                ),
                                description: None,
                                loc: Some(builder.loc_of_span(id.span)),
                                suggestions: None,
                            })?;
                        }
                        name.to_string()
                    }
                    oxc::JSXAttributeName::NamespacedName(ns) => {
                        format!("{}:{}", ns.namespace.name, ns.name.name)
                    }
                };

                // Get the attribute value.
                let value = match &attr.value {
                    Some(oxc::JSXAttributeValue::StringLiteral(s)) => {
                        let str_loc = Some(builder.loc_of_span(s.span));
                        lower_value_to_temporary(
                            builder,
                            InstructionValue::Primitive {
                                value: PrimitiveValue::String(s.value.to_string()),
                                loc: str_loc,
                            },
                        )?
                    }
                    Some(oxc::JSXAttributeValue::ExpressionContainer(container)) => {
                        match &container.expression {
                            oxc::JSXExpression::EmptyExpression(_) => {
                                // Empty expression container - skip this attribute.
                                continue;
                            }
                            expr => {
                                let inner = expr
                                    .as_expression()
                                    .expect("non-empty JSXExpression is an Expression");
                                lower_expression_to_temporary(builder, inner)?
                            }
                        }
                    }
                    Some(oxc::JSXAttributeValue::Element(el)) => {
                        let val = lower_jsx_element_value(builder, el)?;
                        lower_value_to_temporary(builder, val)?
                    }
                    Some(oxc::JSXAttributeValue::Fragment(frag)) => {
                        let val = lower_jsx_fragment_value(builder, frag)?;
                        lower_value_to_temporary(builder, val)?
                    }
                    None => {
                        // No value means boolean true (e.g., <div disabled />).
                        let attr_loc = Some(builder.loc_of_span(attr.span));
                        lower_value_to_temporary(
                            builder,
                            InstructionValue::Primitive {
                                value: PrimitiveValue::Boolean(true),
                                loc: attr_loc,
                            },
                        )?
                    }
                };

                props.push(JsxAttribute::Attribute {
                    name: prop_name,
                    place: value,
                });
            }
        }
    }

    // Check if this is an fbt/fbs tag, which requires special whitespace handling.
    let is_fbt = matches!(&tag, JsxTag::Builtin(b) if b.name == "fbt" || b.name == "fbs");

    // Check that fbt/fbs tags are module-level imports, not local bindings.
    // Matches TS: CompilerError.invariant(tagIdentifier.kind !== 'Identifier', ...).
    if is_fbt {
        let tag_name = match &tag {
            JsxTag::Builtin(b) => b.name.clone(),
            _ => "fbt".to_string(),
        };
        // An fbt host tag is parsed as a host `Identifier` in oxc (lowercase).
        // Check whether the name resolves to a local binding in this function.
        if let oxc::JSXElementName::Identifier(jsx_id) = &jsx_element.opening_element.name {
            let id_loc = Some(builder.loc_of_span(jsx_id.span));
            let is_local_binding = builder.has_local_binding(jsx_id.name.as_str());
            if is_local_binding {
                let reason = format!("<{}> tags should be module-level imports", tag_name);
                return Err(CompilerDiagnostic::new(ErrorCategory::Invariant, &reason, None)
                    .with_detail(CompilerDiagnosticDetail::Error {
                        loc: id_loc,
                        message: Some(reason.clone()),
                        identifier_name: None,
                    })
                    .into());
            }
        }
    }

    // Check for duplicate fbt:enum, fbt:plural, fbt:pronoun tags.
    if is_fbt {
        let tag_name = match &tag {
            JsxTag::Builtin(b) => b.name.as_str(),
            _ => "fbt",
        };
        let mut enum_locs: Vec<Option<SourceLocation>> = Vec::new();
        let mut plural_locs: Vec<Option<SourceLocation>> = Vec::new();
        let mut pronoun_locs: Vec<Option<SourceLocation>> = Vec::new();
        collect_fbt_sub_tags(
            builder,
            &jsx_element.children,
            tag_name,
            &mut enum_locs,
            &mut plural_locs,
            &mut pronoun_locs,
        );

        for (name, locations) in [
            ("enum", &enum_locs),
            ("plural", &plural_locs),
            ("pronoun", &pronoun_locs),
        ] {
            if locations.len() > 1 {
                let details: Vec<CompilerDiagnosticDetail> = locations
                    .iter()
                    .map(|loc| CompilerDiagnosticDetail::Error {
                        message: Some(format!("Multiple `<{}:{}>` tags found", tag_name, name)),
                        loc: loc.clone(),
                        identifier_name: None,
                    })
                    .collect();
                let mut diag = CompilerDiagnostic::new(
                    ErrorCategory::Todo,
                    "Support duplicate fbt tags",
                    Some(format!(
                        "Support `<{}>` tags with multiple `<{}:{}>` values",
                        tag_name, tag_name, name
                    )),
                );
                diag.details = details;
                builder.environment_mut().record_diagnostic(diag);
            }
        }
    }

    // Increment fbt counter before traversing into children, as whitespace
    // in jsx text is handled differently for fbt subtrees.
    if is_fbt {
        builder.fbt_depth += 1;
    }

    // Lower children.
    let children: Vec<Place> = jsx_element
        .children
        .iter()
        .map(|child| lower_jsx_child(builder, child))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect();

    if is_fbt {
        builder.fbt_depth -= 1;
    }

    Ok(InstructionValue::JsxExpression {
        tag,
        props,
        children: if children.is_empty() {
            None
        } else {
            Some(children)
        },
        loc,
        opening_loc,
        closing_loc,
    })
}

/// Lower a `JSXFragment` node into a `JsxFragment` InstructionValue.
pub(crate) fn lower_jsx_fragment_value(
    builder: &mut HirBuilder,
    jsx_fragment: &oxc::JSXFragment,
) -> Result<InstructionValue, CompilerError> {
    let loc = Some(builder.loc_of_span(jsx_fragment.span));

    let children: Vec<Place> = jsx_fragment
        .children
        .iter()
        .map(|child| lower_jsx_child(builder, child))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect();

    Ok(InstructionValue::JsxFragment { children, loc })
}

// =============================================================================
// JSX text trimming (Babel's cleanJSXElementLiteralChild algorithm)
// =============================================================================

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
    // Split on \r\n, \n, or \r to handle all line ending styles (matching TS
    // split(/\r\n|\n|\r/)).
    let lines: Vec<&str> = split_line_endings(original);

    // NOTE: when builder.fbt_depth > 0, the TS skips whitespace trimming
    // entirely. That check is handled by the caller (lower_jsx_child) before
    // calling this function.

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

        // Replace rendered whitespace tabs with spaces.
        let mut trimmed_line = line.replace('\t', " ");

        // Trim whitespace touching a newline (leading whitespace on non-first
        // lines).
        if !is_first_line {
            trimmed_line = trimmed_line.trim_start_matches(' ').to_string();
        }

        // Trim whitespace touching an endline (trailing whitespace on non-last
        // lines).
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

// =============================================================================
// fbt sub-tag collectors
// =============================================================================

/// Collect locations of fbt:enum, fbt:plural, fbt:pronoun sub-tags
/// within the children of an fbt/fbs JSX element.
pub(crate) fn collect_fbt_sub_tags(
    builder: &HirBuilder,
    children: &[oxc::JSXChild],
    tag_name: &str,
    enum_locs: &mut Vec<Option<SourceLocation>>,
    plural_locs: &mut Vec<Option<SourceLocation>>,
    pronoun_locs: &mut Vec<Option<SourceLocation>>,
) {
    for child in children {
        match child {
            oxc::JSXChild::Element(el) => {
                collect_fbt_sub_tags_from_element(
                    builder,
                    el,
                    tag_name,
                    enum_locs,
                    plural_locs,
                    pronoun_locs,
                );
            }
            oxc::JSXChild::Fragment(frag) => {
                collect_fbt_sub_tags(
                    builder,
                    &frag.children,
                    tag_name,
                    enum_locs,
                    plural_locs,
                    pronoun_locs,
                );
            }
            oxc::JSXChild::ExpressionContainer(container) => {
                if let Some(expr) = container.expression.as_expression() {
                    collect_fbt_sub_tags_from_expr(
                        builder,
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
    builder: &HirBuilder,
    el: &oxc::JSXElement,
    tag_name: &str,
    enum_locs: &mut Vec<Option<SourceLocation>>,
    plural_locs: &mut Vec<Option<SourceLocation>>,
    pronoun_locs: &mut Vec<Option<SourceLocation>>,
) {
    if let oxc::JSXElementName::NamespacedName(ns) = &el.opening_element.name {
        if ns.namespace.name == tag_name {
            let loc = Some(builder.loc_of_span(ns.span));
            match ns.name.name.as_str() {
                "enum" => enum_locs.push(loc),
                "plural" => plural_locs.push(loc),
                "pronoun" => pronoun_locs.push(loc),
                _ => {}
            }
        }
    }
    collect_fbt_sub_tags(
        builder,
        &el.children,
        tag_name,
        enum_locs,
        plural_locs,
        pronoun_locs,
    );
    // Also traverse JSX attributes (matching TS expr.traverse which visits all
    // nodes).
    for attr in &el.opening_element.attributes {
        if let oxc::JSXAttributeItem::Attribute(a) = attr {
            match &a.value {
                Some(oxc::JSXAttributeValue::ExpressionContainer(container)) => {
                    if let Some(expr) = container.expression.as_expression() {
                        collect_fbt_sub_tags_from_expr(
                            builder,
                            expr,
                            tag_name,
                            enum_locs,
                            plural_locs,
                            pronoun_locs,
                        );
                    }
                }
                Some(oxc::JSXAttributeValue::Element(nested)) => {
                    collect_fbt_sub_tags_from_element(
                        builder,
                        nested,
                        tag_name,
                        enum_locs,
                        plural_locs,
                        pronoun_locs,
                    );
                }
                _ => {}
            }
        }
    }
}

pub(crate) fn collect_fbt_sub_tags_from_expr(
    builder: &HirBuilder,
    expr: &oxc::Expression,
    tag_name: &str,
    enum_locs: &mut Vec<Option<SourceLocation>>,
    plural_locs: &mut Vec<Option<SourceLocation>>,
    pronoun_locs: &mut Vec<Option<SourceLocation>>,
) {
    match expr {
        oxc::Expression::JSXElement(el) => {
            collect_fbt_sub_tags_from_element(
                builder,
                el,
                tag_name,
                enum_locs,
                plural_locs,
                pronoun_locs,
            );
        }
        oxc::Expression::JSXFragment(frag) => {
            collect_fbt_sub_tags(
                builder,
                &frag.children,
                tag_name,
                enum_locs,
                plural_locs,
                pronoun_locs,
            );
        }
        oxc::Expression::ConditionalExpression(cond) => {
            collect_fbt_sub_tags_from_expr(
                builder,
                &cond.consequent,
                tag_name,
                enum_locs,
                plural_locs,
                pronoun_locs,
            );
            collect_fbt_sub_tags_from_expr(
                builder,
                &cond.alternate,
                tag_name,
                enum_locs,
                plural_locs,
                pronoun_locs,
            );
        }
        oxc::Expression::LogicalExpression(log) => {
            collect_fbt_sub_tags_from_expr(
                builder,
                &log.left,
                tag_name,
                enum_locs,
                plural_locs,
                pronoun_locs,
            );
            collect_fbt_sub_tags_from_expr(
                builder,
                &log.right,
                tag_name,
                enum_locs,
                plural_locs,
                pronoun_locs,
            );
        }
        oxc::Expression::ParenthesizedExpression(paren) => {
            collect_fbt_sub_tags_from_expr(
                builder,
                &paren.expression,
                tag_name,
                enum_locs,
                plural_locs,
                pronoun_locs,
            );
        }
        oxc::Expression::ArrowFunctionExpression(arrow) => {
            // oxc represents `() => expr` as a block body holding a single
            // ExpressionStatement when `expression == true`.
            collect_fbt_sub_tags_from_stmts(
                builder,
                &arrow.body.statements,
                tag_name,
                enum_locs,
                plural_locs,
                pronoun_locs,
            );
        }
        oxc::Expression::CallExpression(call) => {
            for arg in &call.arguments {
                if let Some(arg_expr) = arg.as_expression() {
                    collect_fbt_sub_tags_from_expr(
                        builder,
                        arg_expr,
                        tag_name,
                        enum_locs,
                        plural_locs,
                        pronoun_locs,
                    );
                }
            }
        }
        _ => {}
    }
}

pub(crate) fn collect_fbt_sub_tags_from_stmts(
    builder: &HirBuilder,
    stmts: &[oxc::Statement],
    tag_name: &str,
    enum_locs: &mut Vec<Option<SourceLocation>>,
    plural_locs: &mut Vec<Option<SourceLocation>>,
    pronoun_locs: &mut Vec<Option<SourceLocation>>,
) {
    for stmt in stmts {
        match stmt {
            oxc::Statement::ReturnStatement(ret) => {
                if let Some(arg) = &ret.argument {
                    collect_fbt_sub_tags_from_expr(
                        builder,
                        arg,
                        tag_name,
                        enum_locs,
                        plural_locs,
                        pronoun_locs,
                    );
                }
            }
            oxc::Statement::ExpressionStatement(expr_stmt) => {
                collect_fbt_sub_tags_from_expr(
                    builder,
                    &expr_stmt.expression,
                    tag_name,
                    enum_locs,
                    plural_locs,
                    pronoun_locs,
                );
            }
            _ => {}
        }
    }
}
