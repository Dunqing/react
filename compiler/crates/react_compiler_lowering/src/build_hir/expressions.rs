//! Expression lowering — reads `oxc_ast` directly and produces HIR.
//!
//! Stage N1.2.3 transcribes the expression-lowering logic from the pre-flip
//! `react_compiler_ast` reference (`git show 1a02788:…/build_hir/expressions.rs`)
//! to read `oxc_ast` enums and resolve scope/bindings via `semantic_queries`.
//!
//! The *algorithm* and the HIR it produces are unchanged from the reference;
//! only the AST access changes. Expression kinds that depend on infrastructure
//! not yet transcribed (function/arrow expressions, JSX, class, destructuring
//! assignment, `this`/`super`, type-cast type lowering) keep the graceful
//! `Todo` bail so the crate stays green.

use oxc_ast::ast as oxc;
use oxc_span::GetSpan;
use react_compiler_diagnostics::CompilerError;
use react_compiler_hir::*;

use crate::hir_builder::HirBuilder;
use crate::hir_builder::todo_diagnostic;
use crate::semantic_queries as sq;

use super::build_temporary_place;
use super::lower_value_to_temporary;

// =============================================================================
// Operator conversion (oxc -> HIR)
// =============================================================================

/// Convert an oxc `BinaryOperator` to the HIR `BinaryOperator`.
pub(crate) fn convert_binary_operator(op: oxc_syntax::operator::BinaryOperator) -> BinaryOperator {
    use oxc_syntax::operator::BinaryOperator as Op;
    match op {
        Op::Addition => BinaryOperator::Add,
        Op::Subtraction => BinaryOperator::Subtract,
        Op::Multiplication => BinaryOperator::Multiply,
        Op::Division => BinaryOperator::Divide,
        Op::Remainder => BinaryOperator::Modulo,
        Op::Exponential => BinaryOperator::Exponent,
        Op::Equality => BinaryOperator::Equal,
        Op::StrictEquality => BinaryOperator::StrictEqual,
        Op::Inequality => BinaryOperator::NotEqual,
        Op::StrictInequality => BinaryOperator::StrictNotEqual,
        Op::LessThan => BinaryOperator::LessThan,
        Op::LessEqualThan => BinaryOperator::LessEqual,
        Op::GreaterThan => BinaryOperator::GreaterThan,
        Op::GreaterEqualThan => BinaryOperator::GreaterEqual,
        Op::ShiftLeft => BinaryOperator::ShiftLeft,
        Op::ShiftRight => BinaryOperator::ShiftRight,
        Op::ShiftRightZeroFill => BinaryOperator::UnsignedShiftRight,
        Op::BitwiseOR => BinaryOperator::BitwiseOr,
        Op::BitwiseXOR => BinaryOperator::BitwiseXor,
        Op::BitwiseAnd => BinaryOperator::BitwiseAnd,
        Op::In => BinaryOperator::In,
        Op::Instanceof => BinaryOperator::InstanceOf,
    }
}

/// Convert an oxc non-`delete`/`void` unary operator. `delete`/`throw` are
/// handled separately by the caller; `void`/`typeof` flow through here.
fn convert_unary_operator(op: oxc_syntax::operator::UnaryOperator) -> Option<UnaryOperator> {
    use oxc_syntax::operator::UnaryOperator as Op;
    match op {
        Op::UnaryNegation => Some(UnaryOperator::Minus),
        Op::UnaryPlus => Some(UnaryOperator::Plus),
        Op::LogicalNot => Some(UnaryOperator::Not),
        Op::BitwiseNot => Some(UnaryOperator::BitwiseNot),
        Op::Typeof => Some(UnaryOperator::TypeOf),
        Op::Void => Some(UnaryOperator::Void),
        Op::Delete => None,
    }
}

fn convert_update_operator(op: oxc_syntax::operator::UpdateOperator) -> UpdateOperator {
    use oxc_syntax::operator::UpdateOperator as Op;
    match op {
        Op::Increment => UpdateOperator::Increment,
        Op::Decrement => UpdateOperator::Decrement,
    }
}

fn convert_logical_operator(op: oxc_syntax::operator::LogicalOperator) -> LogicalOperator {
    use oxc_syntax::operator::LogicalOperator as Op;
    match op {
        Op::And => LogicalOperator::And,
        Op::Or => LogicalOperator::Or,
        Op::Coalesce => LogicalOperator::NullishCoalescing,
    }
}

// =============================================================================
// Entry: lower an expression to an InstructionValue
// =============================================================================

pub(crate) fn lower_expression_to_temporary(
    builder: &mut HirBuilder,
    expr: &oxc::Expression,
) -> Result<Place, CompilerError> {
    let value = lower_expression(builder, expr)?;
    lower_value_to_temporary(builder, value)
}

/// Record a graceful Todo and synthesize an `undefined` primitive so the value
/// can be used as a placeholder operand without aborting lowering.
fn todo_value(
    builder: &mut HirBuilder,
    what: &str,
    loc: Option<SourceLocation>,
) -> InstructionValue {
    builder.record_diagnostic(todo_diagnostic(what, loc.clone()));
    InstructionValue::Primitive {
        value: PrimitiveValue::Undefined,
        loc,
    }
}

pub(crate) fn lower_expression(
    builder: &mut HirBuilder,
    expr: &oxc::Expression,
) -> Result<InstructionValue, CompilerError> {
    let loc = Some(builder.loc_of_span(expr.span()));
    match expr {
        // ---- literals ----
        oxc::Expression::NumericLiteral(lit) => Ok(InstructionValue::Primitive {
            value: PrimitiveValue::Number(FloatValue::new(lit.value)),
            loc,
        }),
        oxc::Expression::BooleanLiteral(lit) => Ok(InstructionValue::Primitive {
            value: PrimitiveValue::Boolean(lit.value),
            loc,
        }),
        oxc::Expression::StringLiteral(lit) => Ok(InstructionValue::Primitive {
            value: PrimitiveValue::String(lit.value.to_string()),
            loc,
        }),
        oxc::Expression::NullLiteral(_) => Ok(InstructionValue::Primitive {
            value: PrimitiveValue::Null,
            loc,
        }),
        oxc::Expression::RegExpLiteral(re) => Ok(InstructionValue::RegExpLiteral {
            pattern: re.regex.pattern.text.to_string(),
            flags: re.regex.flags.to_string(),
            loc,
        }),

        // ---- identifier ----
        oxc::Expression::Identifier(ident) => lower_identifier_value(builder, ident),

        // ---- binary / logical / unary / update ----
        oxc::Expression::BinaryExpression(bin) => {
            let left = lower_expression_to_temporary(builder, &bin.left)?;
            let right = lower_expression_to_temporary(builder, &bin.right)?;
            Ok(InstructionValue::BinaryExpression {
                operator: convert_binary_operator(bin.operator),
                left,
                right,
                loc,
            })
        }
        oxc::Expression::UnaryExpression(unary) => lower_unary_expression(builder, unary, loc),
        oxc::Expression::LogicalExpression(logical) => {
            lower_logical_expression(builder, logical, loc)
        }
        oxc::Expression::UpdateExpression(update) => {
            lower_update_expression(builder, update, loc)
        }

        // ---- member access ----
        oxc::Expression::StaticMemberExpression(_)
        | oxc::Expression::ComputedMemberExpression(_) => {
            let member = expr.as_member_expression().unwrap();
            let lowered = lower_member_expression(builder, member, None)?;
            Ok(lowered.value)
        }
        oxc::Expression::PrivateFieldExpression(_) => {
            Ok(todo_value(builder, "expression: PrivateFieldExpression", loc))
        }

        // ---- calls ----
        oxc::Expression::CallExpression(call) => lower_call_expression(builder, call, loc),
        oxc::Expression::NewExpression(new_expr) => {
            let callee = lower_expression_to_temporary(builder, &new_expr.callee)?;
            let args = lower_arguments(builder, &new_expr.arguments)?;
            Ok(InstructionValue::NewExpression { callee, args, loc })
        }

        // ---- optional chaining ----
        oxc::Expression::ChainExpression(chain) => lower_chain_expression(builder, chain),

        // ---- conditional / sequence / assignment ----
        oxc::Expression::ConditionalExpression(cond) => {
            lower_conditional_expression(builder, cond, loc)
        }
        oxc::Expression::SequenceExpression(seq) => {
            lower_sequence_expression(builder, seq, loc)
        }
        oxc::Expression::AssignmentExpression(assign) => {
            lower_assignment_expression(builder, assign, loc)
        }

        // ---- object / array ----
        oxc::Expression::ObjectExpression(obj) => lower_object_expression(builder, obj, loc),
        oxc::Expression::ArrayExpression(arr) => lower_array_expression(builder, arr, loc),

        // ---- templates ----
        oxc::Expression::TemplateLiteral(tmpl) => {
            let subexprs: Vec<Place> = tmpl
                .expressions
                .iter()
                .map(|e| lower_expression_to_temporary(builder, e))
                .collect::<Result<Vec<_>, _>>()?;
            let quasis: Vec<TemplateQuasi> = tmpl
                .quasis
                .iter()
                .map(|q| TemplateQuasi {
                    raw: q.value.raw.to_string(),
                    cooked: q.value.cooked.as_ref().map(|c| c.to_string()),
                })
                .collect();
            Ok(InstructionValue::TemplateLiteral {
                subexprs,
                quasis,
                loc,
            })
        }
        oxc::Expression::TaggedTemplateExpression(tagged) => {
            lower_tagged_template(builder, tagged, loc)
        }

        // ---- await ----
        oxc::Expression::AwaitExpression(await_expr) => {
            let value = lower_expression_to_temporary(builder, &await_expr.argument)?;
            Ok(InstructionValue::Await { value, loc })
        }

        // ---- meta property (import.meta) ----
        oxc::Expression::MetaProperty(meta) => {
            if meta.meta.name == "import" && meta.property.name == "meta" {
                Ok(InstructionValue::MetaProperty {
                    meta: meta.meta.name.to_string(),
                    property: meta.property.name.to_string(),
                    loc,
                })
            } else {
                Ok(todo_value(
                    builder,
                    "expression: MetaProperty other than import.meta",
                    loc,
                ))
            }
        }

        // ---- parenthesized: unwrap ----
        oxc::Expression::ParenthesizedExpression(paren) => {
            lower_expression(builder, &paren.expression)
        }

        // ---- TS wrappers ----
        oxc::Expression::TSNonNullExpression(ts) => lower_expression(builder, &ts.expression),
        oxc::Expression::TSInstantiationExpression(ts) => {
            lower_expression(builder, &ts.expression)
        }
        oxc::Expression::TSAsExpression(ts) => lower_type_cast(builder, &ts.expression, "as", loc),
        oxc::Expression::TSSatisfiesExpression(ts) => {
            lower_type_cast(builder, &ts.expression, "satisfies", loc)
        }
        oxc::Expression::TSTypeAssertion(ts) => {
            lower_type_cast(builder, &ts.expression, "as", loc)
        }

        // ---- not-yet-transcribed kinds: graceful Todo bail ----
        other => Ok(todo_value(
            builder,
            &format!("expression: {}", expression_kind_name(other)),
            loc,
        )),
    }
}

// =============================================================================
// Identifier
// =============================================================================

/// Lower an identifier reference to a LoadLocal / LoadContext / LoadGlobal
/// InstructionValue.
fn lower_identifier_value(
    builder: &mut HirBuilder,
    ident: &oxc::IdentifierReference,
) -> Result<InstructionValue, CompilerError> {
    let loc = Some(builder.loc_of_span(ident.span));
    let symbol_id = sq::resolve_identifier_reference(builder.semantic(), ident);
    let binding = builder.resolve_identifier_symbol(&ident.name, symbol_id, loc.clone())?;
    match binding {
        VariableBinding::Identifier { identifier, .. } => {
            let is_context = builder.is_context_symbol(symbol_id);
            let place = Place {
                identifier,
                effect: Effect::Unknown,
                reactive: false,
                loc: loc.clone(),
            };
            if is_context {
                Ok(InstructionValue::LoadContext { place, loc })
            } else {
                Ok(InstructionValue::LoadLocal { place, loc })
            }
        }
        non_local => Ok(InstructionValue::LoadGlobal {
            binding: non_local_binding_of(non_local),
            loc,
        }),
    }
}

/// Lower an identifier reference to a `Place` (loads via LoadGlobal temporary
/// for non-locals). Mirrors the reference `lower_identifier`.
fn lower_identifier_to_place(
    builder: &mut HirBuilder,
    ident: &oxc::IdentifierReference,
) -> Result<Place, CompilerError> {
    let loc = Some(builder.loc_of_span(ident.span));
    let symbol_id = sq::resolve_identifier_reference(builder.semantic(), ident);
    let binding = builder.resolve_identifier_symbol(&ident.name, symbol_id, loc.clone())?;
    match binding {
        VariableBinding::Identifier { identifier, .. } => Ok(Place {
            identifier,
            effect: Effect::Unknown,
            reactive: false,
            loc,
        }),
        non_local => {
            let instr_value = InstructionValue::LoadGlobal {
                binding: non_local_binding_of(non_local),
                loc: loc.clone(),
            };
            lower_value_to_temporary(builder, instr_value)
        }
    }
}

/// Convert a non-`Identifier` `VariableBinding` to a `NonLocalBinding`.
fn non_local_binding_of(binding: VariableBinding) -> NonLocalBinding {
    match binding {
        VariableBinding::Global { name } => NonLocalBinding::Global { name },
        VariableBinding::ModuleLocal { name } => NonLocalBinding::ModuleLocal { name },
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
        VariableBinding::Identifier { .. } => {
            unreachable!("non_local_binding_of called with Identifier binding")
        }
    }
}

// =============================================================================
// Unary
// =============================================================================

fn lower_unary_expression(
    builder: &mut HirBuilder,
    unary: &oxc::UnaryExpression,
    loc: Option<SourceLocation>,
) -> Result<InstructionValue, CompilerError> {
    use oxc_syntax::operator::UnaryOperator as Op;
    match unary.operator {
        Op::Delete => {
            // delete can apply to member expressions; otherwise it's a syntax
            // error / unsupported.
            if let Some(member) = unary.argument.as_member_expression() {
                let object = lower_expression_to_temporary(builder, member.object())?;
                match member {
                    oxc::MemberExpression::StaticMemberExpression(static_member) => {
                        Ok(InstructionValue::PropertyDelete {
                            object,
                            property: PropertyLiteral::String(
                                static_member.property.name.to_string(),
                            ),
                            loc,
                        })
                    }
                    oxc::MemberExpression::ComputedMemberExpression(computed) => {
                        let property =
                            lower_expression_to_temporary(builder, &computed.expression)?;
                        Ok(InstructionValue::ComputedDelete {
                            object,
                            property,
                            loc,
                        })
                    }
                    oxc::MemberExpression::PrivateFieldExpression(_) => Ok(todo_value(
                        builder,
                        "expression: delete of private field",
                        loc,
                    )),
                }
            } else {
                Ok(todo_value(builder, "expression: delete of non-member", loc))
            }
        }
        op => {
            let operator = convert_unary_operator(op)
                .expect("delete is handled above; remaining operators always convert");
            let value = lower_expression_to_temporary(builder, &unary.argument)?;
            Ok(InstructionValue::UnaryExpression {
                operator,
                value,
                loc,
            })
        }
    }
}

// =============================================================================
// Logical
// =============================================================================

fn lower_logical_expression(
    builder: &mut HirBuilder,
    expr: &oxc::LogicalExpression,
    loc: Option<SourceLocation>,
) -> Result<InstructionValue, CompilerError> {
    let continuation_block = builder.reserve(builder.current_block_kind());
    let continuation_id = continuation_block.id;
    let test_block = builder.reserve(BlockKind::Value);
    let test_block_id = test_block.id;
    let place = build_temporary_place(builder, loc.clone());
    let left_loc = Some(builder.loc_of_span(expr.left.span()));
    let left_place = build_temporary_place(builder, left_loc);

    // Block for short-circuit case: store left value as result, goto continuation.
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

    // Block for evaluating right side.
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

    let hir_op = convert_logical_operator(expr.operator);

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

    // Now in test block: lower left expression, copy to left_place.
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

// =============================================================================
// Conditional (ternary)
// =============================================================================

fn lower_conditional_expression(
    builder: &mut HirBuilder,
    expr: &oxc::ConditionalExpression,
    loc: Option<SourceLocation>,
) -> Result<InstructionValue, CompilerError> {
    let continuation_block = builder.reserve(builder.current_block_kind());
    let continuation_id = continuation_block.id;
    let test_block = builder.reserve(BlockKind::Value);
    let test_block_id = test_block.id;
    let place = build_temporary_place(builder, loc.clone());

    let consequent_ast_loc = Some(builder.loc_of_span(expr.consequent.span()));
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

    let alternate_ast_loc = Some(builder.loc_of_span(expr.alternate.span()));
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

// =============================================================================
// Sequence
// =============================================================================

fn lower_sequence_expression(
    builder: &mut HirBuilder,
    seq: &oxc::SequenceExpression,
    loc: Option<SourceLocation>,
) -> Result<InstructionValue, CompilerError> {
    if seq.expressions.is_empty() {
        return Ok(todo_value(builder, "expression: empty SequenceExpression", loc));
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

// =============================================================================
// Member expression
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

/// Lower an oxc `MemberExpression` (static or computed). If `lowered_object`
/// is provided it is used as the receiver; otherwise the object subexpression
/// is lowered.
pub(crate) fn lower_member_expression(
    builder: &mut HirBuilder,
    member: &oxc::MemberExpression,
    lowered_object: Option<Place>,
) -> Result<LoweredMemberExpression, CompilerError> {
    let loc = Some(builder.loc_of_span(member.span()));
    let object = match lowered_object {
        Some(obj) => obj,
        None => lower_expression_to_temporary(builder, member.object())?,
    };

    match member {
        oxc::MemberExpression::StaticMemberExpression(static_member) => {
            let prop_literal = PropertyLiteral::String(static_member.property.name.to_string());
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
        }
        oxc::MemberExpression::ComputedMemberExpression(computed) => {
            // A numeric-literal computed index is treated as a PropertyLoad in TS.
            if let oxc::Expression::NumericLiteral(lit) = &computed.expression {
                let prop_literal = PropertyLiteral::Number(FloatValue::new(lit.value));
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
            let property = lower_expression_to_temporary(builder, &computed.expression)?;
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
        oxc::MemberExpression::PrivateFieldExpression(_) => {
            // Private fields are not yet transcribed; bail gracefully with an
            // empty-string property so callers can continue.
            builder.record_diagnostic(todo_diagnostic(
                "member expression: private field",
                loc.clone(),
            ));
            Ok(LoweredMemberExpression {
                object,
                property: MemberProperty::Literal(PropertyLiteral::String(String::new())),
                value: InstructionValue::Primitive {
                    value: PrimitiveValue::Undefined,
                    loc,
                },
            })
        }
    }
}

// =============================================================================
// Calls
// =============================================================================

fn lower_call_expression(
    builder: &mut HirBuilder,
    call: &oxc::CallExpression,
    loc: Option<SourceLocation>,
) -> Result<InstructionValue, CompilerError> {
    // A member-expression callee lowers to a MethodCall.
    if let Some(member) = call.callee.as_member_expression() {
        let lowered = lower_member_expression(builder, member, None)?;
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

pub(crate) fn lower_arguments(
    builder: &mut HirBuilder,
    args: &[oxc::Argument],
) -> Result<Vec<PlaceOrSpread>, CompilerError> {
    let mut result = Vec::new();
    for arg in args {
        match arg {
            oxc::Argument::SpreadElement(spread) => {
                let place = lower_expression_to_temporary(builder, &spread.argument)?;
                result.push(PlaceOrSpread::Spread(SpreadPattern { place }));
            }
            other => {
                // Every non-spread Argument is an Expression.
                let expr = other
                    .as_expression()
                    .expect("non-spread Argument is always an Expression");
                let place = lower_expression_to_temporary(builder, expr)?;
                result.push(PlaceOrSpread::Place(place));
            }
        }
    }
    Ok(result)
}

// =============================================================================
// Optional chaining
// =============================================================================

/// Lower a `ChainExpression` (`a?.b`, `a?.()`, etc.). The chain's inner element
/// is a member or call expression with optional links.
fn lower_chain_expression(
    builder: &mut HirBuilder,
    chain: &oxc::ChainExpression,
) -> Result<InstructionValue, CompilerError> {
    match &chain.expression {
        oxc::ChainElement::CallExpression(call) => {
            lower_optional_call_expression(builder, call, None)
        }
        oxc::ChainElement::ComputedMemberExpression(_)
        | oxc::ChainElement::StaticMemberExpression(_) => {
            let member = chain
                .expression
                .as_member_expression()
                .expect("computed/static chain element is a member expression");
            let place = lower_optional_member_expression(builder, member, None)?.1;
            Ok(InstructionValue::LoadLocal {
                loc: place.loc.clone(),
                place,
            })
        }
        oxc::ChainElement::PrivateFieldExpression(pf) => {
            let loc = Some(builder.loc_of_span(pf.span()));
            Ok(todo_value(builder, "expression: optional private field", loc))
        }
        oxc::ChainElement::TSNonNullExpression(ts) => {
            lower_expression(builder, &ts.expression)
        }
    }
}

/// Returns (object, value_place). The `value_place` holds the chain's result.
fn lower_optional_member_expression(
    builder: &mut HirBuilder,
    member: &oxc::MemberExpression,
    parent_alternate: Option<BlockId>,
) -> Result<(Place, Place), CompilerError> {
    let optional = member.optional();
    let loc = Some(builder.loc_of_span(member.span()));
    let place = build_temporary_place(builder, loc.clone());
    let continuation_block = builder.reserve(builder.current_block_kind());
    let continuation_id = continuation_block.id;
    let consequent = builder.reserve(BlockKind::Value);

    let alternate = optional_alternate_block(
        builder,
        parent_alternate,
        place.clone(),
        continuation_id,
        loc.clone(),
    )?;

    let mut object: Option<Place> = None;
    let test_block = builder.try_enter(BlockKind::Value, |builder, _block_id| {
        object = Some(lower_optional_object(builder, member.object(), alternate)?);
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

    builder.try_enter_reserved(consequent, |builder| {
        let lowered = lower_member_expression(builder, member, Some(obj.clone()))?;
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

fn lower_optional_call_expression(
    builder: &mut HirBuilder,
    call: &oxc::CallExpression,
    parent_alternate: Option<BlockId>,
) -> Result<InstructionValue, CompilerError> {
    let optional = call.optional;
    let loc = Some(builder.loc_of_span(call.span()));
    let place = build_temporary_place(builder, loc.clone());
    let continuation_block = builder.reserve(builder.current_block_kind());
    let continuation_id = continuation_block.id;
    let consequent = builder.reserve(BlockKind::Value);

    let alternate = optional_alternate_block(
        builder,
        parent_alternate,
        place.clone(),
        continuation_id,
        loc.clone(),
    )?;

    enum CalleeInfo {
        CallExpression { callee: Place },
        MethodCall { receiver: Place, property: Place },
    }

    let mut callee_info: Option<CalleeInfo> = None;

    let test_block = builder.try_enter(BlockKind::Value, |builder, _block_id| {
        let callee = &call.callee;
        if let oxc::Expression::ChainExpression(inner_chain) = callee {
            // Nested chain link inside the callee position.
            match &inner_chain.expression {
                oxc::ChainElement::CallExpression(inner_call) => {
                    let value =
                        lower_optional_call_expression(builder, inner_call, Some(alternate))?;
                    let value_place = lower_value_to_temporary(builder, value)?;
                    callee_info = Some(CalleeInfo::CallExpression {
                        callee: value_place,
                    });
                }
                oxc::ChainElement::ComputedMemberExpression(_)
                | oxc::ChainElement::StaticMemberExpression(_) => {
                    let inner_member = inner_chain
                        .expression
                        .as_member_expression()
                        .expect("chain element is a member expression");
                    let (obj, value) =
                        lower_optional_member_expression(builder, inner_member, Some(alternate))?;
                    callee_info = Some(CalleeInfo::MethodCall {
                        receiver: obj,
                        property: value,
                    });
                }
                _ => {
                    let callee_place = lower_expression_to_temporary(builder, callee)?;
                    callee_info = Some(CalleeInfo::CallExpression {
                        callee: callee_place,
                    });
                }
            }
        } else if let Some(member) = callee.as_member_expression() {
            if member.optional() {
                let (obj, value) =
                    lower_optional_member_expression(builder, member, Some(alternate))?;
                callee_info = Some(CalleeInfo::MethodCall {
                    receiver: obj,
                    property: value,
                });
            } else {
                let lowered = lower_member_expression(builder, member, None)?;
                let property_place = lower_value_to_temporary(builder, lowered.value)?;
                callee_info = Some(CalleeInfo::MethodCall {
                    receiver: lowered.object,
                    property: property_place,
                });
            }
        } else {
            let callee_place = lower_expression_to_temporary(builder, callee)?;
            callee_info = Some(CalleeInfo::CallExpression {
                callee: callee_place,
            });
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

    builder.try_enter_reserved(consequent, |builder| {
        let args = lower_arguments(builder, &call.arguments)?;
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

/// Lower the object position of an optional member/call: if it's itself a chain
/// link, thread the shared `alternate` block; otherwise lower normally.
fn lower_optional_object(
    builder: &mut HirBuilder,
    object: &oxc::Expression,
    alternate: BlockId,
) -> Result<Place, CompilerError> {
    if let oxc::Expression::ChainExpression(chain) = object {
        match &chain.expression {
            oxc::ChainElement::CallExpression(call) => {
                let value = lower_optional_call_expression(builder, call, Some(alternate))?;
                return lower_value_to_temporary(builder, value);
            }
            oxc::ChainElement::ComputedMemberExpression(_)
            | oxc::ChainElement::StaticMemberExpression(_) => {
                let member = chain
                    .expression
                    .as_member_expression()
                    .expect("chain element is a member expression");
                let (_obj, value) =
                    lower_optional_member_expression(builder, member, Some(alternate))?;
                return Ok(value);
            }
            _ => {}
        }
    }
    // An optional member whose object is itself an (un-chained) optional member
    // appears directly (oxc wraps the *outermost* link in ChainExpression).
    if let Some(member) = object.as_member_expression() {
        if member.optional() {
            let (_obj, value) =
                lower_optional_member_expression(builder, member, Some(alternate))?;
            return Ok(value);
        }
    }
    lower_expression_to_temporary(builder, object)
}

/// Build the shared alternate block for an optional chain (sets result to
/// `undefined` and jumps to the continuation), reusing the parent's alternate
/// when continuing a nested chain.
fn optional_alternate_block(
    builder: &mut HirBuilder,
    parent_alternate: Option<BlockId>,
    place: Place,
    continuation_id: BlockId,
    loc: Option<SourceLocation>,
) -> Result<BlockId, CompilerError> {
    if let Some(parent_alt) = parent_alternate {
        return Ok(parent_alt);
    }
    Ok(builder.try_enter(BlockKind::Value, |builder, _block_id| {
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
    })?)
}

// =============================================================================
// Object expression
// =============================================================================

fn lower_object_expression(
    builder: &mut HirBuilder,
    obj: &oxc::ObjectExpression,
    loc: Option<SourceLocation>,
) -> Result<InstructionValue, CompilerError> {
    let mut properties: Vec<ObjectPropertyOrSpread> = Vec::new();
    for prop in &obj.properties {
        match prop {
            oxc::ObjectPropertyKind::ObjectProperty(p) => {
                if p.method {
                    // Object methods need function lowering (later stage); bail.
                    let prop_loc = Some(builder.loc_of_span(p.span));
                    builder.record_diagnostic(todo_diagnostic(
                        "object expression: method property",
                        prop_loc,
                    ));
                    continue;
                }
                let key = match lower_object_property_key(builder, &p.key, p.computed)? {
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
            oxc::ObjectPropertyKind::SpreadProperty(spread) => {
                let place = lower_expression_to_temporary(builder, &spread.argument)?;
                properties.push(ObjectPropertyOrSpread::Spread(SpreadPattern { place }));
            }
        }
    }
    Ok(InstructionValue::ObjectExpression { properties, loc })
}

/// Lower an object property key to an `ObjectPropertyKey`.
fn lower_object_property_key(
    builder: &mut HirBuilder,
    key: &oxc::PropertyKey,
    computed: bool,
) -> Result<Option<ObjectPropertyKey>, CompilerError> {
    match key {
        oxc::PropertyKey::StaticIdentifier(ident) if !computed => {
            Ok(Some(ObjectPropertyKey::Identifier {
                name: ident.name.to_string(),
            }))
        }
        oxc::PropertyKey::StringLiteral(lit) => Ok(Some(ObjectPropertyKey::String {
            name: lit.value.to_string(),
        })),
        oxc::PropertyKey::Identifier(ident) if !computed => {
            Ok(Some(ObjectPropertyKey::Identifier {
                name: ident.name.to_string(),
            }))
        }
        oxc::PropertyKey::NumericLiteral(lit) if !computed => {
            Ok(Some(ObjectPropertyKey::Identifier {
                name: format_number_key(lit.value),
            }))
        }
        _ if computed => {
            let expr = key
                .as_expression()
                .expect("computed property key is always an expression");
            let place = lower_expression_to_temporary(builder, expr)?;
            Ok(Some(ObjectPropertyKey::Computed { name: place }))
        }
        _ => {
            let loc = Some(builder.loc_of_span(key.span()));
            builder.record_diagnostic(todo_diagnostic(
                "object expression: unsupported key type",
                loc,
            ));
            Ok(None)
        }
    }
}

/// Format a numeric object key the way JS would stringify it (matching the
/// reference's `lit.value.to_string()` for plausible integer keys).
fn format_number_key(value: f64) -> String {
    if value.fract() == 0.0 && value.is_finite() {
        format!("{}", value as i64)
    } else {
        format!("{}", value)
    }
}

// =============================================================================
// Array expression
// =============================================================================

fn lower_array_expression(
    builder: &mut HirBuilder,
    arr: &oxc::ArrayExpression,
    loc: Option<SourceLocation>,
) -> Result<InstructionValue, CompilerError> {
    let mut elements: Vec<ArrayElement> = Vec::new();
    for element in &arr.elements {
        match element {
            oxc::ArrayExpressionElement::Elision(_) => {
                elements.push(ArrayElement::Hole);
            }
            oxc::ArrayExpressionElement::SpreadElement(spread) => {
                let place = lower_expression_to_temporary(builder, &spread.argument)?;
                elements.push(ArrayElement::Spread(SpreadPattern { place }));
            }
            other => {
                let expr = other
                    .as_expression()
                    .expect("non-spread/elision array element is an expression");
                let place = lower_expression_to_temporary(builder, expr)?;
                elements.push(ArrayElement::Place(place));
            }
        }
    }
    Ok(InstructionValue::ArrayExpression { elements, loc })
}

// =============================================================================
// Tagged template
// =============================================================================

fn lower_tagged_template(
    builder: &mut HirBuilder,
    tagged: &oxc::TaggedTemplateExpression,
    loc: Option<SourceLocation>,
) -> Result<InstructionValue, CompilerError> {
    if !tagged.quasi.expressions.is_empty() {
        return Ok(todo_value(
            builder,
            "expression: tagged template with interpolations",
            loc,
        ));
    }
    if tagged.quasi.quasis.len() != 1 {
        return Ok(todo_value(
            builder,
            "expression: tagged template with multiple quasis",
            loc,
        ));
    }
    let quasi = &tagged.quasi.quasis[0];
    let cooked = quasi.value.cooked.as_ref().map(|c| c.to_string());
    if quasi.value.raw.as_str() != cooked.clone().unwrap_or_default() {
        return Ok(todo_value(
            builder,
            "expression: tagged template where cooked differs from raw",
            loc,
        ));
    }
    let value = TemplateQuasi {
        raw: quasi.value.raw.to_string(),
        cooked,
    };
    let tag = lower_expression_to_temporary(builder, &tagged.tag)?;
    Ok(InstructionValue::TaggedTemplateExpression { tag, value, loc })
}

// =============================================================================
// Type cast (TS as / satisfies / type assertion)
// =============================================================================

/// Lower a TS type-cast wrapper. Full type-annotation lowering is deferred to a
/// later stage, so we emit a fresh type var and carry the cast kind.
fn lower_type_cast(
    builder: &mut HirBuilder,
    inner: &oxc::Expression,
    kind: &str,
    loc: Option<SourceLocation>,
) -> Result<InstructionValue, CompilerError> {
    let value = lower_expression_to_temporary(builder, inner)?;
    let type_ = builder.make_type();
    Ok(InstructionValue::TypeCastExpression {
        value,
        type_,
        type_annotation_name: None,
        type_annotation_kind: Some(kind.to_string()),
        type_annotation: None,
        loc,
    })
}

// =============================================================================
// Update expression (++ / --)
// =============================================================================

fn lower_update_expression(
    builder: &mut HirBuilder,
    update: &oxc::UpdateExpression,
    loc: Option<SourceLocation>,
) -> Result<InstructionValue, CompilerError> {
    use oxc::SimpleAssignmentTarget as Target;
    match &update.argument {
        Target::AssignmentTargetIdentifier(ident) => {
            lower_update_identifier(builder, update, ident, loc)
        }
        Target::ComputedMemberExpression(_) | Target::StaticMemberExpression(_) => {
            let member = update
                .argument
                .as_member_expression()
                .expect("computed/static target is a member expression");
            lower_update_member(builder, update, member, loc)
        }
        _ => Ok(todo_value(
            builder,
            "expression: update of unsupported target",
            loc,
        )),
    }
}

fn lower_update_identifier(
    builder: &mut HirBuilder,
    update: &oxc::UpdateExpression,
    ident: &oxc::IdentifierReference,
    loc: Option<SourceLocation>,
) -> Result<InstructionValue, CompilerError> {
    let symbol_id = sq::resolve_identifier_reference(builder.semantic(), ident);
    if builder.is_context_symbol(symbol_id) {
        return Ok(todo_value(
            builder,
            "expression: update of context-captured variable",
            loc,
        ));
    }

    let ident_loc = Some(builder.loc_of_span(ident.span));
    let binding =
        builder.resolve_identifier_symbol(&ident.name, symbol_id, ident_loc.clone())?;
    let identifier = match binding {
        VariableBinding::Identifier { identifier, .. } => identifier,
        _ => {
            return Ok(todo_value(
                builder,
                "expression: update where argument is a global",
                loc,
            ));
        }
    };
    let lvalue_place = Place {
        identifier,
        effect: Effect::Unknown,
        reactive: false,
        loc: ident_loc.clone(),
    };

    // Load the current value.
    let value = lower_identifier_to_place(builder, ident)?;
    let operation = convert_update_operator(update.operator);

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

fn lower_update_member(
    builder: &mut HirBuilder,
    update: &oxc::UpdateExpression,
    member: &oxc::MemberExpression,
    loc: Option<SourceLocation>,
) -> Result<InstructionValue, CompilerError> {
    let _ = loc;
    let binary_op = match update.operator {
        oxc_syntax::operator::UpdateOperator::Increment => BinaryOperator::Add,
        oxc_syntax::operator::UpdateOperator::Decrement => BinaryOperator::Subtract,
    };
    // The inner operations use the member's loc (matching TS leftExpr.node.loc).
    let member_loc = Some(builder.loc_of_span(member.span()));
    let lowered = lower_member_expression(builder, member, None)?;
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

// =============================================================================
// Assignment-as-expression
// =============================================================================

fn lower_assignment_expression(
    builder: &mut HirBuilder,
    expr: &oxc::AssignmentExpression,
    loc: Option<SourceLocation>,
) -> Result<InstructionValue, CompilerError> {
    use oxc_syntax::operator::AssignmentOperator as Op;
    if matches!(expr.operator, Op::Assign) {
        lower_simple_assignment(builder, expr, loc)
    } else {
        lower_compound_assignment(builder, expr, loc)
    }
}

/// Lower a `=` assignment expression.
fn lower_simple_assignment(
    builder: &mut HirBuilder,
    expr: &oxc::AssignmentExpression,
    loc: Option<SourceLocation>,
) -> Result<InstructionValue, CompilerError> {
    use oxc::AssignmentTarget as Target;
    match &expr.left {
        Target::AssignmentTargetIdentifier(ident) => {
            let right = lower_expression_to_temporary(builder, &expr.right)?;
            let ident_loc = Some(builder.loc_of_span(ident.span));
            let symbol_id = sq::resolve_identifier_reference(builder.semantic(), ident);
            let binding =
                builder.resolve_identifier_symbol(&ident.name, symbol_id, ident_loc.clone())?;
            match binding {
                VariableBinding::Identifier {
                    identifier,
                    binding_kind,
                } => {
                    if binding_kind == BindingKind::Const {
                        builder.record_diagnostic(todo_diagnostic(
                            "expression: reassignment of const variable",
                            ident_loc.clone(),
                        ));
                        return Ok(InstructionValue::LoadLocal {
                            place: right.clone(),
                            loc: ident_loc,
                        });
                    }
                    let place = Place {
                        identifier,
                        reactive: false,
                        effect: Effect::Unknown,
                        loc: ident_loc,
                    };
                    if builder.is_context_symbol(symbol_id) {
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
                    // Global / module-local assignment.
                    let temp = lower_value_to_temporary(
                        builder,
                        InstructionValue::StoreGlobal {
                            name: ident.name.to_string(),
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
        Target::StaticMemberExpression(_) | Target::ComputedMemberExpression(_) => {
            let member = expr
                .left
                .as_member_expression()
                .expect("static/computed assignment target is a member expression");
            lower_member_assignment(builder, member, &expr.right)
        }
        _ => {
            // Destructuring assignment needs pattern lowering (later stage).
            Ok(todo_value(
                builder,
                "expression: destructuring assignment",
                loc,
            ))
        }
    }
}

/// Lower `a.b = value` / `a[b] = value`.
fn lower_member_assignment(
    builder: &mut HirBuilder,
    member: &oxc::MemberExpression,
    right_expr: &oxc::Expression,
) -> Result<InstructionValue, CompilerError> {
    let right = lower_expression_to_temporary(builder, right_expr)?;
    let left_loc = Some(builder.loc_of_span(member.span()));
    let object = lower_expression_to_temporary(builder, member.object())?;

    let temp = match member {
        oxc::MemberExpression::StaticMemberExpression(static_member) => lower_value_to_temporary(
            builder,
            InstructionValue::PropertyStore {
                object,
                property: PropertyLiteral::String(static_member.property.name.to_string()),
                value: right,
                loc: left_loc,
            },
        )?,
        oxc::MemberExpression::ComputedMemberExpression(computed) => {
            if let oxc::Expression::NumericLiteral(num) = &computed.expression {
                lower_value_to_temporary(
                    builder,
                    InstructionValue::PropertyStore {
                        object,
                        property: PropertyLiteral::Number(FloatValue::new(num.value)),
                        value: right,
                        loc: left_loc,
                    },
                )?
            } else {
                let prop = lower_expression_to_temporary(builder, &computed.expression)?;
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
        oxc::MemberExpression::PrivateFieldExpression(_) => {
            return Ok(todo_value(
                builder,
                "expression: assignment to private field",
                left_loc,
            ));
        }
    };
    Ok(InstructionValue::LoadLocal {
        place: temp.clone(),
        loc: temp.loc.clone(),
    })
}

/// Lower a compound assignment (`+=`, `-=`, …). Logical assignments
/// (`||=`, `&&=`, `??=`) are not yet supported.
fn lower_compound_assignment(
    builder: &mut HirBuilder,
    expr: &oxc::AssignmentExpression,
    loc: Option<SourceLocation>,
) -> Result<InstructionValue, CompilerError> {
    use oxc::AssignmentTarget as Target;
    use oxc_syntax::operator::AssignmentOperator as Op;
    let binary_op = match expr.operator {
        Op::Addition => BinaryOperator::Add,
        Op::Subtraction => BinaryOperator::Subtract,
        Op::Multiplication => BinaryOperator::Multiply,
        Op::Division => BinaryOperator::Divide,
        Op::Remainder => BinaryOperator::Modulo,
        Op::Exponential => BinaryOperator::Exponent,
        Op::ShiftLeft => BinaryOperator::ShiftLeft,
        Op::ShiftRight => BinaryOperator::ShiftRight,
        Op::ShiftRightZeroFill => BinaryOperator::UnsignedShiftRight,
        Op::BitwiseOR => BinaryOperator::BitwiseOr,
        Op::BitwiseXOR => BinaryOperator::BitwiseXor,
        Op::BitwiseAnd => BinaryOperator::BitwiseAnd,
        Op::LogicalOr | Op::LogicalAnd | Op::LogicalNullish => {
            return Ok(todo_value(
                builder,
                "expression: logical assignment (||=, &&=, ??=)",
                loc,
            ));
        }
        Op::Assign => unreachable!("Assign handled by lower_simple_assignment"),
    };

    match &expr.left {
        Target::AssignmentTargetIdentifier(ident) => {
            let symbol_id = sq::resolve_identifier_reference(builder.semantic(), ident);
            let left_place = lower_identifier_to_place(builder, ident)?;
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
            let ident_loc = Some(builder.loc_of_span(ident.span));
            let binding =
                builder.resolve_identifier_symbol(&ident.name, symbol_id, ident_loc.clone())?;
            match binding {
                VariableBinding::Identifier { identifier, .. } => {
                    let place = Place {
                        identifier,
                        reactive: false,
                        effect: Effect::Unknown,
                        loc: ident_loc,
                    };
                    if builder.is_context_symbol(symbol_id) {
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
                    let temp = lower_value_to_temporary(
                        builder,
                        InstructionValue::StoreGlobal {
                            name: ident.name.to_string(),
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
        Target::StaticMemberExpression(_) | Target::ComputedMemberExpression(_) => {
            let member = expr
                .left
                .as_member_expression()
                .expect("static/computed compound target is a member expression");
            let member_loc = Some(builder.loc_of_span(member.span()));
            let lowered = lower_member_expression(builder, member, None)?;
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
            match lowered_property {
                MemberProperty::Literal(prop_literal) => Ok(InstructionValue::PropertyStore {
                    object,
                    property: prop_literal,
                    value: result,
                    loc: member_loc,
                }),
                MemberProperty::Computed(prop_place) => Ok(InstructionValue::ComputedStore {
                    object,
                    property: prop_place,
                    value: result,
                    loc: member_loc,
                }),
            }
        }
        _ => Ok(todo_value(
            builder,
            "expression: compound assignment to complex pattern",
            loc,
        )),
    }
}

// =============================================================================
// Diagnostics helper
// =============================================================================

pub(crate) fn expression_kind_name(expr: &oxc::Expression) -> &'static str {
    use oxc::Expression::*;
    match expr {
        BooleanLiteral(_) => "BooleanLiteral",
        NullLiteral(_) => "NullLiteral",
        NumericLiteral(_) => "NumericLiteral",
        BigIntLiteral(_) => "BigIntLiteral",
        RegExpLiteral(_) => "RegExpLiteral",
        StringLiteral(_) => "StringLiteral",
        TemplateLiteral(_) => "TemplateLiteral",
        Identifier(_) => "Identifier",
        MetaProperty(_) => "MetaProperty",
        Super(_) => "Super",
        ArrayExpression(_) => "ArrayExpression",
        ArrowFunctionExpression(_) => "ArrowFunctionExpression",
        AssignmentExpression(_) => "AssignmentExpression",
        AwaitExpression(_) => "AwaitExpression",
        BinaryExpression(_) => "BinaryExpression",
        CallExpression(_) => "CallExpression",
        ChainExpression(_) => "ChainExpression",
        ClassExpression(_) => "ClassExpression",
        ConditionalExpression(_) => "ConditionalExpression",
        FunctionExpression(_) => "FunctionExpression",
        ImportExpression(_) => "ImportExpression",
        LogicalExpression(_) => "LogicalExpression",
        NewExpression(_) => "NewExpression",
        ObjectExpression(_) => "ObjectExpression",
        ParenthesizedExpression(_) => "ParenthesizedExpression",
        SequenceExpression(_) => "SequenceExpression",
        TaggedTemplateExpression(_) => "TaggedTemplateExpression",
        ThisExpression(_) => "ThisExpression",
        UnaryExpression(_) => "UnaryExpression",
        UpdateExpression(_) => "UpdateExpression",
        YieldExpression(_) => "YieldExpression",
        PrivateInExpression(_) => "PrivateInExpression",
        JSXElement(_) => "JSXElement",
        JSXFragment(_) => "JSXFragment",
        StaticMemberExpression(_) => "StaticMemberExpression",
        ComputedMemberExpression(_) => "ComputedMemberExpression",
        PrivateFieldExpression(_) => "PrivateFieldExpression",
        _ => "TSExpression/Other",
    }
}
