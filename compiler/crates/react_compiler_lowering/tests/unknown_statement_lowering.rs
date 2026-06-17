//! An untranscribed statement inside a function body degrades gracefully: the
//! oxc-direct lowering records a `Todo` diagnostic (caught by the fault-tolerant
//! pipeline) rather than panicking, and still returns an `HirFunction`.
//!
//! Stage N1.2.1 only lowers the function shell + trivial constructs for real;
//! everything else bails with a graceful `Todo`. This test pins that behavior.

use oxc_allocator::Allocator;
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use react_compiler_diagnostics::ErrorCategory;
use react_compiler_hir::environment::Environment;
use react_compiler_lowering::{FunctionForm, lower};

#[test]
fn unknown_statement_in_function_body_records_todo_bailout() {
    // A `for` statement is not yet transcribed to the oxc path, so lowering it
    // must record a graceful Todo and still produce HIR.
    let source = "function useValue() {\n  for (;;) {}\n}\n";

    let allocator = Allocator::default();
    let source_type = SourceType::tsx();
    let parsed = Parser::new(&allocator, source, source_type).parse();
    assert!(
        parsed.diagnostics.is_empty(),
        "parse errors: {:?}",
        parsed.diagnostics
    );

    let semantic_ret = SemanticBuilder::new()
        .with_build_nodes(true)
        .build(&parsed.program);
    assert!(
        semantic_ret.diagnostics.is_empty(),
        "semantic errors: {:?}",
        semantic_ret.diagnostics
    );
    let semantic = semantic_ret.semantic;

    // The first top-level statement is the function declaration.
    let func = match parsed.program.body.first() {
        Some(oxc_ast::ast::Statement::FunctionDeclaration(f)) => f,
        other => panic!("expected a function declaration, got {other:?}"),
    };

    let mut env = Environment::new();
    let result = lower(
        &FunctionForm::Function(func),
        Some("useValue"),
        &semantic,
        source,
        &mut env,
    );

    let hir = result.expect("lowering degrades gracefully; it does not fail outright");
    assert!(env.has_errors(), "expected a recorded Todo error");

    // The recorded error is a Todo (graceful bailout), not an Invariant/panic.
    let has_todo = env.errors().details.iter().any(|d| match d {
        react_compiler_diagnostics::CompilerErrorOrDiagnostic::Diagnostic(d) => {
            d.category == ErrorCategory::Todo
        }
        react_compiler_diagnostics::CompilerErrorOrDiagnostic::ErrorDetail(d) => {
            d.category == ErrorCategory::Todo
        }
    });
    assert!(has_todo, "expected a Todo bailout, got: {:?}", env.errors());

    // The function shell still lowers (params/body block/return exist).
    assert_eq!(hir.id.as_deref(), Some("useValue"));
    assert!(
        !hir.body.blocks.is_empty(),
        "expected the function shell to produce at least one block"
    );
}
