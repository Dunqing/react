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
//!
//! `@gating`: when an artifact carries a [`GatingPlan`], the function is emitted
//! in a gated form — both the compiled and the original function are kept and a
//! runtime gating flag selects between them. The common case becomes a
//! conditional (`const F = gating() ? <compiled> : <original>`); a function
//! referenced before its declaration uses a hoistable dispatcher. The resolved
//! gating import is injected after the `_c` import. Mirrors
//! `insertGatedFunctionDeclaration` in `Entrypoint/Gating.ts`.

use std::collections::HashMap;
use std::collections::HashSet;

use oxc_allocator::Allocator;
use oxc_allocator::Box as ArenaBox;
use oxc_ast::AstBuilder;
use oxc_ast::ast as oxc;
use oxc_ast::ast::Str;
use oxc_span::GetSpan;
use oxc_span::SPAN;
use oxc_span::SourceType;
use react_compiler::entrypoint::native_codegen::GatingPlan;
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
                    gating: artifact.gating.clone(),
                    insert_after: artifact.insert_after_span,
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
    // span. TS inserts each outlined declaration directly after its parent
    // function (`insertNewOutlinedFunctionNode`), so we group outlined nodes by
    // their parent's source-span start and emit them immediately after the
    // parent's spliced statement. Any outlined node whose parent is missing
    // (e.g. the parent bailed) falls back to being appended at the end.
    let (outlined, spanned): (Vec<CompiledNode<'_>>, Vec<CompiledNode<'_>>) =
        compiled.into_iter().partition(|c| c.span == (0, 0));

    let mut outlined_by_parent: HashMap<u32, Vec<CompiledNode<'_>>> = HashMap::new();
    let mut orphan_outlined: Vec<CompiledNode<'_>> = Vec::new();
    let spanned_starts: HashSet<u32> = spanned.iter().map(|c| c.span.0).collect();
    for node in outlined {
        match node.insert_after {
            Some(parent_span) if spanned_starts.contains(&parent_span.0) => {
                outlined_by_parent
                    .entry(parent_span.0)
                    .or_default()
                    .push(node);
            }
            _ => orphan_outlined.push(node),
        }
    }

    // Splice compiled functions into the program body by matching spans. Gating
    // imports needed by gated functions are collected as we go. Outlined nodes
    // are emitted right after their parent statement.
    let mut gating_imports: Vec<GatingImport> = Vec::new();
    splice_functions(
        &builder,
        &mut program,
        spanned,
        &mut gating_imports,
        &mut outlined_by_parent,
    );

    // Append any outlined functions whose parent was not spliced.
    for node in orphan_outlined {
        let decl = build_replacement(&builder, node);
        program.body.push(decl);
    }

    // Inject the runtime cache import if any compiled function used memo slots.
    if any_memo {
        inject_memo_import(&builder, &mut program, runtime_module, source_type);
    }

    // Inject the gating import(s) after the `_c` import (TS emits the gating
    // import right after the memo-cache import). Dedupe identical specifiers.
    inject_gating_imports(&builder, &mut program, &gating_imports);

    Some(oxc_codegen::Codegen::new().build(&program).code)
}

struct CompiledNode<'a> {
    span: (u32, u32),
    is_arrow: bool,
    function: oxc::Function<'a>,
    gating: Option<GatingPlan>,
    /// For outlined functions, the parent function's source span. The outlined
    /// declaration is inserted directly after the parent's spliced statement.
    insert_after: Option<(u32, u32)>,
}

/// A gating import to inject: `import { <imported> [as <local>] } from "<source>"`.
#[derive(Clone, PartialEq, Eq)]
struct GatingImport {
    source: String,
    imported: String,
    local: String,
}

/// Replace each original function statement with its compiled form, matched by
/// the original source span. Supports top-level `function F`, `export [default]
/// function F`, `const F = (fn|arrow)`, `export default <arrow|fnexpr>`,
/// reassignment `F = <fn>`, and object-property values. Gated functions
/// (carrying a [`GatingPlan`]) are emitted in their gated form, which may expand
/// a single statement into several.
fn splice_functions<'a>(
    builder: &AstBuilder<'a>,
    program: &mut oxc::Program<'a>,
    compiled: Vec<CompiledNode<'a>>,
    gating_imports: &mut Vec<GatingImport>,
    outlined_by_parent: &mut HashMap<u32, Vec<CompiledNode<'a>>>,
) {
    // Map span.start -> compiled node, consumed as we walk the body.
    let mut by_start: HashMap<u32, CompiledNode<'a>> =
        compiled.into_iter().map(|c| (c.span.0, c)).collect();

    // Rebuild the body, since gated functions can expand into multiple
    // statements (the use-before-declaration dispatcher form).
    let old_body = std::mem::replace(&mut program.body, builder.vec());
    for mut stmt in old_body {
        // Match on the function-declaration span. For bare `function F` this is
        // the statement span; for `export [default] function F` the function
        // span is nested inside the export wrapper, so look there too.
        if let Some(start) = function_declaration_start(&stmt)
            && let Some(node) = by_start.remove(&start) {
                emit_top_level(builder, &mut program.body, stmt, node, gating_imports);
                emit_outlined_children(builder, &mut program.body, start, outlined_by_parent);
                continue;
            }
        // For variable declarations / exports / assignments / object props the
        // function span starts at the init expression, not the statement. Record
        // which nested span(s) were spliced so any outlined children can be
        // emitted directly after this statement.
        let spliced = try_splice_nested(builder, &mut stmt, &mut by_start, gating_imports);
        program.body.push(stmt);
        for start in spliced {
            emit_outlined_children(builder, &mut program.body, start, outlined_by_parent);
        }
    }
}

/// Emit the outlined function declarations registered for the parent at
/// `parent_start`, directly after the parent's spliced statement. Mirrors TS
/// `insertNewOutlinedFunctionNode`.
fn emit_outlined_children<'a>(
    builder: &AstBuilder<'a>,
    body: &mut oxc_allocator::Vec<'a, oxc::Statement<'a>>,
    parent_start: u32,
    outlined_by_parent: &mut HashMap<u32, Vec<CompiledNode<'a>>>,
) {
    if let Some(children) = outlined_by_parent.remove(&parent_start) {
        for child in children {
            body.push(build_replacement(builder, child));
        }
    }
}

/// Emit a top-level statement whose span matched a compiled node directly. This
/// is the `function F(...) {}` / `export [default] function F(...) {}` case.
fn emit_top_level<'a>(
    builder: &AstBuilder<'a>,
    body: &mut oxc_allocator::Vec<'a, oxc::Statement<'a>>,
    stmt: oxc::Statement<'a>,
    node: CompiledNode<'a>,
    gating_imports: &mut Vec<GatingImport>,
) {
    // Locate the original function declaration and its surrounding form.
    let (orig_fn, wrapper) = unwrap_function_declaration(stmt);
    let orig_fn = match orig_fn {
        Some(f) => f,
        None => {
            // Shouldn't happen, but fall back to plain replacement.
            body.push(build_replacement(builder, node));
            return;
        }
    };

    let Some(plan) = node.gating.clone() else {
        // No gating: replace the function body with the compiled version,
        // preserving the original `export` / `export default` wrapper.
        body.push(rewrap_function_declaration(builder, node, &wrapper, &orig_fn));
        return;
    };

    gating_imports.push(GatingImport {
        source: plan.gating_source.clone(),
        imported: plan.gating_imported.clone(),
        local: plan.gating_local_name.clone(),
    });

    if plan.referenced_before_declaration {
        emit_dispatcher(builder, body, orig_fn, node, &plan);
        return;
    }

    let original_name = orig_fn.id.as_ref().map(|id| id.name.to_string());
    let compiled_expr = function_expression(builder, node);
    let original_expr = function_decl_to_expression(builder, orig_fn);
    let gating_expr =
        gating_conditional(builder, &plan.gating_local_name, compiled_expr, original_expr);

    match (wrapper, original_name) {
        // `export default function F` -> `const F = <gating>; export default F;`
        (Wrapper::ExportDefault, Some(name)) => {
            body.push(const_decl(builder, &name, gating_expr));
            body.push(export_default_ident(builder, &name));
        }
        // `export function F` -> `export const F = <gating>;`
        (Wrapper::ExportNamed, Some(name)) => {
            body.push(export_const_decl(builder, &name, gating_expr));
        }
        // `function F` -> `const F = <gating>;`
        (Wrapper::None, Some(name)) => {
            body.push(const_decl(builder, &name, gating_expr));
        }
        // Anonymous `export default function` (no id) -> `export default <gating>`.
        (Wrapper::ExportDefault, None) => {
            body.push(export_default_expr(builder, gating_expr));
        }
        (_, None) => {
            body.push(builder.statement_expression(SPAN, gating_expr));
        }
    }
}

/// The use-before-declaration dispatcher form. Mirrors
/// `insertAdditionalFunctionDeclaration` in `Entrypoint/Gating.ts`:
///
/// ```js
/// const <result> = <gating>();
/// function <orig>_optimized(...) { <compiled body> }
/// function <orig>_unoptimized(...) { <original body> }
/// function <orig>(arg0, ...) {
///   if (<result>) return <orig>_optimized(arg0, ...);
///   else return <orig>_unoptimized(arg0, ...);
/// }
/// ```
fn emit_dispatcher<'a>(
    builder: &AstBuilder<'a>,
    body: &mut oxc_allocator::Vec<'a, oxc::Statement<'a>>,
    mut orig_fn: ArenaBox<'a, oxc::Function<'a>>,
    node: CompiledNode<'a>,
    plan: &GatingPlan,
) {
    let result_name = plan.result_name.as_deref().unwrap_or("gating_result");
    let optimized_name = plan.optimized_name.as_deref().unwrap_or("optimized");
    let unoptimized_name = plan.unoptimized_name.as_deref().unwrap_or("unoptimized");
    let dispatcher_name = orig_fn
        .id
        .as_ref()
        .map(|id| id.name.to_string())
        .unwrap_or_default();

    // const <result> = <gating>();
    let gating_call = call_no_args(builder, &plan.gating_local_name);
    body.push(const_decl(builder, result_name, gating_call));

    // function <orig>_optimized(...) { <compiled body> }
    let mut compiled_fn = node.function;
    compiled_fn.r#type = oxc::FunctionType::FunctionDeclaration;
    compiled_fn.id = Some(builder.binding_identifier(SPAN, builder.str(optimized_name)));
    body.push(oxc::Statement::FunctionDeclaration(
        builder.alloc(compiled_fn),
    ));

    // function <orig>_unoptimized(...) { <original body> } (the original, renamed).
    let orig_param_count = orig_fn.params.items.len();
    let orig_has_rest = orig_fn.params.rest.is_some();
    orig_fn.r#type = oxc::FunctionType::FunctionDeclaration;
    orig_fn.id = Some(builder.binding_identifier(SPAN, builder.str(unoptimized_name)));
    body.push(oxc::Statement::FunctionDeclaration(orig_fn));

    // function <orig>(arg0, ...) { if (<result>) return <opt>(args); else return <unopt>(args); }
    let dispatcher = build_dispatcher(
        builder,
        &dispatcher_name,
        orig_param_count,
        orig_has_rest,
        result_name,
        optimized_name,
        unoptimized_name,
    );
    body.push(dispatcher);
}

/// Build the dispatcher `function <name>(arg0, ...) { if (<result>) return
/// <optimized>(args); else return <unoptimized>(args); }`.
#[allow(clippy::too_many_arguments)]
fn build_dispatcher<'a>(
    builder: &AstBuilder<'a>,
    name: &str,
    param_count: usize,
    has_rest: bool,
    result_name: &str,
    optimized_name: &str,
    unoptimized_name: &str,
) -> oxc::Statement<'a> {
    // Intern the `arg0..argN` names once; each is reused for the param pattern
    // here and for both the optimized/unoptimized dispatcher call argument lists
    // (Str is Copy, so reuse avoids re-formatting + re-interning the same name).
    let arg_atoms: Vec<Str<'a>> = (0..param_count)
        .map(|i| builder.str(&format!("arg{i}")))
        .collect();

    // Build params arg0..argN. If the original had a rest parameter, the last
    // param is a rest element (and is spread in the calls). The rest element
    // lives in a dedicated slot on `FormalParameters` rather than `items`.
    let mut params = builder.vec();
    let mut rest = None;
    for (i, &arg_atom) in arg_atoms.iter().enumerate() {
        if has_rest && i == param_count - 1 {
            let pat = builder.binding_pattern_binding_identifier(SPAN, arg_atom);
            let rest_elem = builder.binding_rest_element(SPAN, pat);
            rest = Some(builder.alloc(builder.formal_parameter_rest(
                SPAN,
                builder.vec(),
                rest_elem,
                None::<ArenaBox<'a, oxc::TSTypeAnnotation<'a>>>,
            )));
        } else {
            let pat = builder.binding_pattern_binding_identifier(SPAN, arg_atom);
            let fp = builder.formal_parameter(
                SPAN,
                builder.vec(),
                pat,
                None::<ArenaBox<'a, oxc::TSTypeAnnotation<'a>>>,
                None::<ArenaBox<'a, oxc::Expression<'a>>>,
                false,
                None,
                false,
                false,
            );
            params.push(fp);
        }
    }
    let formal_params = builder.formal_parameters(
        SPAN,
        oxc::FormalParameterKind::FormalParameter,
        params,
        rest,
    );

    // if (<result>) return <optimized>(args); else return <unoptimized>(args);
    let test = builder.expression_identifier(SPAN, builder.str(result_name));
    let opt_call = builder.expression_call(
        SPAN,
        builder.expression_identifier(SPAN, builder.str(optimized_name)),
        None::<ArenaBox<'a, oxc::TSTypeParameterInstantiation<'a>>>,
        dispatcher_args(builder, &arg_atoms, has_rest),
        false,
    );
    let consequent = builder.statement_return(SPAN, Some(opt_call));
    let unopt_call = builder.expression_call(
        SPAN,
        builder.expression_identifier(SPAN, builder.str(unoptimized_name)),
        None::<ArenaBox<'a, oxc::TSTypeParameterInstantiation<'a>>>,
        dispatcher_args(builder, &arg_atoms, has_rest),
        false,
    );
    let alternate = builder.statement_return(SPAN, Some(unopt_call));
    let if_stmt = builder.statement_if(SPAN, test, consequent, Some(alternate));

    let mut stmts = builder.vec();
    stmts.push(if_stmt);
    let fn_body = builder.function_body(SPAN, builder.vec(), stmts);

    let id = builder.binding_identifier(SPAN, builder.str(name));
    let function = builder.function(
        SPAN,
        oxc::FunctionType::FunctionDeclaration,
        Some(id),
        false,
        false,
        false,
        None::<ArenaBox<'a, oxc::TSTypeParameterDeclaration<'a>>>,
        None::<ArenaBox<'a, oxc::TSThisParameter<'a>>>,
        formal_params,
        None::<ArenaBox<'a, oxc::TSTypeAnnotation<'a>>>,
        Some(fn_body),
    );
    oxc::Statement::FunctionDeclaration(builder.alloc(function))
}

/// Build the call arguments `arg0, ..., [...argN]` for a dispatcher call, reusing
/// the already-interned `arg0..argN` atoms.
fn dispatcher_args<'a>(
    builder: &AstBuilder<'a>,
    arg_atoms: &[Str<'a>],
    has_rest: bool,
) -> oxc_allocator::Vec<'a, oxc::Argument<'a>> {
    let last = arg_atoms.len().wrapping_sub(1);
    let mut args = builder.vec();
    for (i, &arg_atom) in arg_atoms.iter().enumerate() {
        let ident = builder.expression_identifier(SPAN, arg_atom);
        if has_rest && i == last {
            args.push(oxc::Argument::SpreadElement(
                builder.alloc(builder.spread_element(SPAN, ident)),
            ));
        } else {
            args.push(oxc::Argument::from(ident));
        }
    }
    args
}

/// Try to splice a compiled function nested inside the statement (variable
/// declarator init, export wrappers, assignment, object property). Returns true
/// if a replacement happened.
fn try_splice_nested<'a>(
    builder: &AstBuilder<'a>,
    stmt: &mut oxc::Statement<'a>,
    by_start: &mut HashMap<u32, CompiledNode<'a>>,
    gating_imports: &mut Vec<GatingImport>,
) -> Vec<u32> {
    match stmt {
        oxc::Statement::VariableDeclaration(var) => {
            for decl in var.declarations.iter_mut() {
                if let Some(init) = &mut decl.init
                    && let Some(start) = splice_into_init(builder, init, by_start, gating_imports)
                {
                    return vec![start];
                }
            }
            Vec::new()
        }
        oxc::Statement::ExportNamedDeclaration(export) => {
            if let Some(oxc::Declaration::VariableDeclaration(var)) = &mut export.declaration {
                for decl in var.declarations.iter_mut() {
                    if let Some(init) = &mut decl.init
                        && let Some(start) =
                            splice_into_init(builder, init, by_start, gating_imports)
                    {
                        return vec![start];
                    }
                }
            }
            Vec::new()
        }
        // `export default <arrow|fnexpr>` or `export default React.memo(<fn>)`.
        oxc::Statement::ExportDefaultDeclaration(export) => {
            let is_fn = matches!(
                &export.declaration,
                oxc::ExportDefaultDeclarationKind::ArrowFunctionExpression(_)
                    | oxc::ExportDefaultDeclarationKind::FunctionExpression(_)
            );
            if is_fn {
                let decl_start = export.declaration.span().start;
                if let Some(node) = by_start.remove(&decl_start) {
                    let original = std::mem::replace(
                        &mut export.declaration,
                        oxc::ExportDefaultDeclarationKind::NullLiteral(
                            builder.alloc(builder.null_literal(SPAN)),
                        ),
                    );
                    let original_expr = export_default_kind_to_expr(builder, original);
                    let new_expr = build_init_expr(builder, node, original_expr, gating_imports);
                    export.declaration = oxc::ExportDefaultDeclarationKind::from(new_expr);
                    return vec![decl_start];
                }
            } else if let Some(expr) = export.declaration.as_expression_mut() {
                // `export default React.memo(<fn>)` — splice into the call arg.
                if let Some(start) = splice_into_init(builder, expr, by_start, gating_imports) {
                    return vec![start];
                }
            }
            Vec::new()
        }
        // Reassignment `X = <fn>` as an expression statement, object property
        // values such as `{ key: <arrow> }`, or a bare `React.memo(<fn>)`
        // statement.
        oxc::Statement::ExpressionStatement(expr_stmt) => {
            match &mut expr_stmt.expression {
                oxc::Expression::AssignmentExpression(assign) => {
                    if let Some(start) =
                        splice_into_init(builder, &mut assign.right, by_start, gating_imports)
                    {
                        return vec![start];
                    }
                }
                // Bare `React.memo(<fn>)` / `forwardRef(<fn>)` call statement.
                expr @ oxc::Expression::CallExpression(_) => {
                    if let Some(start) = splice_into_init(builder, expr, by_start, gating_imports) {
                        return vec![start];
                    }
                }
                _ => {}
            }
            splice_in_expression(builder, &mut expr_stmt.expression, by_start, gating_imports)
        }
        _ => Vec::new(),
    }
}

/// Splice a compiled function into an initializer/value/argument expression.
///
/// Handles two shapes at this position:
///   * the expression IS the compiled function literal (its span is keyed in
///     `by_start`), in which case it is replaced wholesale; or
///   * the expression is a `memo(<fn>)`/`React.memo(<fn>)`/`forwardRef(<fn>)`/
///     `React.forwardRef(<fn>)` call whose first argument is the compiled
///     function literal, in which case only the argument is replaced and the
///     call wrapper is preserved (matching TS `getComponentOrHookLike`'s
///     forwardRef/memo callback handling).
///
/// Returns the spliced span start when a replacement happened.
fn splice_into_init<'a>(
    builder: &AstBuilder<'a>,
    expr: &mut oxc::Expression<'a>,
    by_start: &mut HashMap<u32, CompiledNode<'a>>,
    gating_imports: &mut Vec<GatingImport>,
) -> Option<u32> {
    // Direct function literal at this position.
    let start = expr.span().start;
    if let Some(node) = by_start.remove(&start) {
        let original = std::mem::replace(expr, builder.expression_null_literal(SPAN));
        *expr = build_init_expr(builder, node, original, gating_imports);
        return Some(start);
    }
    // `memo(<fn>)` / `forwardRef(<fn>)` wrapper: replace only the first argument.
    if let oxc::Expression::CallExpression(call) = expr
        && let Some(first) = call.arguments.first_mut()
        && let Some(arg) = first.as_expression_mut()
    {
        let arg_start = arg.span().start;
        if let Some(node) = by_start.remove(&arg_start) {
            let original = std::mem::replace(arg, builder.expression_null_literal(SPAN));
            *arg = build_init_expr(builder, node, original, gating_imports);
            return Some(arg_start);
        }
    }
    None
}

/// Recursively look for a compiled function nested in an expression (currently:
/// object property values, e.g. `{ useHook: <arrow> }`). Returns the spliced
/// start span(s).
fn splice_in_expression<'a>(
    builder: &AstBuilder<'a>,
    expr: &mut oxc::Expression<'a>,
    by_start: &mut HashMap<u32, CompiledNode<'a>>,
    gating_imports: &mut Vec<GatingImport>,
) -> Vec<u32> {
    if let oxc::Expression::ObjectExpression(obj) = expr {
        for prop in obj.properties.iter_mut() {
            if let oxc::ObjectPropertyKind::ObjectProperty(p) = prop
                && let Some(start) =
                    splice_into_init(builder, &mut p.value, by_start, gating_imports)
            {
                return vec![start];
            }
        }
    }
    Vec::new()
}

/// Core: produce the (possibly gated) replacement expression for an
/// initializer/value position, given the *original* expression at that span.
fn build_init_expr<'a>(
    builder: &AstBuilder<'a>,
    node: CompiledNode<'a>,
    original: oxc::Expression<'a>,
    gating_imports: &mut Vec<GatingImport>,
) -> oxc::Expression<'a> {
    let Some(plan) = node.gating.clone() else {
        return function_expression(builder, node);
    };
    gating_imports.push(GatingImport {
        source: plan.gating_source.clone(),
        imported: plan.gating_imported.clone(),
        local: plan.gating_local_name.clone(),
    });
    let compiled_expr = function_expression(builder, node);
    gating_conditional(builder, &plan.gating_local_name, compiled_expr, original)
}

/// Build `gating() ? <compiled> : <original>`.
fn gating_conditional<'a>(
    builder: &AstBuilder<'a>,
    gating_local_name: &str,
    compiled: oxc::Expression<'a>,
    original: oxc::Expression<'a>,
) -> oxc::Expression<'a> {
    let test = call_no_args(builder, gating_local_name);
    builder.expression_conditional(SPAN, test, compiled, original)
}

/// `<name>()` call with no arguments.
fn call_no_args<'a>(builder: &AstBuilder<'a>, name: &str) -> oxc::Expression<'a> {
    builder.expression_call(
        SPAN,
        builder.expression_identifier(SPAN, builder.str(name)),
        None::<ArenaBox<'a, oxc::TSTypeParameterInstantiation<'a>>>,
        builder.vec(),
        false,
    )
}

/// `const <name> = <init>;`.
fn const_decl<'a>(
    builder: &AstBuilder<'a>,
    name: &str,
    init: oxc::Expression<'a>,
) -> oxc::Statement<'a> {
    let pat = builder.binding_pattern_binding_identifier(SPAN, builder.str(name));
    let declarator = builder.variable_declarator(
        SPAN,
        oxc::VariableDeclarationKind::Const,
        pat,
        None::<ArenaBox<'a, oxc::TSTypeAnnotation<'a>>>,
        Some(init),
        false,
    );
    let mut decls = builder.vec();
    decls.push(declarator);
    let decl =
        builder.variable_declaration(SPAN, oxc::VariableDeclarationKind::Const, decls, false);
    oxc::Statement::VariableDeclaration(builder.alloc(decl))
}

/// `export const <name> = <init>;`.
fn export_const_decl<'a>(
    builder: &AstBuilder<'a>,
    name: &str,
    init: oxc::Expression<'a>,
) -> oxc::Statement<'a> {
    let pat = builder.binding_pattern_binding_identifier(SPAN, builder.str(name));
    let declarator = builder.variable_declarator(
        SPAN,
        oxc::VariableDeclarationKind::Const,
        pat,
        None::<ArenaBox<'a, oxc::TSTypeAnnotation<'a>>>,
        Some(init),
        false,
    );
    let mut decls = builder.vec();
    decls.push(declarator);
    let var_decl =
        builder.variable_declaration(SPAN, oxc::VariableDeclarationKind::Const, decls, false);
    let export = builder.export_named_declaration(
        SPAN,
        Some(oxc::Declaration::VariableDeclaration(
            builder.alloc(var_decl),
        )),
        builder.vec(),
        None,
        oxc::ImportOrExportKind::Value,
        None::<ArenaBox<'a, oxc::WithClause<'a>>>,
    );
    oxc::Statement::ExportNamedDeclaration(builder.alloc(export))
}

/// `export default <name>;`.
fn export_default_ident<'a>(builder: &AstBuilder<'a>, name: &str) -> oxc::Statement<'a> {
    let ident = builder.expression_identifier(SPAN, builder.str(name));
    export_default_expr(builder, ident)
}

/// `export default <expr>;`.
fn export_default_expr<'a>(
    builder: &AstBuilder<'a>,
    expr: oxc::Expression<'a>,
) -> oxc::Statement<'a> {
    let export =
        builder.export_default_declaration(SPAN, oxc::ExportDefaultDeclarationKind::from(expr));
    oxc::Statement::ExportDefaultDeclaration(builder.alloc(export))
}

/// Convert an `ExportDefaultDeclarationKind` (arrow/fnexpr) into an expression.
fn export_default_kind_to_expr<'a>(
    builder: &AstBuilder<'a>,
    kind: oxc::ExportDefaultDeclarationKind<'a>,
) -> oxc::Expression<'a> {
    match kind {
        oxc::ExportDefaultDeclarationKind::ArrowFunctionExpression(arrow) => {
            oxc::Expression::ArrowFunctionExpression(arrow)
        }
        oxc::ExportDefaultDeclarationKind::FunctionExpression(func) => {
            oxc::Expression::FunctionExpression(func)
        }
        // Other forms shouldn't reach here (guarded by the caller); fall back to
        // a null literal so the program remains well-formed.
        _ => builder.expression_null_literal(SPAN),
    }
}

/// The source-span start of the function declaration carried by `stmt`, if it
/// is a bare `function F`, `export function F`, or `export default function F`.
/// Used to match a statement against a compiled artifact (whose span is the
/// inner function span, not the export wrapper span).
fn function_declaration_start(stmt: &oxc::Statement<'_>) -> Option<u32> {
    match stmt {
        oxc::Statement::FunctionDeclaration(func) => Some(func.span().start),
        oxc::Statement::ExportNamedDeclaration(export) => {
            if let Some(oxc::Declaration::FunctionDeclaration(func)) = &export.declaration {
                Some(func.span().start)
            } else {
                None
            }
        }
        oxc::Statement::ExportDefaultDeclaration(export) => {
            if let oxc::ExportDefaultDeclarationKind::FunctionDeclaration(func) = &export.declaration
            {
                Some(func.span().start)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Pull the `Function` out of a top-level function-declaration statement (bare,
/// `export`, or `export default`), reporting which wrapper it was in.
fn unwrap_function_declaration<'a>(
    stmt: oxc::Statement<'a>,
) -> (Option<ArenaBox<'a, oxc::Function<'a>>>, Wrapper) {
    match stmt {
        oxc::Statement::FunctionDeclaration(func) => (Some(func), Wrapper::None),
        oxc::Statement::ExportNamedDeclaration(export) => {
            let export = export.unbox();
            if let Some(oxc::Declaration::FunctionDeclaration(func)) = export.declaration {
                (Some(func), Wrapper::ExportNamed)
            } else {
                (None, Wrapper::None)
            }
        }
        oxc::Statement::ExportDefaultDeclaration(export) => {
            let export = export.unbox();
            if let oxc::ExportDefaultDeclarationKind::FunctionDeclaration(func) = export.declaration
            {
                (Some(func), Wrapper::ExportDefault)
            } else {
                (None, Wrapper::None)
            }
        }
        _ => (None, Wrapper::None),
    }
}

enum Wrapper {
    None,
    ExportNamed,
    ExportDefault,
}

/// Convert a `FunctionDeclaration` (as a boxed `Function`) into a
/// `FunctionExpression` expression for use as the "original" side of a gating
/// conditional, preserving id / params / body.
fn function_decl_to_expression<'a>(
    builder: &AstBuilder<'a>,
    func: ArenaBox<'a, oxc::Function<'a>>,
) -> oxc::Expression<'a> {
    let mut function = func.unbox();
    function.r#type = oxc::FunctionType::FunctionExpression;
    oxc::Expression::FunctionExpression(builder.alloc(function))
}

/// Build a top-level replacement statement for a compiled function (no gating).
fn build_replacement<'a>(builder: &AstBuilder<'a>, node: CompiledNode<'a>) -> oxc::Statement<'a> {
    // Top-level matched node is a function declaration form.
    let mut function = node.function;
    function.r#type = oxc::FunctionType::FunctionDeclaration;
    oxc::Statement::FunctionDeclaration(builder.alloc(function))
}

/// Build the compiled function declaration and rewrap it in the original
/// `export` / `export default` wrapper (non-gated path). `orig_fn` supplies the
/// original name for anonymous `export default function` forms.
fn rewrap_function_declaration<'a>(
    builder: &AstBuilder<'a>,
    node: CompiledNode<'a>,
    wrapper: &Wrapper,
    orig_fn: &oxc::Function<'a>,
) -> oxc::Statement<'a> {
    let mut function = node.function;
    function.r#type = oxc::FunctionType::FunctionDeclaration;
    // Preserve the original function name (codegen keeps it, but be safe for
    // anonymous default exports).
    if function.id.is_none()
        && let Some(id) = &orig_fn.id {
            function.id = Some(builder.binding_identifier(SPAN, builder.str(id.name.as_str())));
        }
    let func_box = builder.alloc(function);
    match wrapper {
        Wrapper::None => oxc::Statement::FunctionDeclaration(func_box),
        Wrapper::ExportNamed => {
            let decl = oxc::Declaration::FunctionDeclaration(func_box);
            let export = builder.export_named_declaration(
                SPAN,
                Some(decl),
                builder.vec(),
                None,
                oxc::ImportOrExportKind::Value,
                None::<ArenaBox<'a, oxc::WithClause<'a>>>,
            );
            oxc::Statement::ExportNamedDeclaration(builder.alloc(export))
        }
        Wrapper::ExportDefault => {
            let kind = oxc::ExportDefaultDeclarationKind::FunctionDeclaration(func_box);
            let export = builder.export_default_declaration(SPAN, kind);
            oxc::Statement::ExportDefaultDeclaration(builder.alloc(export))
        }
    }
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
        //
        // The compiled body is always a block statement. TS's top-level arrow
        // splice (`applyCompiledFunction` in Entrypoint/Program.ts) uses
        // `body: compiledFn.body` directly — it does NOT collapse a single
        // `return` into a concise expression body. (That concise collapse only
        // happens for *nested* FunctionExpressions in CodegenReactiveFunction.)
        // So a top-level `const f = x => x` round-trips as `x => { return x; }`.
        let params = function.params.unbox();
        let is_async = function.r#async;
        let body = function
            .body
            .expect("function body present after codegen")
            .unbox();
        let directives = body.directives;
        let statements = body.statements;

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

/// Prepend the runtime cache import to the program body. In `module` source
/// type, emits `import { c as _c } from "<runtime_module>";`. In `script`
/// source type, emits `const { c: _c } = require("<runtime_module>");`
/// (CommonJS), matching the TS plugin's `addImportDeclaration` in
/// `Entrypoint/Imports.ts` which keys off `program.node.sourceType`.
fn inject_memo_import<'a>(
    builder: &AstBuilder<'a>,
    program: &mut oxc::Program<'a>,
    runtime_module: &str,
    source_type: SourceType,
) {
    if source_type.is_script() {
        let stmt = build_require_destructure(builder, runtime_module, "c", MEMO_LOCAL_NAME);
        program.body.insert(0, stmt);
        return;
    }
    let imported =
        oxc::ModuleExportName::IdentifierName(builder.identifier_name(SPAN, builder.str("c")));
    let local = builder.binding_identifier(SPAN, builder.str(MEMO_LOCAL_NAME));
    let specifier = builder.import_specifier(SPAN, imported, local, oxc::ImportOrExportKind::Value);
    let mut specifiers = builder.vec();
    specifiers.push(oxc::ImportDeclarationSpecifier::ImportSpecifier(
        builder.alloc(specifier),
    ));
    let source = builder.string_literal(SPAN, builder.str(runtime_module), None);
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

/// Build `const { <imported>: <local> } = require("<module>");` — the CommonJS
/// destructuring form used in `script` source type.
fn build_require_destructure<'a>(
    builder: &AstBuilder<'a>,
    module: &str,
    imported: &str,
    local: &str,
) -> oxc::Statement<'a> {
    // Object pattern `{ <imported>: <local> }`.
    let key = oxc::PropertyKey::StaticIdentifier(
        builder.alloc(builder.identifier_name(SPAN, builder.str(imported))),
    );
    let value = builder.binding_pattern_binding_identifier(SPAN, builder.str(local));
    let shorthand = imported == local;
    let prop = builder.binding_property(SPAN, key, value, shorthand, false);
    let mut properties = builder.vec();
    properties.push(prop);
    let binding = builder.binding_pattern_object_pattern(
        SPAN,
        properties,
        None::<oxc::BindingRestElement<'a>>,
    );

    // `require("<module>")`.
    let callee = oxc::Expression::Identifier(
        builder.alloc(builder.identifier_reference(SPAN, builder.str("require"))),
    );
    let module_arg = oxc::Argument::StringLiteral(
        builder.alloc(builder.string_literal(SPAN, builder.str(module), None)),
    );
    let mut args = builder.vec();
    args.push(module_arg);
    let require_call = oxc::Expression::CallExpression(builder.alloc(builder.call_expression(
        SPAN,
        callee,
        None::<ArenaBox<'a, oxc::TSTypeParameterInstantiation<'a>>>,
        args,
        false,
    )));

    let declarator = builder.variable_declarator(
        SPAN,
        oxc::VariableDeclarationKind::Const,
        binding,
        None::<ArenaBox<'a, oxc::TSTypeAnnotation<'a>>>,
        Some(require_call),
        false,
    );
    let mut decls = builder.vec();
    decls.push(declarator);
    let decl =
        builder.variable_declaration(SPAN, oxc::VariableDeclarationKind::Const, decls, false);
    oxc::Statement::VariableDeclaration(builder.alloc(decl))
}

/// Inject the gating import(s) — `import { <imported> [as <local>] } from
/// "<source>";` — after the leading `_c` import (matching TS). Duplicate
/// specifiers are emitted once.
fn inject_gating_imports<'a>(
    builder: &AstBuilder<'a>,
    program: &mut oxc::Program<'a>,
    gating_imports: &[GatingImport],
) {
    if gating_imports.is_empty() {
        return;
    }
    // Dedupe identical (source, imported, local) triples while preserving order.
    let mut seen: Vec<&GatingImport> = Vec::new();
    for gi in gating_imports {
        if !seen.iter().any(|s| **s == *gi) {
            seen.push(gi);
        }
    }

    // Insert position: right after the first import (the `_c` memo import, if
    // present); otherwise at the top.
    let mut insert_at = if matches!(
        program.body.first(),
        Some(oxc::Statement::ImportDeclaration(_))
    ) {
        1usize
    } else {
        0usize
    };

    for gi in seen {
        let imported = oxc::ModuleExportName::IdentifierName(
            builder.identifier_name(SPAN, builder.str(&gi.imported)),
        );
        let local = builder.binding_identifier(SPAN, builder.str(&gi.local));
        let specifier =
            builder.import_specifier(SPAN, imported, local, oxc::ImportOrExportKind::Value);
        let mut specifiers = builder.vec();
        specifiers.push(oxc::ImportDeclarationSpecifier::ImportSpecifier(
            builder.alloc(specifier),
        ));
        let source = builder.string_literal(SPAN, builder.str(&gi.source), None);
        let import_decl = builder.import_declaration(
            SPAN,
            Some(specifiers),
            source,
            None,
            None::<ArenaBox<'a, oxc::WithClause<'a>>>,
            oxc::ImportOrExportKind::Value,
        );
        let import_stmt = oxc::Statement::ImportDeclaration(builder.alloc(import_decl));
        program.body.insert(insert_at, import_stmt);
        insert_at += 1;
    }
}
