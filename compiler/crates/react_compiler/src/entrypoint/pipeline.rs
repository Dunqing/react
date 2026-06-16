// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the root directory of this source tree.

//! Compilation pipeline for a single function.
//!
//! Analogous to TS `Pipeline.ts` (`compileFn` → `run` → `runWithEnvironment`).
//! Currently runs BuildHIR (lowering) and PruneMaybeThrows.

use oxc_semantic::Semantic;
use react_compiler_diagnostics::CompilerError;
use react_compiler_hir::ReactFunctionType;
use react_compiler_hir::environment::Environment;
use react_compiler_hir::environment::OutputMode;
use react_compiler_hir::environment_config::EnvironmentConfig;
use react_compiler_lowering::FunctionForm;

use super::compile_result::CompileFnStats;
use super::compile_result::CompilerErrorDetailInfo;
use super::compile_result::CompilerErrorItemInfo;
use super::compile_result::DebugLogEntry;
use super::compile_result::LoggerPosition;
use super::compile_result::LoggerSourceLocation;
use super::imports::ProgramContext;
use super::native_codegen as native;
use super::plugin_options::CompilerOutputMode;
use crate::debug_print;

/// Run the compilation pipeline on a single function.
///
/// Creates an Environment, runs the full lowering → reactive-scope pipeline,
/// and pushes [`native::NativeArtifact`]s onto the context for native oxc
/// codegen. Returns the per-function memoization stats (for the
/// `CompileSuccess` logger event); the actual compiled code is emitted by the
/// native codegen path from the artifacts.
#[allow(clippy::too_many_arguments)]
pub fn compile_fn(
    func: &FunctionForm<'_>,
    fn_name: Option<&str>,
    semantic: &Semantic,
    source_text: &str,
    fn_type: ReactFunctionType,
    mode: CompilerOutputMode,
    env_config: &EnvironmentConfig,
    context: &mut ProgramContext,
) -> Result<CompileFnStats, CompilerError> {
    // N2.1: capture the source form (span + arrow-ness) for native oxc codegen
    // assembly before lowering moves on.
    let native_fn_span = {
        let s = func.span();
        (s.start, s.end)
    };
    let native_is_arrow = matches!(func, FunctionForm::Arrow(_));

    let mut env = Environment::with_config(env_config.clone());
    env.fn_type = fn_type;
    env.output_mode = match mode {
        CompilerOutputMode::Ssr => OutputMode::Ssr,
        CompilerOutputMode::Client => OutputMode::Client,
        CompilerOutputMode::Lint => OutputMode::Lint,
    };
    env.code = context.code.clone();
    env.filename = context.filename.clone();
    env.instrument_fn_name = context.instrument_fn_name.clone();
    env.instrument_gating_name = context.instrument_gating_name.clone();
    env.hook_guard_name = context.hook_guard_name.clone();
    env.seed_uid_known_names(&context.known_referenced_names());

    // N1.2: reference node ids were a bridge-only construct; the oxc path
    // resolves references directly via semantic. Left empty for now.
    env.reference_node_ids = Default::default();

    context.timing.start("lower");
    let mut hir = react_compiler_lowering::lower(func, fn_name, semantic, source_text, &mut env)?;
    context.timing.stop();

    // Copy renames from lowering to context (keep on env for codegen to apply to type annotations)
    if !env.renames.is_empty() {
        context.renames.extend(env.renames.iter().cloned());
    }

    // Check for Invariant errors after lowering, before logging HIR.
    // In TS, Invariant errors throw from recordError(), aborting lower() before
    // the HIR entry is logged. The thrown error contains ONLY the Invariant error,
    // not other recorded (non-Invariant) errors.
    if env.has_invariant_errors() {
        return Err(env.take_invariant_errors());
    }

    if context.debug_enabled {
        context.timing.start("debug_print:HIR");
        let debug_hir = debug_print::debug_hir(&hir, &env);
        context.log_debug(DebugLogEntry::new("HIR", debug_hir));
        context.timing.stop();
    }

    context.timing.start("PruneMaybeThrows");
    react_compiler_optimization::prune_maybe_throws(&mut hir, &mut env.functions)?;
    context.timing.stop();

    if context.debug_enabled {
        context.timing.start("debug_print:PruneMaybeThrows");
        let debug_prune = debug_print::debug_hir(&hir, &env);
        context.log_debug(DebugLogEntry::new("PruneMaybeThrows", debug_prune));
        context.timing.stop();
    }

    context.timing.start("ValidateContextVariableLValues");
    react_compiler_validation::validate_context_variable_lvalues(&hir, &mut env)?;
    if context.debug_enabled {
        context.log_debug(DebugLogEntry::new(
            "ValidateContextVariableLValues",
            "ok".to_string(),
        ));
    }
    context.timing.stop();

    context.timing.start("ValidateUseMemo");
    let void_memo_errors = react_compiler_validation::validate_use_memo(&hir, &mut env);
    log_errors_as_events(&void_memo_errors, context);
    if context.debug_enabled {
        context.log_debug(DebugLogEntry::new("ValidateUseMemo", "ok".to_string()));
    }
    context.timing.stop();

    context.timing.start("DropManualMemoization");
    react_compiler_optimization::drop_manual_memoization(&mut hir, &mut env)?;
    context.timing.stop();

    if context.debug_enabled {
        context.timing.start("debug_print:DropManualMemoization");
        let debug_drop_memo = debug_print::debug_hir(&hir, &env);
        context.log_debug(DebugLogEntry::new("DropManualMemoization", debug_drop_memo));
        context.timing.stop();
    }

    context
        .timing
        .start("InlineImmediatelyInvokedFunctionExpressions");
    react_compiler_optimization::inline_immediately_invoked_function_expressions(
        &mut hir, &mut env,
    );
    context.timing.stop();

    if context.debug_enabled {
        context
            .timing
            .start("debug_print:InlineImmediatelyInvokedFunctionExpressions");
        let debug_inline_iifes = debug_print::debug_hir(&hir, &env);
        context.log_debug(DebugLogEntry::new(
            "InlineImmediatelyInvokedFunctionExpressions",
            debug_inline_iifes,
        ));
        context.timing.stop();
    }

    context.timing.start("MergeConsecutiveBlocks");
    react_compiler_optimization::merge_consecutive_blocks::merge_consecutive_blocks(
        &mut hir,
        &mut env.functions,
    );
    context.timing.stop();

    if context.debug_enabled {
        context.timing.start("debug_print:MergeConsecutiveBlocks");
        let debug_merge = debug_print::debug_hir(&hir, &env);
        context.log_debug(DebugLogEntry::new("MergeConsecutiveBlocks", debug_merge));
        context.timing.stop();
    }

    // TODO: port assertConsistentIdentifiers
    if context.debug_enabled {
        context.log_debug(DebugLogEntry::new(
            "AssertConsistentIdentifiers",
            "ok".to_string(),
        ));
    }
    // TODO: port assertTerminalSuccessorsExist
    if context.debug_enabled {
        context.log_debug(DebugLogEntry::new(
            "AssertTerminalSuccessorsExist",
            "ok".to_string(),
        ));
    }

    context.timing.start("EnterSSA");
    react_compiler_ssa::enter_ssa(&mut hir, &mut env).map_err(|diag| {
        let loc = diag.primary_location().cloned();
        let mut err = CompilerError::new();
        err.push_error_detail(react_compiler_diagnostics::CompilerErrorDetail {
            category: diag.category,
            reason: diag.reason,
            description: diag.description,
            loc,
            suggestions: diag.suggestions,
        });
        err
    })?;
    context.timing.stop();

    if context.debug_enabled {
        context.timing.start("debug_print:SSA");
        let debug_ssa = debug_print::debug_hir(&hir, &env);
        context.log_debug(DebugLogEntry::new("SSA", debug_ssa));
        context.timing.stop();
    }

    context.timing.start("EliminateRedundantPhi");
    react_compiler_ssa::eliminate_redundant_phi(&mut hir, &mut env);
    context.timing.stop();

    if context.debug_enabled {
        context.timing.start("debug_print:EliminateRedundantPhi");
        let debug_eliminate_phi = debug_print::debug_hir(&hir, &env);
        context.log_debug(DebugLogEntry::new(
            "EliminateRedundantPhi",
            debug_eliminate_phi,
        ));
        context.timing.stop();
    }

    // TODO: port assertConsistentIdentifiers
    if context.debug_enabled {
        context.log_debug(DebugLogEntry::new(
            "AssertConsistentIdentifiers",
            "ok".to_string(),
        ));
    }

    context.timing.start("ConstantPropagation");
    react_compiler_optimization::constant_propagation(&mut hir, &mut env);
    context.timing.stop();

    if context.debug_enabled {
        context.timing.start("debug_print:ConstantPropagation");
        let debug_const_prop = debug_print::debug_hir(&hir, &env);
        context.log_debug(DebugLogEntry::new("ConstantPropagation", debug_const_prop));
        context.timing.stop();
    }

    context.timing.start("InferTypes");
    react_compiler_typeinference::infer_types(&mut hir, &mut env)?;
    context.timing.stop();

    if context.debug_enabled {
        context.timing.start("debug_print:InferTypes");
        let debug_infer_types = debug_print::debug_hir(&hir, &env);
        context.log_debug(DebugLogEntry::new("InferTypes", debug_infer_types));
        context.timing.stop();
    }

    if env.enable_validations() {
        if env.config.validate_hooks_usage {
            context.timing.start("ValidateHooksUsage");
            react_compiler_validation::validate_hooks_usage(&hir, &mut env)?;
            if context.debug_enabled {
                context.log_debug(DebugLogEntry::new("ValidateHooksUsage", "ok".to_string()));
            }
            context.timing.stop();
        }

        if env.config.validate_no_capitalized_calls.is_some() {
            context.timing.start("ValidateNoCapitalizedCalls");
            react_compiler_validation::validate_no_capitalized_calls(&hir, &mut env)?;
            if context.debug_enabled {
                context.log_debug(DebugLogEntry::new(
                    "ValidateNoCapitalizedCalls",
                    "ok".to_string(),
                ));
            }
            context.timing.stop();
        }
    }

    context.timing.start("OptimizePropsMethodCalls");
    react_compiler_optimization::optimize_props_method_calls(&mut hir, &env);
    context.timing.stop();

    if context.debug_enabled {
        context.timing.start("debug_print:OptimizePropsMethodCalls");
        let debug_optimize_props = debug_print::debug_hir(&hir, &env);
        context.log_debug(DebugLogEntry::new(
            "OptimizePropsMethodCalls",
            debug_optimize_props,
        ));
        context.timing.stop();
    }

    context.timing.start("AnalyseFunctions");
    let mut inner_logs: Vec<String> = Vec::new();
    let debug_inner = context.debug_enabled;
    let analyse_result = react_compiler_inference::analyse_functions(
        &mut hir,
        &mut env,
        &mut |inner_func, inner_env| {
            if debug_inner {
                inner_logs.push(debug_print::debug_hir(inner_func, inner_env));
            }
        },
    );
    context.timing.stop();

    // Always flush inner logs before propagating errors
    if context.debug_enabled {
        for inner_log in inner_logs {
            context.log_debug(DebugLogEntry::new("AnalyseFunction (inner)", inner_log));
        }
    }

    analyse_result?;

    if env.has_invariant_errors() {
        return Err(env.take_invariant_errors());
    }

    if context.debug_enabled {
        context.timing.start("debug_print:AnalyseFunctions");
        let debug_analyse_functions = debug_print::debug_hir(&hir, &env);
        context.log_debug(DebugLogEntry::new(
            "AnalyseFunctions",
            debug_analyse_functions,
        ));
        context.timing.stop();
    }

    context.timing.start("InferMutationAliasingEffects");
    react_compiler_inference::infer_mutation_aliasing_effects(&mut hir, &mut env, false)?;
    context.timing.stop();

    if context.debug_enabled {
        context
            .timing
            .start("debug_print:InferMutationAliasingEffects");
        let debug_infer_effects = debug_print::debug_hir(&hir, &env);
        context.log_debug(DebugLogEntry::new(
            "InferMutationAliasingEffects",
            debug_infer_effects,
        ));
        context.timing.stop();
    }

    if env.output_mode == OutputMode::Ssr {
        context.timing.start("OptimizeForSSR");
        react_compiler_optimization::optimize_for_ssr(&mut hir, &env);
        context.timing.stop();

        if context.debug_enabled {
            context.timing.start("debug_print:OptimizeForSSR");
            let debug_ssr = debug_print::debug_hir(&hir, &env);
            context.log_debug(DebugLogEntry::new("OptimizeForSSR", debug_ssr));
            context.timing.stop();
        }
    }

    context.timing.start("DeadCodeElimination");
    react_compiler_optimization::dead_code_elimination(&mut hir, &env);
    context.timing.stop();

    if context.debug_enabled {
        context.timing.start("debug_print:DeadCodeElimination");
        let debug_dce = debug_print::debug_hir(&hir, &env);
        context.log_debug(DebugLogEntry::new("DeadCodeElimination", debug_dce));
        context.timing.stop();
    }

    context.timing.start("PruneMaybeThrows2");
    react_compiler_optimization::prune_maybe_throws(&mut hir, &mut env.functions)?;
    context.timing.stop();

    if context.debug_enabled {
        context.timing.start("debug_print:PruneMaybeThrows2");
        let debug_prune2 = debug_print::debug_hir(&hir, &env);
        context.log_debug(DebugLogEntry::new("PruneMaybeThrows", debug_prune2));
        context.timing.stop();
    }

    context.timing.start("InferMutationAliasingRanges");
    react_compiler_inference::infer_mutation_aliasing_ranges(&mut hir, &mut env, false)?;
    context.timing.stop();

    if context.debug_enabled {
        context
            .timing
            .start("debug_print:InferMutationAliasingRanges");
        let debug_infer_ranges = debug_print::debug_hir(&hir, &env);
        context.log_debug(DebugLogEntry::new(
            "InferMutationAliasingRanges",
            debug_infer_ranges,
        ));
        context.timing.stop();
    }

    if env.enable_validations() {
        context
            .timing
            .start("ValidateLocalsNotReassignedAfterRender");
        react_compiler_validation::validate_locals_not_reassigned_after_render(&hir, &mut env);
        if context.debug_enabled {
            context.log_debug(DebugLogEntry::new(
                "ValidateLocalsNotReassignedAfterRender",
                "ok".to_string(),
            ));
        }
        context.timing.stop();

        if env.config.validate_ref_access_during_render {
            context.timing.start("ValidateNoRefAccessInRender");
            react_compiler_validation::validate_no_ref_access_in_render(&hir, &mut env);
            if context.debug_enabled {
                context.log_debug(DebugLogEntry::new(
                    "ValidateNoRefAccessInRender",
                    "ok".to_string(),
                ));
            }
            context.timing.stop();
        }

        if env.config.validate_no_set_state_in_render {
            context.timing.start("ValidateNoSetStateInRender");
            react_compiler_validation::validate_no_set_state_in_render(&hir, &mut env)?;
            if context.debug_enabled {
                context.log_debug(DebugLogEntry::new(
                    "ValidateNoSetStateInRender",
                    "ok".to_string(),
                ));
            }
            context.timing.stop();
        }

        if env.config.validate_no_derived_computations_in_effects_exp
            && env.output_mode == OutputMode::Lint
        {
            context
                .timing
                .start("ValidateNoDerivedComputationsInEffects");
            let errors =
                react_compiler_validation::validate_no_derived_computations_in_effects_exp(
                    &hir, &env,
                )?;
            log_errors_as_events(&errors, context);
            if context.debug_enabled {
                context.log_debug(DebugLogEntry::new(
                    "ValidateNoDerivedComputationsInEffects",
                    "ok".to_string(),
                ));
            }
            context.timing.stop();
        } else if env.config.validate_no_derived_computations_in_effects {
            context
                .timing
                .start("ValidateNoDerivedComputationsInEffects");
            react_compiler_validation::validate_no_derived_computations_in_effects(&hir, &mut env)?;
            if context.debug_enabled {
                context.log_debug(DebugLogEntry::new(
                    "ValidateNoDerivedComputationsInEffects",
                    "ok".to_string(),
                ));
            }
            context.timing.stop();
        }

        if env.config.validate_no_set_state_in_effects && env.output_mode == OutputMode::Lint {
            context.timing.start("ValidateNoSetStateInEffects");
            let errors = react_compiler_validation::validate_no_set_state_in_effects(&hir, &env)?;
            log_errors_as_events(&errors, context);
            if context.debug_enabled {
                context.log_debug(DebugLogEntry::new(
                    "ValidateNoSetStateInEffects",
                    "ok".to_string(),
                ));
            }
            context.timing.stop();
        }

        if env.config.validate_no_jsx_in_try_statements && env.output_mode == OutputMode::Lint {
            context.timing.start("ValidateNoJSXInTryStatement");
            let errors = react_compiler_validation::validate_no_jsx_in_try_statement(&hir);
            log_errors_as_events(&errors, context);
            if context.debug_enabled {
                context.log_debug(DebugLogEntry::new(
                    "ValidateNoJSXInTryStatement",
                    "ok".to_string(),
                ));
            }
            context.timing.stop();
        }

        context
            .timing
            .start("ValidateNoFreezingKnownMutableFunctions");
        react_compiler_validation::validate_no_freezing_known_mutable_functions(&hir, &mut env);
        if context.debug_enabled {
            context.log_debug(DebugLogEntry::new(
                "ValidateNoFreezingKnownMutableFunctions",
                "ok".to_string(),
            ));
        }
        context.timing.stop();
    }

    context.timing.start("InferReactivePlaces");
    react_compiler_inference::infer_reactive_places(&mut hir, &mut env)?;
    context.timing.stop();

    if context.debug_enabled {
        context.timing.start("debug_print:InferReactivePlaces");
        let debug_reactive_places = debug_print::debug_hir(&hir, &env);
        context.log_debug(DebugLogEntry::new(
            "InferReactivePlaces",
            debug_reactive_places,
        ));
        context.timing.stop();
    }

    if env.enable_validations() {
        context.timing.start("ValidateExhaustiveDependencies");
        react_compiler_validation::validate_exhaustive_dependencies(&mut hir, &mut env)?;
        if context.debug_enabled {
            context.log_debug(DebugLogEntry::new(
                "ValidateExhaustiveDependencies",
                "ok".to_string(),
            ));
        }
        context.timing.stop();
    }

    context
        .timing
        .start("RewriteInstructionKindsBasedOnReassignment");
    react_compiler_ssa::rewrite_instruction_kinds_based_on_reassignment(&mut hir, &env)?;
    context.timing.stop();

    if context.debug_enabled {
        context
            .timing
            .start("debug_print:RewriteInstructionKindsBasedOnReassignment");
        let debug_rewrite = debug_print::debug_hir(&hir, &env);
        context.log_debug(DebugLogEntry::new(
            "RewriteInstructionKindsBasedOnReassignment",
            debug_rewrite,
        ));
        context.timing.stop();
    }

    if env.enable_validations()
        && env.config.validate_static_components
        && env.output_mode == OutputMode::Lint
    {
        context.timing.start("ValidateStaticComponents");
        let errors = react_compiler_validation::validate_static_components(&hir);
        log_errors_as_events(&errors, context);
        if context.debug_enabled {
            context.log_debug(DebugLogEntry::new(
                "ValidateStaticComponents",
                "ok".to_string(),
            ));
        }
        context.timing.stop();
    }

    if env.enable_memoization() {
        context.timing.start("InferReactiveScopeVariables");
        react_compiler_inference::infer_reactive_scope_variables(&mut hir, &mut env)?;
        context.timing.stop();

        if context.debug_enabled {
            context
                .timing
                .start("debug_print:InferReactiveScopeVariables");
            let debug_infer_scopes = debug_print::debug_hir(&hir, &env);
            context.log_debug(DebugLogEntry::new(
                "InferReactiveScopeVariables",
                debug_infer_scopes,
            ));
            context.timing.stop();
        }
    }

    context
        .timing
        .start("MemoizeFbtAndMacroOperandsInSameScope");
    let fbt_operands =
        react_compiler_inference::memoize_fbt_and_macro_operands_in_same_scope(&hir, &mut env);
    context.timing.stop();

    if context.debug_enabled {
        context
            .timing
            .start("debug_print:MemoizeFbtAndMacroOperandsInSameScope");
        let debug_fbt = debug_print::debug_hir(&hir, &env);
        context.log_debug(DebugLogEntry::new(
            "MemoizeFbtAndMacroOperandsInSameScope",
            debug_fbt,
        ));
        context.timing.stop();
    }

    if env.config.enable_jsx_outlining {
        context.timing.start("OutlineJsx");
        react_compiler_optimization::outline_jsx(&mut hir, &mut env);
        context.timing.stop();
    }

    if env.config.enable_name_anonymous_functions {
        context.timing.start("NameAnonymousFunctions");
        react_compiler_optimization::name_anonymous_functions(&mut hir, &mut env);
        context.timing.stop();

        if context.debug_enabled {
            context.timing.start("debug_print:NameAnonymousFunctions");
            let debug_name_anon = debug_print::debug_hir(&hir, &env);
            context.log_debug(DebugLogEntry::new(
                "NameAnonymousFunctions",
                debug_name_anon,
            ));
            context.timing.stop();
        }
    }

    if env.config.enable_function_outlining {
        context.timing.start("OutlineFunctions");
        react_compiler_optimization::outline_functions(&mut hir, &mut env, &fbt_operands);
        context.timing.stop();

        if context.debug_enabled {
            context.timing.start("debug_print:OutlineFunctions");
            let debug_outline = debug_print::debug_hir(&hir, &env);
            context.log_debug(DebugLogEntry::new("OutlineFunctions", debug_outline));
            context.timing.stop();
        }
    }

    context.timing.start("AlignMethodCallScopes");
    react_compiler_inference::align_method_call_scopes(&mut hir, &mut env);
    context.timing.stop();

    if context.debug_enabled {
        context.timing.start("debug_print:AlignMethodCallScopes");
        let debug_align = debug_print::debug_hir(&hir, &env);
        context.log_debug(DebugLogEntry::new("AlignMethodCallScopes", debug_align));
        context.timing.stop();
    }

    context.timing.start("AlignObjectMethodScopes");
    react_compiler_inference::align_object_method_scopes(&mut hir, &mut env);
    context.timing.stop();

    if context.debug_enabled {
        context.timing.start("debug_print:AlignObjectMethodScopes");
        let debug_align_obj = debug_print::debug_hir(&hir, &env);
        context.log_debug(DebugLogEntry::new(
            "AlignObjectMethodScopes",
            debug_align_obj,
        ));
        context.timing.stop();
    }

    context.timing.start("PruneUnusedLabelsHIR");
    react_compiler_optimization::prune_unused_labels_hir(&mut hir);
    context.timing.stop();

    if context.debug_enabled {
        context.timing.start("debug_print:PruneUnusedLabelsHIR");
        let debug_prune_labels = debug_print::debug_hir(&hir, &env);
        context.log_debug(DebugLogEntry::new(
            "PruneUnusedLabelsHIR",
            debug_prune_labels,
        ));
        context.timing.stop();
    }

    context.timing.start("AlignReactiveScopesToBlockScopesHIR");
    react_compiler_inference::align_reactive_scopes_to_block_scopes_hir(&mut hir, &mut env);
    context.timing.stop();

    if context.debug_enabled {
        context
            .timing
            .start("debug_print:AlignReactiveScopesToBlockScopesHIR");
        let debug_align_block_scopes = debug_print::debug_hir(&hir, &env);
        context.log_debug(DebugLogEntry::new(
            "AlignReactiveScopesToBlockScopesHIR",
            debug_align_block_scopes,
        ));
        context.timing.stop();
    }

    context.timing.start("MergeOverlappingReactiveScopesHIR");
    react_compiler_inference::merge_overlapping_reactive_scopes_hir(&mut hir, &mut env);
    context.timing.stop();

    if context.debug_enabled {
        context
            .timing
            .start("debug_print:MergeOverlappingReactiveScopesHIR");
        let debug_merge_overlapping = debug_print::debug_hir(&hir, &env);
        context.log_debug(DebugLogEntry::new(
            "MergeOverlappingReactiveScopesHIR",
            debug_merge_overlapping,
        ));
        context.timing.stop();
    }

    // TODO: port assertValidBlockNesting
    if context.debug_enabled {
        context.log_debug(DebugLogEntry::new(
            "AssertValidBlockNesting",
            "ok".to_string(),
        ));
    }

    context.timing.start("BuildReactiveScopeTerminalsHIR");
    react_compiler_inference::build_reactive_scope_terminals_hir(&mut hir, &mut env);
    context.timing.stop();

    if context.debug_enabled {
        context
            .timing
            .start("debug_print:BuildReactiveScopeTerminalsHIR");
        let debug_build_scope_terminals = debug_print::debug_hir(&hir, &env);
        context.log_debug(DebugLogEntry::new(
            "BuildReactiveScopeTerminalsHIR",
            debug_build_scope_terminals,
        ));
        context.timing.stop();
    }

    // TODO: port assertValidBlockNesting
    if context.debug_enabled {
        context.log_debug(DebugLogEntry::new(
            "AssertValidBlockNesting",
            "ok".to_string(),
        ));
    }

    context.timing.start("FlattenReactiveLoopsHIR");
    react_compiler_inference::flatten_reactive_loops_hir(&mut hir);
    context.timing.stop();

    if context.debug_enabled {
        context.timing.start("debug_print:FlattenReactiveLoopsHIR");
        let debug_flatten_loops = debug_print::debug_hir(&hir, &env);
        context.log_debug(DebugLogEntry::new(
            "FlattenReactiveLoopsHIR",
            debug_flatten_loops,
        ));
        context.timing.stop();
    }

    context.timing.start("FlattenScopesWithHooksOrUseHIR");
    react_compiler_inference::flatten_scopes_with_hooks_or_use_hir(&mut hir, &env)?;
    context.timing.stop();

    if context.debug_enabled {
        context
            .timing
            .start("debug_print:FlattenScopesWithHooksOrUseHIR");
        let debug_flatten_hooks = debug_print::debug_hir(&hir, &env);
        context.log_debug(DebugLogEntry::new(
            "FlattenScopesWithHooksOrUseHIR",
            debug_flatten_hooks,
        ));
        context.timing.stop();
    }

    // TODO: port assertTerminalSuccessorsExist
    if context.debug_enabled {
        context.log_debug(DebugLogEntry::new(
            "AssertTerminalSuccessorsExist",
            "ok".to_string(),
        ));
    }
    // TODO: port assertTerminalPredsExist
    if context.debug_enabled {
        context.log_debug(DebugLogEntry::new(
            "AssertTerminalPredsExist",
            "ok".to_string(),
        ));
    }

    context.timing.start("PropagateScopeDependenciesHIR");
    react_compiler_inference::propagate_scope_dependencies_hir(&mut hir, &mut env);
    context.timing.stop();

    if context.debug_enabled {
        context
            .timing
            .start("debug_print:PropagateScopeDependenciesHIR");
        let debug_propagate_deps = debug_print::debug_hir(&hir, &env);
        context.log_debug(DebugLogEntry::new(
            "PropagateScopeDependenciesHIR",
            debug_propagate_deps,
        ));
        context.timing.stop();
    }

    context.timing.start("BuildReactiveFunction");
    let mut reactive_fn = react_compiler_reactive_scopes::build_reactive_function(&hir, &env)?;
    context.timing.stop();

    let hir_formatter = |fmt: &mut react_compiler_hir::print::PrintFormatter,
                         func: &react_compiler_hir::HirFunction| {
        debug_print::format_hir_function_into(fmt, func);
    };

    if context.debug_enabled {
        context.timing.start("debug_print:BuildReactiveFunction");
        let debug_reactive = react_compiler_reactive_scopes::print_reactive_function::debug_reactive_function_with_formatter(
            &reactive_fn, &env, Some(&hir_formatter),
        );
        context.log_debug(DebugLogEntry::new("BuildReactiveFunction", debug_reactive));
        context.timing.stop();
    }

    context.timing.start("AssertWellFormedBreakTargets");
    react_compiler_reactive_scopes::assert_well_formed_break_targets(&reactive_fn, &env);
    if context.debug_enabled {
        context.log_debug(DebugLogEntry::new(
            "AssertWellFormedBreakTargets",
            "ok".to_string(),
        ));
    }
    context.timing.stop();

    context.timing.start("PruneUnusedLabels");
    react_compiler_reactive_scopes::prune_unused_labels(&mut reactive_fn, &env)?;
    context.timing.stop();

    if context.debug_enabled {
        context.timing.start("debug_print:PruneUnusedLabels");
        let debug_prune_labels_reactive = react_compiler_reactive_scopes::print_reactive_function::debug_reactive_function_with_formatter(
            &reactive_fn, &env, Some(&hir_formatter),
        );
        context.log_debug(DebugLogEntry::new(
            "PruneUnusedLabels",
            debug_prune_labels_reactive,
        ));
        context.timing.stop();
    }

    context.timing.start("AssertScopeInstructionsWithinScopes");
    react_compiler_reactive_scopes::assert_scope_instructions_within_scopes(&reactive_fn, &env)?;
    if context.debug_enabled {
        context.log_debug(DebugLogEntry::new(
            "AssertScopeInstructionsWithinScopes",
            "ok".to_string(),
        ));
    }
    context.timing.stop();

    context.timing.start("PruneNonEscapingScopes");
    react_compiler_reactive_scopes::prune_non_escaping_scopes(&mut reactive_fn, &mut env)?;
    context.timing.stop();

    if context.debug_enabled {
        context.timing.start("debug_print:PruneNonEscapingScopes");
        let debug = react_compiler_reactive_scopes::print_reactive_function::debug_reactive_function_with_formatter(
            &reactive_fn, &env, Some(&hir_formatter),
        );
        context.log_debug(DebugLogEntry::new("PruneNonEscapingScopes", debug));
        context.timing.stop();
    }

    context.timing.start("PruneNonReactiveDependencies");
    react_compiler_reactive_scopes::prune_non_reactive_dependencies(&mut reactive_fn, &mut env);
    context.timing.stop();

    if context.debug_enabled {
        context
            .timing
            .start("debug_print:PruneNonReactiveDependencies");
        let debug_prune_non_reactive = react_compiler_reactive_scopes::print_reactive_function::debug_reactive_function_with_formatter(
            &reactive_fn, &env, Some(&hir_formatter),
        );
        context.log_debug(DebugLogEntry::new(
            "PruneNonReactiveDependencies",
            debug_prune_non_reactive,
        ));
        context.timing.stop();
    }

    context.timing.start("PruneUnusedScopes");
    react_compiler_reactive_scopes::prune_unused_scopes(&mut reactive_fn, &env)?;
    context.timing.stop();

    if context.debug_enabled {
        context.timing.start("debug_print:PruneUnusedScopes");
        let debug_prune_unused_scopes = react_compiler_reactive_scopes::print_reactive_function::debug_reactive_function_with_formatter(
            &reactive_fn, &env, Some(&hir_formatter),
        );
        context.log_debug(DebugLogEntry::new(
            "PruneUnusedScopes",
            debug_prune_unused_scopes,
        ));
        context.timing.stop();
    }

    context
        .timing
        .start("MergeReactiveScopesThatInvalidateTogether");
    react_compiler_reactive_scopes::merge_reactive_scopes_that_invalidate_together(
        &mut reactive_fn,
        &mut env,
    )?;
    context.timing.stop();

    if context.debug_enabled {
        context
            .timing
            .start("debug_print:MergeReactiveScopesThatInvalidateTogether");
        let debug = react_compiler_reactive_scopes::print_reactive_function::debug_reactive_function_with_formatter(
            &reactive_fn, &env, Some(&hir_formatter),
        );
        context.log_debug(DebugLogEntry::new(
            "MergeReactiveScopesThatInvalidateTogether",
            debug,
        ));
        context.timing.stop();
    }

    context.timing.start("PruneAlwaysInvalidatingScopes");
    react_compiler_reactive_scopes::prune_always_invalidating_scopes(&mut reactive_fn, &env)?;
    context.timing.stop();

    if context.debug_enabled {
        context
            .timing
            .start("debug_print:PruneAlwaysInvalidatingScopes");
        let debug_prune_always_inv = react_compiler_reactive_scopes::print_reactive_function::debug_reactive_function_with_formatter(
            &reactive_fn, &env, Some(&hir_formatter),
        );
        context.log_debug(DebugLogEntry::new(
            "PruneAlwaysInvalidatingScopes",
            debug_prune_always_inv,
        ));
        context.timing.stop();
    }

    context.timing.start("PropagateEarlyReturns");
    react_compiler_reactive_scopes::propagate_early_returns(&mut reactive_fn, &mut env);
    context.timing.stop();

    if context.debug_enabled {
        context.timing.start("debug_print:PropagateEarlyReturns");
        let debug = react_compiler_reactive_scopes::print_reactive_function::debug_reactive_function_with_formatter(
            &reactive_fn, &env, Some(&hir_formatter),
        );
        context.log_debug(DebugLogEntry::new("PropagateEarlyReturns", debug));
        context.timing.stop();
    }

    context.timing.start("PruneUnusedLValues");
    react_compiler_reactive_scopes::prune_unused_lvalues(&mut reactive_fn, &env);
    context.timing.stop();

    if context.debug_enabled {
        context.timing.start("debug_print:PruneUnusedLValues");
        let debug_prune_lvalues = react_compiler_reactive_scopes::print_reactive_function::debug_reactive_function_with_formatter(
            &reactive_fn, &env, Some(&hir_formatter),
        );
        context.log_debug(DebugLogEntry::new(
            "PruneUnusedLValues",
            debug_prune_lvalues,
        ));
        context.timing.stop();
    }

    context.timing.start("PromoteUsedTemporaries");
    react_compiler_reactive_scopes::promote_used_temporaries(&mut reactive_fn, &mut env);
    context.timing.stop();

    if context.debug_enabled {
        context.timing.start("debug_print:PromoteUsedTemporaries");
        let debug = react_compiler_reactive_scopes::print_reactive_function::debug_reactive_function_with_formatter(
            &reactive_fn, &env, Some(&hir_formatter),
        );
        context.log_debug(DebugLogEntry::new("PromoteUsedTemporaries", debug));
        context.timing.stop();
    }

    context
        .timing
        .start("ExtractScopeDeclarationsFromDestructuring");
    react_compiler_reactive_scopes::extract_scope_declarations_from_destructuring(
        &mut reactive_fn,
        &mut env,
    )?;
    context.timing.stop();

    if context.debug_enabled {
        context
            .timing
            .start("debug_print:ExtractScopeDeclarationsFromDestructuring");
        let debug = react_compiler_reactive_scopes::print_reactive_function::debug_reactive_function_with_formatter(
            &reactive_fn, &env, Some(&hir_formatter),
        );
        context.log_debug(DebugLogEntry::new(
            "ExtractScopeDeclarationsFromDestructuring",
            debug,
        ));
        context.timing.stop();
    }

    context.timing.start("StabilizeBlockIds");
    react_compiler_reactive_scopes::stabilize_block_ids(&mut reactive_fn, &mut env);
    context.timing.stop();

    if context.debug_enabled {
        context.timing.start("debug_print:StabilizeBlockIds");
        let debug_stabilize = react_compiler_reactive_scopes::print_reactive_function::debug_reactive_function_with_formatter(
            &reactive_fn, &env, Some(&hir_formatter),
        );
        context.log_debug(DebugLogEntry::new("StabilizeBlockIds", debug_stabilize));
        context.timing.stop();
    }

    context.timing.start("RenameVariables");
    let unique_identifiers =
        react_compiler_reactive_scopes::rename_variables(&mut reactive_fn, &mut env);
    context.timing.stop();

    for name in &unique_identifiers {
        context.add_new_reference(name.clone());
    }

    if context.debug_enabled {
        context.timing.start("debug_print:RenameVariables");
        let debug = react_compiler_reactive_scopes::print_reactive_function::debug_reactive_function_with_formatter(
            &reactive_fn, &env, Some(&hir_formatter),
        );
        context.log_debug(DebugLogEntry::new("RenameVariables", debug));
        context.timing.stop();
    }

    context.timing.start("PruneHoistedContexts");
    react_compiler_reactive_scopes::prune_hoisted_contexts(&mut reactive_fn, &mut env)?;
    context.timing.stop();

    if context.debug_enabled {
        context.timing.start("debug_print:PruneHoistedContexts");
        let debug = react_compiler_reactive_scopes::print_reactive_function::debug_reactive_function_with_formatter(
            &reactive_fn, &env, Some(&hir_formatter),
        );
        context.log_debug(DebugLogEntry::new("PruneHoistedContexts", debug));
        context.timing.stop();
    }

    if env.config.enable_preserve_existing_memoization_guarantees
        || env.config.validate_preserve_existing_memoization_guarantees
    {
        context.timing.start("ValidatePreservedManualMemoization");
        react_compiler_validation::validate_preserved_manual_memoization(&reactive_fn, &mut env);
        if context.debug_enabled {
            context.log_debug(DebugLogEntry::new(
                "ValidatePreservedManualMemoization",
                "ok".to_string(),
            ));
        }
        context.timing.stop();
    }

    // Native oxc codegen runs after the input semantic borrow ends (see
    // native_codegen.rs / codegen_assembly.rs), so we move the reactive function
    // + unique identifiers into a `NativeArtifact` below. They are the only
    // codegen inputs; `fbt_operands` is unused by the native path.
    let _ = fbt_operands;
    let native_reactive_fn = reactive_fn;
    let native_unique_identifiers = unique_identifiers;

    // Memoization stats for the `CompileSuccess` logger event. The four
    // block/value counts come from a structural walk of the reactive function
    // (`count_memo_blocks`); `memo_slots_used` is the cache-slot count produced
    // by the native oxc codegen — we run it here (against a throwaway allocator,
    // discarding the generated function) so the count matches the emitted code
    // exactly. `env` is read-only during codegen.
    context.timing.start("codegen");
    let (memo_blocks, memo_values, pruned_memo_blocks, pruned_memo_values) =
        react_compiler_reactive_scopes::count_memo_blocks::count_memo_blocks(
            &native_reactive_fn,
            &env,
        );
    let memo_slots_used = {
        let allocator = oxc_allocator::Allocator::default();
        let builder = oxc_ast::AstBuilder::new(&allocator);
        react_compiler_reactive_scopes::codegen_oxc::codegen_oxc_function(
            &native_reactive_fn,
            &env,
            native_unique_identifiers.clone(),
            &builder,
            "_c",
        )
        .map(|out| out.memo_slots_used)
        .unwrap_or(0)
    };
    let stats = CompileFnStats {
        memo_slots_used,
        memo_blocks,
        memo_values,
        pruned_memo_blocks,
        pruned_memo_values,
    };
    context.timing.stop();

    // NOTE: we intentionally do NOT register the memo cache import here.
    // The import is registered during native codegen assembly only for functions
    // that are actually applied to the output. Registering it here would cause
    // a spurious `import { c as _c }` when a function compiles with memo slots
    // but is later discarded (e.g., due to "use no memo" opt-out or errors),
    // while other functions in the same file compile to 0 memo slots.

    let _ = func;

    // Simulate unexpected exception for testing (matches TS Pipeline.ts)
    if env.config.throw_unknown_exception_testonly {
        let mut err = CompilerError::new();
        err.push_error_detail(react_compiler_diagnostics::CompilerErrorDetail {
            category: react_compiler_diagnostics::ErrorCategory::Invariant,
            reason: "unexpected error".to_string(),
            description: None,
            loc: None,
            suggestions: None,
        });
        return Err(err);
    }

    // Check for accumulated errors at the end of the pipeline
    // (matches TS Pipeline.ts: env.hasErrors() → Err at the end)
    if env.has_errors() {
        // Merge UIDs even on error: in TS, Babel's scope.generateUid() permanently
        // registers names in the scope's `uids` map regardless of whether the function
        // compilation succeeds or fails. Without this merge, failed compilations would
        // "leak" _temp names that subsequent successful compilations wouldn't see,
        // causing numbering mismatches vs TS.
        if let Some(uid_names) = env.take_uid_known_names() {
            context.merge_uid_known_names(&uid_names);
        }
        return Err(env.take_errors());
    }

    if let Some(uid_names) = env.take_uid_known_names() {
        context.merge_uid_known_names(&uid_names);
    }

    // N2.1: emit outlined functions for native oxc codegen.
    //
    // `outline_functions` records each extracted closure as a lowered HIR
    // FunctionExpression on `env` (via `env.outline_function`, depth-first so
    // transitively-nested closures are already flattened into the list). These
    // carry a `null` React type, which TS never re-queues through the full
    // pipeline; instead the parent's reactive-scope codegen emits each via a
    // SHORT sequence (build_reactive_function → prune_unused_labels →
    // prune_unused_lvalues → prune_hoisted_contexts → rename_variables). We
    // mirror that: for each outlined HIR function, create a child env (cloning
    // the parent's arenas so the outlined HIR's id references stay valid), build
    // its reactive function, and push a NativeArtifact with a sentinel span of
    // (0, 0). Assembly appends these as top-level `function <name>() {...}`
    // declarations rather than splicing by source span (they have no location).
    //
    // A worklist handles the (rare) case of an outlined entry surfacing further
    // outlined entries on its child env; in practice the depth-first outlining
    // above already flattens them, so the queue typically drains in one pass.
    //
    // `env` is moved into the main artifact below, so the outlined entries (and
    // any child envs we need) must be taken/created BEFORE that move.
    let mut outlined_queue: Vec<react_compiler_hir::environment::OutlinedFunctionEntry> =
        env.take_outlined_functions();
    while let Some(entry) = outlined_queue.pop() {
        let react_compiler_hir::environment::OutlinedFunctionEntry { func, fn_type } = entry;
        let resolved_type = fn_type.unwrap_or(ReactFunctionType::Other);
        let mut child_env = env.for_outlined_fn(resolved_type);
        match build_outlined_reactive_fn(&func, &mut child_env, context) {
            Ok((reactive_fn, unique_identifiers)) => {
                // Drain any further outlined functions surfaced on the child env.
                outlined_queue.extend(child_env.take_outlined_functions());
                if let Some(uid_names) = child_env.take_uid_known_names() {
                    context.merge_uid_known_names(&uid_names);
                }
                context.native_artifacts.push(native::NativeArtifact {
                    reactive_fn,
                    env: child_env,
                    unique_identifiers,
                    // Sentinel span: assembly appends this as a top-level
                    // function declaration rather than splicing by source span.
                    fn_span: (0, 0),
                    fn_type: resolved_type,
                    // Outlined functions are emitted as function declarations.
                    is_arrow: false,
                    // The generated name (e.g. `_temp`) is carried on the
                    // reactive function's `id`, so codegen names the declaration.
                    fn_name: None,
                });
            }
            Err(_err) => {
                // If an outlined function fails to build, skip it (matches the
                // prior compile_outlined_fn Err→skip behavior). Drop the child
                // env without emitting an artifact.
            }
        }
    }

    // N2.1: record the native codegen artifact only on full success, after all
    // error checks above. We move `env` in here; native codegen re-runs
    // cache-slot allocation independently against the snapshotted reactive
    // function.
    context.native_artifacts.push(native::NativeArtifact {
        reactive_fn: native_reactive_fn,
        env,
        unique_identifiers: native_unique_identifiers,
        fn_span: (native_fn_span.0, native_fn_span.1),
        fn_type,
        is_arrow: native_is_arrow,
        fn_name: fn_name.map(|s| s.to_string()),
    });

    Ok(stats)
}

/// Build the `ReactiveFunction` + reserved unique identifiers for an outlined
/// function, ready for native oxc codegen.
///
/// Outlined functions are NOT re-run through the full pipeline. They are stored
/// by `outline_functions` (as the lowered inner FunctionExpression HIR, already
/// SSA'd / effect-analyzed in the parent's pass run) with a `null` React type,
/// which TS never re-queues. Instead, the parent's reactive-scope codegen
/// processes them with a SHORT sequence — `buildReactiveFunction`,
/// `pruneUnusedLabels`, `pruneUnusedLValues`, `pruneHoistedContexts`,
/// `renameVariables` — and emits them directly (see
/// `CodegenReactiveFunction.ts` `codegenFunction`, and the equivalent Rust
/// reference path in `codegen_reactive_function.rs`). Running the full pipeline
/// here would re-run validations (e.g. global-mutation checks) that are not
/// meant to apply to outlined functions.
///
/// This mirrors that short sequence and stops before `codegen_reactive_function`,
/// returning the reactive function + unique identifiers for native codegen to
/// build oxc AST later, after the input semantic borrow ends.
fn build_outlined_reactive_fn(
    hir: &react_compiler_hir::HirFunction,
    env: &mut Environment,
    context: &mut ProgramContext,
) -> Result<
    (
        react_compiler_hir::reactive::ReactiveFunction,
        std::collections::HashSet<String>,
    ),
    CompilerError,
> {
    let mut reactive_fn = react_compiler_reactive_scopes::build_reactive_function(hir, env)?;
    react_compiler_reactive_scopes::prune_unused_labels(&mut reactive_fn, env)?;
    react_compiler_reactive_scopes::prune_unused_lvalues(&mut reactive_fn, env);
    react_compiler_reactive_scopes::prune_hoisted_contexts(&mut reactive_fn, env)?;

    let unique_identifiers =
        react_compiler_reactive_scopes::rename_variables(&mut reactive_fn, env);
    for name in &unique_identifiers {
        context.add_new_reference(name.clone());
    }

    Ok((reactive_fn, unique_identifiers))
}

/// Log CompilerError diagnostics as CompileError events, matching TS `env.logErrors()` behavior.
/// These are logged for telemetry/lint output but not accumulated as compile errors.
fn log_errors_as_events(errors: &CompilerError, context: &mut ProgramContext) {
    // Use the source_filename from the AST (set by parser's sourceFilename option).
    // This is stored on the Environment during lowering.
    let source_filename = context.source_filename();
    for detail in &errors.details {
        let detail_info = match detail {
            react_compiler_diagnostics::CompilerErrorOrDiagnostic::Diagnostic(d) => {
                let items: Option<Vec<CompilerErrorItemInfo>> = {
                    let v: Vec<CompilerErrorItemInfo> = d
                        .details
                        .iter()
                        .map(|item| match item {
                            react_compiler_diagnostics::CompilerDiagnosticDetail::Error {
                                loc,
                                message,
                                identifier_name,
                            } => CompilerErrorItemInfo {
                                kind: "error".to_string(),
                                loc: loc.as_ref().map(|l| LoggerSourceLocation {
                                    start: LoggerPosition {
                                        line: l.start.line,
                                        column: l.start.column,
                                        index: l.start.index,
                                    },
                                    end: LoggerPosition {
                                        line: l.end.line,
                                        column: l.end.column,
                                        index: l.end.index,
                                    },
                                    filename: source_filename.clone(),
                                    identifier_name: identifier_name.clone(),
                                }),
                                message: message.clone(),
                            },
                            react_compiler_diagnostics::CompilerDiagnosticDetail::Hint {
                                message,
                            } => CompilerErrorItemInfo {
                                kind: "hint".to_string(),
                                loc: None,
                                message: Some(message.clone()),
                            },
                        })
                        .collect();
                    if v.is_empty() { None } else { Some(v) }
                };
                CompilerErrorDetailInfo {
                    category: format!("{:?}", d.category),
                    reason: d.reason.clone(),
                    description: d.description.clone(),
                    severity: format!("{:?}", d.logged_severity()),
                    suggestions: None,
                    details: items,
                    loc: None,
                }
            }
            react_compiler_diagnostics::CompilerErrorOrDiagnostic::ErrorDetail(d) => {
                CompilerErrorDetailInfo {
                    category: format!("{:?}", d.category),
                    reason: d.reason.clone(),
                    description: d.description.clone(),
                    severity: format!("{:?}", d.logged_severity()),
                    suggestions: None,
                    details: None,
                    loc: None,
                }
            }
        };
        context.log_event(super::compile_result::LoggerEvent::CompileError {
            fn_loc: None,
            detail: detail_info,
        });
    }
}
