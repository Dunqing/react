//! Destructuring / pattern / lvalue lowering (transcribed from the reference
//! `react_compiler_lowering/src/build_hir/patterns.rs`, reading `oxc_ast`
//! directly and resolving scope via `semantic_queries`).
//!
//! oxc separates the *binding* pattern family (`BindingPattern`: declarations,
//! formal params, for-of/in declaration heads, catch params) from the
//! *assignment-target* family (`AssignmentTarget`: destructuring assignment
//! expressions `({a} = obj)` / `[x] = arr`, and for-of/in reassignment heads).
//! Both families build the same HIR `Destructure` instruction, so this module
//! provides two parallel entry points — `lower_assignment` (bindings) and
//! `lower_assignment_target` (assignment targets) — that share the identifier
//! resolution and emit identical HIR.

use oxc_ast::ast as oxc;
use oxc_span::GetSpan;
use react_compiler_diagnostics::CompilerError;
use react_compiler_diagnostics::CompilerErrorDetail;
use react_compiler_diagnostics::ErrorCategory;
use react_compiler_hir::*;

use super::build_temporary_place;
use super::expressions::lower_expression_to_temporary;
use super::expressions::lower_object_property_key;
use super::lower_value_to_temporary;
use super::promote_temporary;
use crate::hir_builder::HirBuilder;
use crate::hir_builder::is_always_reserved_word;
use crate::hir_builder::reserved_identifier_diagnostic;
use crate::hir_builder::todo_diagnostic;
use crate::semantic_queries as sq;

/// The style of assignment (mirrors the reference `lowerAssignment`'s
/// `assignmentKind` parameter), used by [`lower_assignment`] to decide whether a
/// context-variable element may be destructured directly into place.
///
/// In oxc, destructuring *assignment expressions* (`({a} = obj)` / `[x] = arr`)
/// live in a separate AST family (`AssignmentTarget`) handled by
/// [`lower_assignment_target`]. The binding-pattern [`lower_assignment`] path is
/// reached with [`AssignmentStyle::Destructure`] for object/array *declaration*
/// targets (`const {a} = …` / `let [x] = …`, mirroring the reference's
/// `id.isObjectPattern() || id.isArrayPattern() ? 'Destructure' : 'Assignment'`)
/// and with [`AssignmentStyle::Assignment`] for everything else (single
/// identifier targets, params, for-of/in heads, catch). Under `Destructure`, a
/// context variable is not assigned directly: it is routed through a promoted
/// temporary and stored via a `StoreContext` followup.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AssignmentStyle {
    /// Assignment via `=` (single-identifier declarations, params,
    /// for-of/in heads, catch).
    Assignment,
    /// Destructuring of an object/array declaration target (`const {a} = …`).
    Destructure,
}

/// Result of resolving an identifier for assignment.
pub(crate) enum IdentifierForAssignment {
    /// A local place (identifier binding).
    Place(Place),
    /// A global variable (non-local, non-import).
    Global { name: String },
}

/// Resolve an identifier for use as an assignment target. Returns `None` if the
/// binding could not be found (error recorded).
pub(crate) fn lower_identifier_for_assignment(
    builder: &mut HirBuilder,
    loc: Option<SourceLocation>,
    ident_loc: Option<SourceLocation>,
    kind: InstructionKind,
    name: &str,
    symbol_id: Option<oxc_syntax::symbol::SymbolId>,
) -> Result<Option<IdentifierForAssignment>, CompilerError> {
    let binding = builder.resolve_identifier_symbol(name, symbol_id, ident_loc)?;
    match binding {
        VariableBinding::Identifier {
            identifier,
            binding_kind,
            ..
        } => {
            // Set the identifier's loc from the declaration site (not for
            // reassignments, which keep the original declaration loc).
            if kind != InstructionKind::Reassign {
                builder.set_identifier_declaration_loc(identifier, &ident_loc);
            }
            if binding_kind == BindingKind::Const && kind == InstructionKind::Reassign {
                builder.record_error(CompilerErrorDetail {
                    reason: "Cannot reassign a `const` variable".to_string(),
                    category: ErrorCategory::Syntax,
                    loc,
                    description: Some(format!("`{}` is declared as const", name)),
                    suggestions: None,
                })?;
                return Ok(None);
            }
            Ok(Some(IdentifierForAssignment::Place(Place {
                identifier,
                effect: Effect::Unknown,
                reactive: false,
                loc,
            })))
        }
        VariableBinding::Global { name: gname } => {
            if kind == InstructionKind::Reassign {
                Ok(Some(IdentifierForAssignment::Global { name: gname }))
            } else {
                builder.record_error(CompilerErrorDetail {
                    reason: "Could not find binding for declaration".to_string(),
                    category: ErrorCategory::Invariant,
                    loc,
                    description: None,
                    suggestions: None,
                })?;
                Ok(None)
            }
        }
        _ => {
            // Import bindings can't be assigned to.
            if kind == InstructionKind::Reassign {
                Ok(Some(IdentifierForAssignment::Global {
                    name: name.to_string(),
                }))
            } else {
                builder.record_error(CompilerErrorDetail {
                    reason: "Could not find binding for declaration".to_string(),
                    category: ErrorCategory::Invariant,
                    loc,
                    description: None,
                    suggestions: None,
                })?;
                Ok(None)
            }
        }
    }
}

// =============================================================================
// Binding-pattern family (declarations / params / for-of decl / catch)
// =============================================================================

/// Lower an assignment of `value` into a binding pattern `target`.
/// Returns the temporary holding the stored / destructured value, if any.
pub(crate) fn lower_assignment(
    builder: &mut HirBuilder,
    loc: Option<SourceLocation>,
    kind: InstructionKind,
    target: &oxc::BindingPattern,
    value: Place,
    style: AssignmentStyle,
) -> Result<Option<Place>, CompilerError> {
    match target {
        oxc::BindingPattern::BindingIdentifier(id) => {
            lower_binding_identifier(builder, loc, kind, id, value)
        }
        oxc::BindingPattern::ArrayPattern(pattern) => {
            lower_array_binding(builder, loc, kind, pattern, value, style)
        }
        oxc::BindingPattern::ObjectPattern(pattern) => {
            lower_object_binding(builder, loc, kind, pattern, value, style)
        }
        oxc::BindingPattern::AssignmentPattern(pattern) => {
            // Default value: if value === undefined use the default, else value.
            let pat_loc = Some(builder.loc_of_span(pattern.span));
            let resolved = lower_default(builder, pat_loc, &pattern.right, value)?;
            lower_assignment(builder, pat_loc, kind, &pattern.left, resolved, style)
        }
    }
}

/// Store `value` into a single identifier binding (mirrors the identifier arm of
/// the reference `lower_assignment`).
fn lower_binding_identifier(
    builder: &mut HirBuilder,
    loc: Option<SourceLocation>,
    kind: InstructionKind,
    id: &oxc::BindingIdentifier,
    value: Place,
) -> Result<Option<Place>, CompilerError> {
    if is_always_reserved_word(&id.name) {
        return Err(CompilerError::from(reserved_identifier_diagnostic(
            &id.name,
        )));
    }
    let id_loc = Some(builder.loc_of_span(id.span));
    let symbol_id = id.symbol_id.get();
    let result = lower_identifier_for_assignment(builder, loc, id_loc, kind, &id.name, symbol_id)?;
    match result {
        None => Ok(None),
        Some(IdentifierForAssignment::Global { name }) => {
            let temp = lower_value_to_temporary(
                builder,
                InstructionValue::StoreGlobal { name, value, loc },
            )?;
            Ok(Some(temp))
        }
        Some(IdentifierForAssignment::Place(place)) => {
            if builder.is_context_symbol(symbol_id) {
                let temp = lower_value_to_temporary(
                    builder,
                    InstructionValue::StoreContext {
                        lvalue: LValue { place, kind },
                        value,
                        loc,
                    },
                )?;
                Ok(Some(temp))
            } else {
                let temp = lower_value_to_temporary(
                    builder,
                    InstructionValue::StoreLocal {
                        lvalue: LValue { place, kind },
                        value,
                        type_annotation: None,
                        loc,
                    },
                )?;
                Ok(Some(temp))
            }
        }
    }
}

/// Whether an array element should resolve directly (vs via a promoted
/// temporary + followup). Direct is allowed unless the binding is a context
/// variable in a destructuring assignment, or `force_temporaries` is set.
fn binding_can_use_direct(
    builder: &HirBuilder,
    symbol_id: Option<oxc_syntax::symbol::SymbolId>,
    style: AssignmentStyle,
    force_temporaries: bool,
) -> bool {
    let is_context = builder.is_context_symbol(symbol_id);
    !force_temporaries && (style == AssignmentStyle::Assignment || !is_context)
}

fn lower_array_binding(
    builder: &mut HirBuilder,
    loc: Option<SourceLocation>,
    kind: InstructionKind,
    pattern: &oxc::ArrayPattern,
    value: Place,
    style: AssignmentStyle,
) -> Result<Option<Place>, CompilerError> {
    let mut items: Vec<ArrayPatternElement> = Vec::new();
    let mut followups: Vec<(Place, &oxc::BindingPattern)> = Vec::new();

    let force_temporaries = binding_force_temporaries(builder, kind, pattern)?;

    for element in &pattern.elements {
        match element {
            None => items.push(ArrayPatternElement::Hole),
            Some(oxc::BindingPattern::BindingIdentifier(id)) => {
                let symbol_id = id.symbol_id.get();
                let id_loc = Some(builder.loc_of_span(id.span));
                if binding_can_use_direct(builder, symbol_id, style, force_temporaries) {
                    match lower_identifier_for_assignment(
                        builder, id_loc, id_loc, kind, &id.name, symbol_id,
                    )? {
                        Some(IdentifierForAssignment::Place(place)) => {
                            items.push(ArrayPatternElement::Place(place));
                        }
                        Some(IdentifierForAssignment::Global { .. }) => {
                            let temp = build_temporary_place(builder, id_loc_span(builder, id));
                            promote_temporary(builder, temp.identifier);
                            items.push(ArrayPatternElement::Place(temp.clone()));
                            followups.push((temp, element.as_ref().unwrap()));
                        }
                        None => items.push(ArrayPatternElement::Hole),
                    }
                } else {
                    let temp = build_temporary_place(builder, id_loc_span(builder, id));
                    promote_temporary(builder, temp.identifier);
                    items.push(ArrayPatternElement::Place(temp.clone()));
                    followups.push((temp, element.as_ref().unwrap()));
                }
            }
            Some(other) => {
                let elem_loc = Some(builder.loc_of_span(other.span()));
                let temp = build_temporary_place(builder, elem_loc);
                promote_temporary(builder, temp.identifier);
                items.push(ArrayPatternElement::Place(temp.clone()));
                followups.push((temp, other));
            }
        }
    }

    // Rest element (`...rest`) is a separate field on the oxc ArrayPattern.
    if let Some(rest) = &pattern.rest {
        let rest_loc = Some(builder.loc_of_span(rest.span));
        match &rest.argument {
            oxc::BindingPattern::BindingIdentifier(id) => {
                let symbol_id = id.symbol_id.get();
                let id_loc = Some(builder.loc_of_span(id.span));
                if binding_can_use_direct(builder, symbol_id, style, force_temporaries) {
                    match lower_identifier_for_assignment(
                        builder, rest_loc, id_loc, kind, &id.name, symbol_id,
                    )? {
                        Some(IdentifierForAssignment::Place(place)) => {
                            items.push(ArrayPatternElement::Spread(SpreadPattern { place }));
                        }
                        Some(IdentifierForAssignment::Global { .. }) => {
                            let temp = build_temporary_place(builder, rest_loc);
                            promote_temporary(builder, temp.identifier);
                            items.push(ArrayPatternElement::Spread(SpreadPattern {
                                place: temp.clone(),
                            }));
                            followups.push((temp, &rest.argument));
                        }
                        None => {}
                    }
                } else {
                    let temp = build_temporary_place(builder, rest_loc);
                    promote_temporary(builder, temp.identifier);
                    items.push(ArrayPatternElement::Spread(SpreadPattern {
                        place: temp.clone(),
                    }));
                    followups.push((temp, &rest.argument));
                }
            }
            other => {
                let temp = build_temporary_place(builder, rest_loc);
                promote_temporary(builder, temp.identifier);
                items.push(ArrayPatternElement::Spread(SpreadPattern {
                    place: temp.clone(),
                }));
                followups.push((temp, other));
            }
        }
    }

    let pat_loc = Some(builder.loc_of_span(pattern.span));
    let temporary = lower_value_to_temporary(
        builder,
        InstructionValue::Destructure {
            lvalue: LValuePattern {
                pattern: Pattern::Array(ArrayPattern {
                    items,
                    loc: pat_loc,
                }),
                kind,
            },
            value: value.clone(),
            loc,
        },
    )?;

    for (place, path) in followups {
        let followup_loc = Some(builder.loc_of_span(path.span())).or(loc);
        lower_assignment(builder, followup_loc, kind, path, place, style)?;
    }
    Ok(Some(temporary))
}

fn lower_object_binding(
    builder: &mut HirBuilder,
    loc: Option<SourceLocation>,
    kind: InstructionKind,
    pattern: &oxc::ObjectPattern,
    value: Place,
    style: AssignmentStyle,
) -> Result<Option<Place>, CompilerError> {
    let mut properties: Vec<ObjectPropertyOrSpread> = Vec::new();
    let mut followups: Vec<(Place, &oxc::BindingPattern)> = Vec::new();

    let force_temporaries = binding_object_force_temporaries(builder, kind, pattern)?;

    for prop in &pattern.properties {
        let key = match lower_object_property_key(builder, &prop.key, prop.computed)? {
            Some(k) => k,
            None => continue,
        };
        match &prop.value {
            oxc::BindingPattern::BindingIdentifier(id) => {
                let symbol_id = id.symbol_id.get();
                let id_loc = Some(builder.loc_of_span(id.span));
                if binding_can_use_direct(builder, symbol_id, style, force_temporaries) {
                    match lower_identifier_for_assignment(
                        builder, id_loc, id_loc, kind, &id.name, symbol_id,
                    )? {
                        Some(IdentifierForAssignment::Place(place)) => {
                            properties.push(ObjectPropertyOrSpread::Property(ObjectProperty {
                                key,
                                property_type: ObjectPropertyType::Property,
                                place,
                            }));
                        }
                        Some(IdentifierForAssignment::Global { .. }) => {
                            let temp = build_temporary_place(builder, id_loc_span(builder, id));
                            promote_temporary(builder, temp.identifier);
                            properties.push(ObjectPropertyOrSpread::Property(ObjectProperty {
                                key,
                                property_type: ObjectPropertyType::Property,
                                place: temp.clone(),
                            }));
                            followups.push((temp, &prop.value));
                        }
                        None => continue,
                    }
                } else {
                    let temp = build_temporary_place(builder, id_loc_span(builder, id));
                    promote_temporary(builder, temp.identifier);
                    properties.push(ObjectPropertyOrSpread::Property(ObjectProperty {
                        key,
                        property_type: ObjectPropertyType::Property,
                        place: temp.clone(),
                    }));
                    followups.push((temp, &prop.value));
                }
            }
            other => {
                let elem_loc = Some(builder.loc_of_span(other.span()));
                let temp = build_temporary_place(builder, elem_loc);
                promote_temporary(builder, temp.identifier);
                properties.push(ObjectPropertyOrSpread::Property(ObjectProperty {
                    key,
                    property_type: ObjectPropertyType::Property,
                    place: temp.clone(),
                }));
                followups.push((temp, other));
            }
        }
    }

    // Object rest (`...rest`).
    if let Some(rest) = &pattern.rest {
        match &rest.argument {
            oxc::BindingPattern::BindingIdentifier(id) => {
                let symbol_id = id.symbol_id.get();
                let rest_loc = Some(builder.loc_of_span(rest.span));
                let id_loc = Some(builder.loc_of_span(id.span));
                if binding_can_use_direct(builder, symbol_id, style, force_temporaries) {
                    match lower_identifier_for_assignment(
                        builder, rest_loc, id_loc, kind, &id.name, symbol_id,
                    )? {
                        Some(IdentifierForAssignment::Place(place)) => {
                            properties
                                .push(ObjectPropertyOrSpread::Spread(SpreadPattern { place }));
                        }
                        Some(IdentifierForAssignment::Global { .. }) => {
                            builder.record_error(CompilerErrorDetail {
                                reason:
                                    "Expected reassignment of globals to enable forceTemporaries"
                                        .to_string(),
                                category: ErrorCategory::Todo,
                                loc: rest_loc,
                                description: None,
                                suggestions: None,
                            })?;
                        }
                        None => {}
                    }
                } else {
                    let temp = build_temporary_place(builder, rest_loc);
                    promote_temporary(builder, temp.identifier);
                    properties.push(ObjectPropertyOrSpread::Spread(SpreadPattern {
                        place: temp.clone(),
                    }));
                    followups.push((temp, &rest.argument));
                }
            }
            _ => {
                builder.record_error(CompilerErrorDetail {
                    reason: "(BuildHIR::lowerAssignment) Handle non-identifier rest element in ObjectPattern".to_string(),
                    category: ErrorCategory::Todo,
                    loc: Some(builder.loc_of_span(rest.span)),
                    description: None,
                    suggestions: None,
                })?;
            }
        }
    }

    let pat_loc = Some(builder.loc_of_span(pattern.span));
    let temporary = lower_value_to_temporary(
        builder,
        InstructionValue::Destructure {
            lvalue: LValuePattern {
                pattern: Pattern::Object(ObjectPattern {
                    properties,
                    loc: pat_loc,
                }),
                kind,
            },
            value: value.clone(),
            loc,
        },
    )?;

    for (place, path) in followups {
        let followup_loc = Some(builder.loc_of_span(path.span())).or(loc);
        lower_assignment(builder, followup_loc, kind, path, place, style)?;
    }
    Ok(Some(temporary))
}

/// Compute `forceTemporaries` for an array binding: when reassigning and any
/// element is a non-identifier, a context variable, or a non-local binding.
fn binding_force_temporaries(
    builder: &mut HirBuilder,
    kind: InstructionKind,
    pattern: &oxc::ArrayPattern,
) -> Result<bool, CompilerError> {
    if kind != InstructionKind::Reassign {
        return Ok(false);
    }
    if pattern.rest.is_some() {
        return Ok(true);
    }
    for elem in &pattern.elements {
        match elem {
            Some(oxc::BindingPattern::BindingIdentifier(id)) => {
                let symbol_id = id.symbol_id.get();
                if builder.is_context_symbol(symbol_id) {
                    return Ok(true);
                }
                let id_loc = Some(builder.loc_of_span(id.span));
                match builder.resolve_identifier_symbol(&id.name, symbol_id, id_loc)? {
                    VariableBinding::Identifier { .. } => {}
                    _ => return Ok(true),
                }
            }
            _ => return Ok(true),
        }
    }
    Ok(false)
}

/// Compute `forceTemporaries` for an object binding (same rules).
fn binding_object_force_temporaries(
    builder: &mut HirBuilder,
    kind: InstructionKind,
    pattern: &oxc::ObjectPattern,
) -> Result<bool, CompilerError> {
    if kind != InstructionKind::Reassign {
        return Ok(false);
    }
    if pattern.rest.is_some() {
        return Ok(true);
    }
    for prop in &pattern.properties {
        match &prop.value {
            oxc::BindingPattern::BindingIdentifier(id) => {
                let symbol_id = id.symbol_id.get();
                let id_loc = Some(builder.loc_of_span(id.span));
                match builder.resolve_identifier_symbol(&id.name, symbol_id, id_loc)? {
                    VariableBinding::Identifier { .. } => {}
                    _ => return Ok(true),
                }
            }
            _ => return Ok(true),
        }
    }
    Ok(false)
}

/// Lower a binding-pattern default value: build a temporary `result` such that
/// `result = value === undefined ? default : value`, returning `result`.
pub(crate) fn lower_default(
    builder: &mut HirBuilder,
    pat_loc: Option<SourceLocation>,
    default_expr: &oxc::Expression,
    value: Place,
) -> Result<Place, CompilerError> {
    let temp = build_temporary_place(builder, pat_loc);

    let test_block = builder.reserve(BlockKind::Value);
    let continuation_block = builder.reserve(builder.current_block_kind());
    let continuation_id = continuation_block.id;

    // Consequent: use the default value.
    let consequent = {
        let temp = temp.clone();
        builder.try_enter(BlockKind::Value, move |builder, _| {
            // Because we reorder evaluation, we restrict the allowed default
            // values to those whose evaluation order is unobservable.
            let default_value = lower_reorderable_expression(builder, default_expr)?;
            lower_value_to_temporary(
                builder,
                InstructionValue::StoreLocal {
                    lvalue: LValue {
                        place: temp.clone(),
                        kind: InstructionKind::Const,
                    },
                    value: default_value,
                    type_annotation: None,
                    loc: pat_loc,
                },
            )?;
            Ok(Terminal::Goto {
                block: continuation_id,
                variant: GotoVariant::Break,
                id: EvaluationOrder(0),
                loc: pat_loc,
            })
        })
    };

    // Alternate: use the original value.
    let alternate = {
        let temp = temp.clone();
        let value = value.clone();
        builder.try_enter(BlockKind::Value, move |builder, _| {
            lower_value_to_temporary(
                builder,
                InstructionValue::StoreLocal {
                    lvalue: LValue {
                        place: temp.clone(),
                        kind: InstructionKind::Const,
                    },
                    value: value.clone(),
                    type_annotation: None,
                    loc: pat_loc,
                },
            )?;
            Ok(Terminal::Goto {
                block: continuation_id,
                variant: GotoVariant::Break,
                id: EvaluationOrder(0),
                loc: pat_loc,
            })
        })
    };

    // Ternary terminal; enter the test block.
    builder.terminate_with_continuation(
        Terminal::Ternary {
            test: test_block.id,
            fallthrough: continuation_id,
            id: EvaluationOrder(0),
            loc: pat_loc,
        },
        test_block,
    );

    // In test block: value === undefined ?
    let undef = lower_value_to_temporary(
        builder,
        InstructionValue::Primitive {
            value: PrimitiveValue::Undefined,
            loc: pat_loc,
        },
    )?;
    let test = lower_value_to_temporary(
        builder,
        InstructionValue::BinaryExpression {
            left: value,
            operator: BinaryOperator::StrictEqual,
            right: undef,
            loc: pat_loc,
        },
    )?;
    builder.terminate_with_continuation(
        Terminal::Branch {
            test,
            consequent: consequent?,
            alternate: alternate?,
            fallthrough: continuation_id,
            id: EvaluationOrder(0),
            loc: pat_loc,
        },
        continuation_block,
    );

    Ok(temp)
}

/// Lower a default/reorderable expression, recording a Todo error first if the
/// expression's evaluation order is observable. Mirrors
/// `lowerReorderableExpression` in `BuildHIR.ts`: there are a few places (switch
/// case tests, destructuring defaults) where we do not preserve original
/// evaluation order, so only simple expressions whose evaluation cannot be
/// observed are allowed.
fn lower_reorderable_expression(
    builder: &mut HirBuilder,
    expr: &oxc::Expression,
) -> Result<Place, CompilerError> {
    if !is_reorderable_expression(builder, expr, true)? {
        let loc = Some(builder.loc_of_span(expr.span()));
        builder.record_error(CompilerErrorDetail {
            category: ErrorCategory::Todo,
            reason: format!(
                "(BuildHIR::node.lowerReorderableExpression) Expression type `{}` cannot be safely reordered",
                reorderable_expr_type_name(expr)
            ),
            description: None,
            loc,
            suggestions: None,
        })?;
    }
    lower_expression_to_temporary(builder, expr)
}

/// Returns the babel-style node type name used in the
/// `lowerReorderableExpression` error message. oxc splits `MemberExpression`
/// into static/computed/private variants; babel (and the TS compiler) use the
/// single `MemberExpression` name.
fn reorderable_expr_type_name(expr: &oxc::Expression) -> &'static str {
    match expr {
        oxc::Expression::StaticMemberExpression(_)
        | oxc::Expression::ComputedMemberExpression(_)
        | oxc::Expression::PrivateFieldExpression(_) => "MemberExpression",
        other => super::expressions::expression_kind_name(other),
    }
}

/// Returns true if `expr`'s evaluation order is unobservable, so it is safe to
/// reorder. Mirrors `isReorderableExpression` in `BuildHIR.ts` exactly.
fn is_reorderable_expression(
    builder: &mut HirBuilder,
    expr: &oxc::Expression,
    allow_local_identifiers: bool,
) -> Result<bool, CompilerError> {
    use oxc_syntax::operator::UnaryOperator;
    match expr {
        oxc::Expression::Identifier(ident) => {
            let symbol_id = sq::resolve_identifier_reference(builder.semantic(), ident);
            let loc = Some(builder.loc_of_span(ident.span));
            match builder.resolve_identifier_symbol(&ident.name, symbol_id, loc)? {
                // Local binding: only safe when locals are allowed.
                VariableBinding::Identifier { .. } => Ok(allow_local_identifiers),
                // Global, definitely safe.
                _ => Ok(true),
            }
        }
        oxc::Expression::TSInstantiationExpression(inner) => {
            is_reorderable_expression(builder, &inner.expression, allow_local_identifiers)
        }
        oxc::Expression::RegExpLiteral(_)
        | oxc::Expression::StringLiteral(_)
        | oxc::Expression::NumericLiteral(_)
        | oxc::Expression::NullLiteral(_)
        | oxc::Expression::BooleanLiteral(_)
        | oxc::Expression::BigIntLiteral(_) => Ok(true),
        oxc::Expression::UnaryExpression(unary) => match unary.operator {
            UnaryOperator::LogicalNot | UnaryOperator::UnaryPlus | UnaryOperator::UnaryNegation => {
                is_reorderable_expression(builder, &unary.argument, allow_local_identifiers)
            }
            _ => Ok(false),
        },
        // TS-only casts: babel's TSAsExpression / TSNonNullExpression /
        // TypeCastExpression.
        oxc::Expression::TSAsExpression(inner) => {
            is_reorderable_expression(builder, &inner.expression, allow_local_identifiers)
        }
        oxc::Expression::TSSatisfiesExpression(inner) => {
            is_reorderable_expression(builder, &inner.expression, allow_local_identifiers)
        }
        oxc::Expression::TSNonNullExpression(inner) => {
            is_reorderable_expression(builder, &inner.expression, allow_local_identifiers)
        }
        oxc::Expression::LogicalExpression(logical) => {
            Ok(
                is_reorderable_expression(builder, &logical.left, allow_local_identifiers)?
                    && is_reorderable_expression(builder, &logical.right, allow_local_identifiers)?,
            )
        }
        oxc::Expression::ConditionalExpression(cond) => Ok(is_reorderable_expression(
            builder,
            &cond.test,
            allow_local_identifiers,
        )? && is_reorderable_expression(
            builder,
            &cond.consequent,
            allow_local_identifiers,
        )? && is_reorderable_expression(
            builder,
            &cond.alternate,
            allow_local_identifiers,
        )?),
        oxc::Expression::ArrayExpression(array) => {
            for element in &array.elements {
                match element.as_expression() {
                    Some(e) if is_reorderable_expression(builder, e, allow_local_identifiers)? => {}
                    _ => return Ok(false),
                }
            }
            Ok(true)
        }
        oxc::Expression::ObjectExpression(object) => {
            for property in &object.properties {
                match property {
                    oxc::ObjectPropertyKind::ObjectProperty(prop) if !prop.computed => {
                        if !is_reorderable_expression(
                            builder,
                            &prop.value,
                            allow_local_identifiers,
                        )? {
                            return Ok(false);
                        }
                    }
                    _ => return Ok(false),
                }
            }
            Ok(true)
        }
        oxc::Expression::StaticMemberExpression(_)
        | oxc::Expression::ComputedMemberExpression(_)
        | oxc::Expression::PrivateFieldExpression(_) => {
            // A common pattern is switch statements where the case test values
            // are properties of a global, eg `case ProductOptions.Option: ...`.
            // We allow expressions where the innermost object is a global
            // identifier, and reject all other member expressions (for now).
            let mut inner: &oxc::Expression = expr;
            while let Some(member) = inner.as_member_expression() {
                inner = member.object();
            }
            if let oxc::Expression::Identifier(ident) = inner {
                let symbol_id = sq::resolve_identifier_reference(builder.semantic(), ident);
                let loc = Some(builder.loc_of_span(ident.span));
                match builder.resolve_identifier_symbol(&ident.name, symbol_id, loc)? {
                    // Innermost object is a local -> not safe.
                    VariableBinding::Identifier { .. } => Ok(false),
                    // Property/computed load from a global -> safe to reorder.
                    _ => Ok(true),
                }
            } else {
                Ok(false)
            }
        }
        oxc::Expression::ArrowFunctionExpression(arrow) => {
            if arrow.expression {
                // Expression body `() => expr`: oxc wraps it in a single
                // `ExpressionStatement`. Disallow local identifiers in the body.
                match arrow.body.statements.first() {
                    Some(oxc::Statement::ExpressionStatement(stmt)) => {
                        is_reorderable_expression(builder, &stmt.expression, false)
                    }
                    _ => Ok(false),
                }
            } else {
                // Block body: only an empty block is reorderable.
                Ok(arrow.body.statements.is_empty())
            }
        }
        oxc::Expression::CallExpression(call) => {
            if !is_reorderable_expression(builder, &call.callee, allow_local_identifiers)? {
                return Ok(false);
            }
            for arg in &call.arguments {
                match arg.as_expression() {
                    Some(e) if is_reorderable_expression(builder, e, allow_local_identifiers)? => {}
                    _ => return Ok(false),
                }
            }
            Ok(true)
        }
        oxc::Expression::NewExpression(new_expr) => {
            if !is_reorderable_expression(builder, &new_expr.callee, allow_local_identifiers)? {
                return Ok(false);
            }
            for arg in &new_expr.arguments {
                match arg.as_expression() {
                    Some(e) if is_reorderable_expression(builder, e, allow_local_identifiers)? => {}
                    _ => return Ok(false),
                }
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Helper to read a binding-identifier's HIR loc (`Some(loc)`).
fn id_loc_span(builder: &HirBuilder, id: &oxc::BindingIdentifier) -> Option<SourceLocation> {
    Some(builder.loc_of_span(id.span))
}

// =============================================================================
// Assignment-target family (destructuring assignment / for-of reassign heads)
// =============================================================================

/// Lower an assignment of `value` into an `AssignmentTarget` (the assignment
/// expression / for-of reassignment family). Always reassigns existing bindings.
/// Returns the temporary holding the stored / destructured value, if any.
pub(crate) fn lower_assignment_target(
    builder: &mut HirBuilder,
    loc: Option<SourceLocation>,
    target: &oxc::AssignmentTarget,
    value: Place,
) -> Result<Option<Place>, CompilerError> {
    use oxc::AssignmentTarget as T;
    match target {
        T::AssignmentTargetIdentifier(ident) => {
            lower_assignment_target_identifier(builder, loc, ident, value)
        }
        T::ArrayAssignmentTarget(pattern) => {
            lower_array_assignment_target(builder, loc, pattern, value)
        }
        T::ObjectAssignmentTarget(pattern) => {
            lower_object_assignment_target(builder, loc, pattern, value)
        }
        _ => {
            // Member-expression targets (`a.b = …`, `a[b] = …`) and the TS
            // wrappers (`(x as T) = …`). Member expressions store via
            // PropertyStore / ComputedStore.
            if let Some(member) = target.as_member_expression() {
                lower_member_assignment_target(builder, loc, member, value)
            } else {
                // TS as / satisfies / non-null / type-assertion wrappers: the
                // TS reference records the Todo elsewhere; bail gracefully.
                builder.record_diagnostic(todo_diagnostic(
                    "assignment target: TS type-cast wrapper",
                    loc,
                ));
                Ok(None)
            }
        }
    }
}

/// Reassign an existing binding via an `IdentifierReference` assignment target.
fn lower_assignment_target_identifier(
    builder: &mut HirBuilder,
    loc: Option<SourceLocation>,
    ident: &oxc::IdentifierReference,
    value: Place,
) -> Result<Option<Place>, CompilerError> {
    let ident_loc = Some(builder.loc_of_span(ident.span));
    let symbol_id = sq::resolve_identifier_reference(builder.semantic(), ident);
    let result = lower_identifier_for_assignment(
        builder,
        loc,
        ident_loc,
        InstructionKind::Reassign,
        &ident.name,
        symbol_id,
    )?;
    match result {
        None => Ok(None),
        Some(IdentifierForAssignment::Global { name }) => {
            let temp = lower_value_to_temporary(
                builder,
                InstructionValue::StoreGlobal { name, value, loc },
            )?;
            Ok(Some(temp))
        }
        Some(IdentifierForAssignment::Place(place)) => {
            if builder.is_context_symbol(symbol_id) {
                let temp = lower_value_to_temporary(
                    builder,
                    InstructionValue::StoreContext {
                        lvalue: LValue {
                            kind: InstructionKind::Reassign,
                            place,
                        },
                        value,
                        loc,
                    },
                )?;
                Ok(Some(temp))
            } else {
                let temp = lower_value_to_temporary(
                    builder,
                    InstructionValue::StoreLocal {
                        lvalue: LValue {
                            kind: InstructionKind::Reassign,
                            place,
                        },
                        value,
                        type_annotation: None,
                        loc,
                    },
                )?;
                Ok(Some(temp))
            }
        }
    }
}

/// Store `value` into a member-expression assignment target (`a.b = …`).
fn lower_member_assignment_target(
    builder: &mut HirBuilder,
    loc: Option<SourceLocation>,
    member: &oxc::MemberExpression,
    value: Place,
) -> Result<Option<Place>, CompilerError> {
    let object = lower_expression_to_temporary(builder, member.object())?;
    let temp = match member {
        oxc::MemberExpression::StaticMemberExpression(static_member) => lower_value_to_temporary(
            builder,
            InstructionValue::PropertyStore {
                object,
                property: PropertyLiteral::String(static_member.property.name.to_string()),
                value,
                loc,
            },
        )?,
        oxc::MemberExpression::ComputedMemberExpression(computed) => {
            if let oxc::Expression::NumericLiteral(num) = &computed.expression {
                lower_value_to_temporary(
                    builder,
                    InstructionValue::PropertyStore {
                        object,
                        property: PropertyLiteral::Number(FloatValue::new(num.value)),
                        value,
                        loc,
                    },
                )?
            } else {
                let prop = lower_expression_to_temporary(builder, &computed.expression)?;
                lower_value_to_temporary(
                    builder,
                    InstructionValue::ComputedStore {
                        object,
                        property: prop,
                        value,
                        loc,
                    },
                )?
            }
        }
        oxc::MemberExpression::PrivateFieldExpression(_) => {
            builder.record_diagnostic(todo_diagnostic("assignment target: private field", loc));
            return Ok(None);
        }
    };
    Ok(Some(temp))
}

/// Resolve an `AssignmentTargetMaybeDefault` to a value place, applying a
/// default if present, then assign into the inner target. Used by array
/// assignment-target elements.
fn lower_maybe_default_target(
    builder: &mut HirBuilder,
    loc: Option<SourceLocation>,
    maybe: &oxc::AssignmentTargetMaybeDefault,
    value: Place,
) -> Result<Option<Place>, CompilerError> {
    use oxc::AssignmentTargetMaybeDefault as M;
    match maybe {
        M::AssignmentTargetWithDefault(with_default) => {
            let pat_loc = Some(builder.loc_of_span(with_default.span));
            let resolved = lower_default(builder, pat_loc, &with_default.init, value)?;
            lower_assignment_target(builder, pat_loc, &with_default.binding, resolved)
        }
        // Otherwise it inherits the `AssignmentTarget` variants directly.
        other => {
            let target = other
                .as_assignment_target()
                .expect("AssignmentTargetMaybeDefault without default is an AssignmentTarget");
            lower_assignment_target(builder, loc, target, value)
        }
    }
}

fn lower_array_assignment_target(
    builder: &mut HirBuilder,
    loc: Option<SourceLocation>,
    pattern: &oxc::ArrayAssignmentTarget,
    value: Place,
) -> Result<Option<Place>, CompilerError> {
    let mut items: Vec<ArrayPatternElement> = Vec::new();
    let mut followups: Vec<(Place, FollowupTarget)> = Vec::new();

    // Mirror the reference `forceTemporaries`: a destructuring *reassignment*
    // mixes new declarations only for nested patterns rewritten into followups.
    // A given `Destructure` must be single-kind, so if any element is not a
    // simple resolvable non-context identifier (a default value, a nested
    // pattern, a member-expression target, a context variable, or a non-local
    // binding) we route ALL elements through promoted temporaries and emit the
    // real reassignments as followups.
    let force_temporaries = target_array_force_temporaries(builder, pattern)?;

    for element in &pattern.elements {
        match element {
            None => items.push(ArrayPatternElement::Hole),
            Some(maybe) => {
                let direct = if force_temporaries {
                    None
                } else {
                    direct_simple_target_place(builder, maybe, InstructionKind::Reassign)?
                };
                if let Some(place) = direct {
                    items.push(ArrayPatternElement::Place(place));
                } else {
                    let elem_loc = Some(builder.loc_of_span(maybe.span()));
                    let temp = build_temporary_place(builder, elem_loc);
                    promote_temporary(builder, temp.identifier);
                    items.push(ArrayPatternElement::Place(temp.clone()));
                    followups.push((temp, FollowupTarget::MaybeDefault(maybe)));
                }
            }
        }
    }

    if let Some(rest) = &pattern.rest {
        let rest_loc = Some(builder.loc_of_span(rest.span));
        let direct = if force_temporaries {
            None
        } else {
            direct_simple_assignment_target_place(builder, &rest.target, InstructionKind::Reassign)?
        };
        if let Some(place) = direct {
            items.push(ArrayPatternElement::Spread(SpreadPattern { place }));
        } else {
            let temp = build_temporary_place(builder, rest_loc);
            promote_temporary(builder, temp.identifier);
            items.push(ArrayPatternElement::Spread(SpreadPattern {
                place: temp.clone(),
            }));
            followups.push((temp, FollowupTarget::Target(&rest.target)));
        }
    }

    let pat_loc = Some(builder.loc_of_span(pattern.span));
    let temporary = lower_value_to_temporary(
        builder,
        InstructionValue::Destructure {
            lvalue: LValuePattern {
                pattern: Pattern::Array(ArrayPattern {
                    items,
                    loc: pat_loc,
                }),
                kind: InstructionKind::Reassign,
            },
            value: value.clone(),
            loc,
        },
    )?;

    run_target_followups(builder, followups, loc)?;
    Ok(Some(temporary))
}

fn lower_object_assignment_target(
    builder: &mut HirBuilder,
    loc: Option<SourceLocation>,
    pattern: &oxc::ObjectAssignmentTarget,
    value: Place,
) -> Result<Option<Place>, CompilerError> {
    let mut properties: Vec<ObjectPropertyOrSpread> = Vec::new();
    let mut followups: Vec<(Place, FollowupTarget)> = Vec::new();

    // Mirror the reference `forceTemporaries`: a destructuring *reassignment*
    // that contains a rest element or any property whose target is not a simple
    // resolvable identifier must route ALL of its targets through promoted
    // temporaries (and emit the real reassignments as followups). Otherwise
    // simple non-context identifiers can be destructured directly into place.
    let force_temporaries = target_object_force_temporaries(builder, pattern)?;

    for prop in &pattern.properties {
        match prop {
            oxc::AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(shorthand) => {
                // `({ foo } = obj)` or `({ foo = dflt } = obj)`. The key is the
                // binding name; the binding is the same identifier reference.
                let key = ObjectPropertyKey::Identifier {
                    name: shorthand.binding.name.to_string(),
                };
                // A bare shorthand `{ foo }` (no default) that targets a simple
                // non-context local can be destructured directly into place,
                // mirroring the reference's direct-identifier branch. Otherwise
                // (default value, context var, or forceTemporaries) use a temp +
                // followup.
                let direct = if shorthand.init.is_none() && !force_temporaries {
                    direct_simple_identifier_reference_place(
                        builder,
                        &shorthand.binding,
                        InstructionKind::Reassign,
                    )?
                } else {
                    None
                };
                if let Some(place) = direct {
                    properties.push(ObjectPropertyOrSpread::Property(ObjectProperty {
                        key,
                        property_type: ObjectPropertyType::Property,
                        place,
                    }));
                } else {
                    let id_loc = Some(builder.loc_of_span(shorthand.binding.span));
                    let temp = build_temporary_place(builder, id_loc);
                    promote_temporary(builder, temp.identifier);
                    properties.push(ObjectPropertyOrSpread::Property(ObjectProperty {
                        key,
                        property_type: ObjectPropertyType::Property,
                        place: temp.clone(),
                    }));
                    followups.push((temp, FollowupTarget::ShorthandIdentifier(shorthand)));
                }
            }
            oxc::AssignmentTargetProperty::AssignmentTargetPropertyProperty(named) => {
                // `({ prop: target } = obj)`.
                let key = match lower_object_property_key(builder, &named.name, named.computed)? {
                    Some(k) => k,
                    None => continue,
                };
                let direct = if force_temporaries {
                    None
                } else {
                    direct_simple_target_place(builder, &named.binding, InstructionKind::Reassign)?
                };
                if let Some(place) = direct {
                    properties.push(ObjectPropertyOrSpread::Property(ObjectProperty {
                        key,
                        property_type: ObjectPropertyType::Property,
                        place,
                    }));
                } else {
                    let elem_loc = Some(builder.loc_of_span(named.binding.span()));
                    let temp = build_temporary_place(builder, elem_loc);
                    promote_temporary(builder, temp.identifier);
                    properties.push(ObjectPropertyOrSpread::Property(ObjectProperty {
                        key,
                        property_type: ObjectPropertyType::Property,
                        place: temp.clone(),
                    }));
                    followups.push((temp, FollowupTarget::MaybeDefault(&named.binding)));
                }
            }
        }
    }

    if let Some(rest) = &pattern.rest {
        let rest_loc = Some(builder.loc_of_span(rest.span));
        if let Some(place) =
            direct_simple_assignment_target_place(builder, &rest.target, InstructionKind::Reassign)?
        {
            properties.push(ObjectPropertyOrSpread::Spread(SpreadPattern { place }));
        } else {
            let temp = build_temporary_place(builder, rest_loc);
            promote_temporary(builder, temp.identifier);
            properties.push(ObjectPropertyOrSpread::Spread(SpreadPattern {
                place: temp.clone(),
            }));
            followups.push((temp, FollowupTarget::Target(&rest.target)));
        }
    }

    let pat_loc = Some(builder.loc_of_span(pattern.span));
    let temporary = lower_value_to_temporary(
        builder,
        InstructionValue::Destructure {
            lvalue: LValuePattern {
                pattern: Pattern::Object(ObjectPattern {
                    properties,
                    loc: pat_loc,
                }),
                kind: InstructionKind::Reassign,
            },
            value: value.clone(),
            loc,
        },
    )?;

    run_target_followups(builder, followups, loc)?;
    Ok(Some(temporary))
}

/// A pending followup assignment in the assignment-target family.
enum FollowupTarget<'a> {
    /// An `AssignmentTargetMaybeDefault` (array element / named property value).
    MaybeDefault(&'a oxc::AssignmentTargetMaybeDefault<'a>),
    /// A bare `AssignmentTarget` (rest element).
    Target(&'a oxc::AssignmentTarget<'a>),
    /// A shorthand `{ foo }` / `{ foo = dflt }` property.
    ShorthandIdentifier(&'a oxc::AssignmentTargetPropertyIdentifier<'a>),
}

fn run_target_followups(
    builder: &mut HirBuilder,
    followups: Vec<(Place, FollowupTarget)>,
    loc: Option<SourceLocation>,
) -> Result<(), CompilerError> {
    for (place, target) in followups {
        match target {
            FollowupTarget::MaybeDefault(maybe) => {
                let followup_loc = Some(builder.loc_of_span(maybe.span())).or(loc);
                lower_maybe_default_target(builder, followup_loc, maybe, place)?;
            }
            FollowupTarget::Target(target) => {
                let followup_loc = Some(builder.loc_of_span(target.span())).or(loc);
                lower_assignment_target(builder, followup_loc, target, place)?;
            }
            FollowupTarget::ShorthandIdentifier(shorthand) => {
                let followup_loc = Some(builder.loc_of_span(shorthand.span)).or(loc);
                let value = if let Some(default) = &shorthand.init {
                    lower_default(builder, followup_loc, default, place)?
                } else {
                    place
                };
                // The binding is an `IdentifierReference` reassignment target.
                lower_assignment_target_identifier(
                    builder,
                    followup_loc,
                    &shorthand.binding,
                    value,
                )?;
            }
        }
    }
    Ok(())
}

/// Try to resolve an `AssignmentTargetMaybeDefault` directly to an existing
/// local place (only when it is a bare identifier with no default and the
/// binding is a local, non-context identifier). Returns `None` to signal the
/// caller should use a promoted temporary + followup.
fn direct_simple_target_place(
    builder: &mut HirBuilder,
    maybe: &oxc::AssignmentTargetMaybeDefault,
    kind: InstructionKind,
) -> Result<Option<Place>, CompilerError> {
    use oxc::AssignmentTargetMaybeDefault as M;
    match maybe {
        M::AssignmentTargetWithDefault(_) => Ok(None),
        other => {
            let target = match other.as_assignment_target() {
                Some(t) => t,
                None => return Ok(None),
            };
            direct_simple_assignment_target_place(builder, target, kind)
        }
    }
}

/// Like [`direct_simple_target_place`] for a bare `AssignmentTarget`.
fn direct_simple_assignment_target_place(
    builder: &mut HirBuilder,
    target: &oxc::AssignmentTarget,
    _kind: InstructionKind,
) -> Result<Option<Place>, CompilerError> {
    use oxc::AssignmentTarget as T;
    match target {
        T::AssignmentTargetIdentifier(ident) => {
            let symbol_id = sq::resolve_identifier_reference(builder.semantic(), ident);
            if builder.is_context_symbol(symbol_id) {
                return Ok(None);
            }
            let ident_loc = Some(builder.loc_of_span(ident.span));
            match builder.resolve_identifier_symbol(&ident.name, symbol_id, ident_loc)? {
                VariableBinding::Identifier { identifier, .. } => Ok(Some(Place {
                    identifier,
                    effect: Effect::Unknown,
                    reactive: false,
                    loc: ident_loc,
                })),
                _ => Ok(None),
            }
        }
        _ => Ok(None),
    }
}

/// Resolve a bare shorthand binding (`IdentifierReference`) to an existing local
/// place, returning `None` (signalling "use a promoted temporary + followup")
/// for context variables or non-`Identifier` bindings. Mirrors the reference's
/// direct-identifier branch where `getStoreKind === 'StoreLocal'`.
fn direct_simple_identifier_reference_place(
    builder: &mut HirBuilder,
    ident: &oxc::IdentifierReference,
    _kind: InstructionKind,
) -> Result<Option<Place>, CompilerError> {
    let symbol_id = sq::resolve_identifier_reference(builder.semantic(), ident);
    if builder.is_context_symbol(symbol_id) {
        return Ok(None);
    }
    let ident_loc = Some(builder.loc_of_span(ident.span));
    match builder.resolve_identifier_symbol(&ident.name, symbol_id, ident_loc)? {
        VariableBinding::Identifier { identifier, .. } => Ok(Some(Place {
            identifier,
            effect: Effect::Unknown,
            reactive: false,
            loc: ident_loc,
        })),
        _ => Ok(None),
    }
}

/// Mirror of the reference `forceTemporaries` for an array destructuring
/// *reassignment* target: true if the pattern has a rest element, or any
/// element is not a simple resolvable non-context identifier (a default value,
/// a nested pattern, a member-expression target, a context variable, or a
/// non-local binding). When true, every element is routed through a promoted
/// temporary so the emitted `Destructure` is single-kind and the followup
/// reassignments run in order.
fn target_array_force_temporaries(
    builder: &mut HirBuilder,
    pattern: &oxc::ArrayAssignmentTarget,
) -> Result<bool, CompilerError> {
    if pattern.rest.is_some() {
        return Ok(true);
    }
    for element in &pattern.elements {
        match element {
            // A hole (`[, a]`) is neither a declaration nor a reassignment, so
            // it does not force temporaries on its own.
            None => {}
            Some(maybe) => {
                // `[a = dflt]` has a default → not a plain identifier target.
                let is_plain_identifier = match maybe {
                    oxc::AssignmentTargetMaybeDefault::AssignmentTargetWithDefault(_) => false,
                    other => match other.as_assignment_target() {
                        Some(oxc::AssignmentTarget::AssignmentTargetIdentifier(ident)) => {
                            let symbol_id =
                                sq::resolve_identifier_reference(builder.semantic(), ident);
                            // Context variables reassign via StoreContext, so they
                            // cannot be destructured directly into place.
                            if builder.is_context_symbol(symbol_id) {
                                false
                            } else {
                                let id_loc = Some(builder.loc_of_span(ident.span));
                                matches!(
                                    builder.resolve_identifier_symbol(
                                        &ident.name,
                                        symbol_id,
                                        id_loc
                                    )?,
                                    VariableBinding::Identifier { .. }
                                )
                            }
                        }
                        _ => false,
                    },
                };
                if !is_plain_identifier {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

/// Mirror of the reference `forceTemporaries` for an object destructuring
/// *reassignment* target: true if the pattern has a rest element, or any
/// property whose target is not a simple resolvable identifier (a default value,
/// a nested pattern, or a member-expression target). When true, every property
/// is routed through a promoted temporary so the followup reassignments run in
/// order.
fn target_object_force_temporaries(
    builder: &mut HirBuilder,
    pattern: &oxc::ObjectAssignmentTarget,
) -> Result<bool, CompilerError> {
    if pattern.rest.is_some() {
        return Ok(true);
    }
    for prop in &pattern.properties {
        match prop {
            oxc::AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(shorthand) => {
                // `{ foo = dflt }` has a default → not a plain identifier target.
                if shorthand.init.is_some() {
                    return Ok(true);
                }
                let symbol_id =
                    sq::resolve_identifier_reference(builder.semantic(), &shorthand.binding);
                let id_loc = Some(builder.loc_of_span(shorthand.binding.span));
                match builder.resolve_identifier_symbol(
                    &shorthand.binding.name,
                    symbol_id,
                    id_loc,
                )? {
                    VariableBinding::Identifier { .. } => {}
                    _ => return Ok(true),
                }
            }
            oxc::AssignmentTargetProperty::AssignmentTargetPropertyProperty(named) => {
                // `{ prop: target }`: only a bare, resolvable, non-default
                // identifier target keeps us out of forceTemporaries.
                let is_plain_identifier = match &named.binding {
                    oxc::AssignmentTargetMaybeDefault::AssignmentTargetWithDefault(_) => false,
                    other => match other.as_assignment_target() {
                        Some(oxc::AssignmentTarget::AssignmentTargetIdentifier(ident)) => {
                            let symbol_id =
                                sq::resolve_identifier_reference(builder.semantic(), ident);
                            let id_loc = Some(builder.loc_of_span(ident.span));
                            matches!(
                                builder.resolve_identifier_symbol(
                                    &ident.name,
                                    symbol_id,
                                    id_loc
                                )?,
                                VariableBinding::Identifier { .. }
                            )
                        }
                        _ => false,
                    },
                };
                if !is_plain_identifier {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}
