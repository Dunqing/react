// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the root directory of this source tree.

//! Native oxc codegen (stage N2.1).
//!
//! Builds `oxc_ast` directly from a `ReactiveFunction`, fusing two existing
//! references:
//!
//! - `codegen_reactive_function.rs` — the LOGIC (memo cache `_c(n)` emission,
//!   reactive-scope `if ($[i] !== dep) {…} else {…}` wrapping, return handling).
//!
//! This builds oxc AST nodes directly in a single pass.
//!
//! Scope: this is a vertical slice. The CORE constructs needed by simple
//! memoizing components are implemented; anything else returns [`CodegenBail`]
//! so the caller leaves the original function uncompiled (graceful bail).
//!
//! oxc `Expression<'a>` is arena-allocated and NOT `Clone`, so the inline
//! "temporaries" table from the reference (which stored built expressions) is
//! replaced here by a table of the HIR `ReactiveValue` (which IS `Clone`):
//! a `Place` that references an inlined temporary re-builds its expression.

use std::collections::HashMap;
use std::collections::HashSet;

use oxc_allocator::Box as ArenaBox;
use oxc_allocator::FromIn;
use oxc_allocator::Vec as ArenaVec;
use oxc_ast::AstBuilder;
use oxc_ast::ast as oxc;
use oxc_ast::ast::Str;
use oxc_span::SPAN;
use oxc_syntax::operator::BinaryOperator as OxcBinOp;
use oxc_syntax::operator::LogicalOperator as OxcLogOp;
use oxc_syntax::operator::UnaryOperator as OxcUnOp;
use oxc_syntax::operator::UpdateOperator as OxcUpOp;

use react_compiler_hir::ArrayElement;
use react_compiler_hir::ArrayPatternElement;
use react_compiler_hir::BinaryOperator;
use react_compiler_hir::DeclarationId;
use react_compiler_hir::FunctionExpressionType;
use react_compiler_hir::IdentifierId;
use react_compiler_hir::InstructionKind;
use react_compiler_hir::InstructionValue;
use react_compiler_hir::JsxAttribute;
use react_compiler_hir::JsxTag;
use react_compiler_hir::LValuePattern;
use react_compiler_hir::LogicalOperator;
use react_compiler_hir::ObjectPropertyKey;
use react_compiler_hir::ObjectPropertyOrSpread;
use react_compiler_hir::ParamPattern;
use react_compiler_hir::Pattern;
use react_compiler_hir::Place;
use react_compiler_hir::PlaceOrSpread;
use react_compiler_hir::PrimitiveValue;
use react_compiler_hir::PropertyLiteral;
use react_compiler_hir::ScopeId;
use react_compiler_hir::TemplateQuasi;
use react_compiler_hir::UnaryOperator;
use react_compiler_hir::UpdateOperator;
use react_compiler_hir::environment::Environment;
use react_compiler_hir::reactive::ReactiveBlock;
use react_compiler_hir::reactive::ReactiveFunction;
use react_compiler_hir::reactive::ReactiveScopeBlock;
use react_compiler_hir::reactive::ReactiveStatement;
use react_compiler_hir::reactive::ReactiveTerminal;
use react_compiler_hir::reactive::ReactiveTerminalTargetKind;
use react_compiler_hir::reactive::ReactiveValue;

/// Sentinel from the reference codegen; emitted as `Symbol.for("…")`.
const MEMO_CACHE_SENTINEL: &str = "react.memo_cache_sentinel";

/// Sentinel marking "no early return taken"; emitted as `Symbol.for("…")` in
/// the early-return guard appended after a reactive scope.
const EARLY_RETURN_SENTINEL: &str = "react.early_return_sentinel";

/// Signals that codegen could not emit a function.
///
/// `invariant: false` is the common case — an unsupported construct the native
/// codegen does not yet handle; the caller leaves the function uncompiled (a
/// graceful per-function fallback to the original source).
///
/// `invariant: true` mirrors the reference codegen's `CompilerError::invariant`
/// — a genuinely invalid IR state that the TS compiler also rejects with a
/// fatal error. The pipeline records this as a compiler error so the function
/// errors out (matching TS) rather than silently emitting different output.
#[derive(Debug)]
pub struct CodegenBail {
    pub reason: String,
    pub invariant: bool,
}

impl CodegenBail {
    fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            invariant: false,
        }
    }

    fn invariant(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            invariant: true,
        }
    }
}

type Bail<T> = Result<T, CodegenBail>;

macro_rules! bail {
    ($($arg:tt)*) => {
        return Err(CodegenBail::new(format!($($arg)*)))
    };
}

/// Like [`bail!`] but marks the failure as a hard invariant (see [`CodegenBail`]).
macro_rules! invariant_bail {
    ($($arg:tt)*) => {
        return Err(CodegenBail::invariant(format!($($arg)*)))
    };
}

/// Output of native codegen for one function.
pub struct OxcCodegenOutput<'a> {
    /// The compiled function node (FunctionDeclaration/FunctionExpression form
    /// is decided by the caller via `is_arrow` + name).
    pub function: oxc::Function<'a>,
    /// Number of memo cache slots used (sizes the `_c(N)` call).
    pub memo_slots_used: u32,
}

/// Codegen a `ReactiveFunction` into an oxc `Function<'a>`.
///
/// `memo_local_name` is the local binding name for the runtime cache import
/// (e.g. `_c`), used as the callee of the `const $ = _c(N)` preface.
pub fn codegen_oxc_function<'a>(
    func: &ReactiveFunction,
    env: &Environment,
    unique_identifiers: &HashSet<String>,
    builder: &AstBuilder<'a>,
    memo_local_name: &str,
) -> Bail<OxcCodegenOutput<'a>> {
    let mut cx = Cx {
        b: *builder,
        env,
        next_cache_index: 0,
        cache_name: synthesize_name("$", unique_identifiers),
        memo_local_name: Str::from_in(memo_local_name, builder.allocator),
        temp: HashMap::new(),
        declared: HashSet::new(),
        object_methods: HashMap::new(),
    };

    let (function, cache_count) = cx.codegen_function(func)?;

    Ok(OxcCodegenOutput {
        function,
        memo_slots_used: cache_count,
    })
}

/// Codegen context. Holds the arena builder, a read-only env, the cache-slot
/// counter, and the inline-temporary table (keyed by `DeclarationId`).
struct Cx<'a, 'e> {
    b: AstBuilder<'a>,
    env: &'e Environment,
    next_cache_index: u32,
    cache_name: String,
    /// Pre-interned local binding name for the runtime cache import (e.g. `_c`),
    /// used as the callee of the `const $ = _c(N)` preface. Interned once at
    /// construction so the `_c(N)` emission does not re-intern it.
    memo_local_name: Str<'a>,
    /// declaration_id -> HIR value to inline at use sites (None = bare ident).
    ///
    /// Stores the full `ReactiveValue` (which IS `Clone`), so that compound
    /// reactive values (sequence/conditional/logical/optional expressions) bound
    /// to unnamed temporaries can be inlined too, not just bare
    /// `InstructionValue`s. Mirrors the reference codegen's `temporaries` table,
    /// which stashes the already-built expression. (oxc `Expression<'a>` isn't
    /// `Clone`, so we stash the HIR and re-build at each use site.)
    temp: HashMap<DeclarationId, Option<ReactiveValue>>,
    /// declaration_ids that have been `let`/`const`/param declared.
    declared: HashSet<DeclarationId>,
    /// `ObjectMethod` instructions stashed by their lvalue `IdentifierId`, to be
    /// retrieved when the enclosing `ObjectExpression` emits its `Method`-typed
    /// properties. Mirrors the reference codegen's `cx.object_methods`.
    object_methods: HashMap<IdentifierId, react_compiler_hir::LoweredFunction>,
}

impl<'a, 'e> Cx<'a, 'e> {
    fn atom(&self, s: &str) -> Str<'a> {
        Str::from_in(s, self.b.allocator)
    }

    fn alloc_cache_index(&mut self) -> u32 {
        let i = self.next_cache_index;
        self.next_cache_index += 1;
        i
    }

    fn decl_id(&self, place: &Place) -> DeclarationId {
        self.env.identifiers[place.identifier.0 as usize].declaration_id
    }

    /// Resolve a Place to its source name, using the declaration-id fallback for
    /// unnamed SSA temporaries (mirrors `Environment::identifier_name_for_id`).
    fn place_name(&self, place: &Place) -> Bail<String> {
        match self.env.identifier_name_for_id(place.identifier) {
            Some(name) => Ok(name),
            None => bail!("unnamed identifier with no declaration name"),
        }
    }

    fn ident_name(&self, id: IdentifierId) -> Bail<String> {
        match self.env.identifier_name_for_id(id) {
            Some(name) => Ok(name),
            None => bail!("unnamed identifier with no declaration name"),
        }
    }

    // ===== shells / helpers =====

    fn ident_expr(&self, name: &str) -> oxc::Expression<'a> {
        self.b.expression_identifier(SPAN, self.atom(name))
    }

    fn binding_pattern(&self, name: &str) -> oxc::BindingPattern<'a> {
        self.b
            .binding_pattern_binding_identifier(SPAN, self.atom(name))
    }

    fn formal_param(&self, pattern: oxc::BindingPattern<'a>) -> oxc::FormalParameter<'a> {
        self.b.formal_parameter(
            SPAN,
            self.b.vec(),
            pattern,
            None::<ArenaBox<'a, oxc::TSTypeAnnotation<'a>>>,
            None::<ArenaBox<'a, oxc::Expression<'a>>>,
            false,
            None,
            false,
            false,
        )
    }

    /// `$[index]` computed member expression.
    fn cache_slot(&self, index: u32) -> oxc::Expression<'a> {
        let object = self.ident_expr(&self.cache_name);
        let property =
            self.b
                .expression_numeric_literal(SPAN, index as f64, None, oxc::NumberBase::Decimal);
        oxc::Expression::ComputedMemberExpression(
            self.b.alloc(
                self.b
                    .computed_member_expression(SPAN, object, property, false),
            ),
        )
    }

    /// `$[index]` as an assignment target.
    fn cache_slot_target(&self, index: u32) -> oxc::AssignmentTarget<'a> {
        let object = self.ident_expr(&self.cache_name);
        let property =
            self.b
                .expression_numeric_literal(SPAN, index as f64, None, oxc::NumberBase::Decimal);
        oxc::AssignmentTarget::ComputedMemberExpression(
            self.b.alloc(
                self.b
                    .computed_member_expression(SPAN, object, property, false),
            ),
        )
    }

    /// `const $ = _c(N);`
    fn cache_var_decl(&self, count: u32) -> oxc::Statement<'a> {
        let callee = self.b.expression_identifier(SPAN, self.memo_local_name);
        let arg =
            self.b
                .expression_numeric_literal(SPAN, count as f64, None, oxc::NumberBase::Decimal);
        let mut args = self.b.vec();
        args.push(oxc::Argument::from(arg));
        let call = self.b.expression_call(
            SPAN,
            callee,
            None::<ArenaBox<'a, oxc::TSTypeParameterInstantiation<'a>>>,
            args,
            false,
        );
        let pat = self.binding_pattern(&self.cache_name);
        let declarator = self.b.variable_declarator(
            SPAN,
            oxc::VariableDeclarationKind::Const,
            pat,
            None::<ArenaBox<'a, oxc::TSTypeAnnotation<'a>>>,
            Some(call),
            false,
        );
        let mut decls = self.b.vec();
        decls.push(declarator);
        let decl =
            self.b
                .variable_declaration(SPAN, oxc::VariableDeclarationKind::Const, decls, false);
        oxc::Statement::VariableDeclaration(self.b.alloc(decl))
    }

    /// Codegen a full `oxc::Function` from a `ReactiveFunction`, returning the
    /// function and the number of memo cache slots it used. Nested functions
    /// call this recursively with a saved/restored cache counter.
    fn codegen_function(&mut self, func: &ReactiveFunction) -> Bail<(oxc::Function<'a>, u32)> {
        // Save and reset the cache counter so the function gets its own
        // `const $ = _c(N)` numbering independent of any enclosing function.
        let saved_cache_index = self.next_cache_index;
        self.next_cache_index = 0;

        // Params: each param is registered (declared) so later writes reassign
        // rather than redeclare. Params are never inlined temporaries. A spread
        // param becomes a rest element (`...rest`), which lives in a dedicated
        // slot on `FormalParameters` rather than the `items` list.
        let mut params = self.b.vec();
        let mut rest: Option<oxc::BindingPattern<'a>> = None;
        for p in &func.params {
            match p {
                ParamPattern::Place(place) => {
                    let name = self.place_name(place)?;
                    self.declared.insert(self.decl_id(place));
                    let pat = self.binding_pattern(&name);
                    params.push(self.formal_param(pat));
                }
                ParamPattern::Spread(spread) => {
                    let name = self.place_name(&spread.place)?;
                    self.declared.insert(self.decl_id(&spread.place));
                    rest = Some(self.binding_pattern(&name));
                }
            }
        }

        // Body.
        let mut body_stmts = self.b.vec();
        self.codegen_block(&func.body, &mut body_stmts)?;

        // Strip a trailing bare `return undefined;` (matches the reference).
        if let Some(oxc::Statement::ReturnStatement(r)) = body_stmts.last()
            && r.argument.is_none()
        {
            body_stmts.pop();
        }

        // Cache var preface: const $ = _c(N);
        let cache_count = self.next_cache_index;
        if cache_count != 0 {
            let preface = self.cache_var_decl(cache_count);
            body_stmts.insert(0, preface);
        }

        let function = self.build_function_shell(func, params, rest, body_stmts)?;
        self.next_cache_index = saved_cache_index;
        Ok((function, cache_count))
    }

    fn build_function_shell(
        &self,
        func: &ReactiveFunction,
        params: ArenaVec<'a, oxc::FormalParameter<'a>>,
        rest: Option<oxc::BindingPattern<'a>>,
        body_stmts: ArenaVec<'a, oxc::Statement<'a>>,
    ) -> Bail<oxc::Function<'a>> {
        if func.generator {
            bail!("generator function not yet supported");
        }
        let id = func
            .id
            .as_ref()
            .map(|name| self.b.binding_identifier(SPAN, self.atom(name)));
        let rest = rest.map(|pat| {
            let rest_elem = self.b.binding_rest_element(SPAN, pat);
            self.b.alloc(self.b.formal_parameter_rest(
                SPAN,
                self.b.vec(),
                rest_elem,
                None::<ArenaBox<'a, oxc::TSTypeAnnotation<'a>>>,
            ))
        });
        let formal_params = self.b.formal_parameters(
            SPAN,
            oxc::FormalParameterKind::FormalParameter,
            params,
            rest,
        );
        let body = self.b.function_body(SPAN, self.b.vec(), body_stmts);
        Ok(self.b.function(
            SPAN,
            oxc::FunctionType::FunctionDeclaration,
            id,
            false,
            func.is_async,
            false,
            None::<ArenaBox<'a, oxc::TSTypeParameterDeclaration<'a>>>,
            None::<ArenaBox<'a, oxc::TSThisParameter<'a>>>,
            formal_params,
            None::<ArenaBox<'a, oxc::TSTypeAnnotation<'a>>>,
            Some(body),
        ))
    }

    // ===== block / statement walk =====

    fn codegen_block(
        &mut self,
        block: &ReactiveBlock,
        out: &mut ArenaVec<'a, oxc::Statement<'a>>,
    ) -> Bail<()> {
        for stmt in block {
            match stmt {
                ReactiveStatement::Instruction(instr) => {
                    self.codegen_instruction(instr, out)?;
                }
                ReactiveStatement::Scope(scope_block) => {
                    self.codegen_reactive_scope(scope_block, out)?;
                }
                ReactiveStatement::PrunedScope(pruned) => {
                    // Pruned scopes emit no memo wrapper — flatten inline.
                    self.codegen_block(&pruned.instructions, out)?;
                }
                ReactiveStatement::Terminal(term) => {
                    // Emit the terminal into a scratch buffer so we can apply a
                    // label wrapper (if the statement carries a non-implicit
                    // label) and flatten implicit labels / bare blocks inline.
                    match &term.label {
                        Some(label) if !label.implicit => {
                            let mut scratch = self.b.vec();
                            self.codegen_terminal(&term.terminal, &mut scratch)?;
                            // The label wraps a single statement; if the terminal
                            // produced exactly one block, unwrap to it.
                            let inner = if scratch.len() == 1 {
                                scratch.pop().unwrap()
                            } else {
                                self.b.statement_block(SPAN, scratch)
                            };
                            let label_id = self
                                .b
                                .label_identifier(SPAN, self.atom(&codegen_label(label.id)));
                            out.push(self.b.statement_labeled(SPAN, label_id, inner));
                        }
                        _ => {
                            self.codegen_terminal(&term.terminal, out)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Stash a Reassign store/destructure under its outer lvalue so the use
    /// site rebuilds it as an inline expression (e.g. `f((x = makeObject()))`).
    fn stash_inlined(&mut self, outer: &Place, value: &ReactiveValue) {
        let decl_id = self.decl_id(outer);
        self.declared.insert(decl_id);
        self.temp.insert(decl_id, Some(value.clone()));
    }

    fn codegen_instruction(
        &mut self,
        instr: &react_compiler_hir::reactive::ReactiveInstruction,
        out: &mut ArenaVec<'a, oxc::Statement<'a>>,
    ) -> Bail<()> {
        // Statement-level dispatch for stores/declares (mirrors
        // codegen_instruction_nullable).
        if let ReactiveValue::Instruction(iv) = &instr.value {
            match iv {
                // A `StoreLocal` Reassign that is *also* referenced as an
                // expression (the enclosing instruction has an outer lvalue)
                // must be inlined at its use site, not emitted as a standalone
                // statement — e.g. `f((x = makeObject()))` or `x = y = 1`.
                // Stash the HIR value so the use site rebuilds `x = …` via the
                // StoreLocal expression arm. `StoreContext` is excluded (it is
                // codegen'd as a statement even with an outer lvalue). Mirrors
                // the reference `emit_store` Reassign branch.
                InstructionValue::StoreLocal { lvalue, .. }
                    if matches!(lvalue.kind, InstructionKind::Reassign)
                        && instr.lvalue.is_some() =>
                {
                    self.stash_inlined(instr.lvalue.as_ref().unwrap(), &instr.value);
                    return Ok(());
                }
                // A `StoreContext` Reassign that is *also* referenced as an
                // expression (the enclosing instruction has an outer lvalue that
                // is an unnamed temporary) must be inlined at its use site, e.g.
                // a chained `y = (x = {})` where `x` is a context variable.
                // Stash the HIR value so the use site rebuilds `x = …` via the
                // StoreContext expression arm. Mirrors the reference codegen,
                // which for StoreContext re-dispatches to `codegenInstruction`,
                // and that stashes when the outer lvalue is an unnamed temp.
                InstructionValue::StoreContext { lvalue, .. }
                    if matches!(lvalue.kind, InstructionKind::Reassign)
                        && instr.lvalue.as_ref().is_some_and(|lv| {
                            self.env.identifiers[lv.identifier.0 as usize]
                                .name
                                .is_none()
                        }) =>
                {
                    self.stash_inlined(instr.lvalue.as_ref().unwrap(), &instr.value);
                    return Ok(());
                }
                // Invariant (mirrors the reference codegen's `emit_store`): a
                // `Const`/`Let` declaration that is *also* referenced as an
                // expression (the enclosing instruction has an outer lvalue) is
                // an invalid IR state the TS compiler rejects with a fatal error.
                InstructionValue::StoreLocal { lvalue, .. }
                | InstructionValue::StoreContext { lvalue, .. }
                    if matches!(lvalue.kind, InstructionKind::Const | InstructionKind::Let)
                        && instr.lvalue.is_some() =>
                {
                    invariant_bail!("Const declaration cannot be referenced as an expression");
                }
                // A `StoreContext` Reassign referenced as an expression by a
                // *named*/promoted outer temp: TS re-dispatches to
                // `codegenInstruction`, which (because the outer identifier is
                // named) emits `const <name> = (x = …)` rather than inlining
                // (only unnamed outer temps are stashed/inlined, handled above).
                // Skip the statement-level store path and fall through to the
                // generic expression path below, which builds the `const <name>
                // = …` declaration around the `(x = …)` assignment expression
                // produced by `codegen_value`.
                InstructionValue::StoreContext { lvalue, .. }
                    if matches!(lvalue.kind, InstructionKind::Reassign)
                        && instr.lvalue.is_some() => {}
                InstructionValue::StoreLocal { lvalue, value, .. }
                | InstructionValue::StoreContext { lvalue, value, .. } => {
                    return self.codegen_store(lvalue, value, out);
                }
                // A `Destructure` Reassign that is *also* referenced as an
                // expression (the enclosing instruction has an outer lvalue)
                // must be inlined at its use site, e.g. `f(([x] = makeObject()))`.
                // Stash the HIR value so the use site rebuilds `[x] = …` via the
                // Destructure expression arm. Mirrors the reference codegen's
                // `Reassign` branch (`cx.temp.set(...)` when `instr.lvalue !==
                // null` and kind !== StoreContext).
                InstructionValue::Destructure { lvalue, .. }
                    if matches!(lvalue.kind, InstructionKind::Reassign)
                        && instr.lvalue.is_some() =>
                {
                    self.stash_inlined(instr.lvalue.as_ref().unwrap(), &instr.value);
                    return Ok(());
                }
                // A `Const`/`Let` destructure that is *also* referenced as an
                // expression (the enclosing instruction has an outer lvalue) is
                // an invalid IR state the TS compiler rejects with a fatal error
                // (same `Const`/`Let` invariant as the StoreLocal/StoreContext
                // arm above; kept separate because Destructure's `lvalue` is an
                // `LValuePattern`, not an `LValue`). This arises from nested
                // destructuring-assignment-as-expression, e.g. `f(([[x]] = obj()))`,
                // where the inner level lowers to a Const destructure temp.
                InstructionValue::Destructure { lvalue, .. }
                    if matches!(lvalue.kind, InstructionKind::Const | InstructionKind::Let)
                        && instr.lvalue.is_some() =>
                {
                    invariant_bail!("Const declaration cannot be referenced as an expression");
                }
                InstructionValue::Destructure { lvalue, value, .. } => {
                    return self.codegen_destructure(lvalue, value, out);
                }
                InstructionValue::DeclareLocal { lvalue, .. }
                | InstructionValue::DeclareContext { lvalue, .. } => {
                    // `let x;` with no initializer.
                    let name = self.place_name(&lvalue.place)?;
                    let decl_id = self.decl_id(&lvalue.place);
                    if !self.declared.contains(&decl_id) {
                        self.declared.insert(decl_id);
                        self.temp.insert(decl_id, None);
                        out.push(self.let_decl(&name, None));
                    }
                    return Ok(());
                }
                InstructionValue::StartMemoize { .. } | InstructionValue::FinishMemoize { .. } => {
                    return Ok(()); // dropped
                }
                InstructionValue::Debugger { .. } => {
                    out.push(self.b.statement_debugger(SPAN));
                    return Ok(());
                }
                InstructionValue::ObjectMethod { lowered_func, .. } => {
                    // Stash for retrieval by the enclosing ObjectExpression's
                    // Method-typed property; emit no statement of its own.
                    if let Some(lvalue) = &instr.lvalue {
                        self.object_methods
                            .insert(lvalue.identifier, lowered_func.clone());
                    }
                    return Ok(());
                }
                // An UnsupportedNode that is itself a statement (e.g. a TS
                // `enum` declaration) is re-emitted verbatim. The lowering
                // carried the original source text in `original_node`; re-parse
                // it into an oxc statement and emit it directly, ignoring the
                // temp lvalue. Mirrors the reference `UnsupportedNode` codegen,
                // which returns a non-expression node as the statement itself.
                InstructionValue::UnsupportedNode {
                    node_type,
                    original_node,
                    ..
                } if node_type.as_deref() == Some("TSEnumDeclaration") => {
                    let src = original_node
                        .as_ref()
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| {
                            CodegenBail::new(
                                "UnsupportedNode TSEnumDeclaration missing source text",
                            )
                        })?;
                    let stmt = self.parse_statement(src)?;
                    out.push(stmt);
                    return Ok(());
                }
                _ => {}
            }
        }

        // Otherwise: an expression instruction. If the lvalue is an unnamed
        // temporary, stash the HIR value for inlining and emit nothing. If it
        // is named, emit a const declaration (or reassignment).
        let Some(lvalue) = &instr.lvalue else {
            // No lvalue -> expression statement.
            let expr = self.codegen_value(&instr.value)?;
            out.push(self.b.statement_expression(SPAN, expr));
            return Ok(());
        };

        let ident = &self.env.identifiers[lvalue.identifier.0 as usize];
        let decl_id = ident.declaration_id;
        if ident.name.is_none() {
            // Unnamed temporary -> inline at use sites. Stash the full
            // `ReactiveValue` (simple or compound) and re-build at each use.
            // Mirrors the reference codegen's `cx.temp.insert(decl, value)`.
            self.temp.insert(decl_id, Some(instr.value.clone()));
            return Ok(());
        }

        // Named lvalue.
        let name = self.place_name(lvalue)?;
        let expr = self.codegen_value(&instr.value)?;
        if self.declared.contains(&decl_id) {
            // Reassignment: `name = expr;`
            out.push(self.assign_ident_stmt(&name, expr));
        } else {
            self.declared.insert(decl_id);
            out.push(self.const_decl(&name, Some(expr)));
        }
        Ok(())
    }

    fn codegen_store(
        &mut self,
        lvalue: &react_compiler_hir::LValue,
        value: &Place,
        out: &mut ArenaVec<'a, oxc::Statement<'a>>,
    ) -> Bail<()> {
        let name = self.place_name(&lvalue.place)?;
        let decl_id = self.decl_id(&lvalue.place);
        let value_expr = self.place_expr(value)?;
        match lvalue.kind {
            InstructionKind::Const | InstructionKind::HoistedConst => {
                self.declared.insert(decl_id);
                out.push(self.const_decl(&name, Some(value_expr)));
            }
            InstructionKind::Let | InstructionKind::HoistedLet => {
                self.declared.insert(decl_id);
                out.push(self.let_decl(&name, Some(value_expr)));
            }
            InstructionKind::Reassign => {
                out.push(self.assign_ident_stmt(&name, value_expr));
            }
            InstructionKind::Function | InstructionKind::HoistedFunction => {
                // The function value is already lowered to a FunctionExpression;
                // a function-kind store emits a hoisted `function name(…) {…}`
                // declaration rather than a `let`/assignment. Mirrors the
                // reference `InstructionKind.Function` arm in
                // CodegenReactiveFunction.ts.
                self.declared.insert(decl_id);
                out.push(self.fn_decl_from_expr(&name, value_expr)?);
            }
            InstructionKind::Catch => bail!("catch-kind store not yet supported"),
        }
        Ok(())
    }

    /// Convert an already-lowered function-expression value into a hoisted
    /// `FunctionDeclaration` statement named `name`. Mirrors the reference
    /// `createFunctionDeclaration` call in the `InstructionKind.Function` arm
    /// (which reuses the function value's params/body/generator/async and only
    /// changes the statement form).
    fn fn_decl_from_expr(
        &self,
        name: &str,
        value_expr: oxc::Expression<'a>,
    ) -> Bail<oxc::Statement<'a>> {
        let oxc::Expression::FunctionExpression(func) = value_expr else {
            invariant_bail!("Expected a function as a function declaration value");
        };
        let mut function = func.unbox();
        function.r#type = oxc::FunctionType::FunctionDeclaration;
        function.id = Some(self.b.binding_identifier(SPAN, self.atom(name)));
        Ok(oxc::Statement::FunctionDeclaration(self.b.alloc(function)))
    }

    /// Codegen a `Destructure { lvalue: {pattern, kind}, value }`.
    /// Const/Let -> a variable declaration with a binding pattern.
    /// Reassign -> an assignment-expression statement (or stashed temporary).
    fn codegen_destructure(
        &mut self,
        lvalue: &LValuePattern,
        value: &Place,
        out: &mut ArenaVec<'a, oxc::Statement<'a>>,
    ) -> Bail<()> {
        // Register unnamed pattern operands as declared-but-no-expression so
        // they resolve to bare identifiers (matches the reference).
        if !matches!(lvalue.kind, InstructionKind::Reassign) {
            self.register_pattern_decls(&lvalue.pattern);
        }
        let rhs = self.place_expr(value)?;
        match lvalue.kind {
            InstructionKind::Const | InstructionKind::HoistedConst => {
                let pat = self.binding_pattern_from_pattern(&lvalue.pattern, lvalue.kind)?;
                out.push(self.var_decl_pattern(
                    oxc::VariableDeclarationKind::Const,
                    pat,
                    Some(rhs),
                ));
                Ok(())
            }
            InstructionKind::Let | InstructionKind::HoistedLet => {
                let pat = self.binding_pattern_from_pattern(&lvalue.pattern, lvalue.kind)?;
                out.push(self.var_decl_pattern(oxc::VariableDeclarationKind::Let, pat, Some(rhs)));
                Ok(())
            }
            InstructionKind::Reassign => {
                let target = self.assignment_target_from_pattern(&lvalue.pattern)?;
                let assign = self.b.expression_assignment(
                    SPAN,
                    oxc_syntax::operator::AssignmentOperator::Assign,
                    target,
                    rhs,
                );
                out.push(self.b.statement_expression(SPAN, assign));
                Ok(())
            }
            InstructionKind::Function | InstructionKind::HoistedFunction => {
                bail!("function-kind destructure not yet supported")
            }
            InstructionKind::Catch => bail!("catch-kind destructure not yet supported"),
        }
    }

    /// Register each unnamed operand of a pattern as a declared bare identifier.
    fn register_pattern_decls(&mut self, pattern: &Pattern) {
        for_each_pattern_place(pattern, |place| {
            let decl_id = self.decl_id(place);
            self.declared.insert(decl_id);
            if self.env.identifiers[place.identifier.0 as usize]
                .name
                .is_none()
            {
                self.temp.insert(decl_id, None);
            }
        });
    }

    /// Build a `VariableDeclaration` statement with a destructuring pattern.
    /// Build a single-declarator `VariableDeclaration` (`kind pat = init`).
    fn single_decl(
        &self,
        kind: oxc::VariableDeclarationKind,
        pat: oxc::BindingPattern<'a>,
        init: Option<oxc::Expression<'a>>,
    ) -> oxc::VariableDeclaration<'a> {
        let declarator = self.b.variable_declarator(
            SPAN,
            kind,
            pat,
            None::<ArenaBox<'a, oxc::TSTypeAnnotation<'a>>>,
            init,
            false,
        );
        let mut decls = self.b.vec();
        decls.push(declarator);
        self.b.variable_declaration(SPAN, kind, decls, false)
    }

    fn var_decl_pattern(
        &self,
        kind: oxc::VariableDeclarationKind,
        pat: oxc::BindingPattern<'a>,
        init: Option<oxc::Expression<'a>>,
    ) -> oxc::Statement<'a> {
        let decl = self.single_decl(kind, pat, init);
        oxc::Statement::VariableDeclaration(self.b.alloc(decl))
    }

    /// Build a binding pattern (for declarations) from an HIR `Pattern`.
    fn binding_pattern_from_pattern(
        &mut self,
        pattern: &Pattern,
        _kind: InstructionKind,
    ) -> Bail<oxc::BindingPattern<'a>> {
        match pattern {
            Pattern::Array(arr) => {
                let mut elements = self.b.vec();
                let mut rest: Option<oxc::BindingRestElement<'a>> = None;
                for item in &arr.items {
                    match item {
                        ArrayPatternElement::Place(p) => {
                            let name = self.place_name(p)?;
                            elements.push(Some(self.binding_pattern(&name)));
                        }
                        ArrayPatternElement::Hole => {
                            elements.push(None);
                        }
                        ArrayPatternElement::Spread(s) => {
                            let name = self.place_name(&s.place)?;
                            let arg = self.binding_pattern(&name);
                            rest = Some(self.b.binding_rest_element(SPAN, arg));
                        }
                    }
                }
                Ok(self.b.binding_pattern_array_pattern(SPAN, elements, rest))
            }
            Pattern::Object(obj) => {
                let mut properties = self.b.vec();
                let mut rest: Option<oxc::BindingRestElement<'a>> = None;
                for prop in &obj.properties {
                    match prop {
                        ObjectPropertyOrSpread::Property(p) => {
                            let (key, computed) = self.object_key(&p.key)?;
                            let value_name = self.place_name(&p.place)?;
                            let value = self.binding_pattern(&value_name);
                            let shorthand = object_key_matches_name(&p.key, &value_name);
                            properties.push(
                                self.b
                                    .binding_property(SPAN, key, value, shorthand, computed),
                            );
                        }
                        ObjectPropertyOrSpread::Spread(s) => {
                            let name = self.place_name(&s.place)?;
                            let arg = self.binding_pattern(&name);
                            rest = Some(self.b.binding_rest_element(SPAN, arg));
                        }
                    }
                }
                Ok(self
                    .b
                    .binding_pattern_object_pattern(SPAN, properties, rest))
            }
        }
    }

    /// Build an assignment target (for reassignments) from an HIR `Pattern`.
    fn assignment_target_from_pattern(
        &mut self,
        pattern: &Pattern,
    ) -> Bail<oxc::AssignmentTarget<'a>> {
        match pattern {
            Pattern::Array(arr) => {
                let mut elements = self.b.vec();
                let mut rest: Option<oxc::AssignmentTargetRest<'a>> = None;
                for item in &arr.items {
                    match item {
                        ArrayPatternElement::Place(p) => {
                            let name = self.place_name(p)?;
                            let t = oxc::AssignmentTargetMaybeDefault::from(
                                self.assignment_target_identifier(&name),
                            );
                            elements.push(Some(t));
                        }
                        ArrayPatternElement::Hole => elements.push(None),
                        ArrayPatternElement::Spread(s) => {
                            let name = self.place_name(&s.place)?;
                            let target = self.assignment_target_identifier(&name);
                            rest = Some(self.b.assignment_target_rest(SPAN, target));
                        }
                    }
                }
                Ok(self
                    .b
                    .assignment_target_pattern_array_assignment_target(SPAN, elements, rest)
                    .into())
            }
            Pattern::Object(obj) => {
                let mut properties = self.b.vec();
                let mut rest: Option<oxc::AssignmentTargetRest<'a>> = None;
                for prop in &obj.properties {
                    match prop {
                        ObjectPropertyOrSpread::Property(p) => {
                            let value_name = self.place_name(&p.place)?;
                            if object_key_matches_name(&p.key, &value_name) {
                                // Shorthand: { x } -> identifier property.
                                let binding =
                                    self.b.identifier_reference(SPAN, self.atom(&value_name));
                                properties.push(
                                    self.b
                                        .assignment_target_property_assignment_target_property_identifier(
                                            SPAN,
                                            binding,
                                            None,
                                        ),
                                );
                            } else {
                                let (key, computed) = self.object_key(&p.key)?;
                                let binding = oxc::AssignmentTargetMaybeDefault::from(
                                    self.assignment_target_identifier(&value_name),
                                );
                                properties.push(
                                    self.b
                                        .assignment_target_property_assignment_target_property_property(
                                            SPAN, key, binding, computed,
                                        ),
                                );
                            }
                        }
                        ObjectPropertyOrSpread::Spread(s) => {
                            let name = self.place_name(&s.place)?;
                            let target = self.assignment_target_identifier(&name);
                            rest = Some(self.b.assignment_target_rest(SPAN, target));
                        }
                    }
                }
                Ok(self
                    .b
                    .assignment_target_pattern_object_assignment_target(SPAN, properties, rest)
                    .into())
            }
        }
    }

    /// `name` as a simple assignment target.
    fn assignment_target_identifier(&self, name: &str) -> oxc::AssignmentTarget<'a> {
        oxc::AssignmentTarget::AssignmentTargetIdentifier(
            self.b
                .alloc(self.b.identifier_reference(SPAN, self.atom(name))),
        )
    }

    /// `name = expr` as an expression.
    fn assign_ident_expr(&self, name: &str, expr: oxc::Expression<'a>) -> oxc::Expression<'a> {
        self.b.expression_assignment(
            SPAN,
            oxc_syntax::operator::AssignmentOperator::Assign,
            self.assignment_target_identifier(name),
            expr,
        )
    }

    /// `name = expr;` as an expression statement.
    fn assign_ident_stmt(&self, name: &str, expr: oxc::Expression<'a>) -> oxc::Statement<'a> {
        let assign = self.assign_ident_expr(name, expr);
        self.b.statement_expression(SPAN, assign)
    }

    fn const_decl(&self, name: &str, init: Option<oxc::Expression<'a>>) -> oxc::Statement<'a> {
        self.var_decl(oxc::VariableDeclarationKind::Const, name, init)
    }

    fn let_decl(&self, name: &str, init: Option<oxc::Expression<'a>>) -> oxc::Statement<'a> {
        self.var_decl(oxc::VariableDeclarationKind::Let, name, init)
    }

    fn var_decl(
        &self,
        kind: oxc::VariableDeclarationKind,
        name: &str,
        init: Option<oxc::Expression<'a>>,
    ) -> oxc::Statement<'a> {
        let pat = self.binding_pattern(name);
        let decl = self.single_decl(kind, pat, init);
        oxc::Statement::VariableDeclaration(self.b.alloc(decl))
    }

    // ===== reactive scope (the memoization core) =====

    fn codegen_reactive_scope(
        &mut self,
        scope_block: &ReactiveScopeBlock,
        out: &mut ArenaVec<'a, oxc::Statement<'a>>,
    ) -> Bail<()> {
        let scope_id = scope_block.scope;
        // Clone only the fields actually consumed below, instead of cloning the
        // whole `ReactiveScope` (which would also copy `range`, `merged`, `loc`,
        // `id`) and then re-cloning `dependencies`/`declarations`. The owned
        // copies are needed because the loops below call `&mut self` methods,
        // which precludes holding the `&self` borrow returned by `self.scope`.
        let (deps, mut decls, reassignments, early_return_value) = {
            let scope = self.scope(scope_id)?;
            (
                scope.dependencies.clone(),
                scope.declarations.clone(),
                scope.reassignments.clone(),
                // Only the early-return value's `value` field is consumed below.
                scope.early_return_value.as_ref().map(|e| e.value),
            )
        };

        // --- Dependencies: one slot each (sorted for stable order). ---
        // Mirror the reference `compareScopeDependency`: sort by the dependency's
        // fully-qualified name, i.e. the `.`-joined `[name, ...path]` where each
        // optional path entry is prefixed with `?`, compared lexicographically as
        // a single string. The identifier name (not its numeric `IdentifierId`)
        // is the sort key, so names must be resolved up front (which can bail).
        let mut keyed_deps: Vec<(String, react_compiler_hir::ReactiveScopeDependency)> =
            Vec::with_capacity(deps.len());
        for dep in deps {
            let key = self.scope_dependency_sort_key(&dep)?;
            keyed_deps.push((key, dep));
        }
        keyed_deps.sort_by(|(a, _), (b, _)| a.cmp(b));
        let deps: Vec<react_compiler_hir::ReactiveScopeDependency> =
            keyed_deps.into_iter().map(|(_, dep)| dep).collect();

        let mut change_exprs: Vec<oxc::Expression<'a>> = Vec::new();
        // (slot index, dependency expression-builder inputs)
        let mut dep_stores: Vec<(u32, oxc::Expression<'a>)> = Vec::new();
        for dep in &deps {
            let index = self.alloc_cache_index();
            let dep_expr = self.dependency_expr(dep)?;
            // `$[index] !== dep`. Dependencies are referenced twice (in the
            // `!==` check and in the store); oxc Expressions aren't Clone, so we
            // rebuild from the HIR each time.
            let cmp = self.b.expression_binary(
                SPAN,
                self.cache_slot(index),
                OxcBinOp::StrictInequality,
                self.dependency_expr(dep)?,
            );
            change_exprs.push(cmp);
            dep_stores.push((index, dep_expr));
        }

        // --- Declarations + reassignments: one slot each (sorted). ---
        decls.sort_by(|a, b| a.0.0.cmp(&b.0.0));
        let mut first_output_index: Option<u32> = None;
        // (name, slot index)
        let mut outputs: Vec<(String, u32)> = Vec::new();
        for (id, _decl) in &decls {
            let index = self.alloc_cache_index();
            if first_output_index.is_none() {
                first_output_index = Some(index);
            }
            let name = self.ident_name(*id)?;
            let did = self.env.identifiers[id.0 as usize].declaration_id;
            if !self.declared.contains(&did) {
                self.declared.insert(did);
                self.temp.insert(did, None);
                out.push(self.let_decl(&name, None));
            }
            outputs.push((name, index));
        }
        for id in &reassignments {
            let index = self.alloc_cache_index();
            if first_output_index.is_none() {
                first_output_index = Some(index);
            }
            let name = self.ident_name(*id)?;
            outputs.push((name, index));
        }

        // --- Test condition. ---
        let test = if !change_exprs.is_empty() {
            // Fold `||` left-associatively.
            let mut iter = change_exprs.into_iter();
            let mut acc = iter.next().unwrap();
            for next in iter {
                acc = self.b.expression_logical(SPAN, acc, OxcLogOp::Or, next);
            }
            acc
        } else {
            // No deps -> `$[firstOutput] === Symbol.for("react.memo_cache_sentinel")`.
            let Some(first) = first_output_index else {
                bail!("reactive scope with no deps and no outputs");
            };
            let sentinel = self.symbol_for(MEMO_CACHE_SENTINEL);
            self.b.expression_binary(
                SPAN,
                self.cache_slot(first),
                OxcBinOp::StrictEquality,
                sentinel,
            )
        };

        // --- Recompute (consequent) block. ---
        let mut compute_stmts = self.b.vec();
        self.codegen_block(&scope_block.instructions, &mut compute_stmts)?;
        // Append dependency stores: `$[i] = dep;`
        for (index, dep_expr) in dep_stores {
            compute_stmts.push(self.assign_cache_slot(index, dep_expr));
        }
        // Append output stores: `$[i] = name;`
        for (name, index) in &outputs {
            let value = self.ident_expr(name);
            compute_stmts.push(self.assign_cache_slot(*index, value));
        }

        // --- Else block: `name = $[i];` for each output. ---
        let mut else_stmts = self.b.vec();
        for (name, index) in &outputs {
            else_stmts.push(self.assign_ident_stmt(name, self.cache_slot(*index)));
        }

        let consequent = self.b.statement_block(SPAN, compute_stmts);
        let alternate = if else_stmts.is_empty() {
            None
        } else {
            Some(self.b.statement_block(SPAN, else_stmts))
        };
        out.push(self.b.statement_if(SPAN, test, consequent, alternate));

        // --- Early return guard. ---
        // If this scope carries an early-return value, append:
        //   if (name !== Symbol.for("react.early_return_sentinel")) {
        //     return name;
        //   }
        // The early-return value identifier has been promoted to a named
        // variable by the time codegen runs. Mirrors the reference codegen.
        if let Some(early_return_value) = early_return_value {
            let name = self.ident_name(early_return_value).map_err(|_| {
                CodegenBail::new("early return value not promoted to a named variable")
            })?;
            let sentinel = self.symbol_for(EARLY_RETURN_SENTINEL);
            let test = self.b.expression_binary(
                SPAN,
                self.ident_expr(&name),
                OxcBinOp::StrictInequality,
                sentinel,
            );
            let mut ret_body = self.b.vec();
            ret_body.push(self.b.statement_return(SPAN, Some(self.ident_expr(&name))));
            let consequent = self.b.statement_block(SPAN, ret_body);
            out.push(self.b.statement_if(SPAN, test, consequent, None));
        }

        Ok(())
    }

    fn assign_cache_slot(&self, index: u32, value: oxc::Expression<'a>) -> oxc::Statement<'a> {
        let assign = self.b.expression_assignment(
            SPAN,
            oxc_syntax::operator::AssignmentOperator::Assign,
            self.cache_slot_target(index),
            value,
        );
        self.b.statement_expression(SPAN, assign)
    }

    /// `Symbol.for("…")`
    fn symbol_for(&self, s: &str) -> oxc::Expression<'a> {
        let object = self.ident_expr("Symbol");
        let callee =
            oxc::Expression::StaticMemberExpression(self.b.alloc(self.b.static_member_expression(
                SPAN,
                object,
                self.b.identifier_name(SPAN, self.atom("for")),
                false,
            )));
        let arg = self.b.expression_string_literal(SPAN, self.atom(s), None);
        let mut args = self.b.vec();
        args.push(oxc::Argument::from(arg));
        self.b.expression_call(
            SPAN,
            callee,
            None::<ArenaBox<'a, oxc::TSTypeParameterInstantiation<'a>>>,
            args,
            false,
        )
    }

    fn scope(&self, id: ScopeId) -> Bail<&react_compiler_hir::ReactiveScope> {
        self.env
            .scopes
            .get(id.0 as usize)
            .ok_or_else(|| CodegenBail::new("scope id out of range"))
    }

    // ===== terminals =====

    fn codegen_terminal(
        &mut self,
        terminal: &ReactiveTerminal,
        out: &mut ArenaVec<'a, oxc::Statement<'a>>,
    ) -> Bail<()> {
        match terminal {
            ReactiveTerminal::Return { value, .. } => {
                let name = self.place_name(value).ok();
                if name.as_deref() == Some("undefined") {
                    out.push(self.b.statement_return(SPAN, None));
                } else {
                    let expr = self.place_expr(value)?;
                    // A resolved `undefined` identifier (e.g. an inlined
                    // Primitive::Undefined temporary) also means `return;`.
                    if is_undefined_identifier(&expr) {
                        out.push(self.b.statement_return(SPAN, None));
                    } else {
                        out.push(self.b.statement_return(SPAN, Some(expr)));
                    }
                }
                Ok(())
            }
            ReactiveTerminal::If {
                test,
                consequent,
                alternate,
                ..
            } => {
                let test_expr = self.place_expr(test)?;
                let mut cons = self.b.vec();
                self.codegen_block(consequent, &mut cons)?;
                let cons_stmt = self.b.statement_block(SPAN, cons);
                let alt_stmt = if let Some(alt) = alternate {
                    let mut alt_stmts = self.b.vec();
                    self.codegen_block(alt, &mut alt_stmts)?;
                    Some(self.b.statement_block(SPAN, alt_stmts))
                } else {
                    None
                };
                out.push(self.b.statement_if(SPAN, test_expr, cons_stmt, alt_stmt));
                Ok(())
            }
            ReactiveTerminal::Throw { value, .. } => {
                let expr = self.place_expr(value)?;
                out.push(self.b.statement_throw(SPAN, expr));
                Ok(())
            }
            ReactiveTerminal::Break {
                target,
                target_kind,
                ..
            } => {
                match target_kind {
                    ReactiveTerminalTargetKind::Implicit => { /* fall-through: emit nothing */ }
                    ReactiveTerminalTargetKind::Labeled => {
                        let label = self
                            .b
                            .label_identifier(SPAN, self.atom(&codegen_label(*target)));
                        out.push(self.b.statement_break(SPAN, Some(label)));
                    }
                    ReactiveTerminalTargetKind::Unlabeled => {
                        out.push(self.b.statement_break(SPAN, None));
                    }
                }
                Ok(())
            }
            ReactiveTerminal::Continue {
                target,
                target_kind,
                ..
            } => {
                match target_kind {
                    ReactiveTerminalTargetKind::Implicit => { /* fall-through: emit nothing */ }
                    ReactiveTerminalTargetKind::Labeled => {
                        let label = self
                            .b
                            .label_identifier(SPAN, self.atom(&codegen_label(*target)));
                        out.push(self.b.statement_continue(SPAN, Some(label)));
                    }
                    ReactiveTerminalTargetKind::Unlabeled => {
                        out.push(self.b.statement_continue(SPAN, None));
                    }
                }
                Ok(())
            }
            ReactiveTerminal::While {
                test, loop_block, ..
            } => {
                let test_expr = self.codegen_value(test)?;
                let body = self.codegen_block_as_block_stmt(loop_block)?;
                out.push(self.b.statement_while(SPAN, test_expr, body));
                Ok(())
            }
            ReactiveTerminal::DoWhile {
                loop_block, test, ..
            } => {
                let body = self.codegen_block_as_block_stmt(loop_block)?;
                let test_expr = self.codegen_value(test)?;
                out.push(self.b.statement_do_while(SPAN, body, test_expr));
                Ok(())
            }
            ReactiveTerminal::For {
                init,
                test,
                update,
                loop_block,
                ..
            } => {
                let init_part = self.codegen_for_init(init)?;
                let test_expr = self.codegen_value(test)?;
                let update_expr = match update {
                    Some(u) => Some(self.codegen_value(u)?),
                    None => None,
                };
                let body = self.codegen_block_as_block_stmt(loop_block)?;
                out.push(self.b.statement_for(
                    SPAN,
                    Some(init_part),
                    Some(test_expr),
                    update_expr,
                    body,
                ));
                Ok(())
            }
            ReactiveTerminal::ForIn {
                init, loop_block, ..
            } => {
                // init is a SequenceExpression with exactly 2 instructions:
                // [0] = iterable collection, [1] = iterable item (the left).
                let instrs = self.sequence_instructions(init)?;
                if instrs.len() != 2 {
                    bail!("for-in init not a 2-instruction sequence");
                }
                // Build the for-in iterable expression from instrs[0].
                let right = self.codegen_value(&instrs[0].value)?;
                let left = self.extract_for_in_of_left(&instrs[1].value)?;
                let body = self.codegen_block_as_block_stmt(loop_block)?;
                out.push(self.b.statement_for_in(SPAN, left, right, body));
                Ok(())
            }
            ReactiveTerminal::ForOf {
                init,
                test,
                loop_block,
                ..
            } => {
                // init is a 1-instruction sequence of GetIterator { collection };
                // test is a 2-instruction sequence whose [1] yields the left.
                let init_instrs = self.sequence_instructions(init)?;
                if init_instrs.len() != 1 {
                    bail!("for-of init not a 1-instruction sequence");
                }
                let collection = match &init_instrs[0].value {
                    ReactiveValue::Instruction(InstructionValue::GetIterator {
                        collection,
                        ..
                    }) => collection.clone(),
                    _ => bail!("for-of init is not GetIterator"),
                };
                let test_instrs = self.sequence_instructions(test)?;
                if test_instrs.len() != 2 {
                    bail!("for-of test not a 2-instruction sequence");
                }
                let left = self.extract_for_in_of_left(&test_instrs[1].value)?;
                let right = self.place_expr(&collection)?;
                let body = self.codegen_block_as_block_stmt(loop_block)?;
                out.push(self.b.statement_for_of(SPAN, false, left, right, body));
                Ok(())
            }
            ReactiveTerminal::Switch { test, cases, .. } => {
                let discriminant = self.place_expr(test)?;
                let mut oxc_cases = self.b.vec();
                for case in cases {
                    let test_expr = match &case.test {
                        Some(p) => Some(self.place_expr(p)?),
                        None => None,
                    };
                    // Each case's block is wrapped in its own braced block.
                    let consequent = if let Some(block) = &case.block {
                        let mut stmts = self.b.vec();
                        self.codegen_block(block, &mut stmts)?;
                        if stmts.is_empty() {
                            self.b.vec()
                        } else {
                            let block_stmt = self.b.statement_block(SPAN, stmts);
                            let mut v = self.b.vec();
                            v.push(block_stmt);
                            v
                        }
                    } else {
                        self.b.vec()
                    };
                    oxc_cases.push(self.b.switch_case(SPAN, test_expr, consequent));
                }
                out.push(self.b.statement_switch(SPAN, discriminant, oxc_cases));
                Ok(())
            }
            ReactiveTerminal::Try {
                block,
                handler_binding,
                handler,
                ..
            } => {
                let try_block = self.codegen_block_as_block_stmt(block)?;
                // Register the catch binding as declared-but-no-expression.
                let catch_param = match handler_binding {
                    Some(place) => {
                        let name = self.place_name(place)?;
                        let decl_id = self.decl_id(place);
                        self.declared.insert(decl_id);
                        self.temp.insert(decl_id, None);
                        let pat = self.binding_pattern(&name);
                        Some(self.b.catch_parameter(
                            SPAN,
                            pat,
                            None::<ArenaBox<'a, oxc::TSTypeAnnotation<'a>>>,
                        ))
                    }
                    None => None,
                };
                let handler_block = self.codegen_block_as_block_stmt(handler)?;
                let catch_clause =
                    self.b
                        .catch_clause(SPAN, catch_param, self.statement_to_block(handler_block));
                out.push(self.b.statement_try(
                    SPAN,
                    self.statement_to_block(try_block),
                    Some(catch_clause),
                    None::<ArenaBox<'a, oxc::BlockStatement<'a>>>,
                ));
                Ok(())
            }
            ReactiveTerminal::Label { block, .. } => {
                // The label wrapping (if any) is applied by the caller based on
                // the terminal statement's `label`. Here we flatten the block.
                self.codegen_block(block, out)?;
                Ok(())
            }
        }
    }

    /// Codegen a block into a single `BlockStatement`.
    fn codegen_block_as_block_stmt(&mut self, block: &ReactiveBlock) -> Bail<oxc::Statement<'a>> {
        let mut stmts = self.b.vec();
        self.codegen_block(block, &mut stmts)?;
        Ok(self.b.statement_block(SPAN, stmts))
    }

    /// Unwrap a `Statement::BlockStatement` into the owned `BlockStatement`
    /// (try/catch builders require `BlockStatement`, not `Statement`).
    fn statement_to_block(
        &self,
        stmt: oxc::Statement<'a>,
    ) -> ArenaBox<'a, oxc::BlockStatement<'a>> {
        match stmt {
            oxc::Statement::BlockStatement(b) => b,
            other => {
                let mut v = self.b.vec();
                v.push(other);
                self.b.alloc(self.b.block_statement(SPAN, v))
            }
        }
    }

    /// Build a `ForStatementInit` from a For-loop `init` ReactiveValue.
    /// A sequence init is folded into a single VariableDeclaration; otherwise
    /// the init is emitted as an expression.
    fn codegen_for_init(&mut self, init: &ReactiveValue) -> Bail<oxc::ForStatementInit<'a>> {
        if let ReactiveValue::SequenceExpression { instructions, .. } = init {
            // Emit the instructions as statements, then fold into a single
            // `let`/`const` declaration (matching the reference's logic).
            let mut stmts = self.b.vec();
            for instr in instructions {
                self.codegen_instruction(instr, &mut stmts)?;
            }
            let decl = self.fold_for_init_statements(stmts)?;
            return Ok(oxc::ForStatementInit::VariableDeclaration(
                self.b.alloc(decl),
            ));
        }
        let expr = self.codegen_value(init)?;
        Ok(oxc::ForStatementInit::from(expr))
    }

    /// Fold the statements produced by a for-init sequence into one variable
    /// declaration. Handles the `let i; i = 0` -> `let i = 0` re-association the
    /// reference performs, and merges multiple declarators.
    fn fold_for_init_statements(
        &self,
        stmts: ArenaVec<'a, oxc::Statement<'a>>,
    ) -> Bail<oxc::VariableDeclaration<'a>> {
        let mut declarators = self.b.vec();
        let mut any_let = false;
        for stmt in stmts {
            match stmt {
                oxc::Statement::VariableDeclaration(decl) => {
                    let decl = decl.unbox();
                    if matches!(decl.kind, oxc::VariableDeclarationKind::Let) {
                        any_let = true;
                    }
                    for d in decl.declarations {
                        declarators.push(d);
                    }
                }
                oxc::Statement::ExpressionStatement(es) => {
                    // `i = expr;` — fold RHS into the matching last declarator
                    // whose init is None.
                    let es = es.unbox();
                    if let oxc::Expression::AssignmentExpression(assign) = es.expression {
                        let assign = assign.unbox();
                        if let oxc::AssignmentTarget::AssignmentTargetIdentifier(target) =
                            &assign.left
                        {
                            let name = target.name.as_str().to_string();
                            if let Some(d) = declarators.iter_mut().rev().find(|d| {
                                d.init.is_none()
                                    && binding_pattern_name(&d.id) == Some(name.as_str())
                            }) {
                                d.init = Some(assign.right);
                                continue;
                            }
                        }
                        bail!("for-init: unfoldable assignment");
                    }
                    bail!("for-init: non-assignment expression statement");
                }
                _ => bail!("for-init: unexpected statement kind"),
            }
        }
        if declarators.is_empty() {
            bail!("for-init: empty declarators");
        }
        let kind = if any_let {
            oxc::VariableDeclarationKind::Let
        } else {
            oxc::VariableDeclarationKind::Const
        };
        Ok(self.b.variable_declaration(SPAN, kind, declarators, false))
    }

    /// Get the instructions of a sequence ReactiveValue (for for-in/of inits).
    fn sequence_instructions<'b>(
        &self,
        value: &'b ReactiveValue,
    ) -> Bail<&'b [react_compiler_hir::reactive::ReactiveInstruction]> {
        match value {
            ReactiveValue::SequenceExpression { instructions, .. } => Ok(instructions),
            _ => bail!("expected sequence expression"),
        }
    }

    /// Extract the `for (LEFT of/in ...)` left side from the item instruction
    /// value (StoreLocal or Destructure).
    fn extract_for_in_of_left(&mut self, value: &ReactiveValue) -> Bail<oxc::ForStatementLeft<'a>> {
        let iv = match value {
            ReactiveValue::Instruction(iv) => iv,
            _ => bail!("for-in/of left is not an instruction"),
        };
        match iv {
            InstructionValue::StoreLocal { lvalue, .. } => {
                let kind = var_decl_kind(lvalue.kind)?;
                let name = self.place_name(&lvalue.place)?;
                let decl_id = self.decl_id(&lvalue.place);
                self.declared.insert(decl_id);
                let pat = self.binding_pattern(&name);
                let decl = self.single_decl(kind, pat, None);
                Ok(oxc::ForStatementLeft::VariableDeclaration(
                    self.b.alloc(decl),
                ))
            }
            InstructionValue::Destructure { lvalue, .. } => {
                let kind = var_decl_kind(lvalue.kind)?;
                let pat = self.binding_pattern_from_pattern(&lvalue.pattern, lvalue.kind)?;
                let decl = self.single_decl(kind, pat, None);
                Ok(oxc::ForStatementLeft::VariableDeclaration(
                    self.b.alloc(decl),
                ))
            }
            _ => bail!("for-in/of left is not a StoreLocal or Destructure"),
        }
    }

    // ===== values / expressions =====

    fn codegen_value(&mut self, value: &ReactiveValue) -> Bail<oxc::Expression<'a>> {
        match value {
            ReactiveValue::Instruction(iv) => self.codegen_instruction_value(iv),
            ReactiveValue::LogicalExpression {
                operator,
                left,
                right,
                ..
            } => {
                let l = self.codegen_value(left)?;
                let r = self.codegen_value(right)?;
                Ok(self
                    .b
                    .expression_logical(SPAN, l, map_logical_op(*operator), r))
            }
            ReactiveValue::ConditionalExpression {
                test,
                consequent,
                alternate,
                ..
            } => {
                let t = self.codegen_value(test)?;
                let c = self.codegen_value(consequent)?;
                let a = self.codegen_value(alternate)?;
                Ok(self.b.expression_conditional(SPAN, t, c, a))
            }
            ReactiveValue::SequenceExpression {
                instructions,
                value,
                ..
            } => {
                // Emit the sequence's instructions; most resolve to inline
                // temporaries (emitting no statement). Any statement-producing
                // instruction becomes a comma-expression operand.
                let mut exprs = self.b.vec();
                for instr in instructions {
                    let mut stmts = self.b.vec();
                    self.codegen_instruction(instr, &mut stmts)?;
                    for stmt in stmts {
                        match stmt {
                            oxc::Statement::ExpressionStatement(es) => {
                                exprs.push(es.unbox().expression);
                            }
                            _ => bail!("sequence: non-expression statement"),
                        }
                    }
                }
                let final_expr = self.codegen_value(value)?;
                if exprs.is_empty() {
                    Ok(final_expr)
                } else {
                    exprs.push(final_expr);
                    Ok(self.b.expression_sequence(SPAN, exprs))
                }
            }
            ReactiveValue::OptionalExpression {
                value, optional, ..
            } => {
                let inner = self.codegen_value(value)?;
                self.to_optional(inner, *optional)
            }
        }
    }

    /// Promote a member/call expression to its optional variant (`a?.b`,
    /// `a?.()`) and ensure the whole optional chain lives inside exactly one
    /// `ChainExpression`.
    ///
    /// In oxc an optional chain is a *single* `ChainExpression` wrapping a tree
    /// of plain `Static/ComputedMemberExpression` / `CallExpression` structs
    /// (nested via their `object`/`callee` fields). Only the genuinely-optional
    /// links carry `optional: true`. A `ChainExpression` must never be nested
    /// inside another `ChainExpression` — doing so loses the inner `?.` and
    /// prints stray parens (e.g. `(a?.b).c`).
    ///
    /// The lowering wraps *every* member/call link of an optional chain in an
    /// `OptionalExpression`, so `to_optional` is invoked for each link — and
    /// only for links that genuinely belong to the chain. (A non-chain access
    /// onto a parenthesized chain, like `(a?.b).c`, is a plain `PropertyLoad`,
    /// never routed here, so its inner chain stays parenthesized.) We therefore:
    ///   1. strip a head-level chain wrapper to get the bare member/call node,
    ///   2. flatten any `ChainExpression` sitting in its `object`/`callee` (that
    ///      inner chain is part of *this* chain),
    ///   3. set this link's `optional` flag,
    ///   4. re-wrap the whole thing in exactly one `ChainExpression`.
    fn to_optional(&self, expr: oxc::Expression<'a>, optional: bool) -> Bail<oxc::Expression<'a>> {
        // Unwrap a head-level chain so we operate on the bare member/call node.
        let head = self.unchain(expr);
        match head {
            oxc::Expression::StaticMemberExpression(m) => {
                let mut m = m.unbox();
                m.optional = optional;
                m.object = self.unchain(m.object);
                Ok(self.chain(oxc::Expression::StaticMemberExpression(self.b.alloc(m))))
            }
            oxc::Expression::ComputedMemberExpression(m) => {
                let mut m = m.unbox();
                m.optional = optional;
                m.object = self.unchain(m.object);
                Ok(self.chain(oxc::Expression::ComputedMemberExpression(self.b.alloc(m))))
            }
            oxc::Expression::CallExpression(c) => {
                let mut c = c.unbox();
                c.optional = optional;
                c.callee = self.unchain(c.callee);
                Ok(self.chain(oxc::Expression::CallExpression(self.b.alloc(c))))
            }
            _ => bail!("optional expression on non-member/call"),
        }
    }

    /// If `expr` is a `ChainExpression`, return the plain member/call expression
    /// it wraps; otherwise return `expr` unchanged. Used to flatten a nested
    /// optional chain (in an `object`/`callee` position) into the single
    /// surrounding chain, preserving the inner links' `optional` flags.
    fn unchain(&self, expr: oxc::Expression<'a>) -> oxc::Expression<'a> {
        match expr {
            oxc::Expression::ChainExpression(c) => match c.unbox().expression {
                oxc::ChainElement::CallExpression(c) => oxc::Expression::CallExpression(c),
                oxc::ChainElement::StaticMemberExpression(m) => {
                    oxc::Expression::StaticMemberExpression(m)
                }
                oxc::ChainElement::ComputedMemberExpression(m) => {
                    oxc::Expression::ComputedMemberExpression(m)
                }
                oxc::ChainElement::PrivateFieldExpression(m) => {
                    oxc::Expression::PrivateFieldExpression(m)
                }
                oxc::ChainElement::TSNonNullExpression(e) => {
                    oxc::Expression::TSNonNullExpression(e)
                }
            },
            other => other,
        }
    }

    /// Wrap a bare member/call expression in a single `ChainExpression`.
    fn chain(&self, expr: oxc::Expression<'a>) -> oxc::Expression<'a> {
        let elem = match expr {
            oxc::Expression::CallExpression(c) => oxc::ChainElement::CallExpression(c),
            oxc::Expression::StaticMemberExpression(m) => {
                oxc::ChainElement::StaticMemberExpression(m)
            }
            oxc::Expression::ComputedMemberExpression(m) => {
                oxc::ChainElement::ComputedMemberExpression(m)
            }
            oxc::Expression::PrivateFieldExpression(m) => {
                oxc::ChainElement::PrivateFieldExpression(m)
            }
            // Already a chain or non-chainable — return as-is.
            other => return other,
        };
        oxc::Expression::ChainExpression(self.b.alloc(self.b.chain_expression(SPAN, elem)))
    }

    fn codegen_instruction_value(&mut self, iv: &InstructionValue) -> Bail<oxc::Expression<'a>> {
        match iv {
            InstructionValue::Primitive { value, .. } => self.primitive(value),
            InstructionValue::LoadLocal { place, .. }
            | InstructionValue::LoadContext { place, .. } => self.place_expr(place),
            InstructionValue::LoadGlobal { binding, .. } => Ok(self.ident_expr(binding.name())),
            InstructionValue::BinaryExpression {
                operator,
                left,
                right,
                ..
            } => {
                let l = self.place_expr(left)?;
                let r = self.place_expr(right)?;
                Ok(self
                    .b
                    .expression_binary(SPAN, l, map_binary_op(*operator)?, r))
            }
            InstructionValue::UnaryExpression {
                operator, value, ..
            } => {
                let v = self.place_expr(value)?;
                Ok(self.b.expression_unary(SPAN, map_unary_op(*operator), v))
            }
            InstructionValue::CallExpression { callee, args, .. } => {
                let callee_expr = self.place_expr(callee)?;
                let arguments = self.arguments(args)?;
                Ok(self.b.expression_call(
                    SPAN,
                    callee_expr,
                    None::<ArenaBox<'a, oxc::TSTypeParameterInstantiation<'a>>>,
                    arguments,
                    false,
                ))
            }
            InstructionValue::MethodCall {
                receiver,
                property,
                args,
                ..
            } => {
                // Reconstruct `receiver.prop(args)`: property is a PropertyLoad
                // temporary in the reference; here we resolve it as a member of
                // the receiver. The HIR MethodCall carries the property Place,
                // which is itself a PropertyLoad temporary on `receiver`.
                let callee = self.method_callee(receiver, property)?;
                let arguments = self.arguments(args)?;
                Ok(self.b.expression_call(
                    SPAN,
                    callee,
                    None::<ArenaBox<'a, oxc::TSTypeParameterInstantiation<'a>>>,
                    arguments,
                    false,
                ))
            }
            InstructionValue::PropertyLoad {
                object, property, ..
            } => {
                let obj = self.place_expr(object)?;
                Ok(self.member(obj, property))
            }
            InstructionValue::ComputedLoad {
                object, property, ..
            } => {
                let obj = self.place_expr(object)?;
                let prop = self.place_expr(property)?;
                Ok(oxc::Expression::ComputedMemberExpression(self.b.alloc(
                    self.b.computed_member_expression(SPAN, obj, prop, false),
                )))
            }
            InstructionValue::ObjectExpression { properties, .. } => {
                self.object_expression(properties)
            }
            InstructionValue::ArrayExpression { elements, .. } => self.array_expression(elements),
            InstructionValue::JsxExpression {
                tag,
                props,
                children,
                ..
            } => self.jsx_element(tag, props, children.as_deref()),
            InstructionValue::JsxFragment { children, .. } => self.jsx_fragment(children),
            InstructionValue::JSXText { value, .. } => {
                Ok(self
                    .b
                    .expression_string_literal(SPAN, self.atom(value), None))
            }
            InstructionValue::NewExpression { callee, args, .. } => {
                let callee_expr = self.place_expr(callee)?;
                let arguments = self.arguments(args)?;
                Ok(self.b.expression_new(
                    SPAN,
                    callee_expr,
                    None::<ArenaBox<'a, oxc::TSTypeParameterInstantiation<'a>>>,
                    arguments,
                ))
            }
            InstructionValue::PropertyStore {
                object,
                property,
                value,
                ..
            } => {
                let obj = self.place_expr(object)?;
                let target = self.member_assignment_target(obj, property);
                let val = self.place_expr(value)?;
                Ok(self.b.expression_assignment(
                    SPAN,
                    oxc_syntax::operator::AssignmentOperator::Assign,
                    target,
                    val,
                ))
            }
            InstructionValue::ComputedStore {
                object,
                property,
                value,
                ..
            } => {
                let obj = self.place_expr(object)?;
                let prop = self.place_expr(property)?;
                let mem = self.b.computed_member_expression(SPAN, obj, prop, false);
                let target = oxc::AssignmentTarget::ComputedMemberExpression(self.b.alloc(mem));
                let val = self.place_expr(value)?;
                Ok(self.b.expression_assignment(
                    SPAN,
                    oxc_syntax::operator::AssignmentOperator::Assign,
                    target,
                    val,
                ))
            }
            InstructionValue::StoreGlobal { name, value, .. } => {
                let target = self.assignment_target_identifier(name);
                let val = self.place_expr(value)?;
                Ok(self.b.expression_assignment(
                    SPAN,
                    oxc_syntax::operator::AssignmentOperator::Assign,
                    target,
                    val,
                ))
            }
            InstructionValue::TemplateLiteral {
                subexprs, quasis, ..
            } => self.template_literal(subexprs, quasis),
            InstructionValue::TaggedTemplateExpression { tag, value, .. } => {
                let tag_expr = self.place_expr(tag)?;
                // A tagged template's lowered value carries a single quasi.
                let quasi = self.single_quasi_template(value);
                Ok(self.b.expression_tagged_template(
                    SPAN,
                    tag_expr,
                    None::<ArenaBox<'a, oxc::TSTypeParameterInstantiation<'a>>>,
                    quasi,
                ))
            }
            InstructionValue::Await { value, .. } => {
                let v = self.place_expr(value)?;
                Ok(self.b.expression_await(SPAN, v))
            }
            InstructionValue::FunctionExpression {
                name,
                name_hint,
                lowered_func,
                expr_type,
                ..
            } => self.function_expression(name, name_hint, lowered_func, *expr_type),
            InstructionValue::PostfixUpdate {
                operation, lvalue, ..
            } => self.update_expression(*operation, lvalue, false),
            InstructionValue::PrefixUpdate {
                operation, lvalue, ..
            } => self.update_expression(*operation, lvalue, true),
            InstructionValue::PropertyDelete {
                object, property, ..
            } => {
                // `delete obj.prop`
                let obj = self.place_expr(object)?;
                let member = self.member(obj, property);
                Ok(self.b.expression_unary(SPAN, OxcUnOp::Delete, member))
            }
            InstructionValue::ComputedDelete {
                object, property, ..
            } => {
                // `delete obj[prop]`
                let obj = self.place_expr(object)?;
                let prop = self.place_expr(property)?;
                let member = oxc::Expression::ComputedMemberExpression(
                    self.b
                        .alloc(self.b.computed_member_expression(SPAN, obj, prop, false)),
                );
                Ok(self.b.expression_unary(SPAN, OxcUnOp::Delete, member))
            }
            InstructionValue::RegExpLiteral { pattern, flags, .. } => {
                let parsed_flags = parse_regexp_flags(flags);
                let regex = oxc::RegExp {
                    pattern: oxc::RegExpPattern {
                        text: self.atom(pattern),
                        pattern: None,
                    },
                    flags: parsed_flags,
                };
                Ok(self.b.expression_reg_exp_literal(SPAN, regex, None))
            }
            InstructionValue::MetaProperty { meta, property, .. } => {
                let meta_id = self.b.identifier_name(SPAN, self.atom(meta));
                let property_id = self.b.identifier_name(SPAN, self.atom(property));
                Ok(self.b.expression_meta_property(SPAN, meta_id, property_id))
            }
            InstructionValue::NextPropertyOf { value, .. } => {
                // In a for-in loop's lowered form the loop variable's value IS
                // the place itself; just emit the inner expression.
                self.place_expr(value)
            }
            InstructionValue::StoreLocal { lvalue, value, .. }
            | InstructionValue::StoreContext { lvalue, value, .. } => {
                // StoreLocal/StoreContext reach expression context only as a
                // reassignment (e.g. a for-loop update, while-condition
                // assignment, or chained `y = (x = …)` for a context variable):
                // `name = value`.
                debug_assert!(matches!(lvalue.kind, InstructionKind::Reassign));
                let name = self.place_name(&lvalue.place)?;
                let val = self.place_expr(value)?;
                Ok(self.assign_ident_expr(&name, val))
            }
            InstructionValue::Destructure { lvalue, value, .. } => {
                // Destructure reaches expression context only as a reassignment
                // referenced inline (e.g. `f(([x] = makeObject()))`): build
                // `pattern = value`. Mirrors the reference `Reassign` branch.
                debug_assert!(matches!(lvalue.kind, InstructionKind::Reassign));
                let target = self.assignment_target_from_pattern(&lvalue.pattern)?;
                let val = self.place_expr(value)?;
                Ok(self.b.expression_assignment(
                    SPAN,
                    oxc_syntax::operator::AssignmentOperator::Assign,
                    target,
                    val,
                ))
            }
            InstructionValue::TypeCastExpression {
                value,
                type_annotation_name,
                type_annotation_kind,
                ..
            } => {
                let inner = self.place_expr(value)?;
                let type_text = type_annotation_name.as_deref().ok_or_else(|| {
                    CodegenBail::new("TypeCastExpression missing type annotation text")
                })?;
                let type_annotation = self.parse_ts_type(type_text)?;
                let is_satisfies = type_annotation_kind.as_deref() == Some("satisfies");
                Ok(if is_satisfies {
                    self.b.expression_ts_satisfies(SPAN, inner, type_annotation)
                } else {
                    self.b.expression_ts_as(SPAN, inner, type_annotation)
                })
            }
            other => bail!("instruction value not yet supported: {}", iv_kind(other)),
        }
    }

    /// Re-parse a TS type annotation's source text into a `TSType` node in the
    /// codegen allocator. Used to reconstruct `<expr> as <Type>` /
    /// `<expr> satisfies <Type>` expressions whose type was carried as text
    /// through the HIR (the lowering does not retain the full type node).
    fn parse_ts_type(&self, type_text: &str) -> Bail<oxc::TSType<'a>> {
        // Parse a synthetic `null as <Type>` statement and extract the type.
        // The source string is allocated into the codegen arena so the parsed
        // atoms (which slice into the source) remain valid for lifetime `'a`.
        let src = format!("null as {};", type_text);
        let allocator = self.b.allocator;
        let source: &'a str = allocator.alloc_str(&src);
        let source_type = oxc_span::SourceType::default()
            .with_typescript(true)
            .with_module(true);
        let parsed = oxc_parser::Parser::new(allocator, source, source_type).parse();
        if parsed.panicked {
            return Err(CodegenBail::new("failed to parse TS type annotation"));
        }
        for stmt in parsed.program.body {
            if let oxc::Statement::ExpressionStatement(expr_stmt) = stmt {
                let expr_stmt = expr_stmt.unbox();
                if let oxc::Expression::TSAsExpression(ts) = expr_stmt.expression {
                    return Ok(ts.unbox().type_annotation);
                }
            }
        }
        Err(CodegenBail::new(
            "failed to extract TS type from parsed annotation",
        ))
    }

    /// Re-parse a statement's source text into a `Statement` node in the codegen
    /// allocator. Used to re-emit declarations whose syntax is carried verbatim
    /// through the HIR as an `UnsupportedNode` (e.g. a TS `enum` declaration),
    /// mirroring the reference codegen which returns the original AST node.
    fn parse_statement(&self, src: &str) -> Bail<oxc::Statement<'a>> {
        // The source string is allocated into the codegen arena so the parsed
        // atoms (which slice into the source) remain valid for lifetime `'a`.
        let allocator = self.b.allocator;
        let source: &'a str = allocator.alloc_str(src);
        let source_type = oxc_span::SourceType::default()
            .with_typescript(true)
            .with_module(true);
        let parsed = oxc_parser::Parser::new(allocator, source, source_type).parse();
        if parsed.panicked {
            return Err(CodegenBail::new(
                "failed to parse UnsupportedNode statement",
            ));
        }
        parsed
            .program
            .body
            .into_iter()
            .next()
            .ok_or_else(|| CodegenBail::new("UnsupportedNode statement parsed to no statements"))
    }

    /// Codegen a nested function expression / arrow. Builds the lowered HIR
    /// function into a reactive function, prunes it, then recursively codegens
    /// it (inheriting the outer inline-temporary table so captured temporaries
    /// resolve).
    /// Build + prune the reactive function for a nested function/method
    /// (`build_reactive_function → prune_unused_labels → prune_unused_lvalues`).
    /// When `run_hoisted` is set, also runs `prune_hoisted_contexts` (the nested
    /// function-expression path needs it; the object-method path does not).
    fn lower_nested_reactive_fn(
        &self,
        lowered_func: &react_compiler_hir::LoweredFunction,
        run_hoisted: bool,
    ) -> Bail<ReactiveFunction> {
        let hir = &self.env.functions[lowered_func.func.0 as usize];
        let mut reactive_fn =
            crate::build_reactive_function::build_reactive_function(hir, self.env)
                .map_err(|_| CodegenBail::new("nested function: build_reactive_function failed"))?;
        crate::prune_unused_labels::prune_unused_labels(&mut reactive_fn, self.env)
            .map_err(|_| CodegenBail::new("nested function: prune_unused_labels failed"))?;
        crate::prune_unused_lvalues::prune_unused_lvalues(&mut reactive_fn, self.env);
        if run_hoisted {
            crate::prune_hoisted_contexts::prune_hoisted_contexts(&mut reactive_fn, self.env)
                .map_err(|_| CodegenBail::new("nested function: prune_hoisted_contexts failed"))?;
        }
        Ok(reactive_fn)
    }

    fn function_expression(
        &mut self,
        name: &Option<String>,
        name_hint: &Option<String>,
        lowered_func: &react_compiler_hir::LoweredFunction,
        expr_type: FunctionExpressionType,
    ) -> Bail<oxc::Expression<'a>> {
        let reactive_fn = self.lower_nested_reactive_fn(lowered_func, true)?;

        // Recurse. The nested function shares this Cx (arena, temp table,
        // declared set) and gets its own cache numbering.
        let (function, _nested_cache) = self.codegen_function(&reactive_fn)?;

        let expr = match expr_type {
            FunctionExpressionType::ArrowFunctionExpression => {
                self.build_arrow_from_function(function, &reactive_fn)
            }
            _ => {
                // Function expression: keep the name (if any) for the binding id.
                let mut function = function;
                function.r#type = oxc::FunctionType::FunctionExpression;
                function.id = name
                    .as_ref()
                    .map(|n| self.b.binding_identifier(SPAN, self.atom(n)));
                oxc::Expression::FunctionExpression(self.b.alloc(function))
            }
        };

        // enableNameAnonymousFunctions: an anonymous function (no own name) with
        // a computed name hint is wrapped as `{ "<hint>": <fn> }["<hint>"]` so
        // the function gets an inferred `.name` at runtime. Mirrors
        // CodegenReactiveFunction.ts (codegen of FunctionExpression).
        if self.env.config.enable_name_anonymous_functions
            && name.is_none()
            && let Some(hint) = name_hint
        {
            return Ok(self.wrap_with_name_hint(expr, hint));
        }
        Ok(expr)
    }

    /// Wrap an anonymous function expression as `{ "<hint>": <fn> }["<hint>"]`,
    /// giving it an inferred runtime name. Used for enableNameAnonymousFunctions.
    fn wrap_with_name_hint(&self, value: oxc::Expression<'a>, hint: &str) -> oxc::Expression<'a> {
        // Build the object literal `{ "<hint>": value }`.
        let key = oxc::PropertyKey::StringLiteral(self.b.alloc(self.b.string_literal(
            SPAN,
            self.atom(hint),
            None,
        )));
        let prop = self.b.object_property_kind_object_property(
            SPAN,
            oxc::PropertyKind::Init,
            key,
            value,
            false,
            false,
            false,
        );
        let mut props = self.b.vec();
        props.push(prop);
        let object = self.b.expression_object(SPAN, props);
        // Index it with the same string key: `<object>["<hint>"]`.
        let index = self
            .b
            .expression_string_literal(SPAN, self.atom(hint), None);
        self.b
            .member_expression_computed(SPAN, object, index, false)
            .into()
    }

    /// Build the function value for an object-literal method (`{ m() {…} }`).
    /// Same nested-codegen path as `function_expression`, but the result is a
    /// `FunctionExpression` (with `id: None`) used as the method's value while
    /// the enclosing `ObjectProperty` carries `method: true`.
    fn build_method_function(
        &mut self,
        lowered_func: &react_compiler_hir::LoweredFunction,
    ) -> Bail<oxc::Expression<'a>> {
        let reactive_fn = self.lower_nested_reactive_fn(lowered_func, false)?;

        let (mut function, _nested_cache) = self.codegen_function(&reactive_fn)?;
        function.r#type = oxc::FunctionType::FunctionExpression;
        function.id = None;
        Ok(oxc::Expression::FunctionExpression(self.b.alloc(function)))
    }

    /// Convert a built `oxc::Function` into an arrow expression, applying the
    /// single-return-statement -> expression-body optimization.
    fn build_arrow_from_function(
        &self,
        function: oxc::Function<'a>,
        reactive_fn: &ReactiveFunction,
    ) -> oxc::Expression<'a> {
        let params = function.params.unbox();
        let is_async = function.r#async;
        let body = function
            .body
            .expect("function body present after codegen_function")
            .unbox();
        let directives = body.directives;
        let statements = body.statements;

        // Single-return optimization: `() => { return X; }` becomes `() => X`,
        // only when there are no directives and the sole statement is a return
        // with an argument.
        let single_return_arg = if statements.len() == 1
            && directives.is_empty()
            && reactive_fn.directives.is_empty()
        {
            matches!(statements.first(), Some(oxc::Statement::ReturnStatement(r)) if r.argument.is_some())
        } else {
            false
        };

        if single_return_arg {
            let mut statements = statements;
            if let oxc::Statement::ReturnStatement(ret) = statements.pop().unwrap() {
                let arg = ret.unbox().argument.unwrap();
                let mut v = self.b.vec();
                v.push(self.b.statement_expression(SPAN, arg));
                let fn_body = self.b.function_body(SPAN, self.b.vec(), v);
                return self.b.expression_arrow_function(
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

        let fn_body = self.b.function_body(SPAN, directives, statements);
        self.b.expression_arrow_function(
            SPAN,
            false,
            is_async,
            None::<ArenaBox<'a, oxc::TSTypeParameterDeclaration<'a>>>,
            params,
            None::<ArenaBox<'a, oxc::TSTypeAnnotation<'a>>>,
            fn_body,
        )
    }

    /// Build the callee for a `MethodCall`. The `property` Place is an
    /// unpromoted, unmemoized `MemberExpression` temporary whose object IS the
    /// receiver. Mirroring the reference `CodegenReactiveFunction`, we codegen
    /// the property place to an expression and use it directly as the callee:
    /// the property's stashed value already encodes `receiver.member` (possibly
    /// wrapped in a sequence that re-loads the receiver, or an optional chain).
    ///
    /// `_receiver` is retained for signature parity; the reference only asserts
    /// the member's object equals the receiver, it does not rebuild from it.
    fn method_callee(&mut self, _receiver: &Place, property: &Place) -> Bail<oxc::Expression<'a>> {
        let expr = self.place_expr(property)?;
        // Invariant (mirrors the reference codegen): the property must resolve
        // to a member expression. If it codegens to anything else (e.g. an
        // Identifier, because the member was promoted/memoized into a temporary),
        // the IR is in a state the TS compiler also rejects with a fatal error.
        // An optional member (`a?.b`) is an oxc `ChainExpression`, so accept that
        // too (it corresponds to the reference's `OptionalMemberExpression`).
        let is_member = matches!(
            expr,
            oxc::Expression::StaticMemberExpression(_)
                | oxc::Expression::ComputedMemberExpression(_)
                | oxc::Expression::PrivateFieldExpression(_)
                | oxc::Expression::ChainExpression(_)
        );
        if !is_member {
            invariant_bail!(
                "[Codegen] Internal error: MethodCall::property must be an unpromoted + unmemoized MemberExpression"
            );
        }
        Ok(expr)
    }

    /// Build an assignment target for `object.property` / `object[number]`.
    fn member_assignment_target(
        &self,
        object: oxc::Expression<'a>,
        property: &PropertyLiteral,
    ) -> oxc::AssignmentTarget<'a> {
        match property {
            PropertyLiteral::String(name) => {
                let mem = self.b.static_member_expression(
                    SPAN,
                    object,
                    self.b.identifier_name(SPAN, self.atom(name)),
                    false,
                );
                oxc::AssignmentTarget::StaticMemberExpression(self.b.alloc(mem))
            }
            PropertyLiteral::Number(n) => {
                let prop = self.b.expression_numeric_literal(
                    SPAN,
                    n.value(),
                    None,
                    oxc::NumberBase::Decimal,
                );
                let mem = self.b.computed_member_expression(SPAN, object, prop, false);
                oxc::AssignmentTarget::ComputedMemberExpression(self.b.alloc(mem))
            }
        }
    }

    /// Build a `TemplateLiteral` expression from quasis + subexprs.
    fn template_literal(
        &mut self,
        subexprs: &[Place],
        quasis: &[TemplateQuasi],
    ) -> Bail<oxc::Expression<'a>> {
        let template = self.build_template(subexprs, quasis)?;
        Ok(oxc::Expression::TemplateLiteral(self.b.alloc(template)))
    }

    fn build_template(
        &mut self,
        subexprs: &[Place],
        quasis: &[TemplateQuasi],
    ) -> Bail<oxc::TemplateLiteral<'a>> {
        let mut elems = self.b.vec();
        let last = quasis.len().saturating_sub(1);
        for (i, q) in quasis.iter().enumerate() {
            let raw = self.atom(&q.raw);
            let cooked = q.cooked.as_ref().map(|c| self.atom(c));
            let value = oxc::TemplateElementValue { raw, cooked };
            elems.push(self.b.template_element(SPAN, value, i == last));
        }
        let mut exprs = self.b.vec();
        for s in subexprs {
            exprs.push(self.place_expr(s)?);
        }
        Ok(self.b.template_literal(SPAN, elems, exprs))
    }

    /// Build a single-quasi `TemplateLiteral` for a tagged template.
    fn single_quasi_template(&self, value: &TemplateQuasi) -> oxc::TemplateLiteral<'a> {
        let raw = self.atom(&value.raw);
        let cooked = value.cooked.as_ref().map(|c| self.atom(c));
        let tv = oxc::TemplateElementValue { raw, cooked };
        let mut elems = self.b.vec();
        elems.push(self.b.template_element(SPAN, tv, true));
        self.b.template_literal(SPAN, elems, self.b.vec())
    }

    fn member(
        &self,
        object: oxc::Expression<'a>,
        property: &PropertyLiteral,
    ) -> oxc::Expression<'a> {
        match property {
            PropertyLiteral::String(name) => oxc::Expression::StaticMemberExpression(self.b.alloc(
                self.b.static_member_expression(
                    SPAN,
                    object,
                    self.b.identifier_name(SPAN, self.atom(name)),
                    false,
                ),
            )),
            PropertyLiteral::Number(n) => {
                let prop = self.b.expression_numeric_literal(
                    SPAN,
                    n.value(),
                    None,
                    oxc::NumberBase::Decimal,
                );
                oxc::Expression::ComputedMemberExpression(
                    self.b
                        .alloc(self.b.computed_member_expression(SPAN, object, prop, false)),
                )
            }
        }
    }

    /// Build an `x++` / `x--` / `++x` / `--x` update expression. The lvalue is a
    /// `Place` that resolves to a simple identifier target (mirrors the reference
    /// codegen, where the argument is a Place rendered as an expression).
    fn update_expression(
        &self,
        operation: UpdateOperator,
        lvalue: &Place,
        prefix: bool,
    ) -> Bail<oxc::Expression<'a>> {
        let name = self.place_name(lvalue)?;
        let target = self
            .b
            .simple_assignment_target_assignment_target_identifier(SPAN, self.atom(&name));
        let op = match operation {
            UpdateOperator::Increment => OxcUpOp::Increment,
            UpdateOperator::Decrement => OxcUpOp::Decrement,
        };
        Ok(self.b.expression_update(SPAN, op, prefix, target))
    }

    fn primitive(&self, value: &PrimitiveValue) -> Bail<oxc::Expression<'a>> {
        Ok(match value {
            PrimitiveValue::Null => self.b.expression_null_literal(SPAN),
            PrimitiveValue::Undefined => self.ident_expr("undefined"),
            PrimitiveValue::Boolean(b) => self.b.expression_boolean_literal(SPAN, *b),
            PrimitiveValue::Number(n) => {
                let v = n.value();
                if v < 0.0 {
                    // Negative numbers must be a unary minus on a positive literal.
                    let lit =
                        self.b
                            .expression_numeric_literal(SPAN, -v, None, oxc::NumberBase::Decimal);
                    self.b.expression_unary(SPAN, OxcUnOp::UnaryNegation, lit)
                } else {
                    // Normalize negative zero to positive zero. The TS plugin's
                    // codegenValue does `value < 0 ? -value : numericLiteral(value)`,
                    // and `t.numericLiteral(-0)` prints as `0` (since `-0 < 0` is
                    // false). Match that so `-0` constant-folds to `0`.
                    let v = if v == 0.0 { 0.0 } else { v };
                    self.b
                        .expression_numeric_literal(SPAN, v, None, oxc::NumberBase::Decimal)
                }
            }
            PrimitiveValue::String(s) => self.b.expression_string_literal(SPAN, self.atom(s), None),
        })
    }

    fn arguments(&mut self, args: &[PlaceOrSpread]) -> Bail<ArenaVec<'a, oxc::Argument<'a>>> {
        let mut out = self.b.vec();
        for a in args {
            match a {
                PlaceOrSpread::Place(p) => {
                    let e = self.place_expr(p)?;
                    out.push(oxc::Argument::from(e));
                }
                PlaceOrSpread::Spread(s) => {
                    let e = self.place_expr(&s.place)?;
                    out.push(self.b.argument_spread_element(SPAN, e));
                }
            }
        }
        Ok(out)
    }

    fn object_expression(
        &mut self,
        properties: &[ObjectPropertyOrSpread],
    ) -> Bail<oxc::Expression<'a>> {
        let mut props = self.b.vec();
        for p in properties {
            match p {
                ObjectPropertyOrSpread::Property(prop) => {
                    let (key, computed) = self.object_key(&prop.key)?;
                    if matches!(
                        prop.property_type,
                        react_compiler_hir::ObjectPropertyType::Method
                    ) {
                        // Method shorthand: `{ name(params) { body } }`. The
                        // lowered function was stashed by `codegen_instruction`
                        // keyed by the property's lvalue identifier.
                        let lowered_func = self
                            .object_methods
                            .get(&prop.place.identifier)
                            .cloned()
                            .ok_or_else(|| {
                                CodegenBail::new(
                                    "object method: no stashed ObjectMethod instruction",
                                )
                            })?;
                        let value = self.build_method_function(&lowered_func)?;
                        let object_property = self.b.object_property(
                            SPAN,
                            oxc::PropertyKind::Init,
                            key,
                            value,
                            // method, shorthand, computed
                            true,
                            false,
                            computed,
                        );
                        props.push(oxc::ObjectPropertyKind::ObjectProperty(
                            self.b.alloc(object_property),
                        ));
                        continue;
                    }
                    let value = self.place_expr(&prop.place)?;
                    let object_property = self.b.object_property(
                        SPAN,
                        oxc::PropertyKind::Init,
                        key,
                        value,
                        false,
                        false,
                        computed,
                    );
                    props.push(oxc::ObjectPropertyKind::ObjectProperty(
                        self.b.alloc(object_property),
                    ));
                }
                ObjectPropertyOrSpread::Spread(s) => {
                    let e = self.place_expr(&s.place)?;
                    let spread = self.b.spread_element(SPAN, e);
                    props.push(oxc::ObjectPropertyKind::SpreadProperty(
                        self.b.alloc(spread),
                    ));
                }
            }
        }
        Ok(self.b.expression_object(SPAN, props))
    }

    fn object_key(&mut self, key: &ObjectPropertyKey) -> Bail<(oxc::PropertyKey<'a>, bool)> {
        Ok(match key {
            ObjectPropertyKey::Identifier { name } => (
                self.b.property_key_static_identifier(SPAN, self.atom(name)),
                false,
            ),
            ObjectPropertyKey::String { name } => {
                let lit = self.b.string_literal(SPAN, self.atom(name), None);
                (oxc::PropertyKey::StringLiteral(self.b.alloc(lit)), false)
            }
            ObjectPropertyKey::Number { name } => {
                let lit =
                    self.b
                        .numeric_literal(SPAN, name.value(), None, oxc::NumberBase::Decimal);
                (oxc::PropertyKey::NumericLiteral(self.b.alloc(lit)), false)
            }
            ObjectPropertyKey::Computed { name } => {
                let e = self.place_expr(name)?;
                (oxc::PropertyKey::from(e), true)
            }
        })
    }

    fn array_expression(&mut self, elements: &[ArrayElement]) -> Bail<oxc::Expression<'a>> {
        let mut els = self.b.vec();
        for e in elements {
            match e {
                ArrayElement::Place(p) => {
                    let expr = self.place_expr(p)?;
                    els.push(oxc::ArrayExpressionElement::from(expr));
                }
                ArrayElement::Spread(s) => {
                    let expr = self.place_expr(&s.place)?;
                    els.push(self.b.array_expression_element_spread_element(SPAN, expr));
                }
                ArrayElement::Hole => {
                    els.push(self.b.array_expression_element_elision(SPAN));
                }
            }
        }
        Ok(self.b.expression_array(SPAN, els))
    }

    // ===== JSX =====

    fn jsx_element(
        &mut self,
        tag: &JsxTag,
        props: &[JsxAttribute],
        children: Option<&[Place]>,
    ) -> Bail<oxc::Expression<'a>> {
        let name = self.jsx_element_name(tag)?;
        let closing_name = self.jsx_element_name(tag)?;

        let mut attrs = self.b.vec();
        for attr in props {
            match attr {
                JsxAttribute::Attribute { name, place } => {
                    let attr_name = self.jsx_attribute_name(name);
                    let value = self.jsx_attribute_value(place)?;
                    attrs.push(
                        self.b
                            .jsx_attribute_item_attribute(SPAN, attr_name, Some(value)),
                    );
                }
                JsxAttribute::SpreadAttribute { argument } => {
                    let e = self.place_expr(argument)?;
                    attrs.push(self.b.jsx_attribute_item_spread_attribute(SPAN, e));
                }
            }
        }

        let opening = self.b.jsx_opening_element(
            SPAN,
            name,
            None::<ArenaBox<'a, oxc::TSTypeParameterInstantiation<'a>>>,
            attrs,
        );
        let closing = Some(self.b.jsx_closing_element(SPAN, closing_name));

        let mut child_nodes = self.b.vec();
        if let Some(children) = children {
            for c in children {
                child_nodes.push(self.jsx_child(c)?);
            }
        }

        let element = self.b.jsx_element(SPAN, opening, child_nodes, closing);
        Ok(oxc::Expression::JSXElement(self.b.alloc(element)))
    }

    fn jsx_fragment(&mut self, children: &[Place]) -> Bail<oxc::Expression<'a>> {
        let opening = self.b.jsx_opening_fragment(SPAN);
        let closing = self.b.jsx_closing_fragment(SPAN);
        let mut child_nodes = self.b.vec();
        for c in children {
            child_nodes.push(self.jsx_child(c)?);
        }
        let frag = self.b.jsx_fragment(SPAN, opening, child_nodes, closing);
        Ok(oxc::Expression::JSXFragment(self.b.alloc(frag)))
    }

    fn jsx_element_name(&mut self, tag: &JsxTag) -> Bail<oxc::JSXElementName<'a>> {
        match tag {
            JsxTag::Builtin(builtin) => Ok(self
                .b
                .jsx_element_name_identifier(SPAN, self.atom(&builtin.name))),
            JsxTag::Place(place) => {
                // The tag may be an unnamed temporary holding a member load
                // (`<Foo.Bar>`); inline it via `place_expr` and convert the
                // resulting expression to a JSX element name. Mirrors the
                // reference's `expression_to_jsx_tag`.
                let expr = self.place_expr(place)?;
                self.expr_to_jsx_element_name(expr)
            }
        }
    }

    /// Convert an inlined tag expression (identifier, member chain, or
    /// namespaced string) into a `JSXElementName`. Mirrors the reference
    /// `JsxExpression` codegen (`CodegenReactiveFunction.ts`): a namespaced tag
    /// (`<xml:http>`) is lowered to a `Primitive` string `"xml:http"`, which
    /// `place_expr` rebuilds as a `StringLiteral`; codegen then splits on the
    /// first `:` and emits a `JSXNamespacedName`.
    fn expr_to_jsx_element_name(&self, expr: oxc::Expression<'a>) -> Bail<oxc::JSXElementName<'a>> {
        match expr {
            oxc::Expression::Identifier(ident) => Ok(self
                .b
                .jsx_element_name_identifier_reference(SPAN, ident.unbox().name)),
            oxc::Expression::StaticMemberExpression(m) => {
                let m = m.unbox();
                let object = self.expr_to_jsx_member_object(m.object)?;
                let property = self.b.jsx_identifier(SPAN, m.property.name);
                Ok(self
                    .b
                    .jsx_element_name_member_expression(SPAN, object, property))
            }
            // Namespaced tag (`<xml:http>`): the lowered string literal is split
            // on the first `:` into namespace + name. A string without `:` is a
            // bare builtin tag name (`<div>` produced as a string).
            oxc::Expression::StringLiteral(s) => {
                let value = s.unbox().value;
                let value = value.as_str();
                if let Some((namespace, name)) = value.split_once(':') {
                    let namespace = self.b.jsx_identifier(SPAN, self.atom(namespace));
                    let name = self.b.jsx_identifier(SPAN, self.atom(name));
                    Ok(self
                        .b
                        .jsx_element_name_namespaced_name(SPAN, namespace, name))
                } else {
                    Ok(self.b.jsx_element_name_identifier(SPAN, self.atom(value)))
                }
            }
            _ => bail!("jsx tag expression is not an identifier or member chain"),
        }
    }

    /// Build a `JSXAttributeName`, splitting a namespaced attribute name
    /// (`protocol:version`) on the first `:` into a `JSXNamespacedName`.
    /// Mirrors the reference `codegenJsxAttribute`.
    fn jsx_attribute_name(&self, name: &str) -> oxc::JSXAttributeName<'a> {
        if let Some((namespace, local)) = name.split_once(':') {
            let namespace = self.b.jsx_identifier(SPAN, self.atom(namespace));
            let local = self.b.jsx_identifier(SPAN, self.atom(local));
            self.b
                .jsx_attribute_name_namespaced_name(SPAN, namespace, local)
        } else {
            self.b.jsx_attribute_name_identifier(SPAN, self.atom(name))
        }
    }

    /// Convert the object side of a member-expression tag into a
    /// `JSXMemberExpressionObject` (recursing for nested `A.B.C`).
    fn expr_to_jsx_member_object(
        &self,
        expr: oxc::Expression<'a>,
    ) -> Bail<oxc::JSXMemberExpressionObject<'a>> {
        match expr {
            oxc::Expression::Identifier(ident) => Ok(self
                .b
                .jsx_member_expression_object_identifier_reference(SPAN, ident.unbox().name)),
            oxc::Expression::StaticMemberExpression(m) => {
                let m = m.unbox();
                let object = self.expr_to_jsx_member_object(m.object)?;
                let property = self.b.jsx_identifier(SPAN, m.property.name);
                Ok(self
                    .b
                    .jsx_member_expression_object_member_expression(SPAN, object, property))
            }
            _ => bail!("jsx member tag object is not an identifier or member chain"),
        }
    }

    fn jsx_attribute_value(&mut self, place: &Place) -> Bail<oxc::JSXAttributeValue<'a>> {
        // String-literal shortcut for a primitive string temporary. Inspect the
        // temp by reference and clone only the committed string value.
        let decl_id = self.decl_id(place);
        let primitive_string = match self.temp.get(&decl_id) {
            Some(Some(ReactiveValue::Instruction(InstructionValue::Primitive {
                value: PrimitiveValue::String(s),
                ..
            }))) => Some(s.clone()),
            _ => None,
        };
        if let Some(s) = primitive_string {
            // A string attribute value that contains characters which cannot
            // be faithfully reproduced inside a bare `"…"` JSX attribute (control
            // codes, the `"` and `\` characters, or any non-basic-Latin /
            // astral-plane character) must instead be emitted as an expression
            // container holding a JS string literal, so the generator escapes
            // it. Mirrors the reference `STRING_REQUIRES_EXPR_CONTAINER_PATTERN`.
            // (The reference's fbtOperands exclusion is for deferred fbt.)
            if string_requires_expr_container(&s) {
                let lit = self.b.expression_string_literal(SPAN, self.atom(&s), None);
                let container = self
                    .b
                    .jsx_expression_container(SPAN, oxc::JSXExpression::from(lit));
                return Ok(oxc::JSXAttributeValue::ExpressionContainer(
                    self.b.alloc(container),
                ));
            }
            return Ok(self
                .b
                .jsx_attribute_value_string_literal(SPAN, self.atom(&s), None));
        }
        let expr = self.place_expr(place)?;
        let container = self
            .b
            .jsx_expression_container(SPAN, oxc::JSXExpression::from(expr));
        Ok(oxc::JSXAttributeValue::ExpressionContainer(
            self.b.alloc(container),
        ))
    }

    fn jsx_child(&mut self, place: &Place) -> Bail<oxc::JSXChild<'a>> {
        // JSXText children come through as a temporary JSXText instruction.
        // Inspect the temp by reference and clone only the committed value.
        let decl_id = self.decl_id(place);
        let jsx_text = match self.temp.get(&decl_id) {
            Some(Some(ReactiveValue::Instruction(InstructionValue::JSXText { value, .. }))) => {
                Some(value.clone())
            }
            _ => None,
        };
        if let Some(value) = jsx_text {
            // A JSXText child whose (already entity-decoded) value contains a
            // character that cannot survive being re-emitted as raw JSX text
            // (`< > & { }`) must instead be emitted as an expression container
            // holding a JS string literal, so the generator re-escapes it.
            // Mirrors the reference `JSX_TEXT_CHILD_REQUIRES_EXPR_CONTAINER_PATTERN`.
            if jsx_text_requires_expr_container(&value) {
                let lit = self
                    .b
                    .expression_string_literal(SPAN, self.atom(&value), None);
                let container = self
                    .b
                    .jsx_expression_container(SPAN, oxc::JSXExpression::from(lit));
                return Ok(oxc::JSXChild::ExpressionContainer(self.b.alloc(container)));
            }
            return Ok(self.b.jsx_child_text(SPAN, self.atom(&value), None));
        }
        // A nested JSX element temporary -> embed directly as a child element.
        let jsx_instruction = match self.temp.get(&decl_id) {
            Some(Some(ReactiveValue::Instruction(
                iv
                @ (InstructionValue::JsxExpression { .. } | InstructionValue::JsxFragment { .. }),
            ))) => Some(iv.clone()),
            _ => None,
        };
        if let Some(iv) = jsx_instruction {
            let expr = self.codegen_instruction_value(&iv)?;
            return Ok(match expr {
                oxc::Expression::JSXElement(el) => oxc::JSXChild::Element(el),
                oxc::Expression::JSXFragment(f) => oxc::JSXChild::Fragment(f),
                other => {
                    let container = self
                        .b
                        .jsx_expression_container(SPAN, oxc::JSXExpression::from(other));
                    oxc::JSXChild::ExpressionContainer(self.b.alloc(container))
                }
            });
        }
        let expr = self.place_expr(place)?;
        let container = self
            .b
            .jsx_expression_container(SPAN, oxc::JSXExpression::from(expr));
        Ok(oxc::JSXChild::ExpressionContainer(self.b.alloc(container)))
    }

    // ===== place resolution (inline temporaries) =====

    /// Resolve a Place to an expression: inline its stashed value if it's a
    /// temporary, else emit a bare identifier.
    fn place_expr(&mut self, place: &Place) -> Bail<oxc::Expression<'a>> {
        let decl_id = self.decl_id(place);
        if let Some(entry) = self.temp.get(&decl_id)
            && let Some(rv) = entry.clone()
        {
            return self.codegen_value(&rv);
        }
        // declared but no inline value -> bare identifier below.
        let name = self.place_name(place)?;
        Ok(self.ident_expr(&name))
    }

    // ===== dependencies =====

    fn dependency_expr(
        &self,
        dep: &react_compiler_hir::ReactiveScopeDependency,
    ) -> Bail<oxc::Expression<'a>> {
        let base_name = self.ident_name(dep.identifier)?;
        let mut expr = self.ident_expr(&base_name);
        // If any link in the path is optional, the entire chain is built as
        // optional member expressions (each link keeping its own `optional`
        // flag) and wrapped in a single `ChainExpression`, e.g. `a?.b.c`
        // prints from links `[b(optional), c(non-optional)]`. Mirrors the
        // reference `codegen_dependency`.
        let has_optional = dep.path.iter().any(|p| p.optional);
        for entry in &dep.path {
            expr = self.member(expr, &entry.property);
            if has_optional {
                expr = self.to_optional(expr, entry.optional)?;
            }
        }
        Ok(expr)
    }

    /// Build the dependency's fully-qualified sort key, mirroring the reference
    /// `compareScopeDependency`:
    /// `[identifier.name.value, ...path.map(e => `${e.optional ? '?' : ''}${e.property}`)].join('.')`.
    /// The resulting strings are then compared lexicographically.
    fn scope_dependency_sort_key(
        &self,
        dep: &react_compiler_hir::ReactiveScopeDependency,
    ) -> Bail<String> {
        let mut key = self.ident_name(dep.identifier)?;
        for entry in &dep.path {
            key.push('.');
            if entry.optional {
                key.push('?');
            }
            use std::fmt::Write;
            let _ = write!(key, "{}", entry.property);
        }
        Ok(key)
    }
}

// =============================================================================
// Free helpers
// =============================================================================

/// True if a JSX **attribute** string value cannot be faithfully reproduced
/// inside a bare `"…"` attribute and must instead be emitted as an expression
/// container (`attr={"…"}`). Mirrors the reference
/// `STRING_REQUIRES_EXPR_CONTAINER_PATTERN`:
/// `/[\u{0000}-\u{001F}\u{007F}\u{0080}-\u{FFFF}\u{010000}-\u{10FFFF}]|"|\\/u`.
/// In other words: any control code, the `"` or `\` characters, or any
/// non-basic-Latin character (everything at or above U+0080, which subsumes the
/// astral plane).
fn string_requires_expr_container(s: &str) -> bool {
    s.chars().any(|c| {
        c == '"'
            || c == '\\'
            || (c as u32) <= 0x001F
            || (c as u32) == 0x007F
            || (c as u32) >= 0x0080
    })
}

/// True if a JSX **text child** must be emitted as an expression container
/// (`{"…"}`) rather than raw `JSXText`. Mirrors the reference
/// `JSX_TEXT_CHILD_REQUIRES_EXPR_CONTAINER_PATTERN = /[<>&{}]/`.
fn jsx_text_requires_expr_container(s: &str) -> bool {
    s.chars().any(|c| matches!(c, '<' | '>' | '&' | '{' | '}'))
}

/// Generate a collision-safe name (mirrors `Context::synthesize_name`).
fn synthesize_name(base: &str, taken: &HashSet<String>) -> String {
    if !taken.contains(base) {
        return base.to_string();
    }
    let mut i = 0;
    loop {
        let candidate = format!("{base}{i}");
        if !taken.contains(&candidate) {
            return candidate;
        }
        i += 1;
    }
}

fn map_logical_op(op: LogicalOperator) -> OxcLogOp {
    match op {
        LogicalOperator::And => OxcLogOp::And,
        LogicalOperator::Or => OxcLogOp::Or,
        LogicalOperator::NullishCoalescing => OxcLogOp::Coalesce,
    }
}

fn map_unary_op(op: UnaryOperator) -> OxcUnOp {
    match op {
        UnaryOperator::Minus => OxcUnOp::UnaryNegation,
        UnaryOperator::Plus => OxcUnOp::UnaryPlus,
        UnaryOperator::Not => OxcUnOp::LogicalNot,
        UnaryOperator::BitwiseNot => OxcUnOp::BitwiseNot,
        UnaryOperator::TypeOf => OxcUnOp::Typeof,
        UnaryOperator::Void => OxcUnOp::Void,
    }
}

fn map_binary_op(op: BinaryOperator) -> Bail<OxcBinOp> {
    Ok(match op {
        BinaryOperator::Equal => OxcBinOp::Equality,
        BinaryOperator::NotEqual => OxcBinOp::Inequality,
        BinaryOperator::StrictEqual => OxcBinOp::StrictEquality,
        BinaryOperator::StrictNotEqual => OxcBinOp::StrictInequality,
        BinaryOperator::LessThan => OxcBinOp::LessThan,
        BinaryOperator::LessEqual => OxcBinOp::LessEqualThan,
        BinaryOperator::GreaterThan => OxcBinOp::GreaterThan,
        BinaryOperator::GreaterEqual => OxcBinOp::GreaterEqualThan,
        BinaryOperator::ShiftLeft => OxcBinOp::ShiftLeft,
        BinaryOperator::ShiftRight => OxcBinOp::ShiftRight,
        BinaryOperator::UnsignedShiftRight => OxcBinOp::ShiftRightZeroFill,
        BinaryOperator::Add => OxcBinOp::Addition,
        BinaryOperator::Subtract => OxcBinOp::Subtraction,
        BinaryOperator::Multiply => OxcBinOp::Multiplication,
        BinaryOperator::Divide => OxcBinOp::Division,
        BinaryOperator::Modulo => OxcBinOp::Remainder,
        BinaryOperator::Exponent => OxcBinOp::Exponential,
        BinaryOperator::BitwiseOr => OxcBinOp::BitwiseOR,
        BinaryOperator::BitwiseXor => OxcBinOp::BitwiseXOR,
        BinaryOperator::BitwiseAnd => OxcBinOp::BitwiseAnd,
        BinaryOperator::In => OxcBinOp::In,
        BinaryOperator::InstanceOf => OxcBinOp::Instanceof,
    })
}

fn iv_kind(iv: &InstructionValue) -> &'static str {
    match iv {
        InstructionValue::LoadLocal { .. } => "LoadLocal",
        InstructionValue::LoadContext { .. } => "LoadContext",
        InstructionValue::DeclareLocal { .. } => "DeclareLocal",
        InstructionValue::DeclareContext { .. } => "DeclareContext",
        InstructionValue::StoreLocal { .. } => "StoreLocal",
        InstructionValue::StoreContext { .. } => "StoreContext",
        InstructionValue::Destructure { .. } => "Destructure",
        InstructionValue::Primitive { .. } => "Primitive",
        InstructionValue::JSXText { .. } => "JSXText",
        InstructionValue::BinaryExpression { .. } => "BinaryExpression",
        InstructionValue::NewExpression { .. } => "NewExpression",
        InstructionValue::CallExpression { .. } => "CallExpression",
        InstructionValue::MethodCall { .. } => "MethodCall",
        InstructionValue::UnaryExpression { .. } => "UnaryExpression",
        InstructionValue::TypeCastExpression { .. } => "TypeCastExpression",
        InstructionValue::JsxExpression { .. } => "JsxExpression",
        InstructionValue::ObjectExpression { .. } => "ObjectExpression",
        InstructionValue::ObjectMethod { .. } => "ObjectMethod",
        InstructionValue::ArrayExpression { .. } => "ArrayExpression",
        InstructionValue::JsxFragment { .. } => "JsxFragment",
        InstructionValue::RegExpLiteral { .. } => "RegExpLiteral",
        InstructionValue::MetaProperty { .. } => "MetaProperty",
        InstructionValue::PropertyStore { .. } => "PropertyStore",
        InstructionValue::PropertyLoad { .. } => "PropertyLoad",
        InstructionValue::PropertyDelete { .. } => "PropertyDelete",
        InstructionValue::ComputedStore { .. } => "ComputedStore",
        InstructionValue::ComputedLoad { .. } => "ComputedLoad",
        InstructionValue::ComputedDelete { .. } => "ComputedDelete",
        InstructionValue::LoadGlobal { .. } => "LoadGlobal",
        InstructionValue::StoreGlobal { .. } => "StoreGlobal",
        InstructionValue::FunctionExpression { .. } => "FunctionExpression",
        InstructionValue::TaggedTemplateExpression { .. } => "TaggedTemplateExpression",
        InstructionValue::TemplateLiteral { .. } => "TemplateLiteral",
        InstructionValue::Await { .. } => "Await",
        InstructionValue::GetIterator { .. } => "GetIterator",
        InstructionValue::IteratorNext { .. } => "IteratorNext",
        InstructionValue::NextPropertyOf { .. } => "NextPropertyOf",
        InstructionValue::PrefixUpdate { .. } => "PrefixUpdate",
        InstructionValue::PostfixUpdate { .. } => "PostfixUpdate",
        InstructionValue::Debugger { .. } => "Debugger",
        InstructionValue::StartMemoize { .. } => "StartMemoize",
        InstructionValue::FinishMemoize { .. } => "FinishMemoize",
        InstructionValue::UnsupportedNode { .. } => "UnsupportedNode",
    }
}

/// Parse a regexp flags string (e.g. "gi") into oxc's `RegExpFlags` bitset.
/// Mirrors `parse_regexp_flags` in `convert_ast_reverse`.
fn parse_regexp_flags(flags_str: &str) -> oxc::RegExpFlags {
    let mut flags = oxc::RegExpFlags::empty();
    for ch in flags_str.chars() {
        match ch {
            'd' => flags |= oxc::RegExpFlags::D,
            'g' => flags |= oxc::RegExpFlags::G,
            'i' => flags |= oxc::RegExpFlags::I,
            'm' => flags |= oxc::RegExpFlags::M,
            's' => flags |= oxc::RegExpFlags::S,
            'u' => flags |= oxc::RegExpFlags::U,
            'v' => flags |= oxc::RegExpFlags::V,
            'y' => flags |= oxc::RegExpFlags::Y,
            _ => {}
        }
    }
    flags
}

/// Label name for a break/continue/labeled target (mirrors `codegen_label`).
fn codegen_label(id: react_compiler_hir::BlockId) -> String {
    format!("bb{}", id.0)
}

/// Whether an expression is the bare `undefined` identifier.
fn is_undefined_identifier(expr: &oxc::Expression) -> bool {
    matches!(expr, oxc::Expression::Identifier(id) if id.name.as_str() == "undefined")
}

/// Invoke `f` for each binding Place of a pattern (recursing into nested
/// patterns is unnecessary here: HIR destructure patterns are one level —
/// nested object/array patterns are lowered to separate Destructure
/// instructions).
fn for_each_pattern_place(pattern: &Pattern, mut f: impl FnMut(&Place)) {
    match pattern {
        Pattern::Array(arr) => {
            for item in &arr.items {
                match item {
                    ArrayPatternElement::Place(p) => f(p),
                    ArrayPatternElement::Spread(s) => f(&s.place),
                    ArrayPatternElement::Hole => {}
                }
            }
        }
        Pattern::Object(obj) => {
            for prop in &obj.properties {
                match prop {
                    ObjectPropertyOrSpread::Property(p) => f(&p.place),
                    ObjectPropertyOrSpread::Spread(s) => f(&s.place),
                }
            }
        }
    }
}

/// Whether an object-pattern key matches the bound value name (shorthand form).
fn object_key_matches_name(key: &ObjectPropertyKey, value_name: &str) -> bool {
    match key {
        ObjectPropertyKey::Identifier { name } | ObjectPropertyKey::String { name } => {
            name == value_name
        }
        _ => false,
    }
}

/// Map an `InstructionKind` to a `VariableDeclarationKind` for for-in/of lefts.
fn var_decl_kind(kind: InstructionKind) -> Bail<oxc::VariableDeclarationKind> {
    match kind {
        InstructionKind::Const | InstructionKind::HoistedConst => {
            Ok(oxc::VariableDeclarationKind::Const)
        }
        InstructionKind::Let | InstructionKind::HoistedLet => Ok(oxc::VariableDeclarationKind::Let),
        _ => bail!("invalid for-in/of binding kind"),
    }
}

/// Extract the single binding-identifier name from a binding pattern (used by
/// the for-init folding to match `let i; i = 0`).
fn binding_pattern_name<'a>(pat: &'a oxc::BindingPattern<'a>) -> Option<&'a str> {
    match pat {
        oxc::BindingPattern::BindingIdentifier(id) => Some(id.name.as_str()),
        _ => None,
    }
}
