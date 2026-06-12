
use react_compiler_diagnostics::CompilerDiagnostic;
use react_compiler_diagnostics::CompilerDiagnosticDetail;
use react_compiler_diagnostics::CompilerError;
use react_compiler_diagnostics::CompilerErrorDetail;
use react_compiler_diagnostics::ErrorCategory;
use react_compiler_hir::*;

use crate::hir_builder::HirBuilder;

#[allow(unused_imports)]
use super::*;

pub(crate) fn convert_binary_operator(op: &react_compiler_ast::operators::BinaryOperator) -> BinaryOperator {
    use react_compiler_ast::operators::BinaryOperator as AstOp;
    match op {
        AstOp::Add => BinaryOperator::Add,
        AstOp::Sub => BinaryOperator::Subtract,
        AstOp::Mul => BinaryOperator::Multiply,
        AstOp::Div => BinaryOperator::Divide,
        AstOp::Rem => BinaryOperator::Modulo,
        AstOp::Exp => BinaryOperator::Exponent,
        AstOp::Eq => BinaryOperator::Equal,
        AstOp::StrictEq => BinaryOperator::StrictEqual,
        AstOp::Neq => BinaryOperator::NotEqual,
        AstOp::StrictNeq => BinaryOperator::StrictNotEqual,
        AstOp::Lt => BinaryOperator::LessThan,
        AstOp::Lte => BinaryOperator::LessEqual,
        AstOp::Gt => BinaryOperator::GreaterThan,
        AstOp::Gte => BinaryOperator::GreaterEqual,
        AstOp::Shl => BinaryOperator::ShiftLeft,
        AstOp::Shr => BinaryOperator::ShiftRight,
        AstOp::UShr => BinaryOperator::UnsignedShiftRight,
        AstOp::BitOr => BinaryOperator::BitwiseOr,
        AstOp::BitXor => BinaryOperator::BitwiseXor,
        AstOp::BitAnd => BinaryOperator::BitwiseAnd,
        AstOp::In => BinaryOperator::In,
        AstOp::Instanceof => BinaryOperator::InstanceOf,
        AstOp::Pipeline => {
            unreachable!("Pipeline operator is checked before calling convert_binary_operator")
        }
    }
}

pub(crate) fn convert_unary_operator(op: &react_compiler_ast::operators::UnaryOperator) -> UnaryOperator {
    use react_compiler_ast::operators::UnaryOperator as AstOp;
    match op {
        AstOp::Neg => UnaryOperator::Minus,
        AstOp::Plus => UnaryOperator::Plus,
        AstOp::Not => UnaryOperator::Not,
        AstOp::BitNot => UnaryOperator::BitwiseNot,
        AstOp::TypeOf => UnaryOperator::TypeOf,
        AstOp::Void => UnaryOperator::Void,
        AstOp::Delete | AstOp::Throw => unreachable!("delete/throw handled separately"),
    }
}

// =============================================================================
// lower_identifier
// =============================================================================

/// Resolve an identifier to a Place.
///
/// For local/context identifiers, returns a Place referencing the binding's identifier.
/// For globals/imports, emits a LoadGlobal instruction and returns the temporary Place.
pub(crate) fn lower_identifier(
    builder: &mut HirBuilder,
    name: &str,
    start: u32,
    loc: Option<SourceLocation>,
    node_id: Option<u32>,
) -> Result<Place, CompilerError> {
    let binding = builder.resolve_identifier(name, start, loc.clone(), node_id)?;
    match binding {
        VariableBinding::Identifier { identifier, .. } => Ok(Place {
            identifier,
            effect: Effect::Unknown,
            reactive: false,
            loc,
        }),
        _ => {
            if let VariableBinding::Global { ref name } = binding {
                if name == "eval" {
                    builder.record_error(CompilerErrorDetail {
                        category: ErrorCategory::UnsupportedSyntax,
                        reason: "The 'eval' function is not supported".to_string(),
                        description: Some(
                            "Eval is an anti-pattern in JavaScript, and the code executed cannot be evaluated by React Compiler".to_string(),
                        ),
                        loc: loc.clone(),
                        suggestions: None,
                    })?;
                }
            }
            let non_local_binding = match binding {
                VariableBinding::Global { name } => NonLocalBinding::Global { name },
                VariableBinding::ImportDefault { name, module } => {
                    NonLocalBinding::ImportDefault { name, module }
                }
                VariableBinding::ImportSpecifier {
                    name,
                    module,
                    imported,
                } => NonLocalBinding::ImportSpecifier {
                    name,
                    module,
                    imported,
                },
                VariableBinding::ImportNamespace { name, module } => {
                    NonLocalBinding::ImportNamespace { name, module }
                }
                VariableBinding::ModuleLocal { name } => NonLocalBinding::ModuleLocal { name },
                VariableBinding::Identifier { .. } => unreachable!(),
            };
            let instr_value = InstructionValue::LoadGlobal {
                binding: non_local_binding,
                loc: loc.clone(),
            };
            Ok(lower_value_to_temporary(builder, instr_value)?)
        }
    }
}

// =============================================================================
// lower_arguments
// =============================================================================

pub(crate) fn lower_arguments(
    builder: &mut HirBuilder,
    args: &[react_compiler_ast::expressions::Expression],
) -> Result<Vec<PlaceOrSpread>, CompilerError> {
    use react_compiler_ast::expressions::Expression;
    let mut result = Vec::new();
    for arg in args {
        match arg {
            Expression::SpreadElement(spread) => {
                let place = lower_expression_to_temporary(builder, &spread.argument)?;
                result.push(PlaceOrSpread::Spread(SpreadPattern { place }));
            }
            _ => {
                let place = lower_expression_to_temporary(builder, arg)?;
                result.push(PlaceOrSpread::Place(place));
            }
        }
    }
    Ok(result)
}

pub(crate) fn convert_update_operator(op: &react_compiler_ast::operators::UpdateOperator) -> UpdateOperator {
    match op {
        react_compiler_ast::operators::UpdateOperator::Increment => UpdateOperator::Increment,
        react_compiler_ast::operators::UpdateOperator::Decrement => UpdateOperator::Decrement,
    }
}

// =============================================================================
// lower_member_expression
// =============================================================================

pub(crate) enum MemberProperty {
    Literal(PropertyLiteral),
    Computed(Place),
}

pub(crate) struct LoweredMemberExpression {
    object: Place,
    property: MemberProperty,
    value: InstructionValue,
}

pub(crate) fn lower_member_expression(
    builder: &mut HirBuilder,
    member: &react_compiler_ast::expressions::MemberExpression,
) -> Result<LoweredMemberExpression, CompilerError> {
    Ok(lower_member_expression_impl(builder, member, None)?)
}

pub(crate) fn lower_member_expression_with_object(
    builder: &mut HirBuilder,
    member: &react_compiler_ast::expressions::OptionalMemberExpression,
    lowered_object: Place,
) -> Result<LoweredMemberExpression, CompilerError> {
    // OptionalMemberExpression has the same shape as MemberExpression for property access
    use react_compiler_ast::expressions::Expression;
    let loc = convert_opt_loc(&member.base.loc);
    let object = lowered_object;

    if !member.computed {
        let prop_literal = match member.property.as_ref() {
            Expression::Identifier(id) => PropertyLiteral::String(id.name.clone()),
            Expression::NumericLiteral(lit) => {
                PropertyLiteral::Number(FloatValue::new(lit.precise_value()))
            }
            _ => {
                builder.record_error(CompilerErrorDetail {
                    category: ErrorCategory::Todo,
                    reason: format!(
                        "(BuildHIR::lowerMemberExpression) Handle {:?} property",
                        member.property
                    ),
                    description: None,
                    loc: loc.clone(),
                    suggestions: None,
                })?;
                return Ok(LoweredMemberExpression {
                    object,
                    property: MemberProperty::Literal(PropertyLiteral::String("".to_string())),
                    value: InstructionValue::UnsupportedNode {
                        node_type: Some("OptionalMemberExpression".to_string()),
                        original_node: serialize_expression(
                            &react_compiler_ast::expressions::Expression::OptionalMemberExpression(
                                member.clone(),
                            ),
                        ),
                        loc,
                    },
                });
            }
        };
        let value = InstructionValue::PropertyLoad {
            object: object.clone(),
            property: prop_literal.clone(),
            loc,
        };
        Ok(LoweredMemberExpression {
            object,
            property: MemberProperty::Literal(prop_literal),
            value,
        })
    } else {
        if let Expression::NumericLiteral(lit) = member.property.as_ref() {
            let prop_literal = PropertyLiteral::Number(FloatValue::new(lit.precise_value()));
            let value = InstructionValue::PropertyLoad {
                object: object.clone(),
                property: prop_literal.clone(),
                loc,
            };
            return Ok(LoweredMemberExpression {
                object,
                property: MemberProperty::Literal(prop_literal),
                value,
            });
        }
        let property = lower_expression_to_temporary(builder, &member.property)?;
        let value = InstructionValue::ComputedLoad {
            object: object.clone(),
            property: property.clone(),
            loc,
        };
        Ok(LoweredMemberExpression {
            object,
            property: MemberProperty::Computed(property),
            value,
        })
    }
}

pub(crate) fn lower_member_expression_impl(
    builder: &mut HirBuilder,
    member: &react_compiler_ast::expressions::MemberExpression,
    lowered_object: Option<Place>,
) -> Result<LoweredMemberExpression, CompilerError> {
    use react_compiler_ast::expressions::Expression;
    let loc = convert_opt_loc(&member.base.loc);
    let object = match lowered_object {
        Some(obj) => obj,
        None => lower_expression_to_temporary(builder, &member.object)?,
    };

    if !member.computed {
        // Non-computed: property must be an identifier or numeric literal
        let prop_literal = match member.property.as_ref() {
            Expression::Identifier(id) => PropertyLiteral::String(id.name.clone()),
            Expression::NumericLiteral(lit) => {
                PropertyLiteral::Number(FloatValue::new(lit.precise_value()))
            }
            _ => {
                builder.record_error(CompilerErrorDetail {
                    category: ErrorCategory::Todo,
                    reason: format!(
                        "(BuildHIR::lowerMemberExpression) Handle {:?} property",
                        member.property
                    ),
                    description: None,
                    loc: loc.clone(),
                    suggestions: None,
                })?;
                return Ok(LoweredMemberExpression {
                    object,
                    property: MemberProperty::Literal(PropertyLiteral::String("".to_string())),
                    value: InstructionValue::UnsupportedNode {
                        node_type: Some("MemberExpression".to_string()),
                        original_node: serialize_expression(
                            &react_compiler_ast::expressions::Expression::MemberExpression(
                                member.clone(),
                            ),
                        ),
                        loc,
                    },
                });
            }
        };
        let value = InstructionValue::PropertyLoad {
            object: object.clone(),
            property: prop_literal.clone(),
            loc,
        };
        Ok(LoweredMemberExpression {
            object,
            property: MemberProperty::Literal(prop_literal),
            value,
        })
    } else {
        // Computed: check for numeric literal first (treated as PropertyLoad in TS)
        if let Expression::NumericLiteral(lit) = member.property.as_ref() {
            let prop_literal = PropertyLiteral::Number(FloatValue::new(lit.precise_value()));
            let value = InstructionValue::PropertyLoad {
                object: object.clone(),
                property: prop_literal.clone(),
                loc,
            };
            return Ok(LoweredMemberExpression {
                object,
                property: MemberProperty::Literal(prop_literal),
                value,
            });
        }
        // Otherwise lower property to temporary for ComputedLoad
        let property = lower_expression_to_temporary(builder, &member.property)?;
        let value = InstructionValue::ComputedLoad {
            object: object.clone(),
            property: property.clone(),
            loc,
        };
        Ok(LoweredMemberExpression {
            object,
            property: MemberProperty::Computed(property),
            value,
        })
    }
}

// =============================================================================
// lower_expression
// =============================================================================

pub(crate) fn lower_expression(
    builder: &mut HirBuilder,
    expr: &react_compiler_ast::expressions::Expression,
) -> Result<InstructionValue, CompilerError> {
    use react_compiler_ast::expressions::Expression;

    match expr {
        Expression::Identifier(ident) => {
            let loc = convert_opt_loc(&ident.base.loc);
            let start = ident.base.start.unwrap_or(0);
            let place =
                lower_identifier(builder, &ident.name, start, loc.clone(), ident.base.node_id)?;
            // Determine LoadLocal vs LoadContext based on context identifier check
            if builder.is_context_identifier(&ident.name, start, ident.base.node_id) {
                Ok(InstructionValue::LoadContext { place, loc })
            } else {
                Ok(InstructionValue::LoadLocal { place, loc })
            }
        }
        Expression::NullLiteral(lit) => {
            let loc = convert_opt_loc(&lit.base.loc);
            Ok(InstructionValue::Primitive {
                value: PrimitiveValue::Null,
                loc,
            })
        }
        Expression::BooleanLiteral(lit) => {
            let loc = convert_opt_loc(&lit.base.loc);
            Ok(InstructionValue::Primitive {
                value: PrimitiveValue::Boolean(lit.value),
                loc,
            })
        }
        Expression::NumericLiteral(lit) => {
            let loc = convert_opt_loc(&lit.base.loc);
            Ok(InstructionValue::Primitive {
                value: PrimitiveValue::Number(FloatValue::new(lit.precise_value())),
                loc,
            })
        }
        Expression::StringLiteral(lit) => {
            let loc = convert_opt_loc(&lit.base.loc);
            Ok(InstructionValue::Primitive {
                value: PrimitiveValue::String(lit.value.clone()),
                loc,
            })
        }
        Expression::BinaryExpression(bin) => {
            let loc = convert_opt_loc(&bin.base.loc);
            // Check for pipeline operator before lowering operands
            if matches!(
                bin.operator,
                react_compiler_ast::operators::BinaryOperator::Pipeline
            ) {
                builder.record_error(CompilerErrorDetail {
                    category: ErrorCategory::Todo,
                    reason: "(BuildHIR::lowerExpression) Pipe operator not supported".to_string(),
                    description: None,
                    loc: loc.clone(),
                    suggestions: None,
                })?;
                return Ok(InstructionValue::UnsupportedNode {
                    node_type: Some("BinaryExpression".to_string()),
                    original_node: serialize_expression(expr),
                    loc,
                });
            }
            let left = lower_expression_to_temporary(builder, &bin.left)?;
            let right = lower_expression_to_temporary(builder, &bin.right)?;
            let operator = convert_binary_operator(&bin.operator);
            Ok(InstructionValue::BinaryExpression {
                operator,
                left,
                right,
                loc,
            })
        }
        Expression::UnaryExpression(unary) => {
            let loc = convert_opt_loc(&unary.base.loc);
            match &unary.operator {
                react_compiler_ast::operators::UnaryOperator::Delete => {
                    // Delete can be on member expressions or identifiers
                    let loc = convert_opt_loc(&unary.base.loc);
                    match &*unary.argument {
                        Expression::MemberExpression(member) => {
                            let object = lower_expression_to_temporary(builder, &member.object)?;
                            if !member.computed {
                                match &*member.property {
                                    Expression::Identifier(prop_id) => {
                                        Ok(InstructionValue::PropertyDelete {
                                            object,
                                            property: PropertyLiteral::String(prop_id.name.clone()),
                                            loc,
                                        })
                                    }
                                    _ => {
                                        builder.record_error(CompilerErrorDetail {
                                            reason: "Unsupported delete target".to_string(),
                                            category: ErrorCategory::Todo,
                                            loc: loc.clone(),
                                            description: None,
                                            suggestions: None,
                                        })?;
                                        Ok(InstructionValue::UnsupportedNode {
                                            node_type: Some("UnaryExpression".to_string()),
                                            original_node: serialize_expression(expr),
                                            loc,
                                        })
                                    }
                                }
                            } else {
                                let property =
                                    lower_expression_to_temporary(builder, &member.property)?;
                                Ok(InstructionValue::ComputedDelete {
                                    object,
                                    property,
                                    loc,
                                })
                            }
                        }
                        _ => {
                            // delete on non-member expression (e.g., optional chain, identifier)
                            builder.record_error(CompilerErrorDetail {
                                reason: "Only object properties can be deleted".to_string(),
                                category: ErrorCategory::Syntax,
                                loc: loc.clone(),
                                description: None,
                                suggestions: None,
                            })?;
                            Ok(InstructionValue::UnsupportedNode {
                                node_type: Some("UnaryExpression".to_string()),
                                original_node: serialize_expression(expr),
                                loc,
                            })
                        }
                    }
                }
                react_compiler_ast::operators::UnaryOperator::Throw => {
                    // throw as unary operator (Babel-specific)
                    let loc = convert_opt_loc(&unary.base.loc);
                    builder.record_error(CompilerErrorDetail {
                        reason: "throw expressions are not supported".to_string(),
                        category: ErrorCategory::Todo,
                        loc: loc.clone(),
                        description: None,
                        suggestions: None,
                    })?;
                    Ok(InstructionValue::UnsupportedNode {
                        node_type: Some("UnaryExpression".to_string()),
                        original_node: serialize_expression(expr),
                        loc,
                    })
                }
                op => {
                    let value = lower_expression_to_temporary(builder, &unary.argument)?;
                    let operator = convert_unary_operator(op);
                    Ok(InstructionValue::UnaryExpression {
                        operator,
                        value,
                        loc,
                    })
                }
            }
        }
        Expression::CallExpression(call) => {
            let loc = convert_opt_loc(&call.base.loc);
            // Check if callee is a MemberExpression => MethodCall
            if let Expression::MemberExpression(member) = call.callee.as_ref() {
                let lowered = lower_member_expression(builder, member)?;
                let property = lower_value_to_temporary(builder, lowered.value)?;
                let args = lower_arguments(builder, &call.arguments)?;
                Ok(InstructionValue::MethodCall {
                    receiver: lowered.object,
                    property,
                    args,
                    loc,
                })
            } else {
                let callee = lower_expression_to_temporary(builder, &call.callee)?;
                let args = lower_arguments(builder, &call.arguments)?;
                Ok(InstructionValue::CallExpression { callee, args, loc })
            }
        }
        Expression::MemberExpression(member) => {
            let lowered = lower_member_expression(builder, member)?;
            Ok(lowered.value)
        }
        Expression::OptionalCallExpression(opt_call) => {
            Ok(lower_optional_call_expression(builder, opt_call)?)
        }
        Expression::OptionalMemberExpression(opt_member) => {
            Ok(lower_optional_member_expression(builder, opt_member)?)
        }
        Expression::LogicalExpression(expr) => {
            let loc = convert_opt_loc(&expr.base.loc);
            let continuation_block = builder.reserve(builder.current_block_kind());
            let continuation_id = continuation_block.id;
            let test_block = builder.reserve(BlockKind::Value);
            let test_block_id = test_block.id;
            let place = build_temporary_place(builder, loc.clone());
            let left_loc = expression_loc(&expr.left);
            let left_place = build_temporary_place(builder, left_loc);

            // Block for short-circuit case: store left value as result, goto continuation
            let consequent_block = builder.try_enter(BlockKind::Value, |builder, _block_id| {
                lower_value_to_temporary(
                    builder,
                    InstructionValue::StoreLocal {
                        lvalue: LValue {
                            kind: InstructionKind::Const,
                            place: place.clone(),
                        },
                        value: left_place.clone(),
                        type_annotation: None,
                        loc: left_place.loc.clone(),
                    },
                )?;
                Ok(Terminal::Goto {
                    block: continuation_id,
                    variant: GotoVariant::Break,
                    id: EvaluationOrder(0),
                    loc: left_place.loc.clone(),
                })
            });

            // Block for evaluating right side
            let alternate_block = builder.try_enter(BlockKind::Value, |builder, _block_id| {
                let right = lower_expression_to_temporary(builder, &expr.right)?;
                let right_loc = right.loc.clone();
                lower_value_to_temporary(
                    builder,
                    InstructionValue::StoreLocal {
                        lvalue: LValue {
                            kind: InstructionKind::Const,
                            place: place.clone(),
                        },
                        value: right,
                        type_annotation: None,
                        loc: right_loc.clone(),
                    },
                )?;
                Ok(Terminal::Goto {
                    block: continuation_id,
                    variant: GotoVariant::Break,
                    id: EvaluationOrder(0),
                    loc: right_loc,
                })
            });

            let hir_op = match expr.operator {
                react_compiler_ast::operators::LogicalOperator::And => LogicalOperator::And,
                react_compiler_ast::operators::LogicalOperator::Or => LogicalOperator::Or,
                react_compiler_ast::operators::LogicalOperator::NullishCoalescing => {
                    LogicalOperator::NullishCoalescing
                }
            };

            builder.terminate_with_continuation(
                Terminal::Logical {
                    operator: hir_op,
                    test: test_block_id,
                    fallthrough: continuation_id,
                    id: EvaluationOrder(0),
                    loc: loc.clone(),
                },
                test_block,
            );

            // Now in test block: lower left expression, copy to left_place
            let left_value = lower_expression_to_temporary(builder, &expr.left)?;
            builder.push(Instruction {
                id: EvaluationOrder(0),
                lvalue: left_place.clone(),
                value: InstructionValue::LoadLocal {
                    place: left_value,
                    loc: loc.clone(),
                },
                effects: None,
                loc: loc.clone(),
            });

            builder.terminate_with_continuation(
                Terminal::Branch {
                    test: left_place,
                    consequent: consequent_block?,
                    alternate: alternate_block?,
                    fallthrough: continuation_id,
                    id: EvaluationOrder(0),
                    loc: loc.clone(),
                },
                continuation_block,
            );

            Ok(InstructionValue::LoadLocal {
                place: place.clone(),
                loc: place.loc.clone(),
            })
        }
        Expression::UpdateExpression(update) => {
            let loc = convert_opt_loc(&update.base.loc);
            match update.argument.as_ref() {
                Expression::MemberExpression(member) => {
                    let binary_op = match &update.operator {
                        react_compiler_ast::operators::UpdateOperator::Increment => {
                            BinaryOperator::Add
                        }
                        react_compiler_ast::operators::UpdateOperator::Decrement => {
                            BinaryOperator::Subtract
                        }
                    };
                    // Use the member expression's loc (not the update expression's)
                    // to match TS behavior where the inner operations use leftExpr.node.loc
                    let member_loc = convert_opt_loc(&member.base.loc);
                    let lowered = lower_member_expression(builder, member)?;
                    let object = lowered.object;
                    let lowered_property = lowered.property;
                    let prev_value = lower_value_to_temporary(builder, lowered.value)?;

                    let one = lower_value_to_temporary(
                        builder,
                        InstructionValue::Primitive {
                            value: PrimitiveValue::Number(FloatValue::new(1.0)),
                            loc: None,
                        },
                    )?;
                    let updated = lower_value_to_temporary(
                        builder,
                        InstructionValue::BinaryExpression {
                            operator: binary_op,
                            left: prev_value.clone(),
                            right: one,
                            loc: member_loc.clone(),
                        },
                    )?;

                    // Store back using the property from the lowered member expression.
                    // For prefix, the result is the PropertyStore/ComputedStore lvalue
                    // (matching TS which uses newValuePlace). For postfix, it's prev_value.
                    let new_value_place = match lowered_property {
                        MemberProperty::Literal(prop_literal) => lower_value_to_temporary(
                            builder,
                            InstructionValue::PropertyStore {
                                object,
                                property: prop_literal,
                                value: updated.clone(),
                                loc: member_loc,
                            },
                        )?,
                        MemberProperty::Computed(prop_place) => lower_value_to_temporary(
                            builder,
                            InstructionValue::ComputedStore {
                                object,
                                property: prop_place,
                                value: updated.clone(),
                                loc: member_loc,
                            },
                        )?,
                    };

                    // Return previous for postfix, newValuePlace for prefix
                    let result_place = if update.prefix {
                        new_value_place
                    } else {
                        prev_value
                    };
                    Ok(InstructionValue::LoadLocal {
                        place: result_place.clone(),
                        loc: result_place.loc.clone(),
                    })
                }
                Expression::Identifier(ident) => {
                    let start = ident.base.start.unwrap_or(0);
                    if builder.is_context_identifier(&ident.name, start, ident.base.node_id) {
                        builder.record_error(CompilerErrorDetail {
                            category: ErrorCategory::Todo,
                            reason: "(BuildHIR::lowerExpression) Handle UpdateExpression to variables captured within lambdas.".to_string(),
                            description: None,
                            loc: loc.clone(),
                            suggestions: None,
                        })?;
                        return Ok(InstructionValue::UnsupportedNode {
                            node_type: Some("UpdateExpression".to_string()),
                            original_node: serialize_expression(expr),
                            loc,
                        });
                    }

                    let ident_loc = convert_opt_loc(&ident.base.loc);
                    let binding = builder.resolve_identifier(
                        &ident.name,
                        start,
                        ident_loc.clone(),
                        ident.base.node_id,
                    )?;
                    match &binding {
                        VariableBinding::Global { .. } => {
                            builder.record_error(CompilerErrorDetail {
                                category: ErrorCategory::Todo,
                                reason: "UpdateExpression where argument is a global is not yet supported".to_string(),
                                description: None,
                                loc: loc.clone(),
                                suggestions: None,
                            })?;
                            return Ok(InstructionValue::UnsupportedNode {
                                node_type: Some("UpdateExpression".to_string()),
                                original_node: serialize_expression(expr),
                                loc,
                            });
                        }
                        _ => {}
                    }
                    let identifier = match binding {
                        VariableBinding::Identifier { identifier, .. } => identifier,
                        _ => {
                            builder.record_error(CompilerErrorDetail {
                                category: ErrorCategory::Todo,
                                reason: "(BuildHIR::lowerExpression) Support UpdateExpression where argument is a global".to_string(),
                                description: None,
                                loc: loc.clone(),
                                suggestions: None,
                            })?;
                            return Ok(InstructionValue::UnsupportedNode {
                                node_type: Some("UpdateExpression".to_string()),
                                original_node: serialize_expression(expr),
                                loc,
                            });
                        }
                    };
                    let lvalue_place = Place {
                        identifier,
                        effect: Effect::Unknown,
                        reactive: false,
                        loc: ident_loc.clone(),
                    };

                    // Load the current value
                    let value = lower_identifier(
                        builder,
                        &ident.name,
                        start,
                        ident_loc,
                        ident.base.node_id,
                    )?;

                    let operation = convert_update_operator(&update.operator);

                    if update.prefix {
                        Ok(InstructionValue::PrefixUpdate {
                            lvalue: lvalue_place,
                            operation,
                            value,
                            loc,
                        })
                    } else {
                        Ok(InstructionValue::PostfixUpdate {
                            lvalue: lvalue_place,
                            operation,
                            value,
                            loc,
                        })
                    }
                }
                _ => {
                    builder.record_error(CompilerErrorDetail {
                        category: ErrorCategory::Todo,
                        reason: format!("UpdateExpression with unsupported argument type"),
                        description: None,
                        loc: loc.clone(),
                        suggestions: None,
                    })?;
                    Ok(InstructionValue::UnsupportedNode {
                        node_type: Some("UpdateExpression".to_string()),
                        original_node: serialize_expression(expr),
                        loc,
                    })
                }
            }
        }
        Expression::ConditionalExpression(expr) => {
            let loc = convert_opt_loc(&expr.base.loc);
            let continuation_block = builder.reserve(builder.current_block_kind());
            let continuation_id = continuation_block.id;
            let test_block = builder.reserve(BlockKind::Value);
            let test_block_id = test_block.id;
            let place = build_temporary_place(builder, loc.clone());

            // Block for the consequent (test is truthy)
            let consequent_ast_loc = expression_loc(&expr.consequent);
            let consequent_block = builder.try_enter(BlockKind::Value, |builder, _block_id| {
                let consequent = lower_expression_to_temporary(builder, &expr.consequent)?;
                lower_value_to_temporary(
                    builder,
                    InstructionValue::StoreLocal {
                        lvalue: LValue {
                            kind: InstructionKind::Const,
                            place: place.clone(),
                        },
                        value: consequent,
                        type_annotation: None,
                        loc: loc.clone(),
                    },
                )?;
                Ok(Terminal::Goto {
                    block: continuation_id,
                    variant: GotoVariant::Break,
                    id: EvaluationOrder(0),
                    loc: consequent_ast_loc,
                })
            });

            // Block for the alternate (test is falsy)
            let alternate_ast_loc = expression_loc(&expr.alternate);
            let alternate_block = builder.try_enter(BlockKind::Value, |builder, _block_id| {
                let alternate = lower_expression_to_temporary(builder, &expr.alternate)?;
                lower_value_to_temporary(
                    builder,
                    InstructionValue::StoreLocal {
                        lvalue: LValue {
                            kind: InstructionKind::Const,
                            place: place.clone(),
                        },
                        value: alternate,
                        type_annotation: None,
                        loc: loc.clone(),
                    },
                )?;
                Ok(Terminal::Goto {
                    block: continuation_id,
                    variant: GotoVariant::Break,
                    id: EvaluationOrder(0),
                    loc: alternate_ast_loc,
                })
            });

            builder.terminate_with_continuation(
                Terminal::Ternary {
                    test: test_block_id,
                    fallthrough: continuation_id,
                    id: EvaluationOrder(0),
                    loc: loc.clone(),
                },
                test_block,
            );

            // Now in test block: lower test expression
            let test_place = lower_expression_to_temporary(builder, &expr.test)?;
            builder.terminate_with_continuation(
                Terminal::Branch {
                    test: test_place,
                    consequent: consequent_block?,
                    alternate: alternate_block?,
                    fallthrough: continuation_id,
                    id: EvaluationOrder(0),
                    loc: loc.clone(),
                },
                continuation_block,
            );

            Ok(InstructionValue::LoadLocal {
                place: place.clone(),
                loc: place.loc.clone(),
            })
        }
        Expression::AssignmentExpression(expr) => {
            use react_compiler_ast::operators::AssignmentOperator;
            let loc = convert_opt_loc(&expr.base.loc);

            if matches!(expr.operator, AssignmentOperator::Assign) {
                // Simple `=` assignment
                match &*expr.left {
                    react_compiler_ast::patterns::PatternLike::Identifier(ident) => {
                        // Handle simple identifier assignment directly
                        let start = ident.base.start.unwrap_or(0);
                        let right = lower_expression_to_temporary(builder, &expr.right)?;
                        let ident_loc = convert_opt_loc(&ident.base.loc);
                        let binding = builder.resolve_identifier(
                            &ident.name,
                            start,
                            ident_loc.clone(),
                            ident.base.node_id,
                        )?;
                        match binding {
                            VariableBinding::Identifier {
                                identifier,
                                binding_kind,
                            } => {
                                // Check for const reassignment
                                if binding_kind == BindingKind::Const {
                                    builder.record_error(CompilerErrorDetail {
                                        reason: "Cannot reassign a `const` variable".to_string(),
                                        category: ErrorCategory::Syntax,
                                        loc: ident_loc.clone(),
                                        description: Some(format!(
                                            "`{}` is declared as const",
                                            &ident.name
                                        )),
                                        suggestions: None,
                                    })?;
                                    return Ok(InstructionValue::UnsupportedNode {
                                        node_type: Some("Identifier".to_string()),
                                        original_node: serialize_expression(
                                            &Expression::AssignmentExpression(expr.clone()),
                                        ),
                                        loc: ident_loc,
                                    });
                                }
                                let place = Place {
                                    identifier,
                                    reactive: false,
                                    effect: Effect::Unknown,
                                    loc: ident_loc,
                                };
                                if builder.is_context_identifier(
                                    &ident.name,
                                    start,
                                    ident.base.node_id,
                                ) {
                                    let temp = lower_value_to_temporary(
                                        builder,
                                        InstructionValue::StoreContext {
                                            lvalue: LValue {
                                                kind: InstructionKind::Reassign,
                                                place: place.clone(),
                                            },
                                            value: right,
                                            loc: place.loc.clone(),
                                        },
                                    )?;
                                    Ok(InstructionValue::LoadLocal {
                                        place: temp.clone(),
                                        loc: temp.loc.clone(),
                                    })
                                } else {
                                    let temp = lower_value_to_temporary(
                                        builder,
                                        InstructionValue::StoreLocal {
                                            lvalue: LValue {
                                                kind: InstructionKind::Reassign,
                                                place: place.clone(),
                                            },
                                            value: right,
                                            type_annotation: None,
                                            loc: place.loc.clone(),
                                        },
                                    )?;
                                    Ok(InstructionValue::LoadLocal {
                                        place: temp.clone(),
                                        loc: temp.loc.clone(),
                                    })
                                }
                            }
                            _ => {
                                // Global or import assignment
                                let name = ident.name.clone();
                                let temp = lower_value_to_temporary(
                                    builder,
                                    InstructionValue::StoreGlobal {
                                        name,
                                        value: right,
                                        loc: ident_loc,
                                    },
                                )?;
                                Ok(InstructionValue::LoadLocal {
                                    place: temp.clone(),
                                    loc: temp.loc.clone(),
                                })
                            }
                        }
                    }
                    react_compiler_ast::patterns::PatternLike::MemberExpression(member) => {
                        // Member expression assignment: a.b = value or a[b] = value
                        let right = lower_expression_to_temporary(builder, &expr.right)?;
                        let left_loc = convert_opt_loc(&member.base.loc);
                        let object = lower_expression_to_temporary(builder, &member.object)?;
                        let temp = if !member.computed
                            || matches!(
                                &*member.property,
                                react_compiler_ast::expressions::Expression::NumericLiteral(_)
                            ) {
                            match &*member.property {
                                react_compiler_ast::expressions::Expression::Identifier(
                                    prop_id,
                                ) => lower_value_to_temporary(
                                    builder,
                                    InstructionValue::PropertyStore {
                                        object,
                                        property: PropertyLiteral::String(prop_id.name.clone()),
                                        value: right,
                                        loc: left_loc,
                                    },
                                )?,
                                react_compiler_ast::expressions::Expression::NumericLiteral(
                                    num,
                                ) => lower_value_to_temporary(
                                    builder,
                                    InstructionValue::PropertyStore {
                                        object,
                                        property: PropertyLiteral::Number(FloatValue::new(
                                            num.precise_value(),
                                        )),
                                        value: right,
                                        loc: left_loc,
                                    },
                                )?,
                                _ => {
                                    let prop =
                                        lower_expression_to_temporary(builder, &member.property)?;
                                    lower_value_to_temporary(
                                        builder,
                                        InstructionValue::ComputedStore {
                                            object,
                                            property: prop,
                                            value: right,
                                            loc: left_loc,
                                        },
                                    )?
                                }
                            }
                        } else {
                            let prop = lower_expression_to_temporary(builder, &member.property)?;
                            lower_value_to_temporary(
                                builder,
                                InstructionValue::ComputedStore {
                                    object,
                                    property: prop,
                                    value: right,
                                    loc: left_loc,
                                },
                            )?
                        };
                        Ok(InstructionValue::LoadLocal {
                            place: temp.clone(),
                            loc: temp.loc.clone(),
                        })
                    }
                    _ => {
                        // Destructuring assignment
                        let right = lower_expression_to_temporary(builder, &expr.right)?;
                        let left_loc = pattern_like_hir_loc(&expr.left);
                        let result = lower_assignment(
                            builder,
                            left_loc,
                            InstructionKind::Reassign,
                            &expr.left,
                            right.clone(),
                            AssignmentStyle::Destructure,
                        )?;
                        match result {
                            Some(place) => Ok(InstructionValue::LoadLocal {
                                place: place.clone(),
                                loc: place.loc.clone(),
                            }),
                            None => Ok(InstructionValue::LoadLocal { place: right, loc }),
                        }
                    }
                }
            } else {
                // Compound assignment operators
                let binary_op = match expr.operator {
                    AssignmentOperator::AddAssign => Some(BinaryOperator::Add),
                    AssignmentOperator::SubAssign => Some(BinaryOperator::Subtract),
                    AssignmentOperator::MulAssign => Some(BinaryOperator::Multiply),
                    AssignmentOperator::DivAssign => Some(BinaryOperator::Divide),
                    AssignmentOperator::RemAssign => Some(BinaryOperator::Modulo),
                    AssignmentOperator::ExpAssign => Some(BinaryOperator::Exponent),
                    AssignmentOperator::ShlAssign => Some(BinaryOperator::ShiftLeft),
                    AssignmentOperator::ShrAssign => Some(BinaryOperator::ShiftRight),
                    AssignmentOperator::UShrAssign => Some(BinaryOperator::UnsignedShiftRight),
                    AssignmentOperator::BitOrAssign => Some(BinaryOperator::BitwiseOr),
                    AssignmentOperator::BitXorAssign => Some(BinaryOperator::BitwiseXor),
                    AssignmentOperator::BitAndAssign => Some(BinaryOperator::BitwiseAnd),
                    AssignmentOperator::OrAssign
                    | AssignmentOperator::AndAssign
                    | AssignmentOperator::NullishAssign => {
                        // Logical assignment operators (||=, &&=, ??=) - not yet supported
                        builder.record_error(CompilerErrorDetail {
                            reason:
                                "Logical assignment operators (||=, &&=, ??=) are not yet supported"
                                    .to_string(),
                            category: ErrorCategory::Todo,
                            loc: loc.clone(),
                            description: None,
                            suggestions: None,
                        })?;
                        return Ok(InstructionValue::UnsupportedNode {
                            node_type: Some("AssignmentExpression".to_string()),
                            original_node: serialize_expression(&Expression::AssignmentExpression(
                                expr.clone(),
                            )),
                            loc,
                        });
                    }
                    AssignmentOperator::Assign => unreachable!(),
                };
                let binary_op = match binary_op {
                    Some(op) => op,
                    None => {
                        return Ok(InstructionValue::UnsupportedNode {
                            node_type: Some("AssignmentExpression".to_string()),
                            original_node: serialize_expression(&Expression::AssignmentExpression(
                                expr.clone(),
                            )),
                            loc,
                        });
                    }
                };

                match &*expr.left {
                    react_compiler_ast::patterns::PatternLike::Identifier(ident) => {
                        let start = ident.base.start.unwrap_or(0);
                        let left_place = lower_expression_to_temporary(
                            builder,
                            &react_compiler_ast::expressions::Expression::Identifier(ident.clone()),
                        )?;
                        let right = lower_expression_to_temporary(builder, &expr.right)?;
                        let binary_place = lower_value_to_temporary(
                            builder,
                            InstructionValue::BinaryExpression {
                                operator: binary_op,
                                left: left_place,
                                right,
                                loc: loc.clone(),
                            },
                        )?;
                        let ident_loc = convert_opt_loc(&ident.base.loc);
                        let binding = builder.resolve_identifier(
                            &ident.name,
                            start,
                            ident_loc.clone(),
                            ident.base.node_id,
                        )?;
                        match binding {
                            VariableBinding::Identifier { identifier, .. } => {
                                let place = Place {
                                    identifier,
                                    reactive: false,
                                    effect: Effect::Unknown,
                                    loc: ident_loc,
                                };
                                if builder.is_context_identifier(
                                    &ident.name,
                                    start,
                                    ident.base.node_id,
                                ) {
                                    lower_value_to_temporary(
                                        builder,
                                        InstructionValue::StoreContext {
                                            lvalue: LValue {
                                                kind: InstructionKind::Reassign,
                                                place: place.clone(),
                                            },
                                            value: binary_place,
                                            loc: loc.clone(),
                                        },
                                    )?;
                                    Ok(InstructionValue::LoadContext { place, loc })
                                } else {
                                    lower_value_to_temporary(
                                        builder,
                                        InstructionValue::StoreLocal {
                                            lvalue: LValue {
                                                kind: InstructionKind::Reassign,
                                                place: place.clone(),
                                            },
                                            value: binary_place,
                                            type_annotation: None,
                                            loc: loc.clone(),
                                        },
                                    )?;
                                    Ok(InstructionValue::LoadLocal { place, loc })
                                }
                            }
                            _ => {
                                // Global assignment
                                let name = ident.name.clone();
                                let temp = lower_value_to_temporary(
                                    builder,
                                    InstructionValue::StoreGlobal {
                                        name,
                                        value: binary_place,
                                        loc: loc.clone(),
                                    },
                                )?;
                                Ok(InstructionValue::LoadLocal {
                                    place: temp.clone(),
                                    loc: temp.loc.clone(),
                                })
                            }
                        }
                    }
                    react_compiler_ast::patterns::PatternLike::MemberExpression(member) => {
                        // a.b += right: read, compute, store
                        // Match TS behavior: return the PropertyStore/ComputedStore value
                        // directly (let the caller lower it to a temporary)
                        let member_loc = convert_opt_loc(&member.base.loc);
                        let lowered = lower_member_expression(builder, member)?;
                        let object = lowered.object;
                        let lowered_property = lowered.property;
                        let current_value = lower_value_to_temporary(builder, lowered.value)?;
                        let right = lower_expression_to_temporary(builder, &expr.right)?;
                        let result = lower_value_to_temporary(
                            builder,
                            InstructionValue::BinaryExpression {
                                operator: binary_op,
                                left: current_value,
                                right,
                                loc: member_loc.clone(),
                            },
                        )?;
                        // Return the store instruction value directly (matching TS behavior)
                        match lowered_property {
                            MemberProperty::Literal(prop_literal) => {
                                Ok(InstructionValue::PropertyStore {
                                    object,
                                    property: prop_literal,
                                    value: result,
                                    loc: member_loc,
                                })
                            }
                            MemberProperty::Computed(prop_place) => {
                                Ok(InstructionValue::ComputedStore {
                                    object,
                                    property: prop_place,
                                    value: result,
                                    loc: member_loc,
                                })
                            }
                        }
                    }
                    _ => {
                        builder.record_error(CompilerErrorDetail {
                            reason: "Compound assignment to complex pattern is not yet supported"
                                .to_string(),
                            category: ErrorCategory::Todo,
                            loc: loc.clone(),
                            description: None,
                            suggestions: None,
                        })?;
                        Ok(InstructionValue::UnsupportedNode {
                            node_type: Some("AssignmentExpression".to_string()),
                            original_node: serialize_expression(&Expression::AssignmentExpression(
                                expr.clone(),
                            )),
                            loc,
                        })
                    }
                }
            }
        }
        Expression::SequenceExpression(seq) => {
            let loc = convert_opt_loc(&seq.base.loc);

            if seq.expressions.is_empty() {
                builder.record_error(CompilerErrorDetail {
                    category: ErrorCategory::Syntax,
                    reason: "Expected sequence expression to have at least one expression"
                        .to_string(),
                    description: None,
                    loc: loc.clone(),
                    suggestions: None,
                })?;
                return Ok(InstructionValue::UnsupportedNode {
                    node_type: Some("SequenceExpression".to_string()),
                    original_node: serialize_expression(expr),
                    loc,
                });
            }

            let continuation_block = builder.reserve(builder.current_block_kind());
            let continuation_id = continuation_block.id;
            let place = build_temporary_place(builder, loc.clone());

            let sequence_block = builder.try_enter(BlockKind::Sequence, |builder, _block_id| {
                let mut last: Option<Place> = None;
                for item in &seq.expressions {
                    last = Some(lower_expression_to_temporary(builder, item)?);
                }
                if let Some(last) = last {
                    lower_value_to_temporary(
                        builder,
                        InstructionValue::StoreLocal {
                            lvalue: LValue {
                                kind: InstructionKind::Const,
                                place: place.clone(),
                            },
                            value: last,
                            type_annotation: None,
                            loc: loc.clone(),
                        },
                    )?;
                }
                Ok(Terminal::Goto {
                    block: continuation_id,
                    variant: GotoVariant::Break,
                    id: EvaluationOrder(0),
                    loc: loc.clone(),
                })
            });

            builder.terminate_with_continuation(
                Terminal::Sequence {
                    block: sequence_block?,
                    fallthrough: continuation_id,
                    id: EvaluationOrder(0),
                    loc: loc.clone(),
                },
                continuation_block,
            );
            Ok(InstructionValue::LoadLocal { place, loc })
        }
        Expression::ArrowFunctionExpression(_) => Ok(lower_function_to_value(
            builder,
            expr,
            FunctionExpressionType::ArrowFunctionExpression,
        )?),
        Expression::FunctionExpression(_) => Ok(lower_function_to_value(
            builder,
            expr,
            FunctionExpressionType::FunctionExpression,
        )?),
        Expression::ObjectExpression(obj) => {
            let loc = convert_opt_loc(&obj.base.loc);
            let mut properties: Vec<ObjectPropertyOrSpread> = Vec::new();
            for prop in &obj.properties {
                match prop {
                    react_compiler_ast::expressions::ObjectExpressionProperty::ObjectProperty(
                        p,
                    ) => {
                        let key = lower_object_property_key(builder, &p.key, p.computed)?;
                        let key = match key {
                            Some(k) => k,
                            None => continue,
                        };
                        let value = lower_expression_to_temporary(builder, &p.value)?;
                        properties.push(ObjectPropertyOrSpread::Property(ObjectProperty {
                            key,
                            property_type: ObjectPropertyType::Property,
                            place: value,
                        }));
                    }
                    react_compiler_ast::expressions::ObjectExpressionProperty::SpreadElement(
                        spread,
                    ) => {
                        let place = lower_expression_to_temporary(builder, &spread.argument)?;
                        properties.push(ObjectPropertyOrSpread::Spread(SpreadPattern { place }));
                    }
                    react_compiler_ast::expressions::ObjectExpressionProperty::ObjectMethod(
                        method,
                    ) => {
                        if let Some(prop) = lower_object_method(builder, method)? {
                            properties.push(ObjectPropertyOrSpread::Property(prop));
                        }
                    }
                }
            }
            Ok(InstructionValue::ObjectExpression { properties, loc })
        }
        Expression::ArrayExpression(arr) => {
            let loc = convert_opt_loc(&arr.base.loc);
            let mut elements: Vec<ArrayElement> = Vec::new();
            for element in &arr.elements {
                match element {
                    None => {
                        elements.push(ArrayElement::Hole);
                    }
                    Some(Expression::SpreadElement(spread)) => {
                        let place = lower_expression_to_temporary(builder, &spread.argument)?;
                        elements.push(ArrayElement::Spread(SpreadPattern { place }));
                    }
                    Some(expr) => {
                        let place = lower_expression_to_temporary(builder, expr)?;
                        elements.push(ArrayElement::Place(place));
                    }
                }
            }
            Ok(InstructionValue::ArrayExpression { elements, loc })
        }
        Expression::NewExpression(new_expr) => {
            let loc = convert_opt_loc(&new_expr.base.loc);
            let callee = lower_expression_to_temporary(builder, &new_expr.callee)?;
            let args = lower_arguments(builder, &new_expr.arguments)?;
            Ok(InstructionValue::NewExpression { callee, args, loc })
        }
        Expression::TemplateLiteral(tmpl) => {
            let loc = convert_opt_loc(&tmpl.base.loc);
            let subexprs: Vec<Place> = tmpl
                .expressions
                .iter()
                .map(|e| lower_expression_to_temporary(builder, e))
                .collect::<Result<Vec<_>, _>>()?;
            let quasis: Vec<TemplateQuasi> = tmpl
                .quasis
                .iter()
                .map(|q| TemplateQuasi {
                    raw: q.value.raw.clone(),
                    cooked: q.value.cooked.clone(),
                })
                .collect();
            Ok(InstructionValue::TemplateLiteral {
                subexprs,
                quasis,
                loc,
            })
        }
        Expression::TaggedTemplateExpression(tagged) => {
            let loc = convert_opt_loc(&tagged.base.loc);
            if !tagged.quasi.expressions.is_empty() {
                builder.record_error(CompilerErrorDetail {
                    category: ErrorCategory::Todo,
                    reason:
                        "(BuildHIR::lowerExpression) Handle tagged template with interpolations"
                            .to_string(),
                    description: None,
                    loc: loc.clone(),
                    suggestions: None,
                })?;
                return Ok(InstructionValue::UnsupportedNode {
                    node_type: Some("TaggedTemplateExpression".to_string()),
                    original_node: serialize_expression(expr),
                    loc,
                });
            }
            assert!(
                tagged.quasi.quasis.len() == 1,
                "there should be only one quasi as we don't support interpolations yet"
            );
            let quasi = &tagged.quasi.quasis[0];
            // Check if raw and cooked values differ (e.g., graphql tagged templates)
            if quasi.value.raw != quasi.value.cooked.clone().unwrap_or_default() {
                builder.record_error(CompilerErrorDetail {
                    category: ErrorCategory::Todo,
                    reason: "(BuildHIR::lowerExpression) Handle tagged template where cooked value is different from raw value".to_string(),
                    description: None,
                    loc: loc.clone(),
                    suggestions: None,
                })?;
                return Ok(InstructionValue::UnsupportedNode {
                    node_type: Some("TaggedTemplateExpression".to_string()),
                    original_node: serialize_expression(expr),
                    loc,
                });
            }
            let value = TemplateQuasi {
                raw: quasi.value.raw.clone(),
                cooked: quasi.value.cooked.clone(),
            };
            let tag = lower_expression_to_temporary(builder, &tagged.tag)?;
            Ok(InstructionValue::TaggedTemplateExpression { tag, value, loc })
        }
        Expression::AwaitExpression(await_expr) => {
            let loc = convert_opt_loc(&await_expr.base.loc);
            let value = lower_expression_to_temporary(builder, &await_expr.argument)?;
            Ok(InstructionValue::Await { value, loc })
        }
        Expression::YieldExpression(yld) => {
            let loc = convert_opt_loc(&yld.base.loc);
            builder.record_error(CompilerErrorDetail {
                category: ErrorCategory::Todo,
                reason: "(BuildHIR::lowerExpression) Handle YieldExpression expressions"
                    .to_string(),
                description: None,
                loc: loc.clone(),
                suggestions: None,
            })?;
            Ok(InstructionValue::UnsupportedNode {
                node_type: Some("YieldExpression".to_string()),
                original_node: serialize_expression(expr),
                loc,
            })
        }
        Expression::SpreadElement(spread) => {
            // SpreadElement should be handled by the parent context (array/object/call)
            // If we reach here, just lower the argument expression
            Ok(lower_expression(builder, &spread.argument)?)
        }
        Expression::MetaProperty(meta) => {
            let loc = convert_opt_loc(&meta.base.loc);
            if meta.meta.name == "import" && meta.property.name == "meta" {
                Ok(InstructionValue::MetaProperty {
                    meta: meta.meta.name.clone(),
                    property: meta.property.name.clone(),
                    loc,
                })
            } else {
                builder.record_error(CompilerErrorDetail {
                    category: ErrorCategory::Todo,
                    reason: "(BuildHIR::lowerExpression) Handle MetaProperty expressions other than import.meta".to_string(),
                    description: None,
                    loc: loc.clone(),
                    suggestions: None,
                })?;
                Ok(InstructionValue::UnsupportedNode {
                    node_type: Some("MetaProperty".to_string()),
                    original_node: serialize_expression(expr),
                    loc,
                })
            }
        }
        Expression::ClassExpression(cls) => {
            let loc = convert_opt_loc(&cls.base.loc);
            builder.record_error(CompilerErrorDetail {
                category: ErrorCategory::Todo,
                reason: "(BuildHIR::lowerExpression) Handle ClassExpression expressions"
                    .to_string(),
                description: None,
                loc: loc.clone(),
                suggestions: None,
            })?;
            Ok(InstructionValue::UnsupportedNode {
                node_type: Some("ClassExpression".to_string()),
                original_node: serialize_expression(expr),
                loc,
            })
        }
        Expression::PrivateName(pn) => {
            let loc = convert_opt_loc(&pn.base.loc);
            builder.record_error(CompilerErrorDetail {
                category: ErrorCategory::Todo,
                reason: "(BuildHIR::lowerExpression) Handle PrivateName expressions".to_string(),
                description: None,
                loc: loc.clone(),
                suggestions: None,
            })?;
            Ok(InstructionValue::UnsupportedNode {
                node_type: Some("PrivateName".to_string()),
                original_node: serialize_expression(expr),
                loc,
            })
        }
        Expression::Super(sup) => {
            let loc = convert_opt_loc(&sup.base.loc);
            builder.record_error(CompilerErrorDetail {
                category: ErrorCategory::Todo,
                reason: "(BuildHIR::lowerExpression) Handle Super expressions".to_string(),
                description: None,
                loc: loc.clone(),
                suggestions: None,
            })?;
            Ok(InstructionValue::UnsupportedNode {
                node_type: Some("Super".to_string()),
                original_node: serialize_expression(expr),
                loc,
            })
        }
        Expression::Import(imp) => {
            let loc = convert_opt_loc(&imp.base.loc);
            builder.record_error(CompilerErrorDetail {
                category: ErrorCategory::Todo,
                reason: "(BuildHIR::lowerExpression) Handle Import expressions".to_string(),
                description: None,
                loc: loc.clone(),
                suggestions: None,
            })?;
            Ok(InstructionValue::UnsupportedNode {
                node_type: Some("Import".to_string()),
                original_node: serialize_expression(expr),
                loc,
            })
        }
        Expression::ThisExpression(this) => {
            let loc = convert_opt_loc(&this.base.loc);
            builder.record_error(CompilerErrorDetail {
                category: ErrorCategory::Todo,
                reason: "(BuildHIR::lowerExpression) Handle ThisExpression expressions".to_string(),
                description: None,
                loc: loc.clone(),
                suggestions: None,
            })?;
            Ok(InstructionValue::UnsupportedNode {
                node_type: Some("ThisExpression".to_string()),
                original_node: serialize_expression(expr),
                loc,
            })
        }
        Expression::ParenthesizedExpression(paren) => {
            Ok(lower_expression(builder, &paren.expression)?)
        }
        Expression::JSXElement(jsx_element) => {
            let loc = convert_opt_loc(&jsx_element.base.loc);
            let opening_loc = convert_opt_loc(&jsx_element.opening_element.base.loc);
            let closing_loc = jsx_element
                .closing_element
                .as_ref()
                .and_then(|c| convert_opt_loc(&c.base.loc));

            // Lower the tag name
            let tag = lower_jsx_element_name(builder, &jsx_element.opening_element.name)?;

            // Lower attributes (props)
            let mut props: Vec<JsxAttribute> = Vec::new();
            for attr_item in &jsx_element.opening_element.attributes {
                use react_compiler_ast::jsx::JSXAttributeItem;
                use react_compiler_ast::jsx::JSXAttributeName;
                use react_compiler_ast::jsx::JSXAttributeValue;
                match attr_item {
                    JSXAttributeItem::JSXSpreadAttribute(spread) => {
                        let argument = lower_expression_to_temporary(builder, &spread.argument)?;
                        props.push(JsxAttribute::SpreadAttribute { argument });
                    }
                    JSXAttributeItem::JSXAttribute(attr) => {
                        // Get the attribute name
                        let prop_name = match &attr.name {
                            JSXAttributeName::JSXIdentifier(id) => {
                                let name = &id.name;
                                if name.contains(':') {
                                    builder.record_error(CompilerErrorDetail {
                                        category: ErrorCategory::Todo,
                                        reason: format!(
                                            "(BuildHIR::lowerExpression) Unexpected colon in attribute name `{}`",
                                            name
                                        ),
                                        description: None,
                                        loc: convert_opt_loc(&id.base.loc),
                                        suggestions: None,
                                    })?;
                                }
                                name.clone()
                            }
                            JSXAttributeName::JSXNamespacedName(ns) => {
                                format!("{}:{}", ns.namespace.name, ns.name.name)
                            }
                        };

                        // Get the attribute value
                        let value = match &attr.value {
                            Some(JSXAttributeValue::StringLiteral(s)) => {
                                let str_loc = convert_opt_loc(&s.base.loc);
                                lower_value_to_temporary(
                                    builder,
                                    InstructionValue::Primitive {
                                        value: PrimitiveValue::String(s.value.clone()),
                                        loc: str_loc,
                                    },
                                )?
                            }
                            Some(JSXAttributeValue::JSXExpressionContainer(container)) => {
                                use react_compiler_ast::jsx::JSXExpressionContainerExpr;
                                match &container.expression {
                                    JSXExpressionContainerExpr::JSXEmptyExpression(_) => {
                                        // Empty expression container - skip this attribute
                                        continue;
                                    }
                                    JSXExpressionContainerExpr::Expression(expr) => {
                                        lower_expression_to_temporary(builder, expr)?
                                    }
                                }
                            }
                            Some(JSXAttributeValue::JSXElement(el)) => {
                                let val = lower_expression(
                                    builder,
                                    &react_compiler_ast::expressions::Expression::JSXElement(
                                        el.clone(),
                                    ),
                                )?;
                                lower_value_to_temporary(builder, val)?
                            }
                            Some(JSXAttributeValue::JSXFragment(frag)) => {
                                let val = lower_expression(
                                    builder,
                                    &react_compiler_ast::expressions::Expression::JSXFragment(
                                        frag.clone(),
                                    ),
                                )?;
                                lower_value_to_temporary(builder, val)?
                            }
                            None => {
                                // No value means boolean true (e.g., <div disabled />)
                                let attr_loc = convert_opt_loc(&attr.base.loc);
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

            // Check if this is an fbt/fbs tag, which requires special whitespace handling
            let is_fbt = matches!(&tag, JsxTag::Builtin(b) if b.name == "fbt" || b.name == "fbs");

            // Check that fbt/fbs tags are module-level imports, not local bindings.
            // Matches TS: CompilerError.invariant(tagIdentifier.kind !== 'Identifier', ...)
            if is_fbt {
                let tag_name = match &tag {
                    JsxTag::Builtin(b) => b.name.clone(),
                    _ => "fbt".to_string(),
                };
                // Get the opening element's name identifier and check if it's a local binding
                if let react_compiler_ast::jsx::JSXElementName::JSXIdentifier(jsx_id) =
                    &jsx_element.opening_element.name
                {
                    let id_loc = convert_opt_loc(&jsx_id.base.loc);
                    // Check if fbt/fbs tag name resolves to a local binding.
                    // JSX identifiers may not be in our position-based reference map,
                    // so check if ANY binding with this name exists in the function scope.
                    let is_local_binding = builder.has_local_binding(&jsx_id.name);
                    if is_local_binding {
                        // Record as a Diagnostic (not ErrorDetail) to match TS behavior
                        // where CompilerError.invariant creates a CompilerDiagnostic.
                        // TS invariant() throws immediately, so only the first fbt error
                        // is reported. We return Err to match this behavior.
                        let reason = format!("<{}> tags should be module-level imports", tag_name);
                        return Err(CompilerDiagnostic::new(
                            ErrorCategory::Invariant,
                            &reason,
                            None,
                        )
                        .with_detail(CompilerDiagnosticDetail::Error {
                            loc: id_loc.clone(),
                            message: Some(reason.clone()),
                            identifier_name: None,
                        })
                        .into());
                    }
                }
            }

            // Check for duplicate fbt:enum, fbt:plural, fbt:pronoun tags
            if is_fbt {
                let tag_name = match &tag {
                    JsxTag::Builtin(b) => b.name.as_str(),
                    _ => "fbt",
                };
                let mut enum_locs: Vec<Option<SourceLocation>> = Vec::new();
                let mut plural_locs: Vec<Option<SourceLocation>> = Vec::new();
                let mut pronoun_locs: Vec<Option<SourceLocation>> = Vec::new();
                collect_fbt_sub_tags(
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
                        use react_compiler_diagnostics::CompilerDiagnosticDetail;
                        let details: Vec<CompilerDiagnosticDetail> = locations
                            .iter()
                            .map(|loc| CompilerDiagnosticDetail::Error {
                                message: Some(format!(
                                    "Multiple `<{}:{}>` tags found",
                                    tag_name, name
                                )),
                                loc: loc.clone(),
                                identifier_name: None,
                            })
                            .collect();
                        let mut diag = react_compiler_diagnostics::CompilerDiagnostic::new(
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

            // Lower children
            let children: Vec<Place> = jsx_element
                .children
                .iter()
                .map(|child| lower_jsx_element(builder, child))
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
        Expression::JSXFragment(jsx_fragment) => {
            let loc = convert_opt_loc(&jsx_fragment.base.loc);

            // Lower children
            let children: Vec<Place> = jsx_fragment
                .children
                .iter()
                .map(|child| lower_jsx_element(builder, child))
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .flatten()
                .collect();

            Ok(InstructionValue::JsxFragment { children, loc })
        }
        Expression::AssignmentPattern(_) => {
            let loc = convert_opt_loc(&match expr {
                Expression::AssignmentPattern(p) => p.base.loc.clone(),
                _ => unreachable!(),
            });
            builder.record_error(CompilerErrorDetail {
                reason: "(BuildHIR::lowerExpression) Handle AssignmentPattern expressions"
                    .to_string(),
                category: ErrorCategory::Todo,
                loc: loc.clone(),
                description: None,
                suggestions: None,
            })?;
            Ok(InstructionValue::UnsupportedNode {
                node_type: Some("AssignmentPattern".to_string()),
                original_node: serialize_expression(expr),
                loc,
            })
        }
        Expression::TSAsExpression(ts) => {
            let loc = convert_opt_loc(&ts.base.loc);
            let value = lower_expression_to_temporary(builder, &ts.expression)?;
            let type_annotation = ts.type_annotation.parse_value();
            let type_ = lower_type_annotation(&type_annotation, builder);
            let type_annotation_name = get_type_annotation_name(&type_annotation);
            Ok(InstructionValue::TypeCastExpression {
                value,
                type_,
                type_annotation_name,
                type_annotation_kind: Some("as".to_string()),
                type_annotation: Some(Box::new(type_annotation)),
                loc,
            })
        }
        Expression::TSSatisfiesExpression(ts) => {
            let loc = convert_opt_loc(&ts.base.loc);
            let value = lower_expression_to_temporary(builder, &ts.expression)?;
            let type_annotation = ts.type_annotation.parse_value();
            let type_ = lower_type_annotation(&type_annotation, builder);
            let type_annotation_name = get_type_annotation_name(&type_annotation);
            Ok(InstructionValue::TypeCastExpression {
                value,
                type_,
                type_annotation_name,
                type_annotation_kind: Some("satisfies".to_string()),
                type_annotation: Some(Box::new(type_annotation)),
                loc,
            })
        }
        Expression::TSNonNullExpression(ts) => Ok(lower_expression(builder, &ts.expression)?),
        Expression::TSTypeAssertion(ts) => {
            let loc = convert_opt_loc(&ts.base.loc);
            let value = lower_expression_to_temporary(builder, &ts.expression)?;
            let type_annotation = ts.type_annotation.parse_value();
            let type_ = lower_type_annotation(&type_annotation, builder);
            let type_annotation_name = get_type_annotation_name(&type_annotation);
            Ok(InstructionValue::TypeCastExpression {
                value,
                type_,
                type_annotation_name,
                type_annotation_kind: Some("as".to_string()),
                type_annotation: Some(Box::new(type_annotation)),
                loc,
            })
        }
        Expression::TSInstantiationExpression(ts) => Ok(lower_expression(builder, &ts.expression)?),
        Expression::TypeCastExpression(tc) => {
            let loc = convert_opt_loc(&tc.base.loc);
            let value = lower_expression_to_temporary(builder, &tc.expression)?;
            let annotation_value = tc.type_annotation.parse_value();
            // Flow TypeCastExpression: typeAnnotation is a TypeAnnotation node wrapping the actual type
            let inner_type = annotation_value
                .get("typeAnnotation")
                .unwrap_or(&annotation_value);
            let type_ = lower_type_annotation(inner_type, builder);
            let type_annotation_name = get_type_annotation_name(inner_type);
            Ok(InstructionValue::TypeCastExpression {
                value,
                type_,
                type_annotation_name,
                type_annotation_kind: Some("cast".to_string()),
                type_annotation: Some(Box::new(annotation_value)),
                loc,
            })
        }
        Expression::BigIntLiteral(big) => {
            let loc = convert_opt_loc(&big.base.loc);
            builder.record_error(CompilerErrorDetail {
                category: ErrorCategory::Todo,
                reason: "(BuildHIR::lowerExpression) Handle BigIntLiteral expressions".to_string(),
                description: None,
                loc: loc.clone(),
                suggestions: None,
            })?;
            Ok(InstructionValue::UnsupportedNode {
                node_type: Some("BigIntLiteral".to_string()),
                original_node: serialize_expression(expr),
                loc,
            })
        }
        Expression::RegExpLiteral(re) => {
            let loc = convert_opt_loc(&re.base.loc);
            Ok(InstructionValue::RegExpLiteral {
                pattern: re.pattern.clone(),
                flags: re.flags.clone(),
                loc,
            })
        }
    }
}

pub(crate) fn lower_optional_member_expression(
    builder: &mut HirBuilder,
    expr: &react_compiler_ast::expressions::OptionalMemberExpression,
) -> Result<InstructionValue, CompilerError> {
    let place = lower_optional_member_expression_impl(builder, expr, None)?.1;
    Ok(InstructionValue::LoadLocal {
        loc: place.loc.clone(),
        place,
    })
}

/// Returns (object, value_place) pair.
/// The `value_place` is stored into a temporary; we also return it as an InstructionValue
/// via LoadLocal for the top-level call.
pub(crate) fn lower_optional_member_expression_impl(
    builder: &mut HirBuilder,
    expr: &react_compiler_ast::expressions::OptionalMemberExpression,
    parent_alternate: Option<BlockId>,
) -> Result<(Place, Place), CompilerError> {
    use react_compiler_ast::expressions::Expression;
    let optional = expr.optional;
    let loc = convert_opt_loc(&expr.base.loc);
    let place = build_temporary_place(builder, loc.clone());
    let continuation_block = builder.reserve(builder.current_block_kind());
    let continuation_id = continuation_block.id;
    let consequent = builder.reserve(BlockKind::Value);

    // Block to evaluate if the callee is null/undefined — sets result to undefined.
    // Only create an alternate when first entering an optional subtree.
    let alternate = if let Some(parent_alt) = parent_alternate {
        Ok(parent_alt)
    } else {
        builder.try_enter(BlockKind::Value, |builder, _block_id| {
            let temp = lower_value_to_temporary(
                builder,
                InstructionValue::Primitive {
                    value: PrimitiveValue::Undefined,
                    loc: loc.clone(),
                },
            )?;
            lower_value_to_temporary(
                builder,
                InstructionValue::StoreLocal {
                    lvalue: LValue {
                        kind: InstructionKind::Const,
                        place: place.clone(),
                    },
                    value: temp,
                    type_annotation: None,
                    loc: loc.clone(),
                },
            )?;
            Ok(Terminal::Goto {
                block: continuation_id,
                variant: GotoVariant::Break,
                id: EvaluationOrder(0),
                loc: loc.clone(),
            })
        })
    }?;

    let mut object: Option<Place> = None;
    let test_block = builder.try_enter(BlockKind::Value, |builder, _block_id| {
        match expr.object.as_ref() {
            Expression::OptionalMemberExpression(opt_member) => {
                let (_obj, value) =
                    lower_optional_member_expression_impl(builder, opt_member, Some(alternate))?;
                object = Some(value);
            }
            Expression::OptionalCallExpression(opt_call) => {
                let value =
                    lower_optional_call_expression_impl(builder, opt_call, Some(alternate))?;
                let value_place = lower_value_to_temporary(builder, value)?;
                object = Some(value_place);
            }
            other => {
                object = Some(lower_expression_to_temporary(builder, other)?);
            }
        }
        let test_place = object.as_ref().unwrap().clone();
        Ok(Terminal::Branch {
            test: test_place,
            consequent: consequent.id,
            alternate,
            fallthrough: continuation_id,
            id: EvaluationOrder(0),
            loc: loc.clone(),
        })
    });

    let obj = object.unwrap();

    // Block to evaluate if the callee is non-null/undefined
    builder.try_enter_reserved(consequent, |builder| {
        let lowered = lower_member_expression_with_object(builder, expr, obj.clone())?;
        let temp = lower_value_to_temporary(builder, lowered.value)?;
        lower_value_to_temporary(
            builder,
            InstructionValue::StoreLocal {
                lvalue: LValue {
                    kind: InstructionKind::Const,
                    place: place.clone(),
                },
                value: temp,
                type_annotation: None,
                loc: loc.clone(),
            },
        )?;
        Ok(Terminal::Goto {
            block: continuation_id,
            variant: GotoVariant::Break,
            id: EvaluationOrder(0),
            loc: loc.clone(),
        })
    })?;

    builder.terminate_with_continuation(
        Terminal::Optional {
            optional,
            test: test_block?,
            fallthrough: continuation_id,
            id: EvaluationOrder(0),
            loc: loc.clone(),
        },
        continuation_block,
    );

    Ok((obj, place))
}

pub(crate) fn lower_optional_call_expression(
    builder: &mut HirBuilder,
    expr: &react_compiler_ast::expressions::OptionalCallExpression,
) -> Result<InstructionValue, CompilerError> {
    Ok(lower_optional_call_expression_impl(builder, expr, None)?)
}

pub(crate) fn lower_optional_call_expression_impl(
    builder: &mut HirBuilder,
    expr: &react_compiler_ast::expressions::OptionalCallExpression,
    parent_alternate: Option<BlockId>,
) -> Result<InstructionValue, CompilerError> {
    use react_compiler_ast::expressions::Expression;
    let optional = expr.optional;
    let loc = convert_opt_loc(&expr.base.loc);
    let place = build_temporary_place(builder, loc.clone());
    let continuation_block = builder.reserve(builder.current_block_kind());
    let continuation_id = continuation_block.id;
    let consequent = builder.reserve(BlockKind::Value);

    // Block to evaluate if the callee is null/undefined
    let alternate = if let Some(parent_alt) = parent_alternate {
        Ok(parent_alt)
    } else {
        builder.try_enter(BlockKind::Value, |builder, _block_id| {
            let temp = lower_value_to_temporary(
                builder,
                InstructionValue::Primitive {
                    value: PrimitiveValue::Undefined,
                    loc: loc.clone(),
                },
            )?;
            lower_value_to_temporary(
                builder,
                InstructionValue::StoreLocal {
                    lvalue: LValue {
                        kind: InstructionKind::Const,
                        place: place.clone(),
                    },
                    value: temp,
                    type_annotation: None,
                    loc: loc.clone(),
                },
            )?;
            Ok(Terminal::Goto {
                block: continuation_id,
                variant: GotoVariant::Break,
                id: EvaluationOrder(0),
                loc: loc.clone(),
            })
        })
    }?;

    // Track callee info for building the call in the consequent block
    enum CalleeInfo {
        CallExpression { callee: Place },
        MethodCall { receiver: Place, property: Place },
    }

    let mut callee_info: Option<CalleeInfo> = None;

    let test_block = builder.try_enter(BlockKind::Value, |builder, _block_id| {
        match expr.callee.as_ref() {
            Expression::OptionalCallExpression(opt_call) => {
                let value =
                    lower_optional_call_expression_impl(builder, opt_call, Some(alternate))?;
                let value_place = lower_value_to_temporary(builder, value)?;
                callee_info = Some(CalleeInfo::CallExpression {
                    callee: value_place,
                });
            }
            Expression::OptionalMemberExpression(opt_member) => {
                let (obj, value) =
                    lower_optional_member_expression_impl(builder, opt_member, Some(alternate))?;
                callee_info = Some(CalleeInfo::MethodCall {
                    receiver: obj,
                    property: value,
                });
            }
            Expression::MemberExpression(member) => {
                let lowered = lower_member_expression(builder, member)?;
                let property_place = lower_value_to_temporary(builder, lowered.value)?;
                callee_info = Some(CalleeInfo::MethodCall {
                    receiver: lowered.object,
                    property: property_place,
                });
            }
            other => {
                let callee_place = lower_expression_to_temporary(builder, other)?;
                callee_info = Some(CalleeInfo::CallExpression {
                    callee: callee_place,
                });
            }
        }

        let test_place = match callee_info.as_ref().unwrap() {
            CalleeInfo::CallExpression { callee } => callee.clone(),
            CalleeInfo::MethodCall { property, .. } => property.clone(),
        };

        Ok(Terminal::Branch {
            test: test_place,
            consequent: consequent.id,
            alternate,
            fallthrough: continuation_id,
            id: EvaluationOrder(0),
            loc: loc.clone(),
        })
    });

    // Block to evaluate if the callee is non-null/undefined
    builder.try_enter_reserved(consequent, |builder| {
        let args = lower_arguments(builder, &expr.arguments)?;
        let temp = build_temporary_place(builder, loc.clone());

        match callee_info.as_ref().unwrap() {
            CalleeInfo::CallExpression { callee } => {
                builder.push(Instruction {
                    id: EvaluationOrder(0),
                    lvalue: temp.clone(),
                    value: InstructionValue::CallExpression {
                        callee: callee.clone(),
                        args,
                        loc: loc.clone(),
                    },
                    loc: loc.clone(),
                    effects: None,
                });
            }
            CalleeInfo::MethodCall { receiver, property } => {
                builder.push(Instruction {
                    id: EvaluationOrder(0),
                    lvalue: temp.clone(),
                    value: InstructionValue::MethodCall {
                        receiver: receiver.clone(),
                        property: property.clone(),
                        args,
                        loc: loc.clone(),
                    },
                    loc: loc.clone(),
                    effects: None,
                });
            }
        }

        lower_value_to_temporary(
            builder,
            InstructionValue::StoreLocal {
                lvalue: LValue {
                    kind: InstructionKind::Const,
                    place: place.clone(),
                },
                value: temp,
                type_annotation: None,
                loc: loc.clone(),
            },
        )?;
        Ok(Terminal::Goto {
            block: continuation_id,
            variant: GotoVariant::Break,
            id: EvaluationOrder(0),
            loc: loc.clone(),
        })
    })?;

    builder.terminate_with_continuation(
        Terminal::Optional {
            optional,
            test: test_block?,
            fallthrough: continuation_id,
            id: EvaluationOrder(0),
            loc: loc.clone(),
        },
        continuation_block,
    );

    Ok(InstructionValue::LoadLocal {
        place: place.clone(),
        loc: place.loc,
    })
}

pub(crate) fn lower_reorderable_expression(
    builder: &mut HirBuilder,
    expr: &react_compiler_ast::expressions::Expression,
) -> Result<Place, CompilerError> {
    if !is_reorderable_expression(builder, expr, true) {
        builder.record_error(CompilerErrorDetail {
            category: ErrorCategory::Todo,
            reason: format!(
                "(BuildHIR::node.lowerReorderableExpression) Expression type `{}` cannot be safely reordered",
                expression_type_name(expr)
            ),
            description: None,
            loc: expression_loc(expr),
            suggestions: None,
        })?;
    }
    Ok(lower_expression_to_temporary(builder, expr)?)
}

pub(crate) fn is_reorderable_expression(
    builder: &HirBuilder,
    expr: &react_compiler_ast::expressions::Expression,
    allow_local_identifiers: bool,
) -> bool {
    use react_compiler_ast::expressions::Expression;
    match expr {
        Expression::Identifier(ident) => {
            let binding = builder
                .scope_info()
                .resolve_reference_for_node(ident.base.node_id);
            match binding {
                None => {
                    // global, safe to reorder
                    true
                }
                Some(b) => {
                    if b.scope == builder.scope_info().program_scope {
                        // Module-scope binding (ModuleLocal, imports), safe to reorder
                        true
                    } else {
                        allow_local_identifiers
                    }
                }
            }
        }
        Expression::RegExpLiteral(_)
        | Expression::StringLiteral(_)
        | Expression::NumericLiteral(_)
        | Expression::NullLiteral(_)
        | Expression::BooleanLiteral(_)
        | Expression::BigIntLiteral(_) => true,
        Expression::UnaryExpression(unary) => {
            use react_compiler_ast::operators::UnaryOperator;
            matches!(
                unary.operator,
                UnaryOperator::Not | UnaryOperator::Plus | UnaryOperator::Neg
            ) && is_reorderable_expression(builder, &unary.argument, allow_local_identifiers)
        }
        Expression::LogicalExpression(logical) => {
            is_reorderable_expression(builder, &logical.left, allow_local_identifiers)
                && is_reorderable_expression(builder, &logical.right, allow_local_identifiers)
        }
        Expression::ConditionalExpression(cond) => {
            is_reorderable_expression(builder, &cond.test, allow_local_identifiers)
                && is_reorderable_expression(builder, &cond.consequent, allow_local_identifiers)
                && is_reorderable_expression(builder, &cond.alternate, allow_local_identifiers)
        }
        Expression::ArrayExpression(arr) => {
            arr.elements.iter().all(|element| {
                match element {
                    Some(e) => is_reorderable_expression(builder, e, allow_local_identifiers),
                    None => false, // holes are not reorderable
                }
            })
        }
        Expression::ObjectExpression(obj) => obj.properties.iter().all(|prop| match prop {
            react_compiler_ast::expressions::ObjectExpressionProperty::ObjectProperty(p) => {
                !p.computed && is_reorderable_expression(builder, &p.value, allow_local_identifiers)
            }
            _ => false,
        }),
        Expression::MemberExpression(member) => {
            // Allow member expressions where the innermost object is a global or module-local
            let mut inner = member.object.as_ref();
            while let Expression::MemberExpression(m) = inner {
                inner = m.object.as_ref();
            }
            if let Expression::Identifier(ident) = inner {
                match builder
                    .scope_info()
                    .resolve_reference_for_node(ident.base.node_id)
                {
                    None => true, // global
                    Some(binding) => {
                        // Module-scope bindings (ModuleLocal, imports) are safe to reorder
                        binding.scope == builder.scope_info().program_scope
                    }
                }
            } else {
                false
            }
        }
        Expression::ArrowFunctionExpression(arrow) => {
            use react_compiler_ast::expressions::ArrowFunctionBody;
            match arrow.body.as_ref() {
                ArrowFunctionBody::BlockStatement(block) => block.body.is_empty(),
                ArrowFunctionBody::Expression(body_expr) => {
                    is_reorderable_expression(builder, body_expr, false)
                }
            }
        }
        Expression::CallExpression(call) => {
            is_reorderable_expression(builder, &call.callee, allow_local_identifiers)
                && call
                    .arguments
                    .iter()
                    .all(|arg| is_reorderable_expression(builder, arg, allow_local_identifiers))
        }
        Expression::NewExpression(new_expr) => {
            is_reorderable_expression(builder, &new_expr.callee, allow_local_identifiers)
                && new_expr
                    .arguments
                    .iter()
                    .all(|arg| is_reorderable_expression(builder, arg, allow_local_identifiers))
        }
        // TypeScript/Flow type wrappers: recurse into the inner expression
        Expression::TSAsExpression(ts) => {
            is_reorderable_expression(builder, &ts.expression, allow_local_identifiers)
        }
        Expression::TSSatisfiesExpression(ts) => {
            is_reorderable_expression(builder, &ts.expression, allow_local_identifiers)
        }
        Expression::TSNonNullExpression(ts) => {
            is_reorderable_expression(builder, &ts.expression, allow_local_identifiers)
        }
        Expression::TSInstantiationExpression(ts) => {
            is_reorderable_expression(builder, &ts.expression, allow_local_identifiers)
        }
        Expression::TypeCastExpression(tc) => {
            is_reorderable_expression(builder, &tc.expression, allow_local_identifiers)
        }
        Expression::TSTypeAssertion(ts) => {
            is_reorderable_expression(builder, &ts.expression, allow_local_identifiers)
        }
        Expression::ParenthesizedExpression(p) => {
            is_reorderable_expression(builder, &p.expression, allow_local_identifiers)
        }
        _ => false,
    }
}

/// Extract the type name from a type annotation serde_json::Value.
/// Returns the "type" field value, e.g. "TSTypeReference", "GenericTypeAnnotation".
pub(crate) fn get_type_annotation_name(val: &serde_json::Value) -> Option<String> {
    val.get("type")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Lower a type annotation JSON value to an HIR Type.
/// Mirrors the TS `lowerType` function.
pub(crate) fn lower_type_annotation(val: &serde_json::Value, builder: &mut HirBuilder) -> Type {
    let type_name = match val.get("type").and_then(|v| v.as_str()) {
        Some(name) => name,
        None => return builder.make_type(),
    };
    match type_name {
        "GenericTypeAnnotation" => {
            // Check if it's Array
            if let Some(id) = val.get("id") {
                if id.get("type").and_then(|v| v.as_str()) == Some("Identifier") {
                    if id.get("name").and_then(|v| v.as_str()) == Some("Array") {
                        return Type::Object {
                            shape_id: Some("BuiltInArray".to_string()),
                        };
                    }
                }
            }
            builder.make_type()
        }
        "TSTypeReference" => {
            if let Some(type_name_val) = val.get("typeName") {
                if type_name_val.get("type").and_then(|v| v.as_str()) == Some("Identifier") {
                    if type_name_val.get("name").and_then(|v| v.as_str()) == Some("Array") {
                        return Type::Object {
                            shape_id: Some("BuiltInArray".to_string()),
                        };
                    }
                }
            }
            builder.make_type()
        }
        "ArrayTypeAnnotation" | "TSArrayType" => Type::Object {
            shape_id: Some("BuiltInArray".to_string()),
        },
        "BooleanLiteralTypeAnnotation"
        | "BooleanTypeAnnotation"
        | "NullLiteralTypeAnnotation"
        | "NumberLiteralTypeAnnotation"
        | "NumberTypeAnnotation"
        | "StringLiteralTypeAnnotation"
        | "StringTypeAnnotation"
        | "TSBooleanKeyword"
        | "TSNullKeyword"
        | "TSNumberKeyword"
        | "TSStringKeyword"
        | "TSSymbolKeyword"
        | "TSUndefinedKeyword"
        | "TSVoidKeyword"
        | "VoidTypeAnnotation" => Type::Primitive,
        _ => builder.make_type(),
    }
}
