//! Statement / control-flow lowering — reads `oxc_ast` directly and produces HIR.
//!
//! Stage N1.2.4 transcribes the statement-lowering logic from the pre-flip
//! `react_compiler_ast` reference (`git show 1a02788:…/build_hir/statements.rs`)
//! to read `oxc_ast` enums and resolve scope/bindings via `semantic_queries`.
//!
//! The *algorithm* and the HIR it produces are unchanged from the reference;
//! only the AST access changes. Constructs that depend on infrastructure not
//! yet transcribed (block hoisting / `DeclareContext`, nested function bodies,
//! destructuring declaration targets) keep the graceful `Todo` bail so the
//! crate stays green.

use oxc_ast::ast as oxc;
use oxc_span::GetSpan;
use react_compiler_diagnostics::CompilerError;
use react_compiler_diagnostics::CompilerErrorDetail;
use react_compiler_diagnostics::ErrorCategory;
use react_compiler_hir::*;

use crate::hir_builder::HirBuilder;
use crate::hir_builder::is_always_reserved_word;
use crate::hir_builder::reserved_identifier_diagnostic;
use crate::hir_builder::todo_diagnostic;
use crate::semantic_queries as sq;

use super::lower_expression_to_temporary;
use super::lower_value_to_temporary;

// =============================================================================
// Small loc helper (oxc Statement implements GetSpan)
// =============================================================================

/// HIR source location of a statement.
fn statement_loc(builder: &HirBuilder, stmt: &oxc::Statement) -> Option<SourceLocation> {
    Some(builder.loc_of_span(stmt.span()))
}

// =============================================================================
// lower_statement
// =============================================================================

/// Lower a single statement into the current block, threading the optional
/// loop/switch `label` (for labeled `break`/`continue`).
pub(crate) fn lower_statement(
    builder: &mut HirBuilder,
    stmt: &oxc::Statement,
) -> Result<(), CompilerError> {
    lower_statement_labeled(builder, stmt, None)
}

#[allow(clippy::too_many_lines)]
pub(crate) fn lower_statement_labeled(
    builder: &mut HirBuilder,
    stmt: &oxc::Statement,
    label: Option<&str>,
) -> Result<(), CompilerError> {
    match stmt {
        // ---- trivial ----
        oxc::Statement::EmptyStatement(_) => Ok(()),
        oxc::Statement::DebuggerStatement(dbg) => {
            let loc = Some(builder.loc_of_span(dbg.span));
            lower_value_to_temporary(builder, InstructionValue::Debugger { loc })?;
            Ok(())
        }
        oxc::Statement::ExpressionStatement(expr_stmt) => {
            lower_expression_to_temporary(builder, &expr_stmt.expression)?;
            Ok(())
        }

        // ---- return / throw ----
        oxc::Statement::ReturnStatement(ret) => lower_return_statement(builder, ret),
        oxc::Statement::ThrowStatement(throw) => lower_throw_statement(builder, throw),

        // ---- block ----
        oxc::Statement::BlockStatement(block) => lower_block(builder, block),

        // ---- variable declaration ----
        oxc::Statement::VariableDeclaration(var_decl) => {
            lower_variable_declaration(builder, var_decl)
        }

        // ---- break / continue ----
        oxc::Statement::BreakStatement(brk) => {
            let loc = Some(builder.loc_of_span(brk.span));
            let label_name = brk.label.as_ref().map(|l| l.name.as_str());
            let target = builder.lookup_break(label_name)?;
            let fallthrough = builder.reserve(BlockKind::Block);
            builder.terminate_with_continuation(
                Terminal::Goto {
                    block: target,
                    variant: GotoVariant::Break,
                    id: EvaluationOrder(0),
                    loc,
                },
                fallthrough,
            );
            Ok(())
        }
        oxc::Statement::ContinueStatement(cont) => {
            let loc = Some(builder.loc_of_span(cont.span));
            let label_name = cont.label.as_ref().map(|l| l.name.as_str());
            let target = builder.lookup_continue(label_name)?;
            let fallthrough = builder.reserve(BlockKind::Block);
            builder.terminate_with_continuation(
                Terminal::Goto {
                    block: target,
                    variant: GotoVariant::Continue,
                    id: EvaluationOrder(0),
                    loc,
                },
                fallthrough,
            );
            Ok(())
        }

        // ---- control flow ----
        oxc::Statement::IfStatement(if_stmt) => lower_if_statement(builder, if_stmt),
        oxc::Statement::ForStatement(for_stmt) => lower_for_statement(builder, for_stmt, label),
        oxc::Statement::WhileStatement(while_stmt) => {
            lower_while_statement(builder, while_stmt, label)
        }
        oxc::Statement::DoWhileStatement(do_while_stmt) => {
            lower_do_while_statement(builder, do_while_stmt, label)
        }
        oxc::Statement::ForInStatement(for_in) => lower_for_in_statement(builder, for_in, label),
        oxc::Statement::ForOfStatement(for_of) => lower_for_of_statement(builder, for_of, label),
        oxc::Statement::SwitchStatement(switch_stmt) => {
            lower_switch_statement(builder, switch_stmt, label)
        }
        oxc::Statement::TryStatement(try_stmt) => lower_try_statement(builder, try_stmt),
        oxc::Statement::LabeledStatement(labeled_stmt) => {
            lower_labeled_statement(builder, labeled_stmt)
        }

        // ---- function declaration: lower the body + store to the name binding ----
        oxc::Statement::FunctionDeclaration(func_decl) => {
            super::functions::lower_function_declaration(builder, func_decl)
        }

        // ---- with: unsupported syntax (matches reference) ----
        oxc::Statement::WithStatement(with_stmt) => {
            let loc = Some(builder.loc_of_span(with_stmt.span));
            builder.record_error(CompilerErrorDetail {
                category: ErrorCategory::UnsupportedSyntax,
                reason: "JavaScript 'with' syntax is not supported".to_string(),
                description: Some("'with' syntax is considered deprecated and removed from JavaScript standards, consider alternatives".to_string()),
                loc,
                suggestions: None,
            })?;
            Ok(())
        }

        // ---- class declaration: unsupported syntax (matches reference) ----
        oxc::Statement::ClassDeclaration(cls) => {
            let loc = Some(builder.loc_of_span(cls.span));
            builder.record_error(CompilerErrorDetail {
                category: ErrorCategory::UnsupportedSyntax,
                reason: "Inline `class` declarations are not supported".to_string(),
                description: Some(
                    "Move class declarations outside of components/hooks".to_string(),
                ),
                loc,
                suggestions: None,
            })?;
            Ok(())
        }

        // ---- import / export: only valid at module top level (matches reference) ----
        oxc::Statement::ImportDeclaration(_)
        | oxc::Statement::ExportNamedDeclaration(_)
        | oxc::Statement::ExportDefaultDeclaration(_)
        | oxc::Statement::ExportAllDeclaration(_) => {
            let loc = Some(builder.loc_of_span(stmt.span()));
            builder.record_error(CompilerErrorDetail {
                category: ErrorCategory::Syntax,
                reason: "JavaScript `import` and `export` statements may only appear at the top level of a module".to_string(),
                description: None,
                loc,
                suggestions: None,
            })?;
            Ok(())
        }

        // ---- TypeScript / Flow type-only declarations: skipped ----
        oxc::Statement::TSTypeAliasDeclaration(_)
        | oxc::Statement::TSInterfaceDeclaration(_)
        | oxc::Statement::TSModuleDeclaration(_)
        | oxc::Statement::TSGlobalDeclaration(_)
        | oxc::Statement::TSImportEqualsDeclaration(_)
        | oxc::Statement::TSExportAssignment(_)
        | oxc::Statement::TSNamespaceExportDeclaration(_) => Ok(()),

        // ---- TS enum: lower as an UnsupportedNode that carries the original
        // source text so codegen can re-emit the `enum` declaration verbatim.
        // Mirrors the reference (`BuildHIR.ts`), which lowers a `TSEnumDeclaration`
        // to a temporary holding `{kind: 'UnsupportedNode', node: …}` and re-emits
        // the node in codegen. The enum binding is a runtime value, so it is not
        // pruned as type-only. ----
        oxc::Statement::TSEnumDeclaration(e) => {
            let loc = Some(builder.loc_of_span(e.span));
            let source = builder
                .source_text()
                .get(e.span.start as usize..e.span.end as usize)
                .map(|s| serde_json::Value::String(s.to_string()));
            lower_value_to_temporary(
                builder,
                InstructionValue::UnsupportedNode {
                    node_type: Some("TSEnumDeclaration".to_string()),
                    original_node: source,
                    loc,
                },
            )?;
            Ok(())
        }
    }
}

// =============================================================================
// return / throw
// =============================================================================

fn lower_return_statement(
    builder: &mut HirBuilder,
    ret: &oxc::ReturnStatement,
) -> Result<(), CompilerError> {
    let loc = Some(builder.loc_of_span(ret.span));
    let value = if let Some(arg) = &ret.argument {
        lower_expression_to_temporary(builder, arg)?
    } else {
        let undefined_value = InstructionValue::Primitive {
            value: PrimitiveValue::Undefined,
            loc: None,
        };
        lower_value_to_temporary(builder, undefined_value)?
    };
    let fallthrough = builder.reserve(BlockKind::Block);
    builder.terminate_with_continuation(
        Terminal::Return {
            value,
            return_variant: ReturnVariant::Explicit,
            id: EvaluationOrder(0),
            loc,
            effects: None,
        },
        fallthrough,
    );
    Ok(())
}

fn lower_throw_statement(
    builder: &mut HirBuilder,
    throw: &oxc::ThrowStatement,
) -> Result<(), CompilerError> {
    let loc = Some(builder.loc_of_span(throw.span));
    let value = lower_expression_to_temporary(builder, &throw.argument)?;

    // Throwing from inside a try/catch is not yet transcribed.
    if builder.resolve_throw_handler().is_some() {
        builder.record_error(CompilerErrorDetail {
            category: ErrorCategory::Todo,
            reason: "(BuildHIR::lowerStatement) Support ThrowStatement inside of try/catch"
                .to_string(),
            description: None,
            loc,
            suggestions: None,
        })?;
    }

    let fallthrough = builder.reserve(BlockKind::Block);
    builder.terminate_with_continuation(
        Terminal::Throw {
            value,
            id: EvaluationOrder(0),
            loc,
        },
        fallthrough,
    );
    Ok(())
}

// =============================================================================
// block
// =============================================================================

/// Lower a block statement, performing block-scoped hoisting (`DeclareContext`)
/// for declarations referenced before their lexical position (matches the
/// `case 'BlockStatement'` arm of `BuildHIR.ts`).
fn lower_block(builder: &mut HirBuilder, block: &oxc::BlockStatement) -> Result<(), CompilerError> {
    let block_scope = block.scope_id.get();
    lower_block_statements(builder, block_scope, &block.body)
}

/// Lower an ordered list of statements that share `block_scope`, hoisting any
/// declarations that are referenced before they are declared. Used for both
/// `BlockStatement` bodies and function bodies (which share the function
/// scope in the Babel-shaped view the compiler expects).
pub(crate) fn lower_block_statements(
    builder: &mut HirBuilder,
    block_scope: Option<oxc_syntax::scope::ScopeId>,
    statements: &[oxc::Statement],
) -> Result<(), CompilerError> {
    use oxc_span::GetSpan;

    if let Some(scope) = block_scope {
        let spans: Vec<oxc_span::Span> = statements.iter().map(|s| s.span()).collect();
        let hoists = super::hoisting::compute_block_hoists(builder, scope, &spans);
        for (index, stmt) in statements.iter().enumerate() {
            if let Some(pending) = hoists.get(&index) {
                super::hoisting::emit_hoists(builder, pending)?;
            }
            lower_statement(builder, stmt)?;
        }
    } else {
        for stmt in statements {
            lower_statement(builder, stmt)?;
        }
    }
    Ok(())
}

// =============================================================================
// variable declaration
// =============================================================================

fn lower_variable_declaration(
    builder: &mut HirBuilder,
    var_decl: &oxc::VariableDeclaration,
) -> Result<(), CompilerError> {
    use oxc::VariableDeclarationKind as VK;

    if matches!(var_decl.kind, VK::Var | VK::AwaitUsing) {
        builder.record_error(CompilerErrorDetail {
            reason: "(BuildHIR::lowerStatement) Handle var kinds in VariableDeclaration"
                .to_string(),
            category: ErrorCategory::Todo,
            loc: Some(builder.loc_of_span(var_decl.span)),
            description: None,
            suggestions: None,
        })?;
        // Treat `var` as `let` so references to the variable don't break.
    }

    let kind = match var_decl.kind {
        VK::Let | VK::Var => InstructionKind::Let,
        VK::Const | VK::Using | VK::AwaitUsing => InstructionKind::Const,
    };

    let stmt_loc = Some(builder.loc_of_span(var_decl.span));
    for declarator in &var_decl.declarations {
        if let Some(init) = &declarator.init {
            let value = lower_expression_to_temporary(builder, init)?;
            lower_declarator_assignment(builder, stmt_loc, kind, &declarator.id, value)?;
        } else {
            // No initializer: emit DeclareLocal (or DeclareContext) for identifier
            // targets; bail on destructuring (it's a syntax error without an init,
            // but bail gracefully rather than panic).
            lower_declarator_declare(builder, kind, &declarator.id)?;
        }
    }
    Ok(())
}

/// Lower `<target> = <value>` for a declaration with an initializer.
/// Identifier targets lower for real; destructuring targets bail (patterns stage).
fn lower_declarator_assignment(
    builder: &mut HirBuilder,
    loc: Option<SourceLocation>,
    kind: InstructionKind,
    target: &oxc::BindingPattern,
    value: Place,
) -> Result<(), CompilerError> {
    match target {
        oxc::BindingPattern::BindingIdentifier(id) => {
            store_to_identifier(builder, loc, kind, id, value)?;
            Ok(())
        }
        other => {
            // Destructuring declaration targets (object / array / default).
            super::lower_assignment(
                builder,
                loc,
                kind,
                other,
                value,
                super::AssignmentStyle::Assignment,
            )?;
            Ok(())
        }
    }
}

/// Lower an initializer-less declarator (`let x;`) for an identifier target.
fn lower_declarator_declare(
    builder: &mut HirBuilder,
    kind: InstructionKind,
    target: &oxc::BindingPattern,
) -> Result<(), CompilerError> {
    let id = match target {
        oxc::BindingPattern::BindingIdentifier(id) => id,
        other => {
            builder.record_error(CompilerErrorDetail {
                reason: "Expected variable declaration to be an identifier if no initializer was provided".to_string(),
                category: ErrorCategory::Syntax,
                loc: Some(builder.loc_of_span(other.span())),
                description: None,
                suggestions: None,
            })?;
            return Ok(());
        }
    };

    let id_loc = Some(builder.loc_of_span(id.span));
    let symbol_id = id.symbol_id.get();
    let binding = builder.resolve_identifier_symbol(&id.name, symbol_id, id_loc)?;
    match binding {
        VariableBinding::Identifier { identifier, .. } => {
            builder.set_identifier_declaration_loc(identifier, &id_loc);
            let place = Place {
                identifier,
                effect: Effect::Unknown,
                reactive: false,
                loc: id_loc,
            };
            if builder.is_context_symbol(symbol_id) {
                if kind == InstructionKind::Const {
                    builder.record_error(CompilerErrorDetail {
                        reason: "Expect `const` declaration not to be reassigned".to_string(),
                        category: ErrorCategory::Syntax,
                        loc: id_loc,
                        description: None,
                        suggestions: None,
                    })?;
                }
                lower_value_to_temporary(
                    builder,
                    InstructionValue::DeclareContext {
                        lvalue: LValue {
                            kind: InstructionKind::Let,
                            place,
                        },
                        loc: id_loc,
                    },
                )?;
            } else {
                lower_value_to_temporary(
                    builder,
                    InstructionValue::DeclareLocal {
                        lvalue: LValue { kind, place },
                        type_annotation: None,
                        loc: id_loc,
                    },
                )?;
            }
            Ok(())
        }
        _ => {
            builder.record_error(CompilerErrorDetail {
                reason: "Could not find binding for declaration".to_string(),
                category: ErrorCategory::Invariant,
                loc: id_loc,
                description: None,
                suggestions: None,
            })?;
            Ok(())
        }
    }
}

/// Store `value` into the binding named by `id` (a declaration / for-head
/// identifier target). Emits StoreLocal / StoreContext / StoreGlobal and returns
/// the temporary holding the stored value (mirrors the reference `lower_assignment`
/// identifier path, restricted to the kinds statements produce).
fn store_to_identifier(
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
    let binding = builder.resolve_identifier_symbol(&id.name, symbol_id, id_loc)?;
    match binding {
        VariableBinding::Identifier { identifier, .. } => {
            builder.set_identifier_declaration_loc(identifier, &id_loc);
            let place = Place {
                identifier,
                effect: Effect::Unknown,
                reactive: false,
                loc,
            };
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
        VariableBinding::Global { name } => {
            let temp = lower_value_to_temporary(
                builder,
                InstructionValue::StoreGlobal { name, value, loc },
            )?;
            Ok(Some(temp))
        }
        _ => {
            // Import binding as a declaration target: cannot happen for real
            // declarations; bail gracefully.
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

// =============================================================================
// if
// =============================================================================

fn lower_if_statement(
    builder: &mut HirBuilder,
    if_stmt: &oxc::IfStatement,
) -> Result<(), CompilerError> {
    let loc = Some(builder.loc_of_span(if_stmt.span));
    let continuation_block = builder.reserve(BlockKind::Block);
    let continuation_id = continuation_block.id;

    let consequent_loc = statement_loc(builder, &if_stmt.consequent);
    let consequent_block = builder.try_enter(BlockKind::Block, |builder, _block_id| {
        lower_statement(builder, &if_stmt.consequent)?;
        Ok(Terminal::Goto {
            block: continuation_id,
            variant: GotoVariant::Break,
            id: EvaluationOrder(0),
            loc: consequent_loc,
        })
    })?;

    let alternate_block = if let Some(alternate) = &if_stmt.alternate {
        let alternate_loc = statement_loc(builder, alternate);
        builder.try_enter(BlockKind::Block, |builder, _block_id| {
            lower_statement(builder, alternate)?;
            Ok(Terminal::Goto {
                block: continuation_id,
                variant: GotoVariant::Break,
                id: EvaluationOrder(0),
                loc: alternate_loc,
            })
        })?
    } else {
        continuation_id
    };

    let test = lower_expression_to_temporary(builder, &if_stmt.test)?;
    builder.terminate_with_continuation(
        Terminal::If {
            test,
            consequent: consequent_block,
            alternate: alternate_block,
            fallthrough: continuation_id,
            id: EvaluationOrder(0),
            loc,
        },
        continuation_block,
    );
    Ok(())
}

// =============================================================================
// for
// =============================================================================

fn lower_for_statement(
    builder: &mut HirBuilder,
    for_stmt: &oxc::ForStatement,
    label: Option<&str>,
) -> Result<(), CompilerError> {
    let loc = Some(builder.loc_of_span(for_stmt.span));

    let test_block = builder.reserve(BlockKind::Loop);
    let test_block_id = test_block.id;
    let continuation_block = builder.reserve(BlockKind::Block);
    let continuation_id = continuation_block.id;

    // Init block: lower init expression/declaration, then goto test.
    let init_block = builder.try_enter(BlockKind::Loop, |builder, _block_id| {
        let init_loc = match &for_stmt.init {
            None => {
                let placeholder = InstructionValue::Primitive {
                    value: PrimitiveValue::Undefined,
                    loc,
                };
                lower_value_to_temporary(builder, placeholder)?;
                loc
            }
            Some(oxc::ForStatementInit::VariableDeclaration(var_decl)) => {
                let init_loc = Some(builder.loc_of_span(var_decl.span));
                lower_variable_declaration(builder, var_decl)?;
                init_loc
            }
            Some(init) => {
                // A non-declaration for-init (expression). The reference records a
                // Todo here, then still lowers the expression for parity.
                let expr = init
                    .as_expression()
                    .expect("non-declaration ForStatementInit is an expression");
                let init_loc = Some(builder.loc_of_span(expr.span()));
                builder.record_error(CompilerErrorDetail {
                    category: ErrorCategory::Todo,
                    reason: "(BuildHIR::lowerStatement) Handle non-variable initialization in ForStatement".to_string(),
                    description: None,
                    loc,
                    suggestions: None,
                })?;
                lower_expression_to_temporary(builder, expr)?;
                init_loc
            }
        };
        Ok(Terminal::Goto {
            block: test_block_id,
            variant: GotoVariant::Break,
            id: EvaluationOrder(0),
            loc: init_loc,
        })
    })?;

    // Update block (optional).
    let update_block_id = if let Some(update) = &for_stmt.update {
        let update_loc = Some(builder.loc_of_span(update.span()));
        Some(builder.try_enter(BlockKind::Loop, |builder, _block_id| {
            lower_expression_to_temporary(builder, update)?;
            Ok(Terminal::Goto {
                block: test_block_id,
                variant: GotoVariant::Break,
                id: EvaluationOrder(0),
                loc: update_loc,
            })
        })?)
    } else {
        None
    };

    let continue_target = update_block_id.unwrap_or(test_block_id);
    let body_loc = statement_loc(builder, &for_stmt.body);
    let body_block = builder.try_enter(BlockKind::Block, |builder, _block_id| {
        builder.loop_scope(
            label.map(|s| s.to_string()),
            continue_target,
            continuation_id,
            |builder| {
                lower_statement(builder, &for_stmt.body)?;
                Ok(Terminal::Goto {
                    block: continue_target,
                    variant: GotoVariant::Continue,
                    id: EvaluationOrder(0),
                    loc: body_loc,
                })
            },
        )
    })?;

    builder.terminate_with_continuation(
        Terminal::For {
            init: init_block,
            test: test_block_id,
            update: update_block_id,
            loop_block: body_block,
            fallthrough: continuation_id,
            id: EvaluationOrder(0),
            loc,
        },
        test_block,
    );

    // Fill in the test block.
    if let Some(test_expr) = &for_stmt.test {
        let test = lower_expression_to_temporary(builder, test_expr)?;
        builder.terminate_with_continuation(
            Terminal::Branch {
                test,
                consequent: body_block,
                alternate: continuation_id,
                fallthrough: continuation_id,
                id: EvaluationOrder(0),
                loc,
            },
            continuation_block,
        );
    } else {
        builder.record_error(CompilerErrorDetail {
            category: ErrorCategory::Todo,
            reason: "(BuildHIR::lowerStatement) Handle empty test in ForStatement".to_string(),
            description: None,
            loc,
            suggestions: None,
        })?;
        // Treat `for(;;)` as `while(true)` to keep the builder state consistent.
        let true_val = InstructionValue::Primitive {
            value: PrimitiveValue::Boolean(true),
            loc,
        };
        let test = lower_value_to_temporary(builder, true_val)?;
        builder.terminate_with_continuation(
            Terminal::Branch {
                test,
                consequent: body_block,
                alternate: continuation_id,
                fallthrough: continuation_id,
                id: EvaluationOrder(0),
                loc,
            },
            continuation_block,
        );
    }
    Ok(())
}

// =============================================================================
// while
// =============================================================================

fn lower_while_statement(
    builder: &mut HirBuilder,
    while_stmt: &oxc::WhileStatement,
    label: Option<&str>,
) -> Result<(), CompilerError> {
    let loc = Some(builder.loc_of_span(while_stmt.span));
    let conditional_block = builder.reserve(BlockKind::Loop);
    let conditional_id = conditional_block.id;
    let continuation_block = builder.reserve(BlockKind::Block);
    let continuation_id = continuation_block.id;

    let body_loc = statement_loc(builder, &while_stmt.body);
    let loop_block = builder.try_enter(BlockKind::Block, |builder, _block_id| {
        builder.loop_scope(
            label.map(|s| s.to_string()),
            conditional_id,
            continuation_id,
            |builder| {
                lower_statement(builder, &while_stmt.body)?;
                Ok(Terminal::Goto {
                    block: conditional_id,
                    variant: GotoVariant::Continue,
                    id: EvaluationOrder(0),
                    loc: body_loc,
                })
            },
        )
    })?;

    builder.terminate_with_continuation(
        Terminal::While {
            test: conditional_id,
            loop_block,
            fallthrough: continuation_id,
            id: EvaluationOrder(0),
            loc,
        },
        conditional_block,
    );

    let test = lower_expression_to_temporary(builder, &while_stmt.test)?;
    builder.terminate_with_continuation(
        Terminal::Branch {
            test,
            consequent: loop_block,
            alternate: continuation_id,
            fallthrough: conditional_id,
            id: EvaluationOrder(0),
            loc,
        },
        continuation_block,
    );
    Ok(())
}

// =============================================================================
// do-while
// =============================================================================

fn lower_do_while_statement(
    builder: &mut HirBuilder,
    do_while_stmt: &oxc::DoWhileStatement,
    label: Option<&str>,
) -> Result<(), CompilerError> {
    let loc = Some(builder.loc_of_span(do_while_stmt.span));
    let conditional_block = builder.reserve(BlockKind::Loop);
    let conditional_id = conditional_block.id;
    let continuation_block = builder.reserve(BlockKind::Block);
    let continuation_id = continuation_block.id;

    let body_loc = statement_loc(builder, &do_while_stmt.body);
    let loop_block = builder.try_enter(BlockKind::Block, |builder, _block_id| {
        builder.loop_scope(
            label.map(|s| s.to_string()),
            conditional_id,
            continuation_id,
            |builder| {
                lower_statement(builder, &do_while_stmt.body)?;
                Ok(Terminal::Goto {
                    block: conditional_id,
                    variant: GotoVariant::Continue,
                    id: EvaluationOrder(0),
                    loc: body_loc,
                })
            },
        )
    })?;

    builder.terminate_with_continuation(
        Terminal::DoWhile {
            loop_block,
            test: conditional_id,
            fallthrough: continuation_id,
            id: EvaluationOrder(0),
            loc,
        },
        conditional_block,
    );

    let test = lower_expression_to_temporary(builder, &do_while_stmt.test)?;
    builder.terminate_with_continuation(
        Terminal::Branch {
            test,
            consequent: loop_block,
            alternate: continuation_id,
            fallthrough: conditional_id,
            id: EvaluationOrder(0),
            loc,
        },
        continuation_block,
    );
    Ok(())
}

// =============================================================================
// for-in
// =============================================================================

fn lower_for_in_statement(
    builder: &mut HirBuilder,
    for_in: &oxc::ForInStatement,
    label: Option<&str>,
) -> Result<(), CompilerError> {
    let loc = Some(builder.loc_of_span(for_in.span));
    let continuation_block = builder.reserve(BlockKind::Block);
    let continuation_id = continuation_block.id;
    let init_block = builder.reserve(BlockKind::Loop);
    let init_block_id = init_block.id;

    let body_loc = statement_loc(builder, &for_in.body);
    let loop_block = builder.try_enter(BlockKind::Block, |builder, _block_id| {
        builder.loop_scope(
            label.map(|s| s.to_string()),
            init_block_id,
            continuation_id,
            |builder| {
                lower_statement(builder, &for_in.body)?;
                Ok(Terminal::Goto {
                    block: init_block_id,
                    variant: GotoVariant::Continue,
                    id: EvaluationOrder(0),
                    loc: body_loc,
                })
            },
        )
    })?;

    let value = lower_expression_to_temporary(builder, &for_in.right)?;
    builder.terminate_with_continuation(
        Terminal::ForIn {
            init: init_block_id,
            loop_block,
            fallthrough: continuation_id,
            id: EvaluationOrder(0),
            loc,
        },
        init_block,
    );

    let left_loc = for_in_of_left_loc(builder, &for_in.left).or(loc);
    let next_property = lower_value_to_temporary(
        builder,
        InstructionValue::NextPropertyOf {
            value,
            loc: left_loc,
        },
    )?;

    let assign_result =
        lower_for_head_target(builder, &for_in.left, left_loc, next_property.clone())?;
    let test_value = assign_result.unwrap_or(next_property);
    let test = lower_value_to_temporary(
        builder,
        InstructionValue::LoadLocal {
            place: test_value,
            loc: left_loc,
        },
    )?;
    builder.terminate_with_continuation(
        Terminal::Branch {
            test,
            consequent: loop_block,
            alternate: continuation_id,
            fallthrough: continuation_id,
            id: EvaluationOrder(0),
            loc,
        },
        continuation_block,
    );
    Ok(())
}

// =============================================================================
// for-of
// =============================================================================

fn lower_for_of_statement(
    builder: &mut HirBuilder,
    for_of: &oxc::ForOfStatement,
    label: Option<&str>,
) -> Result<(), CompilerError> {
    let loc = Some(builder.loc_of_span(for_of.span));
    let continuation_block = builder.reserve(BlockKind::Block);
    let continuation_id = continuation_block.id;
    let init_block = builder.reserve(BlockKind::Loop);
    let init_block_id = init_block.id;
    let test_block = builder.reserve(BlockKind::Loop);
    let test_block_id = test_block.id;

    if for_of.r#await {
        builder.record_error(CompilerErrorDetail {
            category: ErrorCategory::Todo,
            reason: "(BuildHIR::lowerStatement) Handle for-await loops".to_string(),
            description: None,
            loc,
            suggestions: None,
        })?;
        return Ok(());
    }

    let body_loc = statement_loc(builder, &for_of.body);
    let loop_block = builder.try_enter(BlockKind::Block, |builder, _block_id| {
        builder.loop_scope(
            label.map(|s| s.to_string()),
            init_block_id,
            continuation_id,
            |builder| {
                lower_statement(builder, &for_of.body)?;
                Ok(Terminal::Goto {
                    block: init_block_id,
                    variant: GotoVariant::Continue,
                    id: EvaluationOrder(0),
                    loc: body_loc,
                })
            },
        )
    })?;

    let value = lower_expression_to_temporary(builder, &for_of.right)?;
    builder.terminate_with_continuation(
        Terminal::ForOf {
            init: init_block_id,
            test: test_block_id,
            loop_block,
            fallthrough: continuation_id,
            id: EvaluationOrder(0),
            loc,
        },
        init_block,
    );

    // Init block: GetIterator, goto test.
    let iterator = lower_value_to_temporary(
        builder,
        InstructionValue::GetIterator {
            collection: value.clone(),
            loc: value.loc,
        },
    )?;
    builder.terminate_with_continuation(
        Terminal::Goto {
            block: test_block_id,
            variant: GotoVariant::Break,
            id: EvaluationOrder(0),
            loc,
        },
        test_block,
    );

    // Test block: IteratorNext, assign, branch.
    let left_loc = for_in_of_left_loc(builder, &for_of.left).or(loc);
    let advance_iterator = lower_value_to_temporary(
        builder,
        InstructionValue::IteratorNext {
            iterator: iterator.clone(),
            collection: value.clone(),
            loc: left_loc,
        },
    )?;

    let assign_result =
        lower_for_head_target(builder, &for_of.left, left_loc, advance_iterator.clone())?;
    let test_value = assign_result.unwrap_or(advance_iterator);
    let test = lower_value_to_temporary(
        builder,
        InstructionValue::LoadLocal {
            place: test_value,
            loc: left_loc,
        },
    )?;
    builder.terminate_with_continuation(
        Terminal::Branch {
            test,
            consequent: loop_block,
            alternate: continuation_id,
            fallthrough: continuation_id,
            id: EvaluationOrder(0),
            loc,
        },
        continuation_block,
    );
    Ok(())
}

/// Source location for a for-in/for-of head target (`ForStatementLeft`).
fn for_in_of_left_loc(
    builder: &HirBuilder,
    left: &oxc::ForStatementLeft,
) -> Option<SourceLocation> {
    match left {
        oxc::ForStatementLeft::VariableDeclaration(v) => Some(builder.loc_of_span(v.span)),
        other => other
            .as_assignment_target()
            .map(|t| builder.loc_of_span(t.span())),
    }
}

/// Lower the assignment target of a for-in/for-of head (`x` / `let x`).
///
/// Identifier targets lower for real (StoreLocal). Destructuring targets and
/// member-expression targets bail gracefully (patterns stage). Returns the
/// stored temporary if one was produced.
fn lower_for_head_target(
    builder: &mut HirBuilder,
    left: &oxc::ForStatementLeft,
    left_loc: Option<SourceLocation>,
    value: Place,
) -> Result<Option<Place>, CompilerError> {
    match left {
        oxc::ForStatementLeft::VariableDeclaration(var_decl) => {
            if var_decl.declarations.len() != 1 {
                builder.record_error(CompilerErrorDetail {
                    category: ErrorCategory::Invariant,
                    reason: format!(
                        "Expected only one declaration in for-in/of head, got {}",
                        var_decl.declarations.len()
                    ),
                    description: None,
                    loc: left_loc,
                    suggestions: None,
                })?;
            }
            let Some(declarator) = var_decl.declarations.first() else {
                return Ok(None);
            };
            match &declarator.id {
                oxc::BindingPattern::BindingIdentifier(id) => {
                    store_to_identifier(builder, left_loc, InstructionKind::Let, id, value)
                }
                other => super::lower_assignment(
                    builder,
                    left_loc,
                    InstructionKind::Let,
                    other,
                    value,
                    super::AssignmentStyle::Assignment,
                ),
            }
        }
        oxc::ForStatementLeft::AssignmentTargetIdentifier(ident) => {
            lower_for_head_reassign_identifier(builder, ident, left_loc, value)
        }
        other => {
            // Destructuring / member-expression assignment-target heads
            // (`for ([a, b] of …)`, `for ({a} of …)`, `for (a.b of …)`).
            if let Some(target) = other.as_assignment_target() {
                super::lower_assignment_target(builder, left_loc, target, value)
            } else {
                builder.record_diagnostic(todo_diagnostic(
                    "statement: non-identifier for-in/of head",
                    left_loc,
                ));
                Ok(None)
            }
        }
    }
}

/// Reassign an existing binding from a for-in/for-of head (`for (x in ...)`).
fn lower_for_head_reassign_identifier(
    builder: &mut HirBuilder,
    ident: &oxc::IdentifierReference,
    left_loc: Option<SourceLocation>,
    value: Place,
) -> Result<Option<Place>, CompilerError> {
    let symbol_id = sq::resolve_identifier_reference(builder.semantic(), ident);
    let binding = builder.resolve_identifier_symbol(&ident.name, symbol_id, left_loc)?;
    match binding {
        VariableBinding::Identifier { identifier, .. } => {
            let place = Place {
                identifier,
                effect: Effect::Unknown,
                reactive: false,
                loc: left_loc,
            };
            if builder.is_context_symbol(symbol_id) {
                let temp = lower_value_to_temporary(
                    builder,
                    InstructionValue::StoreContext {
                        lvalue: LValue {
                            kind: InstructionKind::Reassign,
                            place,
                        },
                        value,
                        loc: left_loc,
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
                        loc: left_loc,
                    },
                )?;
                Ok(Some(temp))
            }
        }
        _ => {
            let temp = lower_value_to_temporary(
                builder,
                InstructionValue::StoreGlobal {
                    name: ident.name.to_string(),
                    value,
                    loc: left_loc,
                },
            )?;
            Ok(Some(temp))
        }
    }
}

// =============================================================================
// switch
// =============================================================================

fn lower_switch_statement(
    builder: &mut HirBuilder,
    switch_stmt: &oxc::SwitchStatement,
    label: Option<&str>,
) -> Result<(), CompilerError> {
    let loc = Some(builder.loc_of_span(switch_stmt.span));
    let continuation_block = builder.reserve(BlockKind::Block);
    let continuation_id = continuation_block.id;

    // Iterate cases in reverse so each block can fall through to its successor.
    let mut fallthrough = continuation_id;
    let mut cases: Vec<Case> = Vec::new();
    let mut has_default = false;

    for ii in (0..switch_stmt.cases.len()).rev() {
        let case = &switch_stmt.cases[ii];
        let case_loc = Some(builder.loc_of_span(case.span));

        if case.test.is_none() {
            if has_default {
                builder.record_error(CompilerErrorDetail {
                    category: ErrorCategory::Syntax,
                    reason: "Expected at most one `default` branch in a switch statement"
                        .to_string(),
                    description: None,
                    loc: case_loc,
                    suggestions: None,
                })?;
                break;
            }
            has_default = true;
        }

        let fallthrough_target = fallthrough;
        let block = builder.try_enter(BlockKind::Block, |builder, _block_id| {
            builder.switch_scope(label.map(|s| s.to_string()), continuation_id, |builder| {
                for consequent in &case.consequent {
                    lower_statement(builder, consequent)?;
                }
                Ok(Terminal::Goto {
                    block: fallthrough_target,
                    variant: GotoVariant::Break,
                    id: EvaluationOrder(0),
                    loc: case_loc,
                })
            })
        })?;

        let test = if let Some(test_expr) = &case.test {
            Some(lower_expression_to_temporary(builder, test_expr)?)
        } else {
            None
        };

        cases.push(Case { test, block });
        fallthrough = block;
    }

    cases.reverse();

    if !has_default {
        cases.push(Case {
            test: None,
            block: continuation_id,
        });
    }

    let test = lower_expression_to_temporary(builder, &switch_stmt.discriminant)?;
    builder.terminate_with_continuation(
        Terminal::Switch {
            test,
            cases,
            fallthrough: continuation_id,
            id: EvaluationOrder(0),
            loc,
        },
        continuation_block,
    );
    Ok(())
}

// =============================================================================
// try / catch / finally
// =============================================================================

fn lower_try_statement(
    builder: &mut HirBuilder,
    try_stmt: &oxc::TryStatement,
) -> Result<(), CompilerError> {
    let loc = Some(builder.loc_of_span(try_stmt.span));
    let continuation_block = builder.reserve(BlockKind::Block);
    let continuation_id = continuation_block.id;

    let handler_clause = match &try_stmt.handler {
        Some(h) => h,
        None => {
            builder.record_error(CompilerErrorDetail {
                category: ErrorCategory::Todo,
                reason: "(BuildHIR::lowerStatement) Handle TryStatement without a catch clause"
                    .to_string(),
                description: None,
                loc,
                suggestions: None,
            })?;
            return Ok(());
        }
    };

    if try_stmt.finalizer.is_some() {
        builder.record_error(CompilerErrorDetail {
            category: ErrorCategory::Todo,
            reason:
                "(BuildHIR::lowerStatement) Handle TryStatement with a finalizer ('finally') clause"
                    .to_string(),
            description: None,
            loc,
            suggestions: None,
        })?;
    }

    // Set up the handler binding if the catch clause has a param.
    // Identifier params lower for real; destructuring catch params bail.
    let handler_binding_info: Option<(Place, &oxc::BindingIdentifier)> =
        if let Some(param) = &handler_clause.param {
            match &param.pattern {
                oxc::BindingPattern::BindingIdentifier(id) => {
                    let param_loc = Some(builder.loc_of_span(id.span));
                    let temp_id = builder.make_temporary(param_loc);
                    super::promote_temporary(builder, temp_id);
                    let place = Place {
                        identifier: temp_id,
                        effect: Effect::Unknown,
                        reactive: false,
                        loc: param_loc,
                    };
                    lower_value_to_temporary(
                        builder,
                        InstructionValue::DeclareLocal {
                            lvalue: LValue {
                                kind: InstructionKind::Catch,
                                place: place.clone(),
                            },
                            type_annotation: None,
                            loc: param_loc,
                        },
                    )?;
                    Some((place, id))
                }
                other => {
                    // Destructuring catch params (`catch ({message})`). Babel
                    // does not register destructured catch bindings in scope, so
                    // the TS reference records a per-identifier invariant — but
                    // that aborts HIR emission. To stay fault-tolerant (and keep
                    // parity with the rest of the lowering, which still emits HIR
                    // here), record a graceful Todo and produce no binding. The
                    // catch body is still lowered below.
                    builder.record_diagnostic(todo_diagnostic(
                        "statement: destructuring catch clause parameter",
                        Some(builder.loc_of_span(other.span())),
                    ));
                    None
                }
            }
        } else {
            None
        };

    let handler_loc = Some(builder.loc_of_span(handler_clause.span));
    let handler_binding_for_block = handler_binding_info.clone();
    let handler_block = builder.try_enter(BlockKind::Catch, |builder, _block_id| {
        if let Some((ref place, id)) = handler_binding_for_block {
            let param_loc = Some(builder.loc_of_span(id.span));
            store_to_identifier(
                builder,
                param_loc.or(handler_loc),
                InstructionKind::Catch,
                id,
                place.clone(),
            )?;
        }
        lower_block(builder, &handler_clause.body)?;
        Ok(Terminal::Goto {
            block: continuation_id,
            variant: GotoVariant::Break,
            id: EvaluationOrder(0),
            loc: handler_loc,
        })
    })?;

    let try_body_loc = Some(builder.loc_of_span(try_stmt.block.span));
    let try_block = builder.try_enter(BlockKind::Block, |builder, _block_id| {
        builder.try_enter_try_catch(handler_block, |builder| {
            lower_block(builder, &try_stmt.block)?;
            Ok(())
        })?;
        Ok(Terminal::Goto {
            block: continuation_id,
            variant: GotoVariant::Try,
            id: EvaluationOrder(0),
            loc: try_body_loc,
        })
    })?;

    builder.terminate_with_continuation(
        Terminal::Try {
            block: try_block,
            handler_binding: handler_binding_info.map(|(place, _)| place),
            handler: handler_block,
            fallthrough: continuation_id,
            id: EvaluationOrder(0),
            loc,
        },
        continuation_block,
    );
    Ok(())
}

// =============================================================================
// labeled statement
// =============================================================================

fn lower_labeled_statement(
    builder: &mut HirBuilder,
    labeled_stmt: &oxc::LabeledStatement,
) -> Result<(), CompilerError> {
    let label_name = labeled_stmt.label.name.as_str();
    let loc = Some(builder.loc_of_span(labeled_stmt.span));

    match &labeled_stmt.body {
        // Labeled loops push the label down so `continue label` resolves.
        oxc::Statement::ForStatement(_)
        | oxc::Statement::WhileStatement(_)
        | oxc::Statement::DoWhileStatement(_)
        | oxc::Statement::ForInStatement(_)
        | oxc::Statement::ForOfStatement(_) => {
            lower_statement_labeled(builder, &labeled_stmt.body, Some(label_name))
        }
        _ => {
            // Other statements create a continuation block to allow `break label`.
            let continuation_block = builder.reserve(BlockKind::Block);
            let continuation_id = continuation_block.id;
            let body_loc = statement_loc(builder, &labeled_stmt.body);
            let label_owned = label_name.to_string();

            let block = builder.try_enter(BlockKind::Block, |builder, _block_id| {
                builder.label_scope(label_owned, continuation_id, |builder| {
                    lower_statement(builder, &labeled_stmt.body)?;
                    Ok(())
                })?;
                Ok(Terminal::Goto {
                    block: continuation_id,
                    variant: GotoVariant::Break,
                    id: EvaluationOrder(0),
                    loc: body_loc,
                })
            })?;

            builder.terminate_with_continuation(
                Terminal::Label {
                    block,
                    fallthrough: continuation_id,
                    id: EvaluationOrder(0),
                    loc,
                },
                continuation_block,
            );
            Ok(())
        }
    }
}
