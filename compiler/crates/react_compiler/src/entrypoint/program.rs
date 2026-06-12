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

/// Classify a function by name. Returns `None` for names that are neither a
/// component nor a hook (in `compilationMode: "all"` these still compile as
/// `Other`, handled by the caller).
fn classify_by_name(name: Option<&str>) -> Option<ReactFunctionType> {
    let name = name?;
    if is_component_name(name) {
        Some(ReactFunctionType::Component)
    } else if is_hook_name(name) {
        Some(ReactFunctionType::Hook)
    } else {
        None
    }
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
            oxc::Statement::ExportDefaultDeclaration(export) => {
                if let oxc::ExportDefaultDeclarationKind::FunctionDeclaration(func) =
                    &export.declaration
                {
                    consider_function(func, None, compile_all, &mut queue);
                }
            }
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
    let name = inferred_name
        .map(|s| s.to_string())
        .or_else(|| func.id.as_ref().map(|id| id.name.to_string()));
    let fn_type = match classify_by_name(name.as_deref()) {
        Some(t) => t,
        None if compile_all => ReactFunctionType::Other,
        None => return,
    };
    queue.push(CompileSource {
        func: FunctionForm::Function(func),
        fn_name: name,
        fn_type,
        fn_span: func.span(),
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
                let fn_type = match classify_by_name(Some(&name)) {
                    Some(t) => t,
                    None if compile_all => ReactFunctionType::Other,
                    None => continue,
                };
                queue.push(CompileSource {
                    func: FunctionForm::Arrow(arrow),
                    fn_name: Some(name),
                    fn_type,
                    fn_span: arrow.span(),
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

    // TODO(N1.3): port should_skip_compilation (existing runtime imports),
    // restricted-import validation, suppressions, and module-scope opt-out
    // directives to the oxc AST. Skipped for the N1.2 input flip.

    let compile_all = options.compilation_mode == "all";

    let mut context = ProgramContext::new(
        options.clone(),
        options.filename.clone(),
        options.source_code.clone(),
        Vec::new(), // suppressions: TODO(N1.3)
        false,      // has_module_scope_opt_out: TODO(N1.3)
    );
    context.set_source_filename(options.filename.clone());
    context.init_from_semantic(semantic);
    context.ordered_log.extend(early_ordered_log);

    // TODO(N1.3): pre-register instrumentation / hook-guard imports.

    let env_config = options.environment.clone();
    let queue = find_functions_to_compile(program, compile_all);

    for source in &queue {
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
                context.log_event(LoggerEvent::CompileSuccess {
                    fn_loc: span_to_logger_loc(
                        source_text,
                        source.fn_span,
                        context.filename.clone(),
                    ),
                    fn_name: source.fn_name.clone(),
                    memo_slots: codegen_fn.memo_slots_used,
                    memo_blocks: codegen_fn.memo_blocks,
                    memo_values: codegen_fn.memo_values,
                    pruned_memo_blocks: codegen_fn.pruned_memo_blocks,
                    pruned_memo_values: codegen_fn.pruned_memo_values,
                });
            }
            Err(err) => {
                let fn_loc =
                    span_to_logger_loc(source_text, source.fn_span, context.filename.clone());
                if let Some(result) = handle_error(&err, fn_loc, &mut context) {
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
    let native_artifacts = std::mem::take(&mut context.native_artifacts);
    CompileProgramResult {
        result: CompileResult::Success {
            ast: None,
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
        ast: None,
        events,
        ordered_log,
        renames: renames.unwrap_or_default(),
        timing: Vec::new(),
    }
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
    fn test_classify_by_name() {
        assert_eq!(
            classify_by_name(Some("Foo")),
            Some(ReactFunctionType::Component)
        );
        assert_eq!(
            classify_by_name(Some("useThing")),
            Some(ReactFunctionType::Hook)
        );
        assert_eq!(classify_by_name(Some("helper")), None);
        assert_eq!(classify_by_name(None), None);
    }
}
