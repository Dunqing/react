pub mod codegen_assembly;
pub mod diagnostics;
pub mod prefilter;
pub mod rename_apply;

use diagnostics::compile_result_to_diagnostics;
use prefilter::has_react_like_functions;
use react_compiler::entrypoint::compile_result::LoggerEvent;
use react_compiler::entrypoint::compile_result::OrderedLogItem;
use react_compiler::entrypoint::plugin_options::PluginOptions;

/// Result of compiling a program via the OXC frontend.
pub struct TransformResult {
    /// N2.1: natively-printed compiled code. When `Some`, the CLI emits this
    /// directly (native oxc codegen path). When `None`, no functions were
    /// compiled via the native path (fall back to passthrough / error).
    pub code: Option<String>,
    pub diagnostics: Vec<oxc_diagnostics::OxcDiagnostic>,
    pub events: Vec<LoggerEvent>,
    /// Unified ordered log interleaving logger events and debug entries
    /// (per-pass HIR dumps) in emission order. Only populated when
    /// `options.debug` (i.e. `__debug`) is enabled. Used by the e2e CLI's
    /// `--dump-hir` flag as a printer-independent oracle.
    pub ordered_log: Vec<OrderedLogItem>,
    /// Variable renames computed during lowering (binding name collisions
    /// resolved to `name_0`, `name_1`, …). These must be applied to ANY part of
    /// the output emitted from the original source AST — i.e. uncompiled
    /// passthrough functions — to mirror the reference compiler, which mutates
    /// the source AST in place via Babel's `scope.rename`. The compiled path
    /// applies them inside `assemble_and_print`; the passthrough path (when
    /// `code` is `None`) must apply them via [`apply_renames_to_program`].
    pub renames: Vec<react_compiler::entrypoint::compile_result::BindingRenameInfo>,
}

/// Result of linting a program via the OXC frontend.
pub struct LintResult {
    pub diagnostics: Vec<oxc_diagnostics::OxcDiagnostic>,
}

/// Primary transform API — accepts pre-parsed OXC AST + semantic.
pub fn transform(
    program: &oxc_ast::ast::Program,
    semantic: &oxc_semantic::Semantic,
    source_text: &str,
    options: PluginOptions,
) -> TransformResult {
    // Prefilter: skip files without React-like functions (unless compilationMode == "all")
    if options.compilation_mode != "all" && !has_react_like_functions(program) {
        return TransformResult {
            code: None,
            diagnostics: vec![],
            events: vec![],
            ordered_log: vec![],
            renames: vec![],
        };
    }

    // N2.1: capture the bits of `options` we need for native codegen assembly
    // before `compile_program` consumes it by value.
    let runtime_module =
        react_compiler::entrypoint::imports::get_react_compiler_runtime_module(&options.target);
    let source_type = source_type_for(source_text, options.filename.as_deref());

    // N1.2: run the compiler DIRECTLY against the oxc AST + semantic model.
    let compiled = react_compiler::entrypoint::program::compile_program(
        program,
        semantic,
        source_text,
        options,
    );
    let result = compiled.result;
    let native_artifacts = compiled.native_artifacts;

    let diagnostics = compile_result_to_diagnostics(&result);
    let (events, ordered_log, renames) = match result {
        react_compiler::entrypoint::compile_result::CompileResult::Success {
            events,
            ordered_log,
            renames,
            ..
        } => (events, ordered_log, renames),
        react_compiler::entrypoint::compile_result::CompileResult::Error {
            events,
            ordered_log,
            ..
        } => (events, ordered_log, Vec::new()),
    };

    // N2.1: native oxc codegen + assembly + print. Runs AFTER the input
    // `semantic` borrow has ended (artifacts are owned), against a fresh
    // re-parse of the source in its own allocator. `code` is `Some` only when
    // at least one function compiled natively.
    let code = codegen_assembly::assemble_and_print(
        source_text,
        source_type,
        &native_artifacts,
        &runtime_module,
        &renames,
    );

    TransformResult {
        code,
        diagnostics,
        events,
        ordered_log,
        renames,
    }
}

/// Parse `source_text` + build its semantic model, then run `f` with the oxc
/// AST + semantic. The allocator lives for the duration of `f`; `f`'s return
/// value is owned, so it safely outlives the parse.
fn with_parsed_semantic<R>(
    source_text: &str,
    source_type: oxc_span::SourceType,
    f: impl FnOnce(&oxc_ast::ast::Program, &oxc_semantic::Semantic) -> R,
) -> R {
    let allocator = oxc_allocator::Allocator::default();
    let parsed = oxc_parser::Parser::new(&allocator, source_text, source_type).parse();
    let semantic = oxc_semantic::SemanticBuilder::new()
        .with_build_nodes(true)
        .build(&parsed.program)
        .semantic;
    f(&parsed.program, &semantic)
}

/// Convenience wrapper — parses source text, runs semantic analysis, then transforms.
pub fn transform_source(
    source_text: &str,
    source_type: oxc_span::SourceType,
    options: PluginOptions,
) -> TransformResult {
    with_parsed_semantic(source_text, source_type, |program, semantic| {
        transform(program, semantic, source_text, options)
    })
}

/// Determine the oxc `SourceType` for re-parsing during native codegen
/// assembly. Mirrors the CLI's logic: TS + JSX enabled, module unless a
/// `@script` pragma appears on the first line.
fn source_type_for(source_text: &str, filename: Option<&str>) -> oxc_span::SourceType {
    let first_line = source_text.lines().next().unwrap_or("");
    let is_script = first_line.contains("@script");
    let base = filename
        .and_then(|f| oxc_span::SourceType::from_path(f).ok())
        .unwrap_or_default();
    base.with_module(!is_script)
        .with_script(is_script)
        .with_jsx(true)
        .with_typescript(true)
}

/// Lint API — accepts pre-parsed OXC AST + semantic.
/// Same as transform but only collects diagnostics, no AST output.
pub fn lint(
    program: &oxc_ast::ast::Program,
    semantic: &oxc_semantic::Semantic,
    source_text: &str,
    options: PluginOptions,
) -> LintResult {
    let mut opts = options;
    opts.no_emit = true;

    let result = transform(program, semantic, source_text, opts);
    LintResult {
        diagnostics: result.diagnostics,
    }
}

/// Convenience wrapper — parses source text, runs semantic analysis, then lints.
pub fn lint_source(
    source_text: &str,
    source_type: oxc_span::SourceType,
    options: PluginOptions,
) -> LintResult {
    with_parsed_semantic(source_text, source_type, |program, semantic| {
        lint(program, semantic, source_text, options)
    })
}
