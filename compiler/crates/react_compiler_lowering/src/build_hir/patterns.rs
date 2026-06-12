
use react_compiler_diagnostics::CompilerError;
use react_compiler_diagnostics::CompilerErrorDetail;
use react_compiler_diagnostics::ErrorCategory;
use react_compiler_hir::*;

use crate::hir_builder::HirBuilder;

#[allow(unused_imports)]
use super::*;

/// Result of resolving an identifier for assignment.
pub(crate) enum IdentifierForAssignment {
    /// A local place (identifier binding)
    Place(Place),
    /// A global variable (non-local, non-import)
    Global { name: String },
}

/// Resolve an identifier for use as an assignment target.
/// Returns None if the binding could not be found (error recorded).
pub(crate) fn lower_identifier_for_assignment(
    builder: &mut HirBuilder,
    loc: Option<SourceLocation>,
    ident_loc: Option<SourceLocation>,
    kind: InstructionKind,
    name: &str,
    start: u32,
    node_id: Option<u32>,
) -> Result<Option<IdentifierForAssignment>, CompilerError> {
    let mut binding = builder.resolve_identifier(name, start, ident_loc.clone(), node_id)?;
    if !matches!(binding, VariableBinding::Identifier { .. }) && kind != InstructionKind::Reassign {
        if let Some((binding_id, binding_data)) = builder
            .scope_info()
            .find_binding_id_in_descendants(name, builder.function_scope())
        {
            let bk = crate::convert_binding_kind(&binding_data.kind);
            let identifier =
                builder.resolve_binding_with_loc(name, binding_id, ident_loc.clone())?;
            binding = VariableBinding::Identifier {
                identifier,
                binding_kind: bk,
            };
        }
    }
    match binding {
        VariableBinding::Identifier {
            identifier,
            binding_kind,
            ..
        } => {
            // Set the identifier's loc from the declaration site (not for reassignments,
            // which should keep the original declaration loc)
            if kind != InstructionKind::Reassign {
                builder.set_identifier_declaration_loc(identifier, &ident_loc);
            }
            if binding_kind == BindingKind::Const && kind == InstructionKind::Reassign {
                builder.record_error(CompilerErrorDetail {
                    reason: "Cannot reassign a `const` variable".to_string(),
                    category: ErrorCategory::Syntax,
                    loc: loc.clone(),
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
            // Import bindings can't be assigned to
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

pub(crate) fn lower_assignment(
    builder: &mut HirBuilder,
    loc: Option<SourceLocation>,
    kind: InstructionKind,
    target: &react_compiler_ast::patterns::PatternLike,
    value: Place,
    assignment_style: AssignmentStyle,
) -> Result<Option<Place>, CompilerError> {
    use react_compiler_ast::patterns::PatternLike;

    match target {
        PatternLike::Identifier(id) => {
            let id_loc = convert_opt_loc(&id.base.loc);
            let result = lower_identifier_for_assignment(
                builder,
                loc.clone(),
                id_loc,
                kind,
                &id.name,
                id.base.start.unwrap_or(0),
                id.base.node_id,
            )?;
            match result {
                None => {
                    // Error already recorded
                    return Ok(None);
                }
                Some(IdentifierForAssignment::Global { name }) => {
                    let temp = lower_value_to_temporary(
                        builder,
                        InstructionValue::StoreGlobal { name, value, loc },
                    )?;
                    return Ok(Some(temp));
                }
                Some(IdentifierForAssignment::Place(place)) => {
                    let start = id.base.start.unwrap_or(0);
                    if builder.is_context_identifier(&id.name, start, id.base.node_id) {
                        // Check if the binding is hoisted before flagging const reassignment
                        let is_hoisted = builder
                            .scope_info()
                            .resolve_reference_for_node(id.base.node_id)
                            .map(|b| builder.environment().is_hoisted_identifier(b.id.0))
                            .unwrap_or(false);
                        if kind == InstructionKind::Const && !is_hoisted {
                            builder.record_error(CompilerErrorDetail {
                                reason: "Expected `const` declaration not to be reassigned"
                                    .to_string(),
                                category: ErrorCategory::Syntax,
                                loc: loc.clone(),
                                suggestions: None,
                                description: None,
                            })?;
                        }
                        if kind != InstructionKind::Const
                            && kind != InstructionKind::Reassign
                            && kind != InstructionKind::Let
                            && kind != InstructionKind::Function
                        {
                            builder.record_error(CompilerErrorDetail {
                                reason: "Unexpected context variable kind".to_string(),
                                category: ErrorCategory::Syntax,
                                loc: loc.clone(),
                                suggestions: None,
                                description: None,
                            })?;
                            let temp = lower_value_to_temporary(
                                builder,
                                InstructionValue::UnsupportedNode {
                                    node_type: Some("Identifier".to_string()),
                                    original_node: serialize_pattern(target),
                                    loc,
                                },
                            )?;
                            return Ok(Some(temp));
                        }
                        let temp = lower_value_to_temporary(
                            builder,
                            InstructionValue::StoreContext {
                                lvalue: LValue { place, kind },
                                value,
                                loc,
                            },
                        )?;
                        return Ok(Some(temp));
                    } else {
                        let type_annotation = extract_type_annotation_name(&id.type_annotation);
                        let temp = lower_value_to_temporary(
                            builder,
                            InstructionValue::StoreLocal {
                                lvalue: LValue { place, kind },
                                value,
                                type_annotation,
                                loc,
                            },
                        )?;
                        return Ok(Some(temp));
                    }
                }
            }
        }

        PatternLike::MemberExpression(member) => {
            // MemberExpression may only appear in an assignment expression (Reassign)
            if kind != InstructionKind::Reassign {
                builder.record_error(CompilerErrorDetail {
                    category: ErrorCategory::Invariant,
                    reason: "MemberExpression may only appear in an assignment expression"
                        .to_string(),
                    description: None,
                    loc: loc.clone(),
                    suggestions: None,
                })?;
                return Ok(None);
            }
            let object = lower_expression_to_temporary(builder, &member.object)?;
            let temp = if !member.computed
                || matches!(
                    &*member.property,
                    react_compiler_ast::expressions::Expression::NumericLiteral(_)
                ) {
                match &*member.property {
                    react_compiler_ast::expressions::Expression::Identifier(prop_id) => {
                        lower_value_to_temporary(
                            builder,
                            InstructionValue::PropertyStore {
                                object,
                                property: PropertyLiteral::String(prop_id.name.clone()),
                                value,
                                loc,
                            },
                        )?
                    }
                    react_compiler_ast::expressions::Expression::NumericLiteral(num) => {
                        lower_value_to_temporary(
                            builder,
                            InstructionValue::PropertyStore {
                                object,
                                property: PropertyLiteral::Number(FloatValue::new(
                                    num.precise_value(),
                                )),
                                value,
                                loc,
                            },
                        )?
                    }
                    _ => {
                        builder.record_error(CompilerErrorDetail {
                            reason: format!("(BuildHIR::lowerAssignment) Handle {} properties in MemberExpression", expression_type_name(&member.property)),
                            category: ErrorCategory::Todo,
                            loc: expression_loc(&member.property),
                            description: None,
                            suggestions: None,
                        })?;
                        lower_value_to_temporary(
                            builder,
                            InstructionValue::UnsupportedNode {
                                node_type: Some("MemberExpression".to_string()),
                                original_node: serialize_pattern(target),
                                loc,
                            },
                        )?
                    }
                }
            } else {
                if matches!(
                    &*member.property,
                    react_compiler_ast::expressions::Expression::PrivateName(_)
                ) {
                    builder.record_error(CompilerErrorDetail {
                        reason: "(BuildHIR::lowerAssignment) Expected private name to appear as a non-computed property".to_string(),
                        category: ErrorCategory::Todo,
                        loc: expression_loc(&member.property),
                        description: None,
                        suggestions: None,
                    })?;
                    lower_value_to_temporary(
                        builder,
                        InstructionValue::UnsupportedNode {
                            node_type: Some("MemberExpression".to_string()),
                            original_node: serialize_pattern(target),
                            loc,
                        },
                    )?
                } else {
                    let property_place = lower_expression_to_temporary(builder, &member.property)?;
                    lower_value_to_temporary(
                        builder,
                        InstructionValue::ComputedStore {
                            object,
                            property: property_place,
                            value,
                            loc,
                        },
                    )?
                }
            };
            Ok(Some(temp))
        }

        PatternLike::ArrayPattern(pattern) => {
            let mut items: Vec<ArrayPatternElement> = Vec::new();
            let mut followups: Vec<(Place, &PatternLike)> = Vec::new();

            // Compute forceTemporaries: when kind is Reassign and any element is
            // non-identifier, a context variable, or a non-local binding
            let force_temporaries = if kind == InstructionKind::Reassign {
                let mut found = false;
                for elem in &pattern.elements {
                    match elem {
                        Some(PatternLike::Identifier(id)) => {
                            let start = id.base.start.unwrap_or(0);
                            if builder.is_context_identifier(&id.name, start, id.base.node_id) {
                                found = true;
                                break;
                            }
                            let ident_loc = convert_opt_loc(&id.base.loc);
                            match builder.resolve_identifier(
                                &id.name,
                                start,
                                ident_loc,
                                id.base.node_id,
                            )? {
                                VariableBinding::Identifier { .. } => {}
                                _ => {
                                    found = true;
                                    break;
                                }
                            }
                        }
                        _ => {
                            // Non-identifier elements (including None/holes and RestElements)
                            // trigger forceTemporaries, matching TS where `!element.isIdentifier()`
                            // returns true for null elements
                            found = true;
                            break;
                        }
                    }
                }
                found
            } else {
                false
            };

            for element in &pattern.elements {
                match element {
                    None => {
                        items.push(ArrayPatternElement::Hole);
                    }
                    Some(PatternLike::RestElement(rest)) => {
                        match &*rest.argument {
                            PatternLike::Identifier(id) => {
                                let start = id.base.start.unwrap_or(0);
                                let is_context =
                                    builder.is_context_identifier(&id.name, start, id.base.node_id);
                                let can_use_direct = !force_temporaries
                                    && (matches!(assignment_style, AssignmentStyle::Assignment)
                                        || !is_context);
                                if can_use_direct {
                                    match lower_identifier_for_assignment(
                                        builder,
                                        convert_opt_loc(&rest.base.loc),
                                        convert_opt_loc(&id.base.loc),
                                        kind,
                                        &id.name,
                                        start,
                                        id.base.node_id,
                                    )? {
                                        Some(IdentifierForAssignment::Place(place)) => {
                                            items.push(ArrayPatternElement::Spread(
                                                SpreadPattern { place },
                                            ));
                                        }
                                        Some(IdentifierForAssignment::Global { .. }) => {
                                            let temp = build_temporary_place(
                                                builder,
                                                convert_opt_loc(&rest.base.loc),
                                            );
                                            promote_temporary(builder, temp.identifier);
                                            items.push(ArrayPatternElement::Spread(
                                                SpreadPattern {
                                                    place: temp.clone(),
                                                },
                                            ));
                                            followups.push((temp, &rest.argument));
                                        }
                                        None => {
                                            // Error already recorded
                                        }
                                    }
                                } else {
                                    let temp = build_temporary_place(
                                        builder,
                                        convert_opt_loc(&rest.base.loc),
                                    );
                                    promote_temporary(builder, temp.identifier);
                                    items.push(ArrayPatternElement::Spread(SpreadPattern {
                                        place: temp.clone(),
                                    }));
                                    followups.push((temp, &rest.argument));
                                }
                            }
                            _ => {
                                let temp =
                                    build_temporary_place(builder, convert_opt_loc(&rest.base.loc));
                                promote_temporary(builder, temp.identifier);
                                items.push(ArrayPatternElement::Spread(SpreadPattern {
                                    place: temp.clone(),
                                }));
                                followups.push((temp, &rest.argument));
                            }
                        }
                    }
                    Some(PatternLike::Identifier(id)) => {
                        let start = id.base.start.unwrap_or(0);
                        let is_context =
                            builder.is_context_identifier(&id.name, start, id.base.node_id);
                        let can_use_direct = !force_temporaries
                            && (matches!(assignment_style, AssignmentStyle::Assignment)
                                || !is_context);
                        if can_use_direct {
                            match lower_identifier_for_assignment(
                                builder,
                                convert_opt_loc(&id.base.loc),
                                convert_opt_loc(&id.base.loc),
                                kind,
                                &id.name,
                                start,
                                id.base.node_id,
                            )? {
                                Some(IdentifierForAssignment::Place(place)) => {
                                    items.push(ArrayPatternElement::Place(place));
                                }
                                Some(IdentifierForAssignment::Global { .. }) => {
                                    let temp = build_temporary_place(
                                        builder,
                                        convert_opt_loc(&id.base.loc),
                                    );
                                    promote_temporary(builder, temp.identifier);
                                    items.push(ArrayPatternElement::Place(temp.clone()));
                                    followups.push((temp, element.as_ref().unwrap()));
                                }
                                None => {
                                    items.push(ArrayPatternElement::Hole);
                                }
                            }
                        } else {
                            // Context variable or force_temporaries: use promoted temporary
                            let temp =
                                build_temporary_place(builder, convert_opt_loc(&id.base.loc));
                            promote_temporary(builder, temp.identifier);
                            items.push(ArrayPatternElement::Place(temp.clone()));
                            followups.push((temp, element.as_ref().unwrap()));
                        }
                    }
                    Some(other) => {
                        // Nested pattern: use temporary + followup
                        let elem_loc = pattern_like_hir_loc(other);
                        let temp = build_temporary_place(builder, elem_loc);
                        promote_temporary(builder, temp.identifier);
                        items.push(ArrayPatternElement::Place(temp.clone()));
                        followups.push((temp, other));
                    }
                }
            }

            let temporary = lower_value_to_temporary(
                builder,
                InstructionValue::Destructure {
                    lvalue: LValuePattern {
                        pattern: Pattern::Array(ArrayPattern {
                            items,
                            loc: convert_opt_loc(&pattern.base.loc),
                        }),
                        kind,
                    },
                    value: value.clone(),
                    loc: loc.clone(),
                },
            )?;

            for (place, path) in followups {
                let followup_loc = pattern_like_hir_loc(path).or(loc.clone());
                lower_assignment(builder, followup_loc, kind, path, place, assignment_style)?;
            }
            Ok(Some(temporary))
        }

        PatternLike::ObjectPattern(pattern) => {
            let mut properties: Vec<ObjectPropertyOrSpread> = Vec::new();
            let mut followups: Vec<(Place, &PatternLike)> = Vec::new();

            // Compute forceTemporaries for ObjectPattern
            let force_temporaries = if kind == InstructionKind::Reassign {
                use react_compiler_ast::patterns::ObjectPatternProperty;
                let mut found = false;
                for prop in &pattern.properties {
                    match prop {
                        ObjectPatternProperty::RestElement(_) => {
                            found = true;
                            break;
                        }
                        ObjectPatternProperty::ObjectProperty(obj_prop) => match &*obj_prop.value {
                            PatternLike::Identifier(id) => {
                                let start = id.base.start.unwrap_or(0);
                                let ident_loc = convert_opt_loc(&id.base.loc);
                                match builder.resolve_identifier(
                                    &id.name,
                                    start,
                                    ident_loc,
                                    id.base.node_id,
                                )? {
                                    VariableBinding::Identifier { .. } => {}
                                    _ => {
                                        found = true;
                                        break;
                                    }
                                }
                            }
                            _ => {
                                found = true;
                                break;
                            }
                        },
                    }
                }
                found
            } else {
                false
            };

            for prop in &pattern.properties {
                match prop {
                    react_compiler_ast::patterns::ObjectPatternProperty::RestElement(rest) => {
                        match &*rest.argument {
                            PatternLike::Identifier(id) => {
                                let start = id.base.start.unwrap_or(0);
                                let is_context =
                                    builder.is_context_identifier(&id.name, start, id.base.node_id);
                                let can_use_direct = !force_temporaries
                                    && (matches!(assignment_style, AssignmentStyle::Assignment)
                                        || !is_context);
                                if can_use_direct {
                                    match lower_identifier_for_assignment(
                                        builder,
                                        convert_opt_loc(&rest.base.loc),
                                        convert_opt_loc(&id.base.loc),
                                        kind,
                                        &id.name,
                                        start,
                                        id.base.node_id,
                                    )? {
                                        Some(IdentifierForAssignment::Place(place)) => {
                                            properties.push(ObjectPropertyOrSpread::Spread(
                                                SpreadPattern { place },
                                            ));
                                        }
                                        Some(IdentifierForAssignment::Global { .. }) => {
                                            builder.record_error(CompilerErrorDetail {
                                                reason: "Expected reassignment of globals to enable forceTemporaries".to_string(),
                                                category: ErrorCategory::Todo,
                                                loc: convert_opt_loc(&rest.base.loc),
                                                description: None,
                                                suggestions: None,
                                            })?;
                                        }
                                        None => {}
                                    }
                                } else {
                                    let temp = build_temporary_place(
                                        builder,
                                        convert_opt_loc(&rest.base.loc),
                                    );
                                    promote_temporary(builder, temp.identifier);
                                    properties.push(ObjectPropertyOrSpread::Spread(
                                        SpreadPattern {
                                            place: temp.clone(),
                                        },
                                    ));
                                    followups.push((temp, &rest.argument));
                                }
                            }
                            _ => {
                                builder.record_error(CompilerErrorDetail {
                                    reason: format!("(BuildHIR::lowerAssignment) Handle {} rest element in ObjectPattern",
                                        match &*rest.argument {
                                            PatternLike::ObjectPattern(_) => "ObjectPattern",
                                            PatternLike::ArrayPattern(_) => "ArrayPattern",
                                            PatternLike::AssignmentPattern(_) => "AssignmentPattern",
                                            PatternLike::MemberExpression(_) => "MemberExpression",
                                            _ => "unknown",
                                        }),
                                    category: ErrorCategory::Todo,
                                    loc: convert_opt_loc(&rest.base.loc),
                                    description: None,
                                    suggestions: None,
                                })?;
                            }
                        }
                    }
                    react_compiler_ast::patterns::ObjectPatternProperty::ObjectProperty(
                        obj_prop,
                    ) => {
                        if obj_prop.computed {
                            builder.record_error(CompilerErrorDetail {
                                reason: "(BuildHIR::lowerAssignment) Handle computed properties in ObjectPattern".to_string(),
                                category: ErrorCategory::Todo,
                                loc: convert_opt_loc(&obj_prop.base.loc),
                                description: None,
                                suggestions: None,
                            })?;
                            continue;
                        }

                        let key = match lower_object_property_key(builder, &obj_prop.key, false)? {
                            Some(k) => k,
                            None => continue,
                        };

                        match &*obj_prop.value {
                            PatternLike::Identifier(id) => {
                                let start = id.base.start.unwrap_or(0);
                                let is_context =
                                    builder.is_context_identifier(&id.name, start, id.base.node_id);
                                let can_use_direct = !force_temporaries
                                    && (matches!(assignment_style, AssignmentStyle::Assignment)
                                        || !is_context);
                                if can_use_direct {
                                    match lower_identifier_for_assignment(
                                        builder,
                                        convert_opt_loc(&id.base.loc),
                                        convert_opt_loc(&id.base.loc),
                                        kind,
                                        &id.name,
                                        start,
                                        id.base.node_id,
                                    )? {
                                        Some(IdentifierForAssignment::Place(place)) => {
                                            properties.push(ObjectPropertyOrSpread::Property(
                                                ObjectProperty {
                                                    key,
                                                    property_type: ObjectPropertyType::Property,
                                                    place,
                                                },
                                            ));
                                        }
                                        Some(IdentifierForAssignment::Global { .. }) => {
                                            builder.record_error(CompilerErrorDetail {
                                                reason: "Expected reassignment of globals to enable forceTemporaries".to_string(),
                                                category: ErrorCategory::Todo,
                                                loc: convert_opt_loc(&id.base.loc),
                                                description: None,
                                                suggestions: None,
                                            })?;
                                        }
                                        None => {
                                            continue;
                                        }
                                    }
                                } else {
                                    // Context variable or force_temporaries: use promoted temporary
                                    let temp = build_temporary_place(
                                        builder,
                                        convert_opt_loc(&id.base.loc),
                                    );
                                    promote_temporary(builder, temp.identifier);
                                    properties.push(ObjectPropertyOrSpread::Property(
                                        ObjectProperty {
                                            key,
                                            property_type: ObjectPropertyType::Property,
                                            place: temp.clone(),
                                        },
                                    ));
                                    followups.push((temp, &*obj_prop.value));
                                }
                            }
                            other => {
                                // Nested pattern: use temporary + followup
                                let elem_loc = pattern_like_hir_loc(other);
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
                }
            }

            let temporary = lower_value_to_temporary(
                builder,
                InstructionValue::Destructure {
                    lvalue: LValuePattern {
                        pattern: Pattern::Object(ObjectPattern {
                            properties,
                            loc: convert_opt_loc(&pattern.base.loc),
                        }),
                        kind,
                    },
                    value: value.clone(),
                    loc: loc.clone(),
                },
            )?;

            for (place, path) in followups {
                let followup_loc = pattern_like_hir_loc(path).or(loc.clone());
                lower_assignment(builder, followup_loc, kind, path, place, assignment_style)?;
            }
            Ok(Some(temporary))
        }

        PatternLike::AssignmentPattern(pattern) => {
            // Default value: if value === undefined, use default, else use value
            let pat_loc = convert_opt_loc(&pattern.base.loc);

            let temp = build_temporary_place(builder, pat_loc.clone());

            let test_block = builder.reserve(BlockKind::Value);
            let continuation_block = builder.reserve(builder.current_block_kind());

            // Consequent: use default value
            let consequent = builder.try_enter(BlockKind::Value, |builder, _| {
                let default_value = lower_reorderable_expression(builder, &pattern.right)?;
                lower_value_to_temporary(
                    builder,
                    InstructionValue::StoreLocal {
                        lvalue: LValue {
                            place: temp.clone(),
                            kind: InstructionKind::Const,
                        },
                        value: default_value,
                        type_annotation: None,
                        loc: pat_loc.clone(),
                    },
                )?;
                Ok(Terminal::Goto {
                    block: continuation_block.id,
                    variant: GotoVariant::Break,
                    id: EvaluationOrder(0),
                    loc: pat_loc.clone(),
                })
            });

            // Alternate: use the original value
            let alternate = builder.try_enter(BlockKind::Value, |builder, _| {
                lower_value_to_temporary(
                    builder,
                    InstructionValue::StoreLocal {
                        lvalue: LValue {
                            place: temp.clone(),
                            kind: InstructionKind::Const,
                        },
                        value: value.clone(),
                        type_annotation: None,
                        loc: pat_loc.clone(),
                    },
                )?;
                Ok(Terminal::Goto {
                    block: continuation_block.id,
                    variant: GotoVariant::Break,
                    id: EvaluationOrder(0),
                    loc: pat_loc.clone(),
                })
            });

            // Ternary terminal
            builder.terminate_with_continuation(
                Terminal::Ternary {
                    test: test_block.id,
                    fallthrough: continuation_block.id,
                    id: EvaluationOrder(0),
                    loc: pat_loc.clone(),
                },
                test_block,
            );

            // In test block: check if value === undefined
            let undef = lower_value_to_temporary(
                builder,
                InstructionValue::Primitive {
                    value: PrimitiveValue::Undefined,
                    loc: pat_loc.clone(),
                },
            )?;
            let test = lower_value_to_temporary(
                builder,
                InstructionValue::BinaryExpression {
                    left: value,
                    operator: BinaryOperator::StrictEqual,
                    right: undef,
                    loc: pat_loc.clone(),
                },
            )?;
            builder.terminate_with_continuation(
                Terminal::Branch {
                    test,
                    consequent: consequent?,
                    alternate: alternate?,
                    fallthrough: continuation_block.id,
                    id: EvaluationOrder(0),
                    loc: pat_loc.clone(),
                },
                continuation_block,
            );

            // Recursively assign the resolved value to the left pattern
            Ok(lower_assignment(
                builder,
                pat_loc,
                kind,
                &pattern.left,
                temp,
                assignment_style,
            )?)
        }

        PatternLike::RestElement(rest) => {
            // Delegate to the argument pattern
            Ok(lower_assignment(
                builder,
                loc,
                kind,
                &rest.argument,
                value,
                assignment_style,
            )?)
        }

        // TS assignment-target wrappers (e.g. `(x as T) = ...`) and the Flow
        // analogue `TypeCastExpression`. For destructuring targets the
        // TS-faithful Todo is recorded once in `find_context_identifiers`, so
        // it is not recorded again here. `for (... of ...)` heads also reach
        // this arm directly without that Todo; emitted code matches the TS
        // reference there, but the recorded diagnostics do not yet.
        PatternLike::TSAsExpression(_)
        | PatternLike::TSSatisfiesExpression(_)
        | PatternLike::TSNonNullExpression(_)
        | PatternLike::TSTypeAssertion(_)
        | PatternLike::TypeCastExpression(_) => Ok(None),
    }
}

/// Helper to extract HIR loc from a PatternLike (converts AST loc)
pub(crate) fn pattern_like_hir_loc(pat: &react_compiler_ast::patterns::PatternLike) -> Option<SourceLocation> {
    convert_opt_loc(&pattern_like_loc(pat))
}

/// The style of assignment (used internally by lower_assignment).
#[derive(Clone, Copy)]
pub enum AssignmentStyle {
    /// Assignment via `=`
    Assignment,
    /// Destructuring assignment
    Destructure,
}
