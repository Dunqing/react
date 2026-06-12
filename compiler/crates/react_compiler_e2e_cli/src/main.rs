// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the root directory of this source tree.

//! CLI for end-to-end testing of the React Compiler via the OXC frontend.
//!
//! Reads source from stdin, compiles via the chosen frontend, writes compiled
//! code to stdout. Errors go to stderr. Exit 0 = success, exit 1 = error.
//!
//! With `--json`, outputs a JSON envelope to stdout containing code/error and
//! logger events. Always exits 0 in JSON mode (errors are in the envelope).
//!
//! The SWC frontend has been removed; only `--frontend oxc` is supported.
//!
//! Usage:
//!   react-compiler-e2e --frontend oxc --filename <path> [--options <json>] [--json]

use std::io::Read;
use std::process;

use clap::Parser;
use react_compiler::entrypoint::compile_result::LoggerEvent;
use react_compiler::entrypoint::compile_result::OrderedLogItem;
use react_compiler::entrypoint::plugin_options::PluginOptions;

#[derive(Parser)]
#[command(name = "react-compiler-e2e")]
struct Cli {
    /// Frontend to use: only "oxc" is supported (swc was removed)
    #[arg(long)]
    frontend: String,

    /// Filename (used to determine source type from extension)
    #[arg(long)]
    filename: String,

    /// JSON-serialized PluginOptions
    #[arg(long)]
    options: Option<String>,

    /// Output JSON envelope with code/error and logger events
    #[arg(long)]
    json: bool,

    /// Dump ScopeInfo as JSON to stderr (for debugging scope analysis differences)
    #[arg(long)]
    dump_scope: bool,

    /// Dump the per-pass HIR debug log to stdout instead of compiled code.
    /// Enables debug logging, runs the oxc compile, and prints each pass's
    /// state as `## <PassName>` blocks (matching test-rust-port.ts format),
    /// to use as a printer-independent oracle. Implies `compilationMode: all`.
    #[arg(long)]
    dump_hir: bool,
}

/// Result of compiling via a frontend, carrying both code/error and logger events.
struct CompileOutput {
    code: Option<String>,
    error: Option<String>,
    events: Vec<LoggerEvent>,
    /// Unified per-pass debug log (events + HIR dumps). Only populated when
    /// debug logging is enabled (via `--dump-hir`).
    ordered_log: Vec<OrderedLogItem>,
}

fn main() {
    let cli = Cli::parse();

    // Read source from stdin
    let mut source = String::new();
    std::io::stdin()
        .read_to_string(&mut source)
        .unwrap_or_else(|e| {
            eprintln!("Failed to read stdin: {e}");
            process::exit(1);
        });

    // Parse options — merge provided JSON over sensible defaults.
    // When dumping HIR, enable debug logging and compile every function
    // (compilationMode: "all"), matching the test-rust-port.ts oracle so the
    // two per-pass HIR logs can be diffed.
    let default_json = if cli.dump_hir {
        r#"{"shouldCompile":true,"enableReanimated":false,"isDev":false,"__debug":true,"compilationMode":"all"}"#
    } else {
        r#"{"shouldCompile":true,"enableReanimated":false,"isDev":false}"#
    };
    let options: PluginOptions = if let Some(ref json) = cli.options {
        // Merge: start with defaults, override with provided values
        let mut base: serde_json::Value = serde_json::from_str(default_json).unwrap();
        let overrides: serde_json::Value = serde_json::from_str(json).unwrap_or_else(|e| {
            eprintln!("Failed to parse options JSON: {e}");
            process::exit(1);
        });
        if let (serde_json::Value::Object(b), serde_json::Value::Object(o)) = (&mut base, overrides)
        {
            for (k, v) in o {
                b.insert(k, v);
            }
        }
        serde_json::from_value(base).unwrap_or_else(|e| {
            eprintln!("Failed to deserialize merged options: {e}");
            process::exit(1);
        })
    } else {
        serde_json::from_str(default_json).unwrap()
    };

    let output = match cli.frontend.as_str() {
        "oxc" => compile_oxc(&source, &cli.filename, options, cli.dump_scope),
        "swc" => {
            eprintln!("The 'swc' frontend has been removed. Use '--frontend oxc'.");
            process::exit(1);
        }
        other => {
            eprintln!("Unknown frontend: {other}. Use 'oxc'.");
            process::exit(1);
        }
    };

    if cli.dump_hir {
        // Dump-HIR mode: print the per-pass HIR debug log to stdout in the
        // same textual format test-rust-port.ts uses for the TS side, so the
        // two can be diffed. Each debug entry becomes a `## <name>` block.
        // The synthetic `EnvironmentConfig` entry is skipped (matching the
        // test-rust-port.ts debugLogIRs handler), and logger events are
        // omitted since this oracle only compares per-pass HIR state.
        print_hir_dump(&output.ordered_log);
        return;
    }

    if cli.json {
        // JSON envelope mode: always output JSON to stdout, exit 0
        let envelope = serde_json::json!({
            "code": output.code,
            "error": output.error,
            "events": output.events,
        });
        println!("{}", serde_json::to_string(&envelope).unwrap());
    } else {
        // Legacy mode: code to stdout, errors to stderr
        match (output.code, output.error) {
            (Some(code), _) => {
                print!("{code}");
            }
            (None, Some(e)) => {
                eprintln!("{e}");
                process::exit(1);
            }
            (None, None) => {
                process::exit(1);
            }
        }
    }
}

/// Print the per-pass HIR debug log to stdout, matching the textual format
/// `test-rust-port.ts` uses for its TS-side oracle.
///
/// Each debug entry is rendered as `## <name>\n<value>`, blocks joined by a
/// blank-newline boundary (the `value` itself ends without a trailing newline,
/// so blocks are separated by a single `\n`). The synthetic `EnvironmentConfig`
/// entry is skipped to match the TS-side `debugLogIRs` handler. Logger events
/// are not printed: this oracle compares per-pass HIR state only.
///
/// Opaque-ID normalization (renumbering IdentifierId/Type/block IDs, stripping
/// mutableRange) is intentionally NOT applied here — `test-rust-port.ts` applies
/// it symmetrically to both sides at diff time, so the raw dump is the right
/// thing to emit.
fn print_hir_dump(ordered_log: &[OrderedLogItem]) {
    let mut blocks: Vec<String> = Vec::new();
    for item in ordered_log {
        if let OrderedLogItem::Debug { entry } = item {
            if entry.name == "EnvironmentConfig" {
                continue;
            }
            blocks.push(format!("## {}\n{}", entry.name, entry.value));
        }
    }
    // Join with a single newline so the output matches `formatLog` (which joins
    // formatted items with "\n").
    println!("{}", blocks.join("\n"));
}

fn compile_oxc(
    source: &str,
    filename: &str,
    mut options: PluginOptions,
    dump_scope: bool,
) -> CompileOutput {
    options.filename = Some(filename.to_string());
    // Always enable TypeScript parsing (like the TS/Babel baseline uses
    // ['typescript', 'jsx'] plugins). Some .js fixtures contain TS syntax.
    // Check for @script pragma in the first line to use script source type.
    let first_line = source.lines().next().unwrap_or("");
    let is_script = first_line.contains("@script");
    let source_type = oxc_span::SourceType::from_path(filename)
        .unwrap_or_default()
        .with_module(!is_script)
        .with_script(is_script)
        .with_jsx(true)
        .with_typescript(true);

    let allocator = oxc_allocator::Allocator::default();
    let parsed = oxc_parser::Parser::new(&allocator, source, source_type).parse();

    if parsed.panicked || !parsed.errors.is_empty() {
        let err_msgs: Vec<String> = parsed.errors.iter().map(|e| e.to_string()).collect();
        return CompileOutput {
            code: None,
            error: Some(format!("OXC parse errors: {}", err_msgs.join("; "))),
            events: vec![],
            ordered_log: vec![],
        };
    }

    let semantic = oxc_semantic::SemanticBuilder::new()
        .build(&parsed.program)
        .semantic;

    if dump_scope {
        let scope_info =
            react_compiler_oxc::convert_scope::convert_scope_info(&semantic, &parsed.program);
        eprintln!("{}", serde_json::to_string_pretty(&scope_info).unwrap());
    }

    let mut result = react_compiler_oxc::transform(&parsed.program, &semantic, source, options);
    let events = std::mem::take(&mut result.events);
    let ordered_log = std::mem::take(&mut result.ordered_log);
    let rename_plan = std::mem::take(&mut result.rename_plan);

    // Check for error-level diagnostics, similar to SWC path.
    // OxcDiagnostic uses miette's Severity.
    let has_errors = result
        .diagnostics
        .iter()
        .any(|d| d.severity == oxc_diagnostics::Severity::Error);

    match result.file {
        Some(ref file) => {
            let emit_allocator = oxc_allocator::Allocator::default();
            CompileOutput {
                code: Some(react_compiler_oxc::emit(
                    file,
                    &emit_allocator,
                    Some(source),
                    &rename_plan,
                )),
                error: None,
                events,
                ordered_log,
            }
        }
        None => {
            if has_errors {
                // Compilation had errors — mimic TS plugin throwing
                let messages: Vec<String> = result
                    .diagnostics
                    .iter()
                    .map(|d| d.message.to_string())
                    .collect();
                CompileOutput {
                    code: None,
                    error: Some(messages.join("\n")),
                    events,
                    ordered_log,
                }
            } else {
                // No changes — emit the original parsed program (already has comments)
                CompileOutput {
                    code: Some(oxc_codegen::Codegen::new().build(&parsed.program).code),
                    error: None,
                    events,
                    ordered_log,
                }
            }
        }
    }
}
