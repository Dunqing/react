// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the root directory of this source tree.

//! Program assembly + print for native oxc codegen (stage N2.1).
//!
//! Solves the borrow ordering: the compile pipeline runs while the *input*
//! `oxc_ast::Program` + `oxc_semantic::Semantic` are immutably borrowed, so we
//! cannot splice compiled functions into that program. Instead the pipeline
//! returns owned (allocator-free) [`NativeArtifact`]s; this module — running
//! after the semantic borrow has ended — re-parses the original source into a
//! *fresh owned* program in its own allocator, builds each compiled function's
//! oxc AST via `codegen_oxc_function`, replaces the original function nodes by
//! span, injects the `import { c as _c } from "<runtime>"`, and prints.

use std::collections::HashSet;

use oxc_allocator::Allocator;
use oxc_allocator::Box as ArenaBox;
use oxc_ast::AstBuilder;
use oxc_ast::ast as oxc;
use oxc_span::GetSpan;
use oxc_span::SPAN;
use oxc_span::SourceType;
use react_compiler::entrypoint::native_codegen::NativeArtifact;
use react_compiler_reactive_scopes::codegen_oxc::codegen_oxc_function;

/// The local binding name for the runtime cache import (`import { c as _c }`).
const MEMO_LOCAL_NAME: &str = "_c";

/// Build + print the compiled program from native codegen artifacts.
///
/// Returns `Some(code)` if at least one artifact compiled successfully, else
/// `None` (the caller falls back to passthrough). `runtime_module` is the
/// module specifier for the cache import (e.g. `react/compiler-runtime`).
pub fn assemble_and_print(
    source_text: &str,
    source_type: SourceType,
    artifacts: &[NativeArtifact],
    runtime_module: &str,
) -> Option<String> {
    if artifacts.is_empty() {
        return None;
    }

    let allocator = Allocator::default();
    let parsed = oxc_parser::Parser::new(&allocator, source_text, source_type).parse();
    if parsed.panicked {
        return None;
    }
    let mut program = parsed.program;
    let builder = AstBuilder::new(&allocator);

    // Compile each artifact into an oxc function, keyed by its source span.
    // Bail (skip) on artifacts whose codegen returns an error.
    let mut compiled: Vec<CompiledNode<'_>> = Vec::new();
    let mut any_memo = false;
    for artifact in artifacts {
        match codegen_oxc_function(
            &artifact.reactive_fn,
            &artifact.env,
            artifact.unique_identifiers.clone(),
            &builder,
            MEMO_LOCAL_NAME,
        ) {
            Ok(output) => {
                if output.memo_slots_used > 0 {
                    any_memo = true;
                }
                compiled.push(CompiledNode {
                    span: artifact.fn_span,
                    is_arrow: artifact.is_arrow,
                    function: output.function,
                });
            }
            Err(_bail) => {
                // Graceful bail: leave this function uncompiled.
                // For categorization tooling, optionally emit the bail reason.
                if std::env::var("REACT_COMPILER_CODEGEN_BAIL_DEBUG").is_ok() {
                    eprintln!("CODEGEN_BAIL: {}", _bail.reason);
                }
            }
        }
    }

    if compiled.is_empty() {
        return None;
    }

    // Partition outlined functions (sentinel span (0, 0)) from spanned ones.
    // Outlined functions have no source location, so they cannot be spliced by
    // span; they are appended to the program body as top-level function
    // declarations after splicing (mirroring TS `insertNewOutlinedFunctionNode`).
    let (outlined, spanned): (Vec<CompiledNode<'_>>, Vec<CompiledNode<'_>>) =
        compiled.into_iter().partition(|c| c.span == (0, 0));

    // Splice compiled functions into the program body by matching spans.
    splice_functions(&builder, &mut program, spanned);

    // Append outlined functions as top-level function declarations.
    for node in outlined {
        let decl = build_replacement(&builder, node);
        program.body.push(decl);
    }

    // Inject the runtime cache import if any compiled function used memo slots.
    if any_memo {
        inject_memo_import(&builder, &mut program, runtime_module);
    }

    Some(oxc_codegen::Codegen::new().build(&program).code)
}

struct CompiledNode<'a> {
    span: (u32, u32),
    is_arrow: bool,
    function: oxc::Function<'a>,
}

/// Replace each original function statement with its compiled form, matched by
/// the original source span. Supports top-level `function F`, `export [default]
/// function F`, and `const F = (fn|arrow)`.
fn splice_functions<'a>(
    builder: &AstBuilder<'a>,
    program: &mut oxc::Program<'a>,
    compiled: Vec<CompiledNode<'a>>,
) {
    // Map span.start -> compiled node, consumed as we walk the body.
    let mut by_start: std::collections::HashMap<u32, CompiledNode<'a>> =
        compiled.into_iter().map(|c| (c.span.0, c)).collect();

    for stmt in program.body.iter_mut() {
        let stmt_span = stmt.span();
        // Direct match on the statement span (function declarations).
        if let Some(node) = by_start.remove(&stmt_span.start) {
            *stmt = build_replacement(builder, node);
            continue;
        }
        // For variable declarations the function span starts at the init
        // expression, not the statement. Try to match nested forms.
        if try_splice_nested(builder, stmt, &mut by_start) {
            continue;
        }
    }
}

/// Try to splice a compiled function nested inside the statement (variable
/// declarator init, export wrappers). Returns true if a replacement happened.
fn try_splice_nested<'a>(
    builder: &AstBuilder<'a>,
    stmt: &mut oxc::Statement<'a>,
    by_start: &mut std::collections::HashMap<u32, CompiledNode<'a>>,
) -> bool {
    match stmt {
        oxc::Statement::VariableDeclaration(var) => {
            for decl in var.declarations.iter_mut() {
                if let Some(init) = &decl.init {
                    let init_start = init.span().start;
                    if let Some(node) = by_start.remove(&init_start) {
                        // Replace the initializer with a function expression
                        // (or arrow-as-function-expression).
                        decl.init = Some(function_expression(builder, node));
                        return true;
                    }
                }
            }
            false
        }
        oxc::Statement::ExportNamedDeclaration(export) => {
            if let Some(oxc::Declaration::VariableDeclaration(var)) = &mut export.declaration {
                for decl in var.declarations.iter_mut() {
                    if let Some(init) = &decl.init {
                        let init_start = init.span().start;
                        if let Some(node) = by_start.remove(&init_start) {
                            decl.init = Some(function_expression(builder, node));
                            return true;
                        }
                    }
                }
            }
            false
        }
        _ => false,
    }
}

/// Build a top-level replacement statement for a compiled function.
fn build_replacement<'a>(builder: &AstBuilder<'a>, node: CompiledNode<'a>) -> oxc::Statement<'a> {
    // Top-level matched node is a function declaration form.
    let mut function = node.function;
    function.r#type = oxc::FunctionType::FunctionDeclaration;
    oxc::Statement::FunctionDeclaration(builder.alloc(function))
}

/// Build a function expression initializer for a `const X = ...` form.
fn function_expression<'a>(
    builder: &AstBuilder<'a>,
    node: CompiledNode<'a>,
) -> oxc::Expression<'a> {
    let mut function = node.function;
    if node.is_arrow {
        // Render the compiled function as a true arrow to preserve the original
        // form (`X = () => {...}`). Arrows are anonymous, so drop any name.
        // Also apply the single-return optimization (`() => { return X; }`
        // becomes `() => X`).
        let params = function.params.unbox();
        let is_async = function.r#async;
        let body = function
            .body
            .expect("function body present after codegen")
            .unbox();
        let directives = body.directives;
        let statements = body.statements;

        // Single-return optimization: only when there are no directives and the
        // sole statement is a return with an argument.
        let single_return_arg = statements.len() == 1
            && directives.is_empty()
            && matches!(statements.first(), Some(oxc::Statement::ReturnStatement(r)) if r.argument.is_some());

        if single_return_arg {
            let mut statements = statements;
            if let oxc::Statement::ReturnStatement(ret) = statements.pop().unwrap() {
                let arg = ret.unbox().argument.unwrap();
                let mut v = builder.vec();
                v.push(builder.statement_expression(SPAN, arg));
                let fn_body = builder.function_body(SPAN, builder.vec(), v);
                return builder.expression_arrow_function(
                    SPAN,
                    true,
                    is_async,
                    None::<ArenaBox<'a, oxc::TSTypeParameterDeclaration<'a>>>,
                    params,
                    None::<ArenaBox<'a, oxc::TSTypeAnnotation<'a>>>,
                    fn_body,
                );
            }
            unreachable!();
        }

        let fn_body = builder.function_body(SPAN, directives, statements);
        builder.expression_arrow_function(
            SPAN,
            false,
            is_async,
            None::<ArenaBox<'a, oxc::TSTypeParameterDeclaration<'a>>>,
            params,
            None::<ArenaBox<'a, oxc::TSTypeAnnotation<'a>>>,
            fn_body,
        )
    } else {
        function.r#type = oxc::FunctionType::FunctionExpression;
        oxc::Expression::FunctionExpression(builder.alloc(function))
    }
}

/// Prepend `import { c as _c } from "<runtime_module>";` to the program body.
fn inject_memo_import<'a>(
    builder: &AstBuilder<'a>,
    program: &mut oxc::Program<'a>,
    runtime_module: &str,
) {
    // Avoid duplicate import if the source already imports it (rare in the
    // simple slice; cheap to guard).
    let already = program.body.iter().any(|stmt| {
        if let oxc::Statement::ImportDeclaration(import) = stmt {
            import.source.value.as_str() == runtime_module
        } else {
            false
        }
    });
    let _ = already; // intentionally do not dedupe imported specifier yet

    let imported =
        oxc::ModuleExportName::IdentifierName(builder.identifier_name(SPAN, builder.atom("c")));
    let local = builder.binding_identifier(SPAN, builder.atom(MEMO_LOCAL_NAME));
    let specifier = builder.import_specifier(SPAN, imported, local, oxc::ImportOrExportKind::Value);
    let mut specifiers = builder.vec();
    specifiers.push(oxc::ImportDeclarationSpecifier::ImportSpecifier(
        builder.alloc(specifier),
    ));
    let source = builder.string_literal(SPAN, builder.atom(runtime_module), None);
    let import_decl = builder.import_declaration(
        SPAN,
        Some(specifiers),
        source,
        None,
        None::<ArenaBox<'a, oxc::WithClause<'a>>>,
        oxc::ImportOrExportKind::Value,
    );
    let import_stmt = oxc::Statement::ImportDeclaration(builder.alloc(import_decl));
    program.body.insert(0, import_stmt);
}

/// Collect the set of top-level statement span starts (helper, unused for now
/// but documents the matching contract).
#[allow(dead_code)]
fn top_level_starts(program: &oxc::Program) -> HashSet<u32> {
    program.body.iter().map(|s| s.span().start).collect()
}
