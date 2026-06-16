// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the root directory of this source tree.

//! Main entrypoint for the React Compiler (oxc input path).
//!
//! Stage N1.2.1 flips this entrypoint to read `oxc_ast` + `oxc_semantic`
//! directly. It:
//! 1. Builds the program context from the oxc semantic model.
//! 2. Discovers top-level functions to compile (declarations, `export default
//!    function`, and `const X = (arrow|function)`), classifying each by name.
//! 3. Runs each discovered function through `pipeline::compile_fn`, which lowers
//!    to HIR and runs the pipeline passes (the per-pass HIR dump is the oracle
//!    for N1.2 — see CLAUDE.md).
//! 4. Accumulates logger events / per-pass debug entries onto the context.
//!
//! Codegen / output reassembly is NOT performed here: this stage returns
//! `ast: None` so the HIR-oracle path stays green. Native codegen lands in N2.
//!
//! Fidelity gaps deliberately deferred to later stages are marked
//! `// TODO(N1.3): ...`.

use oxc_ast::ast as oxc;
use oxc_ast_visit::Visit;
use oxc_semantic::Semantic;
use oxc_span::GetSpan;
use oxc_span::Span;
use react_compiler_diagnostics::CompilerError;
use react_compiler_diagnostics::CompilerErrorOrDiagnostic;
use react_compiler_diagnostics::ErrorCategory;
use react_compiler_diagnostics::SourceLocation;
use react_compiler_hir::ReactFunctionType;
use react_compiler_lowering::FunctionForm;

use super::compile_result::BindingRenameInfo;
use super::compile_result::CompileResult;
use super::compile_result::CompilerErrorDetailInfo;
use super::compile_result::CompilerErrorInfo;
use super::compile_result::CompilerErrorItemInfo;
use super::compile_result::DebugLogEntry;
use super::compile_result::LoggerEvent;
use super::compile_result::LoggerPosition;
use super::compile_result::LoggerSourceLocation;
use super::compile_result::LoggerSuggestionInfo;
use super::compile_result::LoggerSuggestionOp;
use super::compile_result::OrderedLogItem;
use super::imports::ProgramContext;
use super::native_codegen::NativeArtifact;
use super::pipeline;
use super::plugin_options::CompilerOutputMode;
use super::plugin_options::PluginOptions;

/// Result of [`compile_program`]: the serializable [`CompileResult`] plus the
/// out-of-band native oxc codegen artifacts (N2.1). The artifacts are owned
/// (allocator-free) and consumed by `react_compiler_oxc::transform` to build +
/// splice the compiled oxc AST after the input semantic borrow ends.
pub struct CompileProgramResult {
    pub result: CompileResult,
    pub native_artifacts: Vec<NativeArtifact>,
}

// =============================================================================
// Discovery
// =============================================================================

/// A function discovered for compilation, in oxc form.
struct CompileSource<'a> {
    func: FunctionForm<'a>,
    fn_name: Option<String>,
    fn_type: ReactFunctionType,
    fn_span: Span,
    /// The symbol id of the function's own name binding, when it is a named
    /// `function Foo` declaration (used for the gating "referenced before
    /// declaration" check). `None` for arrows, function expressions assigned to
    /// variables, and anonymous declarations.
    fn_symbol_id: Option<oxc_semantic::SymbolId>,
}

/// Name-based React function classification.
///
/// Ported from the bridge `is_hook_name` / `is_component_name`. Hooks are
/// `use` followed by an uppercase letter or digit; components start with an
/// uppercase letter.
fn is_hook_name(s: &str) -> bool {
    let bytes = s.as_bytes();
    bytes.len() >= 4
        && bytes[0] == b'u'
        && bytes[1] == b's'
        && bytes[2] == b'e'
        && bytes
            .get(3)
            .is_some_and(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

fn is_component_name(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

/// Classify a function, mirroring TS `getReactFunctionType` for the
/// `compilationMode: "all"` / `"infer"` paths.
///
/// This ports `getComponentOrHookLike` (Program.ts ~1096): a component-named
/// function is only a `Component` if it calls hooks or creates JSX in its own
/// body, has valid component params, and does not return a non-node value; a
/// hook-named function is only a `Hook` if it calls hooks or creates JSX. The
/// final fallback differs by mode: when `compile_all` is true a `None` result
/// becomes `Other`, otherwise it stays `None` (function is skipped).
///
/// (The `forwardRef`/`memo` callback branch of `getComponentOrHookLike` is not
/// yet ported — current oxc discovery does not surface those anyway.)
fn classify_function(
    name: Option<&str>,
    params: &oxc::FormalParameters,
    body: &dyn FnBody,
    compile_all: bool,
) -> Option<ReactFunctionType> {
    let result = get_component_or_hook_like(name, params, body);
    match result {
        Some(t) => Some(t),
        None if compile_all => Some(ReactFunctionType::Other),
        None => None,
    }
}

/// Abstraction over the two function forms (`Function` / arrow) for the body
/// heuristics. `walk_body` traverses the function's own statements (pruning
/// nested functions); `concise_return` is the arrow expression-body return value
/// (`() => expr`), or `None` for block bodies.
trait FnBody {
    /// The statements making up the function body (empty for a bodyless TS
    /// declaration).
    fn statements(&self) -> &[oxc::Statement<'_>];
    /// For an arrow with a concise (expression) body, the returned expression;
    /// `None` for block-bodied functions and arrows.
    fn concise_return(&self) -> Option<&oxc::Expression<'_>>;
}

struct FunctionBodyRef<'a, 'b>(&'b oxc::Function<'a>);
impl<'a, 'b> FnBody for FunctionBodyRef<'a, 'b> {
    fn statements(&self) -> &[oxc::Statement<'_>] {
        self.0.body.as_ref().map_or(&[], |b| b.statements.as_slice())
    }
    fn concise_return(&self) -> Option<&oxc::Expression<'_>> {
        None
    }
}

struct ArrowBodyRef<'a, 'b>(&'b oxc::ArrowFunctionExpression<'a>);
impl<'a, 'b> FnBody for ArrowBodyRef<'a, 'b> {
    fn statements(&self) -> &[oxc::Statement<'_>] {
        self.0.body.statements.as_slice()
    }
    fn concise_return(&self) -> Option<&oxc::Expression<'_>> {
        if !self.0.expression {
            return None;
        }
        // A concise arrow body is parsed as a `FunctionBody` containing a single
        // `ExpressionStatement` whose expression is the implicit return value.
        match self.0.body.statements.first() {
            Some(oxc::Statement::ExpressionStatement(stmt)) => Some(&stmt.expression),
            _ => None,
        }
    }
}

/// Port of `getComponentOrHookLike` (Program.ts ~1096-1125). Returns the
/// classification implied by name + body, or `None`.
fn get_component_or_hook_like(
    name: Option<&str>,
    params: &oxc::FormalParameters,
    body: &dyn FnBody,
) -> Option<ReactFunctionType> {
    let name = name?;
    if is_component_name(name) {
        let is_component = calls_hooks_or_creates_jsx(body)
            && is_valid_component_params(params)
            && !returns_non_node(body);
        if is_component {
            Some(ReactFunctionType::Component)
        } else {
            None
        }
    } else if is_hook_name(name) {
        // Hooks have hook invocations or JSX, but can take any # of arguments.
        if calls_hooks_or_creates_jsx(body) {
            Some(ReactFunctionType::Hook)
        } else {
            None
        }
    } else {
        None
    }
}

/// Port of `isHook` (Program.ts ~953) for an expression callee: a hook is either
/// an identifier whose name matches `use[A-Z0-9]`, or a non-computed member
/// expression `<PascalCaseNamespace>.useX`.
fn is_hook_callee(callee: &oxc::Expression) -> bool {
    match callee {
        oxc::Expression::Identifier(ident) => is_hook_name(&ident.name),
        oxc::Expression::StaticMemberExpression(member) => {
            // `!path.node.computed` is implied by `StaticMemberExpression`.
            is_hook_name(&member.property.name)
                && matches!(
                    &member.object,
                    oxc::Expression::Identifier(obj) if is_pascal_case_namespace(&obj.name)
                )
        }
        _ => false,
    }
}

/// Matches the TS `/^[A-Z].*/` namespace check in `isHook`.
fn is_pascal_case_namespace(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

/// Port of `callsHooksOrCreatesJsx` (Program.ts ~1143): traverse the function's
/// own body and return true if any JSX element/fragment is created or any call
/// to a hook is found. Nested functions are pruned (their hooks/JSX do not
/// count).
fn calls_hooks_or_creates_jsx(body: &dyn FnBody) -> bool {
    let mut visitor = HooksOrJsxVisitor { found: false };
    visitor.visit_function_statements(body.statements());
    visitor.found
}

struct HooksOrJsxVisitor {
    found: bool,
}

impl HooksOrJsxVisitor {
    fn visit_function_statements<'a>(&mut self, statements: &[oxc::Statement<'a>]) {
        for stmt in statements {
            if self.found {
                return;
            }
            self.visit_statement(stmt);
        }
    }
}

impl<'a> Visit<'a> for HooksOrJsxVisitor {
    fn visit_jsx_element(&mut self, _it: &oxc::JSXElement<'a>) {
        self.found = true;
    }

    fn visit_jsx_fragment(&mut self, _it: &oxc::JSXFragment<'a>) {
        self.found = true;
    }

    fn visit_call_expression(&mut self, call: &oxc::CallExpression<'a>) {
        if self.found {
            return;
        }
        if is_hook_callee(&call.callee) {
            self.found = true;
            return;
        }
        // Keep descending (arguments may contain JSX or further hook calls).
        oxc_ast_visit::walk::walk_call_expression(self, call);
    }

    // Skip nested functions: hooks/JSX inside them do not count.
    fn visit_function(&mut self, _func: &oxc::Function<'a>, _flags: oxc_semantic::ScopeFlags) {}
    fn visit_arrow_function_expression(&mut self, _expr: &oxc::ArrowFunctionExpression<'a>) {}
}

/// Port of `isValidPropsAnnotation` (Program.ts ~1019). A param with no type
/// annotation is valid; with a TS annotation it is invalid only for the listed
/// "primitive-ish" types. (Flow annotations are not represented in oxc's
/// `type_annotation` field, so only the TS branch is ported.)
fn is_valid_props_annotation(param: &oxc::FormalParameter) -> bool {
    let Some(annot) = &param.type_annotation else {
        return true;
    };
    !matches!(
        &annot.type_annotation,
        oxc::TSType::TSArrayType(_)
            | oxc::TSType::TSBigIntKeyword(_)
            | oxc::TSType::TSBooleanKeyword(_)
            | oxc::TSType::TSConstructorType(_)
            | oxc::TSType::TSFunctionType(_)
            | oxc::TSType::TSLiteralType(_)
            | oxc::TSType::TSNeverKeyword(_)
            | oxc::TSType::TSNumberKeyword(_)
            | oxc::TSType::TSStringKeyword(_)
            | oxc::TSType::TSSymbolKeyword(_)
            | oxc::TSType::TSTupleType(_)
    )
}

/// Port of `isValidComponentParams` (Program.ts ~1064).
fn is_valid_component_params(params: &oxc::FormalParameters) -> bool {
    let items = &params.items;
    let has_rest = params.rest.is_some();
    // Total param count, including a trailing rest element.
    let total = items.len() + usize::from(has_rest);

    if total == 0 {
        return true;
    }
    if total > 2 {
        return false;
    }

    // The first param: if there is at least one non-rest item, it is `items[0]`;
    // otherwise the only param is the rest element.
    if let Some(first) = items.first() {
        if !is_valid_props_annotation(first) {
            return false;
        }
    }

    if total == 1 {
        // A single rest param (`...props`) is not valid.
        return !(items.is_empty() && has_rest);
    }

    // total == 2: the second param must be an identifier whose name looks like a
    // ref. If the second slot is the rest element, it is not an identifier.
    match items.get(1) {
        Some(second) => match &second.pattern {
            oxc::BindingPattern::BindingIdentifier(id) => {
                id.name.contains("ref") || id.name.contains("Ref")
            }
            _ => false,
        },
        None => false,
    }
}

/// Port of `isNonNode` (Program.ts ~1169): an absent argument is treated as a
/// non-node, as are object/function/class/bigint/new expressions.
fn is_non_node(expr: Option<&oxc::Expression>) -> bool {
    let Some(expr) = expr else {
        return true;
    };
    matches!(
        expr.get_inner_expression(),
        oxc::Expression::ObjectExpression(_)
            | oxc::Expression::ArrowFunctionExpression(_)
            | oxc::Expression::FunctionExpression(_)
            | oxc::Expression::BigIntLiteral(_)
            | oxc::Expression::ClassExpression(_)
            | oxc::Expression::NewExpression(_)
    )
}

/// Port of `returnsNonNode` (Program.ts ~1185). For a concise-body arrow the
/// result is `isNonNode(body)`. Otherwise the body's return statements are
/// traversed (pruning nested functions and object methods) and the LAST return
/// seen wins (matching the TS overwrite-on-each-return behavior).
fn returns_non_node(body: &dyn FnBody) -> bool {
    if let Some(concise) = body.concise_return() {
        return is_non_node(Some(concise));
    }
    let mut visitor = ReturnsNonNodeVisitor { value: false };
    visitor.visit_function_statements(body.statements());
    visitor.value
}

struct ReturnsNonNodeVisitor {
    value: bool,
}

impl ReturnsNonNodeVisitor {
    fn visit_function_statements<'a>(&mut self, statements: &[oxc::Statement<'a>]) {
        for stmt in statements {
            self.visit_statement(stmt);
        }
    }
}

impl<'a> Visit<'a> for ReturnsNonNodeVisitor {
    fn visit_return_statement(&mut self, ret: &oxc::ReturnStatement<'a>) {
        // TS overwrites on every return, so the last one encountered wins.
        self.value = is_non_node(ret.argument.as_ref());
    }

    // Skip nested functions and their return statements.
    fn visit_function(&mut self, _func: &oxc::Function<'a>, _flags: oxc_semantic::ScopeFlags) {}
    fn visit_arrow_function_expression(&mut self, _expr: &oxc::ArrowFunctionExpression<'a>) {}
}

/// Discover the top-level functions to compile.
///
/// Considers: top-level `function Foo() {}`, `export default function`, and
/// `const X = (arrow|function expr)` where `X` is a component/hook name (or any
/// name when `compilationMode == "all"`).
///
/// TODO(N1.3): nested functions, `forwardRef`/`memo` wrappers, object-method
/// components, and the full `calls_hooks_or_creates_jsx` body heuristic are not
/// yet ported — only name-based top-level discovery.
fn find_functions_to_compile<'a>(
    program: &'a oxc::Program<'a>,
    compile_all: bool,
) -> Vec<CompileSource<'a>> {
    let mut queue: Vec<CompileSource<'a>> = Vec::new();

    for stmt in &program.body {
        match stmt {
            // function Foo() {}
            oxc::Statement::FunctionDeclaration(func) => {
                consider_function(func, None, compile_all, &mut queue);
            }
            // export default function Foo() {} / export default function () {}
            // export default () => {} / export default function () {}
            oxc::Statement::ExportDefaultDeclaration(export) => match &export.declaration {
                oxc::ExportDefaultDeclarationKind::FunctionDeclaration(func) => {
                    consider_function(func, None, compile_all, &mut queue);
                }
                oxc::ExportDefaultDeclarationKind::FunctionExpression(func) => {
                    consider_function(func, None, compile_all, &mut queue);
                }
                oxc::ExportDefaultDeclarationKind::ArrowFunctionExpression(arrow) => {
                    if let Some(fn_type) = classify_function(
                        None,
                        &arrow.params,
                        &ArrowBodyRef(arrow),
                        compile_all,
                    ) {
                        queue.push(CompileSource {
                            func: FunctionForm::Arrow(arrow),
                            fn_name: None,
                            fn_type,
                            fn_span: arrow.span(),
                            // Anonymous; never referenced-before-declared.
                            fn_symbol_id: None,
                        });
                    }
                }
                _ => {}
            },
            // export function Foo() {}
            oxc::Statement::ExportNamedDeclaration(export) => {
                if let Some(oxc::Declaration::FunctionDeclaration(func)) = &export.declaration {
                    consider_function(func, None, compile_all, &mut queue);
                } else if let Some(oxc::Declaration::VariableDeclaration(var)) = &export.declaration
                {
                    consider_variable_declaration(var, compile_all, &mut queue);
                }
            }
            // const Foo = () => {} / const Foo = function () {}
            oxc::Statement::VariableDeclaration(var) => {
                consider_variable_declaration(var, compile_all, &mut queue);
            }
            // Foo = () => {} / Foo = function () {} (reassignment of a binding).
            // Unlike a `const Foo = () => {}` *declarator*, an assignment does
            // not give the arrow an inferred name (matching Babel's name
            // inference, which only fires on variable declarators), so the
            // function is classified anonymously (`fn_type: Other` for an arrow
            // with no name). The binding name is preserved by the original
            // assignment target during assembly (only the RHS is spliced).
            oxc::Statement::ExpressionStatement(expr_stmt) => {
                if let oxc::Expression::AssignmentExpression(assign) = &expr_stmt.expression {
                    match &assign.right {
                        oxc::Expression::ArrowFunctionExpression(arrow) => {
                            if let Some(fn_type) = classify_function(
                                None,
                                &arrow.params,
                                &ArrowBodyRef(arrow),
                                compile_all,
                            ) {
                                queue.push(CompileSource {
                                    func: FunctionForm::Arrow(arrow),
                                    fn_name: None,
                                    fn_type,
                                    fn_span: arrow.span(),
                                    fn_symbol_id: None,
                                });
                            }
                        }
                        oxc::Expression::FunctionExpression(func) => {
                            consider_function(func, None, compile_all, &mut queue);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    queue
}

fn consider_function<'a>(
    func: &'a oxc::Function<'a>,
    inferred_name: Option<&str>,
    compile_all: bool,
    queue: &mut Vec<CompileSource<'a>>,
) {
    // A function expression's emitted id is ALWAYS its own name (`func.id`),
    // never the binding name. This matches TS BuildHIR, which derives the HIR
    // function id solely from `func.id`:
    //   - `const X = function f(){}` -> id `f`
    //   - `const X = function (){}`  -> id null (stays anonymous)
    // The inferred binding name is used ONLY for classification (is this a
    // component/hook?), not for the emitted function id.
    let emitted_name = func.id.as_ref().map(|id| id.name.to_string());
    // Classification still considers the binding name (e.g. capitalized => a
    // component, `use*` => a hook), falling back to the function's own name.
    let classify_name = inferred_name
        .map(|s| s.to_string())
        .or_else(|| emitted_name.clone());
    let fn_type = match classify_function(
        classify_name.as_deref(),
        &func.params,
        &FunctionBodyRef(func),
        compile_all,
    ) {
        Some(t) => t,
        None => return,
    };
    queue.push(CompileSource {
        func: FunctionForm::Function(func),
        fn_name: emitted_name,
        fn_type,
        fn_span: func.span(),
        fn_symbol_id: func.id.as_ref().and_then(|id| id.symbol_id.get()),
    });
}

fn consider_variable_declaration<'a>(
    var: &'a oxc::VariableDeclaration<'a>,
    compile_all: bool,
    queue: &mut Vec<CompileSource<'a>>,
) {
    for decl in &var.declarations {
        let name = match &decl.id {
            oxc::BindingPattern::BindingIdentifier(id) => id.name.to_string(),
            _ => continue,
        };
        let Some(init) = &decl.init else { continue };
        match init {
            oxc::Expression::ArrowFunctionExpression(arrow) => {
                let fn_type = match classify_function(
                    Some(&name),
                    &arrow.params,
                    &ArrowBodyRef(arrow),
                    compile_all,
                ) {
                    Some(t) => t,
                    None => continue,
                };
                queue.push(CompileSource {
                    func: FunctionForm::Arrow(arrow),
                    fn_name: Some(name),
                    fn_type,
                    fn_span: arrow.span(),
                    // Arrows have no own-name binding; never referenced-before-declared.
                    fn_symbol_id: None,
                });
            }
            oxc::Expression::FunctionExpression(func) => {
                consider_function(func, Some(&name), compile_all, queue);
            }
            _ => {}
        }
    }
}

// =============================================================================
// Entry point
// =============================================================================

/// Compile a program from the oxc AST + semantic model.
///
/// Returns a [`CompileResult`]. During N1.2 `ast` is always `None` (codegen /
/// output reassembly is deferred to N2); the per-pass HIR debug log on
/// `ordered_log` is the oracle.
pub fn compile_program(
    program: &oxc::Program,
    semantic: &Semantic,
    source_text: &str,
    options: PluginOptions,
) -> CompileProgramResult {
    let output_mode = CompilerOutputMode::from_opts(&options);

    // Log environment config for debugLogIRs (skipped by the dump-hir printer).
    let mut early_ordered_log: Vec<OrderedLogItem> = Vec::new();
    if options.debug {
        early_ordered_log.push(OrderedLogItem::Debug {
            entry: DebugLogEntry::new(
                "EnvironmentConfig",
                serde_json::to_string_pretty(&options.environment).unwrap_or_default(),
            ),
        });
    }

    if !options.should_compile {
        return CompileProgramResult {
            result: success(None, early_ordered_log, Vec::new()),
            native_artifacts: Vec::new(),
        };
    }

    // TODO(N1.3): port should_skip_compilation (existing runtime imports) and
    // restricted-import validation to the oxc AST. Skipped for the N1.2 input
    // flip.

    // React ESLint / Flow suppressions: if any suppression range affects a
    // function, that function is *not* compiled and an error is reported. This
    // mirrors `findProgramSuppressions` + the per-function check in
    // `Entrypoint/Program.ts`. When the compiler already validates both hooks
    // usage and exhaustive memoization deps, ESLint suppressions are NOT checked
    // (TS passes `ruleNames = null`); Flow suppressions are always honored.
    const DEFAULT_ESLINT_SUPPRESSIONS: &[&str] =
        &["react-hooks/exhaustive-deps", "react-hooks/rules-of-hooks"];
    let rule_names: Option<Vec<String>> = if options.environment.validate_exhaustive_memoization_dependencies
        && options.environment.validate_hooks_usage
    {
        None
    } else {
        Some(
            options
                .eslint_suppression_rules
                .clone()
                .unwrap_or_else(|| {
                    DEFAULT_ESLINT_SUPPRESSIONS
                        .iter()
                        .map(|s| s.to_string())
                        .collect()
                }),
        )
    };
    let suppressions = super::suppression::find_program_suppressions(
        &super::suppression::oxc_comments_to_ast_comments(&program.comments, source_text),
        rule_names.as_deref(),
        options.flow_suppressions,
    );

    // A top-level `'use no forget'` / `'use no memo'` (or custom opt-out)
    // program directive disables memoization for the entire module: every
    // function is still run through the pipeline (for validation) but its
    // compiled output is discarded so the original source is kept. Mirrors
    // `hasModuleScopeOptOut` in `Entrypoint/Program.ts`.
    let has_module_scope_opt_out = react_compiler_lowering::find_directive_disabling_memoization(
        program.directives.as_slice(),
        options.custom_opt_out_directives.as_deref(),
    )
    .is_some();

    let compile_all = options.compilation_mode == "all";

    let mut context = ProgramContext::new(
        options.clone(),
        options.filename.clone(),
        options.source_code.clone(),
        suppressions,
        has_module_scope_opt_out,
    );
    context.set_source_filename(options.filename.clone());
    context.init_from_semantic(semantic);
    context.ordered_log.extend(early_ordered_log);

    // TODO(N1.3): pre-register instrumentation / hook-guard imports.

    let env_config = options.environment.clone();
    let queue = find_functions_to_compile(program, compile_all);

    for source in &queue {
        // A React ESLint / Flow suppression that overlaps this function means
        // the user disabled a React rule for it. The compiler refuses to
        // optimize such a function and reports an error instead of compiling
        // it. Mirrors the per-function suppression check in `processFn`
        // (`Entrypoint/Program.ts`): the function is skipped (no pipeline run)
        // and its error is routed through the normal error handler.
        let suppressions_in_fn = super::suppression::filter_suppressions_that_affect_function(
            &context.suppressions,
            source.fn_span.start,
            source.fn_span.end,
        );
        if !suppressions_in_fn.is_empty() {
            let suppression_ranges: Vec<_> =
                suppressions_in_fn.into_iter().cloned().collect();
            let err = super::suppression::suppressions_to_compiler_error(&suppression_ranges);
            let fn_loc =
                span_to_logger_loc(source_text, source.fn_span, context.filename.clone());
            if let Some(result) = handle_error(&err, fn_loc, &mut context) {
                return CompileProgramResult {
                    result,
                    native_artifacts: Vec::new(),
                };
            }
            continue;
        }

        // When dynamic gating is configured, the function's `'use memo if(...)'`
        // directives are validated: a non-identifier gating expression, or more
        // than one gating directive, is an error and the function is not
        // compiled. Mirrors `findDirectivesDynamicGating` /
        // `tryFindDirectiveEnablingMemoization` in `Entrypoint/Program.ts`.
        if context.opts.dynamic_gating.is_some()
            && let Some(err) =
                validate_dynamic_gating_directives(source.func.body_directives(), source_text)
        {
            let fn_loc = span_to_logger_loc(source_text, source.fn_span, context.filename.clone());
            if let Some(result) = handle_error(&err, fn_loc, &mut context) {
                return CompileProgramResult {
                    result,
                    native_artifacts: Vec::new(),
                };
            }
            continue;
        }

        // Record how many native artifacts existed before compiling this
        // function so we can discard exactly the ones it produces (the main
        // artifact plus any outlined-function artifacts) if it turns out to be
        // opted out of compilation.
        let artifacts_before = context.native_artifacts.len();
        match pipeline::compile_fn(
            &source.func,
            source.fn_name.as_deref(),
            semantic,
            source_text,
            source.fn_type,
            output_mode,
            &env_config,
            &mut context,
        ) {
            Ok(codegen_fn) => {
                // A function is left uncompiled (original source kept) when
                // either the module carries a module-scope opt-out directive,
                // or `ignore_use_no_forget` is false and the function body has
                // an opt-out directive. The compiler still ran the function
                // through the pipeline (for validation) — we just discard the
                // compiled output. Mirrors `applyCompiledFunction`
                // (`hasModuleScopeOptOut`) and `processFn` (`directives.optOut`)
                // in `Entrypoint/Program.ts`.
                let body_opt_out = react_compiler_lowering::find_directive_disabling_memoization(
                    source.func.body_directives(),
                    context.opts.custom_opt_out_directives.as_deref(),
                );
                let body_skip = !context.opts.ignore_use_no_forget && body_opt_out.is_some();
                let skip_emit = has_module_scope_opt_out || body_skip;

                let fn_loc =
                    span_to_logger_loc(source_text, source.fn_span, context.filename.clone());

                if skip_emit {
                    // Drop the artifacts this function just produced so source
                    // assembly falls back to the original (uncompiled) source.
                    context.native_artifacts.truncate(artifacts_before);
                } else if let Some(gating) =
                    resolve_function_gating(&context, source.func.body_directives())
                {
                    // `@gating` (static or dynamic) is configured for this
                    // function: emit BOTH the compiled and original function and
                    // select between them at runtime via the imported gating
                    // flag. Resolve every collision-sensitive name here (where
                    // the ProgramContext import/uid state lives), then carry the
                    // plan on the main artifact for assembly. Mirrors
                    // `insertGatedFunctionDeclaration` in `Entrypoint/Gating.ts`.
                    let gating_local_name = context
                        .add_import_specifier(
                            &gating.source,
                            &gating.import_specifier_name,
                            None,
                        )
                        .name;

                    // Only a named `function Foo` declaration can be referenced
                    // before its declaration at top level (arrows / function
                    // expressions assigned to variables cannot).
                    let referenced_before_declaration = source.fn_symbol_id.is_some_and(|sid| {
                        is_referenced_before_declaration_at_top_level(semantic, sid)
                    });

                    let (result_name, optimized_name, unoptimized_name) =
                        if referenced_before_declaration {
                            // Allocate the dispatcher uids in TS order
                            // (`gatingCondition`, `unoptimized`, `optimized` —
                            // see `insertAdditionalFunctionDeclaration`).
                            let orig_name = source.fn_name.clone().unwrap_or_default();
                            let result = context.new_uid(&format!("{gating_local_name}_result"));
                            let unoptimized = context.new_uid(&format!("{orig_name}_unoptimized"));
                            let optimized = context.new_uid(&format!("{orig_name}_optimized"));
                            (Some(result), Some(optimized), Some(unoptimized))
                        } else {
                            (None, None, None)
                        };

                    let plan = crate::entrypoint::native_codegen::GatingPlan {
                        gating_local_name,
                        gating_source: gating.source,
                        gating_imported: gating.import_specifier_name,
                        referenced_before_declaration,
                        result_name,
                        optimized_name,
                        unoptimized_name,
                    };

                    // Attach the plan to the main artifact for this function (the
                    // one whose span matches the source function). Outlined
                    // artifacts keep `gating: None`.
                    let fn_start = source.fn_span.start;
                    if let Some(artifact) = context.native_artifacts[artifacts_before..]
                        .iter_mut()
                        .find(|a| a.fn_span.0 == fn_start)
                    {
                        artifact.gating = Some(plan);
                    }
                }

                if body_skip {
                    // TS logs a `CompileSkip` event for a body-level opt-out
                    // directive (the `processFn` arm).
                    let directive = body_opt_out
                        .map(|d| d.expression.value.as_str())
                        .unwrap_or_default();
                    context.log_event(LoggerEvent::CompileSkip {
                        fn_loc,
                        reason: format!("Skipped due to '{directive}' directive."),
                        loc: None,
                    });
                } else {
                    // Module-scope opt-out still logs CompileSuccess in TS (the
                    // skip happens later in `applyCompiledFunction`), as does a
                    // normally-compiled function.
                    context.log_event(LoggerEvent::CompileSuccess {
                        fn_loc,
                        fn_name: source.fn_name.clone(),
                        memo_slots: codegen_fn.memo_slots_used,
                        memo_blocks: codegen_fn.memo_blocks,
                        memo_values: codegen_fn.memo_values,
                        pruned_memo_blocks: codegen_fn.pruned_memo_blocks,
                        pruned_memo_values: codegen_fn.pruned_memo_values,
                    });
                }
            }
            Err(err) => {
                let fn_loc =
                    span_to_logger_loc(source_text, source.fn_span, context.filename.clone());
                // A function carrying an opt-out directive (`'use no forget'` /
                // `'use no memo'`) is allowed to fail: the compiler still ran it
                // through validation, but the error is *logged*, not surfaced as
                // fatal, and the function is left uncompiled while the rest of
                // the file continues. Mirrors `processFn` in
                // `Entrypoint/Program.ts` (the `directives.optOut != null` arm).
                let opt_out = react_compiler_lowering::find_directive_disabling_memoization(
                    source.func.body_directives(),
                    context.opts.custom_opt_out_directives.as_deref(),
                );
                if opt_out.is_some() {
                    log_error(&err, fn_loc, &mut context);
                } else if let Some(result) = handle_error(&err, fn_loc, &mut context) {
                    return CompileProgramResult {
                        result,
                        native_artifacts: Vec::new(),
                    };
                }
            }
        }
    }

    // N1.2: HIR-oracle `ast` stays None; N2.1 native codegen happens in
    // `react_compiler_oxc::transform` using the artifacts returned below.
    let renames = convert_renames(&context.renames);
    // In lint output mode the compiler runs purely for diagnostics: the original
    // source is emitted unchanged and no compiled function is inserted. Mirrors
    // TS `applyCompiledFunction` in `Entrypoint/Program.ts`, which returns `null`
    // (skipping insertion) when `outputMode === 'lint'`. Dropping the native
    // artifacts makes assembly fall back to source passthrough.
    let native_artifacts = if output_mode == CompilerOutputMode::Lint {
        Vec::new()
    } else {
        std::mem::take(&mut context.native_artifacts)
    };
    CompileProgramResult {
        result: CompileResult::Success {
            events: context.events,
            ordered_log: context.ordered_log,
            renames,
            timing: Vec::new(),
        },
        native_artifacts,
    }
}

fn success(
    renames: Option<Vec<BindingRenameInfo>>,
    ordered_log: Vec<OrderedLogItem>,
    events: Vec<LoggerEvent>,
) -> CompileResult {
    CompileResult::Success {
        events,
        ordered_log,
        renames: renames.unwrap_or_default(),
        timing: Vec::new(),
    }
}

// =============================================================================
// Dynamic gating directive validation (`'use memo if(<ident>)'`)
// =============================================================================

/// Convert an oxc byte-offset span into a diagnostics `SourceLocation`.
fn span_to_diag_loc(source: &str, span: Span) -> SourceLocation {
    let start = position_of_offset(source, span.start);
    let end = position_of_offset(source, span.end);
    SourceLocation {
        start: react_compiler_diagnostics::Position {
            line: start.line,
            column: start.column,
            index: start.index,
        },
        end: react_compiler_diagnostics::Position {
            line: end.line,
            column: end.column,
            index: end.index,
        },
    }
}

/// A name is a valid JavaScript identifier if it is a syntactically valid
/// identifier *and* not a reserved word. Mirrors Babel's `t.isValidIdentifier`
/// (used by `findDirectivesDynamicGating`).
fn is_valid_js_identifier(name: &str) -> bool {
    !name.is_empty()
        && oxc_syntax::identifier::is_identifier_name(name)
        && !oxc_syntax::keyword::is_reserved_keyword(name)
}

/// Validate the `'use memo if(<ident>)'` dynamic-gating directives on a
/// function's body. Returns `Some(error)` when a directive is malformed (the
/// gating expression is not a valid identifier) or when more than one gating
/// directive is present. Mirrors `findDirectivesDynamicGating` in
/// `Entrypoint/Program.ts`. Only runs when dynamic gating is configured.
fn validate_dynamic_gating_directives(
    directives: &[oxc::Directive],
    source_text: &str,
) -> Option<CompilerError> {
    const PREFIX: &str = "use memo if(";
    let mut error = CompilerError::new();
    let mut matches: Vec<&oxc::Directive> = Vec::new();

    for directive in directives {
        let value = directive.expression.value.as_str();
        // Match `^use memo if\(([^)]*)\)$`.
        let Some(inner) = value
            .strip_prefix(PREFIX)
            .and_then(|rest| rest.strip_suffix(')'))
        else {
            continue;
        };
        // `[^)]*` — the captured group must not contain a `)`.
        if inner.contains(')') {
            continue;
        }
        if is_valid_js_identifier(inner) {
            matches.push(directive);
        } else {
            let diag = react_compiler_diagnostics::CompilerDiagnostic::new(
                ErrorCategory::Gating,
                "Dynamic gating directive is not a valid JavaScript identifier",
                Some(format!("Found '{value}'")),
            )
            .with_detail(react_compiler_diagnostics::CompilerDiagnosticDetail::Error {
                loc: Some(span_to_diag_loc(source_text, directive.span)),
                message: None,
                identifier_name: None,
            });
            error.push_diagnostic(diag);
        }
    }

    if error.has_any_errors() {
        return Some(error);
    }
    if matches.len() > 1 {
        let found = matches
            .iter()
            .map(|d| d.expression.value.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let mut error = CompilerError::new();
        let diag = react_compiler_diagnostics::CompilerDiagnostic::new(
            ErrorCategory::Gating,
            "Multiple dynamic gating directives found",
            Some(format!("Expected a single directive but found [{found}]")),
        )
        .with_detail(react_compiler_diagnostics::CompilerDiagnosticDetail::Error {
            loc: Some(span_to_diag_loc(source_text, matches[0].span)),
            message: None,
            identifier_name: None,
        });
        error.push_diagnostic(diag);
        return Some(error);
    }
    None
}

/// Find the single dynamic-gating directive match (`'use memo if(<ident>)'`) on
/// a function's body, returning the captured identifier. Returns `None` when
/// there is no match, more than one match, or the captured group is not a valid
/// identifier (validation has already reported those cases — this is the
/// resolution counterpart of [`validate_dynamic_gating_directives`], mirroring
/// the success branch of `findDirectivesDynamicGating` in `Entrypoint/Program.ts`).
fn find_dynamic_gating_match(directives: &[oxc::Directive]) -> Option<String> {
    const PREFIX: &str = "use memo if(";
    let mut matches: Vec<String> = Vec::new();
    for directive in directives {
        let value = directive.expression.value.as_str();
        let Some(inner) = value
            .strip_prefix(PREFIX)
            .and_then(|rest| rest.strip_suffix(')'))
        else {
            continue;
        };
        if inner.contains(')') {
            continue;
        }
        if is_valid_js_identifier(inner) {
            matches.push(inner.to_string());
        }
    }
    if matches.len() == 1 {
        matches.pop()
    } else {
        None
    }
}

/// The resolved gating function for a function: a module + imported specifier
/// name. Mirrors TS `ExternalFunction` (`{source, importSpecifierName}`).
struct ResolvedGating {
    source: String,
    import_specifier_name: String,
}

/// Resolve the per-function gating function: `dynamicGating(directive) ?? opts.gating`.
/// Mirrors `applyCompiledFunctions` in `Entrypoint/Program.ts` (`functionGating`).
fn resolve_function_gating(
    context: &ProgramContext,
    directives: &[oxc::Directive],
) -> Option<ResolvedGating> {
    if let Some(dynamic) = &context.opts.dynamic_gating {
        if let Some(import_specifier_name) = find_dynamic_gating_match(directives) {
            return Some(ResolvedGating {
                source: dynamic.source.clone(),
                import_specifier_name,
            });
        }
    }
    context.opts.gating.as_ref().map(|g| ResolvedGating {
        source: g.source.clone(),
        import_specifier_name: g.import_specifier_name.clone(),
    })
}

/// Determine whether the top-level function with `symbol_id` is referenced
/// before its declaration at the top level. Mirrors
/// `getFunctionReferencedBeforeDeclarationAtTopLevel` in `Entrypoint/Program.ts`:
/// the function (which must be a named declaration) is referenced-before-declared
/// when its binding is referenced from the module top-level scope (not from
/// inside any function) at a position *before* its own declaration. The TS
/// traversal stops tracking a name once it reaches the declaration id, so only
/// references that appear earlier in source order count (a reference *after* the
/// declaration is a normal forward use and does not require the dispatcher form).
fn is_referenced_before_declaration_at_top_level(
    semantic: &Semantic,
    symbol_id: oxc_semantic::SymbolId,
) -> bool {
    let scoping = semantic.scoping();
    let root_scope = scoping.root_scope_id();
    // The declaration site (the function's binding identifier span). References
    // before this offset and in the top-level scope are referenced-before-decl.
    let decl_start = scoping.symbol_span(symbol_id).start;
    // `get_resolved_reference_ids` yields the references that resolve to this
    // symbol. A reference located in the module top-level scope (i.e. not nested
    // inside any function/arrow scope) and occurring before the declaration is a
    // referenced-before-declaration use.
    for &reference_id in scoping.get_resolved_reference_ids(symbol_id) {
        let reference = scoping.get_reference(reference_id);
        let node_id = reference.node_id();
        let node = semantic.nodes().get_node(node_id);
        if node.span().start >= decl_start {
            continue;
        }
        if is_top_level_scope(scoping, node.scope_id(), root_scope) {
            return true;
        }
    }
    false
}

/// Whether `scope` resolves to the top-level (module) scope without passing
/// through any function scope.
fn is_top_level_scope(
    scoping: &oxc_semantic::Scoping,
    scope: oxc_semantic::ScopeId,
    root_scope: oxc_semantic::ScopeId,
) -> bool {
    use oxc_syntax::scope::ScopeFlags;
    let mut current = Some(scope);
    while let Some(s) = current {
        if s == root_scope {
            return true;
        }
        let flags = scoping.scope_flags(s);
        if flags.contains(ScopeFlags::Function) || flags.contains(ScopeFlags::Arrow) {
            return false;
        }
        current = scoping.scope_parent_id(s);
    }
    false
}

// =============================================================================
// Source location helpers (oxc Span -> logger location)
// =============================================================================

fn position_of_offset(source: &str, offset: u32) -> LoggerPosition {
    let off = offset as usize;
    let mut line: u32 = 1;
    let mut line_start: usize = 0;
    for (i, b) in source.as_bytes().iter().enumerate() {
        if i >= off {
            break;
        }
        if *b == b'\n' {
            line += 1;
            line_start = i + 1;
        }
    }
    LoggerPosition {
        line,
        column: (off.saturating_sub(line_start)) as u32,
        index: Some(offset),
    }
}

fn span_to_logger_loc(
    source: &str,
    span: Span,
    filename: Option<String>,
) -> Option<LoggerSourceLocation> {
    Some(LoggerSourceLocation {
        start: position_of_offset(source, span.start),
        end: position_of_offset(source, span.end),
        filename,
        identifier_name: None,
    })
}

// =============================================================================
// Error handling / logging (preserved from the bridge implementation)
// =============================================================================

/// Handle an error according to `panicThreshold`. Returns
/// `Some(CompileResult::Error)` if it should surface as fatal, else `None`.
fn handle_error(
    err: &CompilerError,
    fn_loc: Option<LoggerSourceLocation>,
    context: &mut ProgramContext,
) -> Option<CompileResult> {
    log_error(err, fn_loc, context);

    let should_panic = match context.opts.panic_threshold.as_str() {
        "all_errors" => true,
        "critical_errors" => err.has_errors(),
        _ => false,
    };

    let is_config_error = err.details.iter().any(|d| match d {
        CompilerErrorOrDiagnostic::Diagnostic(d) => d.category == ErrorCategory::Config,
        CompilerErrorOrDiagnostic::ErrorDetail(d) => d.category == ErrorCategory::Config,
    });

    if should_panic || is_config_error {
        let source_fn = context.source_filename();
        let mut error_info = compiler_error_to_info(err, source_fn.as_deref());

        let is_simulated_unknown = err.details.len() == 1
            && err.details.iter().all(|d| match d {
                CompilerErrorOrDiagnostic::ErrorDetail(d) => {
                    d.category == ErrorCategory::Invariant && d.reason == "unexpected error"
                }
                _ => false,
            });
        if is_simulated_unknown {
            error_info.raw_message = Some("unexpected error".to_string());
        }

        if error_info.raw_message.is_none() {
            if let Some(ref source) = context.code {
                error_info.formatted_message = Some(
                    react_compiler_diagnostics::code_frame::format_compiler_error(
                        err,
                        source,
                        source_fn.as_deref(),
                    ),
                );
            }
        }

        Some(CompileResult::Error {
            error: error_info,
            events: context.events.clone(),
            ordered_log: context.ordered_log.clone(),
            timing: Vec::new(),
        })
    } else {
        None
    }
}

fn log_error(
    err: &CompilerError,
    fn_loc: Option<LoggerSourceLocation>,
    context: &mut ProgramContext,
) {
    let source_filename = fn_loc.as_ref().and_then(|l| l.filename.clone());

    let is_simulated_unknown = err.details.len() == 1
        && err.details.iter().all(|d| match d {
            CompilerErrorOrDiagnostic::ErrorDetail(d) => {
                d.category == ErrorCategory::Invariant && d.reason == "unexpected error"
            }
            _ => false,
        });
    if is_simulated_unknown {
        context.log_event(LoggerEvent::PipelineError {
            fn_loc: fn_loc.clone(),
            data: "Error: unexpected error".to_string(),
        });
        return;
    }

    for detail in &err.details {
        let detail_info = match detail {
            CompilerErrorOrDiagnostic::Diagnostic(d) => CompilerErrorDetailInfo {
                category: format!("{:?}", d.category),
                reason: d.reason.clone(),
                description: d.description.clone(),
                severity: format!("{:?}", d.logged_severity()),
                suggestions: suggestions_to_logger(&d.suggestions),
                details: diagnostic_details_to_items(d, source_filename.as_deref()),
                loc: None,
            },
            CompilerErrorOrDiagnostic::ErrorDetail(d) => CompilerErrorDetailInfo {
                category: format!("{:?}", d.category),
                reason: d.reason.clone(),
                description: d.description.clone(),
                severity: format!("{:?}", d.logged_severity()),
                suggestions: suggestions_to_logger(&d.suggestions),
                details: None,
                loc: d
                    .loc
                    .as_ref()
                    .map(|l| diag_loc_to_logger_loc(l, source_filename.as_deref())),
            },
        };
        if let Some(ref loc) = fn_loc {
            context.log_event(LoggerEvent::CompileErrorWithLoc {
                fn_loc: loc.clone(),
                detail: detail_info,
            });
        } else {
            context.log_event(LoggerEvent::CompileError {
                fn_loc: None,
                detail: detail_info,
            });
        }
    }
}

fn compiler_error_to_info(err: &CompilerError, filename: Option<&str>) -> CompilerErrorInfo {
    let details: Vec<CompilerErrorDetailInfo> = err
        .details
        .iter()
        .map(|d| match d {
            CompilerErrorOrDiagnostic::Diagnostic(d) => CompilerErrorDetailInfo {
                category: format!("{:?}", d.category),
                reason: d.reason.clone(),
                description: d.description.clone(),
                severity: format!("{:?}", d.severity()),
                suggestions: suggestions_to_logger(&d.suggestions),
                details: diagnostic_details_to_items(d, filename),
                loc: None,
            },
            CompilerErrorOrDiagnostic::ErrorDetail(d) => CompilerErrorDetailInfo {
                category: format!("{:?}", d.category),
                reason: d.reason.clone(),
                description: d.description.clone(),
                severity: format!("{:?}", d.severity()),
                suggestions: suggestions_to_logger(&d.suggestions),
                details: None,
                loc: d.loc.as_ref().map(|l| diag_loc_to_logger_loc(l, filename)),
            },
        })
        .collect();

    let (reason, description) = details
        .first()
        .map(|d| (d.reason.clone(), d.description.clone()))
        .unwrap_or_else(|| ("Unknown error".to_string(), None));

    CompilerErrorInfo {
        reason,
        description,
        details,
        raw_message: None,
        formatted_message: None,
    }
}

fn diagnostic_details_to_items(
    d: &react_compiler_diagnostics::CompilerDiagnostic,
    filename: Option<&str>,
) -> Option<Vec<CompilerErrorItemInfo>> {
    let items: Vec<CompilerErrorItemInfo> = d
        .details
        .iter()
        .map(|item| match item {
            react_compiler_diagnostics::CompilerDiagnosticDetail::Error {
                loc,
                message,
                identifier_name,
            } => CompilerErrorItemInfo {
                kind: "error".to_string(),
                loc: loc.as_ref().map(|l| {
                    let mut logger_loc = diag_loc_to_logger_loc(l, filename);
                    logger_loc.identifier_name = identifier_name.clone();
                    logger_loc
                }),
                message: message.clone(),
            },
            react_compiler_diagnostics::CompilerDiagnosticDetail::Hint { message } => {
                CompilerErrorItemInfo {
                    kind: "hint".to_string(),
                    loc: None,
                    message: Some(message.clone()),
                }
            }
        })
        .collect();
    if items.is_empty() { None } else { Some(items) }
}

fn diag_loc_to_logger_loc(loc: &SourceLocation, filename: Option<&str>) -> LoggerSourceLocation {
    LoggerSourceLocation {
        start: LoggerPosition {
            line: loc.start.line,
            column: loc.start.column,
            index: loc.start.index,
        },
        end: LoggerPosition {
            line: loc.end.line,
            column: loc.end.column,
            index: loc.end.index,
        },
        filename: filename.map(|s| s.to_string()),
        identifier_name: None,
    }
}

fn suggestions_to_logger(
    suggestions: &Option<Vec<react_compiler_diagnostics::CompilerSuggestion>>,
) -> Option<Vec<LoggerSuggestionInfo>> {
    suggestions.as_ref().map(|suggestions| {
        suggestions
            .iter()
            .map(|s| {
                let op = match s.op {
                    react_compiler_diagnostics::CompilerSuggestionOperation::InsertBefore => {
                        LoggerSuggestionOp::InsertBefore
                    }
                    react_compiler_diagnostics::CompilerSuggestionOperation::InsertAfter => {
                        LoggerSuggestionOp::InsertAfter
                    }
                    react_compiler_diagnostics::CompilerSuggestionOperation::Remove => {
                        LoggerSuggestionOp::Remove
                    }
                    react_compiler_diagnostics::CompilerSuggestionOperation::Replace => {
                        LoggerSuggestionOp::Replace
                    }
                };
                LoggerSuggestionInfo {
                    description: s.description.clone(),
                    op,
                    range: s.range,
                    text: s.text.clone(),
                }
            })
            .collect()
    })
}

fn convert_renames(
    renames: &[react_compiler_hir::environment::BindingRename],
) -> Vec<BindingRenameInfo> {
    renames
        .iter()
        .map(|r| BindingRenameInfo {
            original: r.original.clone(),
            renamed: r.renamed.clone(),
            declaration_start: r.declaration_start,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_hook_name() {
        assert!(is_hook_name("useState"));
        assert!(is_hook_name("useEffect"));
        assert!(is_hook_name("use0Something"));
        assert!(!is_hook_name("use"));
        assert!(!is_hook_name("user"));
        assert!(!is_hook_name("usethis"));
    }

    #[test]
    fn test_is_component_name() {
        assert!(is_component_name("Foo"));
        assert!(is_component_name("App"));
        assert!(!is_component_name("foo"));
        assert!(!is_component_name(""));
    }

    #[test]
    fn test_is_pascal_case_namespace() {
        assert!(is_pascal_case_namespace("React"));
        assert!(is_pascal_case_namespace("MyLib"));
        assert!(!is_pascal_case_namespace("react"));
        assert!(!is_pascal_case_namespace(""));
    }
}
