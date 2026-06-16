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
use std::time::Instant;

use clap::Parser;
use react_compiler::entrypoint::compile_result::LoggerEvent;
use react_compiler::entrypoint::compile_result::OrderedLogItem;
use react_compiler::entrypoint::plugin_options::PluginOptions;

#[derive(Parser)]
#[command(name = "react-compiler-e2e")]
struct Cli {
    /// Frontend to use: only "oxc" is supported (swc was removed)
    #[arg(long, default_value = "oxc")]
    frontend: String,

    /// Filename (used to determine source type from extension)
    #[arg(long, default_value = "")]
    filename: String,

    /// JSON-serialized PluginOptions
    #[arg(long)]
    options: Option<String>,

    /// Output JSON envelope with code/error and logger events
    #[arg(long)]
    json: bool,

    /// Dump the per-pass HIR debug log to stdout instead of compiled code.
    /// Enables debug logging, runs the oxc compile, and prints each pass's
    /// state as `## <PassName>` blocks (matching test-rust-port.ts format),
    /// to use as a printer-independent oracle. Implies `compilationMode: all`.
    #[arg(long)]
    dump_hir: bool,

    /// Benchmark mode: compile every `.js` fixture under <DIR> (recursively,
    /// excluding `*.flow.js`) in a single process. All sources are read up
    /// front (IO is excluded from timing); only the compile loop
    /// (parse + semantic + native transform + codegen) is timed. The whole
    /// corpus is compiled `--iterations` times; the first (warmup) iteration
    /// is discarded and per-iteration / per-fixture statistics are reported.
    /// Uses `compilationMode: all` so every function in every file is compiled
    /// (matching the TS baseline harness and bypassing the React prefilter).
    #[arg(long)]
    bench: Option<String>,

    /// Number of timed iterations over the corpus in `--bench` mode. The first
    /// iteration is always run as warmup and discarded, so the reported stats
    /// come from `iterations` measurements. Default: 6 (1 warmup + 5 reported,
    /// effectively — actually iterations measurements after a separate warmup).
    #[arg(long, default_value_t = 6)]
    iterations: usize,

    /// In `--bench` mode, only run parse + semantic (skip the React compiler
    /// transform + codegen) to measure the front-end cost in isolation. Used
    /// for the parse-vs-full-pipeline breakdown.
    #[arg(long)]
    bench_parse_only: bool,
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

    // Benchmark mode: compile a whole corpus in one process and report timing.
    if let Some(ref dir) = cli.bench {
        run_bench(dir, cli.iterations, cli.bench_parse_only);
        return;
    }

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
        "oxc" => compile_oxc(&source, &cli.filename, options),
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

/// A single fixture loaded for benchmarking: its source text and filename.
struct BenchFixture {
    filename: String,
    source: String,
}

/// Recursively collect `.js` fixtures under `dir`, excluding `*.flow.js`
/// (oxc has no Flow parser). Returns sorted paths for determinism.
fn collect_fixture_paths(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<std::path::PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect_fixture_paths(&path, out);
        } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name.ends_with(".js") && !name.ends_with(".flow.js") {
                out.push(path);
            }
        }
    }
}

/// Compile one already-loaded fixture through the full native path, mirroring
/// `compile_oxc`: parse -> semantic -> transform (which internally runs the
/// React compiler passes + native oxc codegen). When `parse_only` is true, only
/// parse + semantic run (the front-end cost in isolation). Returns whether the
/// fixture produced compiled code, so the timed loop can't be optimized away.
#[inline]
fn bench_compile_one(fixture: &BenchFixture, parse_only: bool) -> bool {
    let first_line = fixture.source.lines().next().unwrap_or("");
    let is_script = first_line.contains("@script");
    let source_type = oxc_span::SourceType::from_path(&fixture.filename)
        .unwrap_or_default()
        .with_module(!is_script)
        .with_script(is_script)
        .with_jsx(true)
        .with_typescript(true);

    let allocator = oxc_allocator::Allocator::default();
    let parsed = oxc_parser::Parser::new(&allocator, &fixture.source, source_type).parse();
    if parsed.panicked {
        return false;
    }

    let semantic = oxc_semantic::SemanticBuilder::new()
        .build(&parsed.program)
        .semantic;

    if parse_only {
        // Touch the semantic result so the build can't be optimized away.
        std::hint::black_box(&semantic);
        return !parsed.program.body.is_empty();
    }

    // compilationMode "all" so every function is compiled (bypass prefilter),
    // matching the TS baseline harness which uses compilationMode: 'all'.
    let options: PluginOptions = serde_json::from_str(
        r#"{"shouldCompile":true,"enableReanimated":false,"isDev":false,"compilationMode":"all","panicThreshold":"all_errors"}"#,
    )
    .unwrap();

    let mut result =
        react_compiler_oxc::transform(&parsed.program, &semantic, &fixture.source, options);
    let code = result.code.take();
    code.is_some()
}

/// Run the corpus benchmark: read all fixtures up front (IO excluded from the
/// timed region), then compile the whole corpus `iterations` times. The first
/// iteration is a separate warmup pass (discarded); statistics are reported
/// over the `iterations` timed passes.
fn run_bench(dir: &str, iterations: usize, parse_only: bool) {
    let root = std::path::Path::new(dir);
    let mut paths = Vec::new();
    if root.is_file() {
        paths.push(root.to_path_buf());
    } else {
        collect_fixture_paths(root, &mut paths);
    }

    // Load all sources up front — IO is OUTSIDE the timed region.
    let mut fixtures: Vec<BenchFixture> = Vec::with_capacity(paths.len());
    for path in &paths {
        match std::fs::read_to_string(path) {
            Ok(source) => fixtures.push(BenchFixture {
                filename: path.to_string_lossy().into_owned(),
                source,
            }),
            Err(e) => eprintln!("skip {}: {e}", path.display()),
        }
    }

    let n = fixtures.len();
    if n == 0 {
        eprintln!("No fixtures found under {dir}");
        process::exit(1);
    }

    let mode = if parse_only {
        "parse+semantic only"
    } else {
        "full pipeline (parse+semantic+compile+codegen)"
    };
    eprintln!("Benchmarking {n} fixtures, {iterations} timed iterations, mode: {mode}");

    // Warmup pass (discarded): also counts how many fixtures compile to code.
    let mut compiled_count = 0usize;
    for fixture in &fixtures {
        if bench_compile_one(fixture, parse_only) {
            compiled_count += 1;
        }
    }

    // Timed iterations.
    let mut iter_secs: Vec<f64> = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        let mut sink = 0usize;
        for fixture in &fixtures {
            if bench_compile_one(fixture, parse_only) {
                sink += 1;
            }
        }
        std::hint::black_box(sink);
        iter_secs.push(start.elapsed().as_secs_f64());
    }

    iter_secs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = iter_secs[iter_secs.len() / 2];
    let min = iter_secs[0];
    let max = iter_secs[iter_secs.len() - 1];
    let fixtures_per_sec = n as f64 / median;
    let per_fixture_ms = (median / n as f64) * 1000.0;

    println!("=== Rust native-oxc bench ({mode}) ===");
    println!("fixtures:            {n}");
    println!("compiled to code:    {compiled_count}");
    println!("iterations (timed):  {iterations}");
    println!("per-iteration total: median {median:.4}s  min {min:.4}s  max {max:.4}s");
    println!("fixtures/sec:        {fixtures_per_sec:.1}");
    println!("per-fixture median:  {per_fixture_ms:.4} ms");
}

fn compile_oxc(source: &str, filename: &str, mut options: PluginOptions) -> CompileOutput {
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

    let mut result = react_compiler_oxc::transform(&parsed.program, &semantic, source, options);
    let events = std::mem::take(&mut result.events);
    let ordered_log = std::mem::take(&mut result.ordered_log);
    let _ = std::mem::take(&mut result.rename_plan);

    // Check for error-level diagnostics, similar to SWC path.
    // OxcDiagnostic uses miette's Severity.
    let has_errors = result
        .diagnostics
        .iter()
        .any(|d| d.severity == oxc_diagnostics::Severity::Error);

    // N2.1: native oxc codegen path — when `code` is populated, emit it
    // directly. This is the re-enabled CODE oracle output.
    if let Some(code) = result.code.take() {
        return CompileOutput {
            code: Some(code),
            error: None,
            events,
            ordered_log,
        };
    }

    // No function compiled natively (`result.code` was `None`).
    if has_errors {
        // Compilation had errors — mimic TS plugin throwing.
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
        // No changes — emit the original parsed program (already has comments).
        CompileOutput {
            code: Some(oxc_codegen::Codegen::new().build(&parsed.program).code),
            error: None,
            events,
            ordered_log,
        }
    }
}
