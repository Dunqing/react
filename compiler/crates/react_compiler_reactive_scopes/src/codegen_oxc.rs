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
//! - `convert_ast_reverse.rs` — the oxc 0.121 `AstBuilder` CONSTRUCTION patterns.
//!
//! Instead of building `react_compiler_ast` then converting, this builds oxc
//! nodes in one pass.
//!
//! Scope: this is a vertical slice. The CORE constructs needed by simple
//! memoizing components are implemented; anything else returns [`CodegenBail`]
//! so the caller leaves the original function uncompiled (graceful bail).
//!
//! oxc `Expression<'a>` is arena-allocated and NOT `Clone`, so the inline
//! "temporaries" table from the reference (which stored built expressions) is
//! replaced here by a table of the HIR `InstructionValue` (which IS `Clone`):
//! a `Place` that references an inlined temporary re-builds its expression.

use std::collections::HashMap;
use std::collections::HashSet;

use oxc_allocator::Box as ArenaBox;
use oxc_allocator::FromIn;
use oxc_allocator::Vec as ArenaVec;
use oxc_ast::AstBuilder;
use oxc_ast::ast as oxc;
use oxc_span::Atom;
use oxc_span::SPAN;
use oxc_syntax::operator::BinaryOperator as OxcBinOp;
use oxc_syntax::operator::LogicalOperator as OxcLogOp;
use oxc_syntax::operator::UnaryOperator as OxcUnOp;

use react_compiler_hir::ArrayElement;
use react_compiler_hir::BinaryOperator;
use react_compiler_hir::DeclarationId;
use react_compiler_hir::IdentifierId;
use react_compiler_hir::InstructionKind;
use react_compiler_hir::InstructionValue;
use react_compiler_hir::JsxAttribute;
use react_compiler_hir::JsxTag;
use react_compiler_hir::LogicalOperator;
use react_compiler_hir::ObjectPropertyKey;
use react_compiler_hir::ObjectPropertyOrSpread;
use react_compiler_hir::ParamPattern;
use react_compiler_hir::Place;
use react_compiler_hir::PlaceOrSpread;
use react_compiler_hir::PrimitiveValue;
use react_compiler_hir::PropertyLiteral;
use react_compiler_hir::ScopeId;
use react_compiler_hir::UnaryOperator;
use react_compiler_hir::environment::Environment;
use react_compiler_hir::reactive::ReactiveBlock;
use react_compiler_hir::reactive::ReactiveFunction;
use react_compiler_hir::reactive::ReactiveScopeBlock;
use react_compiler_hir::reactive::ReactiveStatement;
use react_compiler_hir::reactive::ReactiveTerminal;
use react_compiler_hir::reactive::ReactiveValue;

/// Sentinel from the reference codegen; emitted as `Symbol.for("…")`.
const MEMO_CACHE_SENTINEL: &str = "react.memo_cache_sentinel";

/// Signals an unsupported construct. The caller leaves the function uncompiled.
#[derive(Debug)]
pub struct CodegenBail {
    pub reason: String,
}

impl CodegenBail {
    fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

type Bail<T> = Result<T, CodegenBail>;

macro_rules! bail {
    ($($arg:tt)*) => {
        return Err(CodegenBail::new(format!($($arg)*)))
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
pub fn codegen_oxc_function<'a, 'e>(
    func: &ReactiveFunction,
    env: &'e Environment,
    unique_identifiers: HashSet<String>,
    builder: &AstBuilder<'a>,
    memo_local_name: &str,
) -> Bail<OxcCodegenOutput<'a>> {
    let mut cx = Cx {
        b: *builder,
        env,
        next_cache_index: 0,
        cache_name: synthesize_name("$", &unique_identifiers),
        memo_local_name: memo_local_name.to_string(),
        temp: HashMap::new(),
        declared: HashSet::new(),
    };

    // Params: each param is registered (declared) so later writes reassign
    // rather than redeclare. Params are never inlined temporaries.
    let mut params: Vec<oxc::FormalParameter<'a>> = Vec::new();
    for p in &func.params {
        match p {
            ParamPattern::Place(place) => {
                let name = cx.place_name(place)?;
                cx.declared.insert(cx.decl_id(place));
                let pat = cx.binding_pattern(&name);
                params.push(cx.formal_param(pat));
            }
            ParamPattern::Spread(_) => bail!("spread param not yet supported"),
        }
    }

    // Body.
    let mut body_stmts: Vec<oxc::Statement<'a>> = Vec::new();
    cx.codegen_block(&func.body, &mut body_stmts)?;

    // Strip a trailing bare `return undefined;` (matches the reference).
    if let Some(oxc::Statement::ReturnStatement(r)) = body_stmts.last() {
        if r.argument.is_none() {
            body_stmts.pop();
        }
    }

    // Cache var preface: const $ = _c(N);
    let cache_count = cx.next_cache_index;
    if cache_count != 0 {
        let preface = cx.cache_var_decl(cache_count);
        body_stmts.insert(0, preface);
    }

    let function = cx.build_function_shell(func, params, body_stmts)?;

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
    memo_local_name: String,
    /// declaration_id -> HIR value to inline at use sites (None = bare ident).
    temp: HashMap<DeclarationId, Option<InstructionValue>>,
    /// declaration_ids that have been `let`/`const`/param declared.
    declared: HashSet<DeclarationId>,
}

impl<'a, 'e> Cx<'a, 'e> {
    fn atom(&self, s: &str) -> Atom<'a> {
        Atom::from_in(s, self.b.allocator)
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
        let callee = self.ident_expr(&self.memo_local_name);
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

    fn build_function_shell(
        &self,
        func: &ReactiveFunction,
        params: Vec<oxc::FormalParameter<'a>>,
        body_stmts: Vec<oxc::Statement<'a>>,
    ) -> Bail<oxc::Function<'a>> {
        if func.generator {
            bail!("generator function not yet supported");
        }
        let id = func
            .id
            .as_ref()
            .map(|name| self.b.binding_identifier(SPAN, self.atom(name)));
        let formal_params = self.b.formal_parameters(
            SPAN,
            oxc::FormalParameterKind::FormalParameter,
            self.b.vec_from_iter(params),
            None::<ArenaBox<'a, oxc::FormalParameterRest<'a>>>,
        );
        let body = self
            .b
            .function_body(SPAN, self.b.vec(), self.b.vec_from_iter(body_stmts));
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
        out: &mut Vec<oxc::Statement<'a>>,
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
                    self.codegen_terminal(&term.terminal, out)?;
                }
            }
        }
        Ok(())
    }

    fn codegen_instruction(
        &mut self,
        instr: &react_compiler_hir::reactive::ReactiveInstruction,
        out: &mut Vec<oxc::Statement<'a>>,
    ) -> Bail<()> {
        // Statement-level dispatch for stores/declares (mirrors
        // codegen_instruction_nullable).
        if let ReactiveValue::Instruction(iv) = &instr.value {
            match iv {
                InstructionValue::StoreLocal { lvalue, value, .. }
                | InstructionValue::StoreContext { lvalue, value, .. } => {
                    return self.codegen_store(lvalue, value, out);
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
            // Unnamed temporary -> inline at use sites.
            if let ReactiveValue::Instruction(iv) = &instr.value {
                self.temp.insert(decl_id, Some(iv.clone()));
                return Ok(());
            }
            // Compound reactive values can't be stashed as InstructionValue;
            // bail to keep correctness.
            bail!("unnamed temporary holds a compound reactive value");
        }

        // Named lvalue.
        let name = self.place_name(lvalue)?;
        let expr = self.codegen_value(&instr.value)?;
        if self.declared.contains(&decl_id) {
            // Reassignment: `name = expr;`
            let target = oxc::AssignmentTarget::AssignmentTargetIdentifier(
                self.b
                    .alloc(self.b.identifier_reference(SPAN, self.atom(&name))),
            );
            let assign = self.b.expression_assignment(
                SPAN,
                oxc_syntax::operator::AssignmentOperator::Assign,
                target,
                expr,
            );
            out.push(self.b.statement_expression(SPAN, assign));
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
        out: &mut Vec<oxc::Statement<'a>>,
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
                let target = oxc::AssignmentTarget::AssignmentTargetIdentifier(
                    self.b
                        .alloc(self.b.identifier_reference(SPAN, self.atom(&name))),
                );
                let assign = self.b.expression_assignment(
                    SPAN,
                    oxc_syntax::operator::AssignmentOperator::Assign,
                    target,
                    value_expr,
                );
                out.push(self.b.statement_expression(SPAN, assign));
            }
            InstructionKind::Function | InstructionKind::HoistedFunction => {
                bail!("function-kind store not yet supported")
            }
            InstructionKind::Catch => bail!("catch-kind store not yet supported"),
        }
        Ok(())
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
        let decl = self.b.variable_declaration(SPAN, kind, decls, false);
        oxc::Statement::VariableDeclaration(self.b.alloc(decl))
    }

    // ===== reactive scope (the memoization core) =====

    fn codegen_reactive_scope(
        &mut self,
        scope_block: &ReactiveScopeBlock,
        out: &mut Vec<oxc::Statement<'a>>,
    ) -> Bail<()> {
        let scope_id = scope_block.scope;
        let scope = self.scope(scope_id)?.clone();

        if scope.early_return_value.is_some() {
            bail!("reactive scope with early return not yet supported");
        }

        // --- Dependencies: one slot each (sorted for stable order). ---
        let mut deps = scope.dependencies.clone();
        deps.sort_by(|a, b| compare_scope_dependency(a, b));

        let mut change_exprs: Vec<oxc::Expression<'a>> = Vec::new();
        // (slot index, dependency expression-builder inputs)
        let mut dep_stores: Vec<(u32, oxc::Expression<'a>)> = Vec::new();
        for dep in &deps {
            let index = self.alloc_cache_index();
            let dep_expr = self.dependency_expr(dep)?;
            // `$[index] !== dep`
            let cmp = self.b.expression_binary(
                SPAN,
                self.cache_slot(index),
                OxcBinOp::StrictInequality,
                self.clone_expr_via_rebuild_dep(dep)?,
            );
            change_exprs.push(cmp);
            dep_stores.push((index, dep_expr));
        }

        // --- Declarations + reassignments: one slot each (sorted). ---
        let mut decls = scope.declarations.clone();
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
        for id in &scope.reassignments {
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
        let mut compute_stmts: Vec<oxc::Statement<'a>> = Vec::new();
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
        let mut else_stmts: Vec<oxc::Statement<'a>> = Vec::new();
        for (name, index) in &outputs {
            let target = oxc::AssignmentTarget::AssignmentTargetIdentifier(
                self.b
                    .alloc(self.b.identifier_reference(SPAN, self.atom(name))),
            );
            let assign = self.b.expression_assignment(
                SPAN,
                oxc_syntax::operator::AssignmentOperator::Assign,
                target,
                self.cache_slot(*index),
            );
            else_stmts.push(self.b.statement_expression(SPAN, assign));
        }

        let consequent = self
            .b
            .statement_block(SPAN, self.b.vec_from_iter(compute_stmts));
        let alternate = if else_stmts.is_empty() {
            None
        } else {
            Some(
                self.b
                    .statement_block(SPAN, self.b.vec_from_iter(else_stmts)),
            )
        };
        out.push(self.b.statement_if(SPAN, test, consequent, alternate));
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
        out: &mut Vec<oxc::Statement<'a>>,
    ) -> Bail<()> {
        match terminal {
            ReactiveTerminal::Return { value, .. } => {
                let name = self.place_name(value).ok();
                if name.as_deref() == Some("undefined") {
                    out.push(self.b.statement_return(SPAN, None));
                } else {
                    let expr = self.place_expr(value)?;
                    out.push(self.b.statement_return(SPAN, Some(expr)));
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
                let mut cons = Vec::new();
                self.codegen_block(consequent, &mut cons)?;
                let cons_stmt = self.b.statement_block(SPAN, self.b.vec_from_iter(cons));
                let alt_stmt = if let Some(alt) = alternate {
                    let mut alt_stmts = Vec::new();
                    self.codegen_block(alt, &mut alt_stmts)?;
                    Some(
                        self.b
                            .statement_block(SPAN, self.b.vec_from_iter(alt_stmts)),
                    )
                } else {
                    None
                };
                out.push(self.b.statement_if(SPAN, test_expr, cons_stmt, alt_stmt));
                Ok(())
            }
            other => bail!("terminal not yet supported: {:?}", terminal_kind(other)),
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
            ReactiveValue::SequenceExpression { .. } => {
                bail!("sequence expression not yet supported")
            }
            ReactiveValue::OptionalExpression { .. } => {
                bail!("optional expression not yet supported")
            }
        }
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
            other => bail!("instruction value not yet supported: {}", iv_kind(other)),
        }
    }

    /// Build the callee for a `MethodCall`. The `property` Place is a
    /// PropertyLoad temporary; we look it up in the temp table to find the
    /// member name, building `receiver.name`.
    fn method_callee(&mut self, receiver: &Place, property: &Place) -> Bail<oxc::Expression<'a>> {
        // Resolve the property temporary back to its PropertyLoad.
        let prop_decl = self.decl_id(property);
        if let Some(Some(InstructionValue::PropertyLoad {
            property: prop_lit, ..
        })) = self.temp.get(&prop_decl).cloned()
        {
            let obj = self.place_expr(receiver)?;
            return Ok(self.member(obj, &prop_lit));
        }
        // Fallback: computed member receiver[property].
        let obj = self.place_expr(receiver)?;
        let prop = self.place_expr(property)?;
        Ok(oxc::Expression::ComputedMemberExpression(self.b.alloc(
            self.b.computed_member_expression(SPAN, obj, prop, false),
        )))
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
                    if matches!(
                        prop.property_type,
                        react_compiler_hir::ObjectPropertyType::Method
                    ) {
                        bail!("object method not yet supported");
                    }
                    let (key, computed) = self.object_key(&prop.key)?;
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
                    let attr_name = self.b.jsx_attribute_name_identifier(SPAN, self.atom(name));
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

    fn jsx_element_name(&self, tag: &JsxTag) -> Bail<oxc::JSXElementName<'a>> {
        match tag {
            JsxTag::Builtin(builtin) => Ok(self
                .b
                .jsx_element_name_identifier(SPAN, self.atom(&builtin.name))),
            JsxTag::Place(place) => {
                let name = self.place_name(place)?;
                Ok(self
                    .b
                    .jsx_element_name_identifier_reference(SPAN, self.atom(&name)))
            }
        }
    }

    fn jsx_attribute_value(&mut self, place: &Place) -> Bail<oxc::JSXAttributeValue<'a>> {
        // String-literal shortcut for a primitive string temporary.
        let decl_id = self.decl_id(place);
        if let Some(Some(InstructionValue::Primitive {
            value: PrimitiveValue::String(s),
            ..
        })) = self.temp.get(&decl_id).cloned()
        {
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
        let decl_id = self.decl_id(place);
        if let Some(Some(InstructionValue::JSXText { value, .. })) =
            self.temp.get(&decl_id).cloned()
        {
            return Ok(self.b.jsx_child_text(SPAN, self.atom(&value), None));
        }
        // A nested JSX element temporary -> embed directly as a child element.
        if let Some(Some(iv)) = self.temp.get(&decl_id).cloned() {
            if let InstructionValue::JsxExpression { .. } | InstructionValue::JsxFragment { .. } =
                &iv
            {
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
        if let Some(entry) = self.temp.get(&decl_id) {
            if let Some(iv) = entry.clone() {
                return self.codegen_instruction_value(&iv);
            }
            // declared but no inline value -> bare identifier below.
        }
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
        for entry in &dep.path {
            if entry.optional {
                bail!("optional dependency path not yet supported");
            }
            expr = self.member(expr, &entry.property);
        }
        Ok(expr)
    }

    /// Dependencies are referenced twice (in the `!==` check and in the store);
    /// oxc Expressions aren't Clone, so we rebuild from the HIR each time.
    fn clone_expr_via_rebuild_dep(
        &self,
        dep: &react_compiler_hir::ReactiveScopeDependency,
    ) -> Bail<oxc::Expression<'a>> {
        self.dependency_expr(dep)
    }
}

// =============================================================================
// Free helpers
// =============================================================================

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

fn compare_scope_dependency(
    a: &react_compiler_hir::ReactiveScopeDependency,
    b: &react_compiler_hir::ReactiveScopeDependency,
) -> std::cmp::Ordering {
    a.identifier
        .0
        .cmp(&b.identifier.0)
        .then_with(|| a.path.len().cmp(&b.path.len()))
        .then_with(|| {
            for (pa, pb) in a.path.iter().zip(b.path.iter()) {
                let ord =
                    property_literal_key(&pa.property).cmp(&property_literal_key(&pb.property));
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            std::cmp::Ordering::Equal
        })
}

fn property_literal_key(p: &PropertyLiteral) -> String {
    match p {
        PropertyLiteral::String(s) => format!("s:{s}"),
        PropertyLiteral::Number(n) => format!("n:{}", n.value()),
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

fn terminal_kind(t: &ReactiveTerminal) -> &'static str {
    match t {
        ReactiveTerminal::Break { .. } => "Break",
        ReactiveTerminal::Continue { .. } => "Continue",
        ReactiveTerminal::Return { .. } => "Return",
        ReactiveTerminal::Throw { .. } => "Throw",
        ReactiveTerminal::Switch { .. } => "Switch",
        ReactiveTerminal::DoWhile { .. } => "DoWhile",
        ReactiveTerminal::While { .. } => "While",
        ReactiveTerminal::For { .. } => "For",
        ReactiveTerminal::ForOf { .. } => "ForOf",
        ReactiveTerminal::ForIn { .. } => "ForIn",
        ReactiveTerminal::If { .. } => "If",
        ReactiveTerminal::Label { .. } => "Label",
        ReactiveTerminal::Try { .. } => "Try",
    }
}
