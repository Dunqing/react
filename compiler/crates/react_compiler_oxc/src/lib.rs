pub mod apply_renames;
pub mod codegen_assembly;
pub mod convert_ast_reverse;
pub mod convert_scope;
pub mod diagnostics;
pub mod prefilter;

use std::collections::HashMap;

use diagnostics::compile_result_to_diagnostics;
use prefilter::has_react_like_functions;
use react_compiler::entrypoint::compile_result::LoggerEvent;
use react_compiler::entrypoint::compile_result::OrderedLogItem;
use react_compiler::entrypoint::plugin_options::PluginOptions;

/// Result of compiling a program via the OXC frontend.
pub struct TransformResult {
    /// The compiled program as a react_compiler_ast File (None if no changes needed).
    pub file: Option<react_compiler_ast::File>,
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
    /// Pre-computed rename plan: maps source positions (span.start) to new
    /// identifier names. Built from the compiler's binding renames and the
    /// original scope info. Applied during `emit()` to fix references in
    /// uncompiled sibling functions.
    pub rename_plan: HashMap<u32, String>,
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
            file: None,
            code: None,
            diagnostics: vec![],
            events: vec![],
            ordered_log: vec![],
            rename_plan: HashMap::new(),
        };
    }

    // N2.1: capture the bits of `options` we need for native codegen assembly
    // before `compile_program` consumes it by value.
    let runtime_module =
        react_compiler::entrypoint::imports::get_react_compiler_runtime_module(&options.target);
    let source_type = source_type_for(source_text, options.filename.as_deref());

    // N1.2: run the compiler DIRECTLY against the oxc AST + semantic model
    // (no react_compiler_ast / ScopeInfo bridge).
    let compiled = react_compiler::entrypoint::program::compile_program(
        program,
        semantic,
        source_text,
        options,
    );
    let result = compiled.result;
    let native_artifacts = compiled.native_artifacts;

    let diagnostics = compile_result_to_diagnostics(&result);
    let (events, ordered_log, _renames) = match result {
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
    );

    TransformResult {
        file: None,
        code,
        diagnostics,
        events,
        ordered_log,
        rename_plan: HashMap::new(),
    }
}

/// Convenience wrapper — parses source text, runs semantic analysis, then transforms.
pub fn transform_source(
    source_text: &str,
    source_type: oxc_span::SourceType,
    options: PluginOptions,
) -> TransformResult {
    let allocator = oxc_allocator::Allocator::default();
    let parsed = oxc_parser::Parser::new(&allocator, source_text, source_type).parse();

    let semantic = oxc_semantic::SemanticBuilder::new()
        .build(&parsed.program)
        .semantic;

    transform(&parsed.program, &semantic, source_text, options)
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

/// Emit a react_compiler_ast::File to a string via OXC codegen.
/// Converts the File to an OXC Program, then uses oxc_codegen to emit.
///
/// If `source_text` is provided, comments from the original source will be
/// preserved in the output by re-parsing the source to extract comments and
/// injecting them into the OXC program before codegen.
///
/// If `rename_plan` is non-empty, binding renames are applied to the OXC
/// program before emission. This fixes references in uncompiled sibling
/// functions when the compiler renames a shared binding.
pub fn emit(
    file: &react_compiler_ast::File,
    allocator: &oxc_allocator::Allocator,
    source_text: Option<&str>,
    rename_plan: &HashMap<u32, String>,
) -> String {
    let mut program = if let Some(source) = source_text {
        convert_ast_reverse::convert_program_to_oxc_with_source(file, allocator, source)
    } else {
        convert_ast_reverse::convert_program_to_oxc(file, allocator)
    };

    if let Some(source) = source_text {
        // Re-parse the original source to extract comments.
        // We use a separate allocator for the parse since we only need the comments.
        let comment_allocator = oxc_allocator::Allocator::default();
        // Parse as TSX to handle maximum syntax variety
        let source_type = oxc_span::SourceType::tsx();
        let parsed = oxc_parser::Parser::new(&comment_allocator, source, source_type).parse();

        // Collect the span starts of top-level statements in the compiled
        // program. Only comments attached to these positions should be
        // preserved — comments inside function bodies would have
        // `attached_to` values that don't match any top-level statement.
        let mut top_level_starts = std::collections::HashSet::new();
        top_level_starts.insert(0u32); // position 0 for comments at the very start
        for stmt in &program.body {
            use oxc_span::GetSpan;
            let start = stmt.span().start;
            if start > 0 {
                top_level_starts.insert(start);
            }
        }

        // Copy only comments attached to top-level statements.
        let mut comments =
            oxc_allocator::Vec::with_capacity_in(parsed.program.comments.len(), allocator);
        for comment in &parsed.program.comments {
            if top_level_starts.contains(&comment.attached_to) {
                comments.push(*comment);
            }
        }
        program.comments = comments;

        // Set the source_text so the codegen can extract comment content
        // from the original source spans.
        // We copy the source into the allocator to guarantee the lifetime.
        let source_in_alloc = oxc_allocator::StringBuilder::from_str_in(source, allocator);
        program.source_text = source_in_alloc.into_str();
    }

    // Apply binding renames to fix references in uncompiled sibling functions
    apply_renames::apply_renames(&mut program, rename_plan, allocator);

    oxc_codegen::Codegen::new().build(&program).code
}

/// Convenience wrapper — parses source text, runs semantic analysis, then lints.
pub fn lint_source(
    source_text: &str,
    source_type: oxc_span::SourceType,
    options: PluginOptions,
) -> LintResult {
    let allocator = oxc_allocator::Allocator::default();
    let parsed = oxc_parser::Parser::new(&allocator, source_text, source_type).parse();

    let semantic = oxc_semantic::SemanticBuilder::new()
        .build(&parsed.program)
        .semantic;

    lint(&parsed.program, &semantic, source_text, options)
}
