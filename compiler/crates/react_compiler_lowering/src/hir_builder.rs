use indexmap::IndexMap;
use indexmap::IndexSet;
use oxc_semantic::Semantic;
use oxc_syntax::scope::ScopeId;
use oxc_syntax::symbol::SymbolId;
use react_compiler_diagnostics::CompilerDiagnostic;
use react_compiler_diagnostics::CompilerDiagnosticDetail;
use react_compiler_diagnostics::CompilerError;
use react_compiler_diagnostics::CompilerErrorDetail;
use react_compiler_diagnostics::ErrorCategory;
use react_compiler_hir::environment::Environment;
use react_compiler_hir::visitors::each_terminal_successor;
use react_compiler_hir::visitors::terminal_fallthrough;
use react_compiler_hir::*;

use crate::semantic_queries as sq;

// ---------------------------------------------------------------------------
// Reserved word check (matches TS isReservedWord)
// ---------------------------------------------------------------------------

pub(crate) fn is_always_reserved_word(s: &str) -> bool {
    matches!(
        s,
        "break"
            | "case"
            | "catch"
            | "continue"
            | "debugger"
            | "default"
            | "do"
            | "else"
            | "finally"
            | "for"
            | "function"
            | "if"
            | "in"
            | "instanceof"
            | "new"
            | "return"
            | "switch"
            | "this"
            | "throw"
            | "try"
            | "typeof"
            | "var"
            | "void"
            | "while"
            | "with"
            | "class"
            | "const"
            | "enum"
            | "export"
            | "extends"
            | "import"
            | "super"
            | "null"
            | "true"
            | "false"
            | "delete"
    )
}

pub(crate) fn reserved_identifier_diagnostic(name: &str) -> CompilerDiagnostic {
    CompilerDiagnostic::new(
        ErrorCategory::Syntax,
        "Expected a non-reserved identifier name",
        Some(format!(
            "`{}` is a reserved word in JavaScript and cannot be used as an identifier name",
            name
        )),
    )
    .with_detail(CompilerDiagnosticDetail::Error {
        loc: None, // GeneratedSource in TS
        message: Some("reserved word".to_string()),
        identifier_name: None,
    })
}

/// Graceful `Todo` bail used while the oxc-direct lowering is being transcribed.
///
/// During stage N1.2.1 only the function shell + trivial constructs lower for
/// real; every other construct records a `Todo` (caught by the fault-tolerant
/// pipeline) instead of panicking, so the crate stays green and trivial
/// fixtures still produce HIR.
pub(crate) fn todo_diagnostic(what: &str, loc: Option<SourceLocation>) -> CompilerDiagnostic {
    CompilerDiagnostic::new(
        ErrorCategory::Todo,
        "(BuildHIR::N1.2) Handle oxc-direct lowering",
        Some(format!("[BuildHIR] Not yet transcribed to oxc: {what}")),
    )
    .with_detail(CompilerDiagnosticDetail::Error {
        loc,
        message: Some(format!("unsupported (oxc port): {what}")),
        identifier_name: None,
    })
}

// ---------------------------------------------------------------------------
// Scope types for tracking break/continue targets
// ---------------------------------------------------------------------------

enum Scope {
    Loop {
        label: Option<String>,
        continue_block: BlockId,
        break_block: BlockId,
    },
    Label {
        label: String,
        break_block: BlockId,
    },
    Switch {
        label: Option<String>,
        break_block: BlockId,
    },
}

impl Scope {
    fn label(&self) -> Option<&str> {
        match self {
            Scope::Loop { label, .. } => label.as_deref(),
            Scope::Label { label, .. } => Some(label.as_str()),
            Scope::Switch { label, .. } => label.as_deref(),
        }
    }

    fn break_block(&self) -> BlockId {
        match self {
            Scope::Loop { break_block, .. } => *break_block,
            Scope::Label { break_block, .. } => *break_block,
            Scope::Switch { break_block, .. } => *break_block,
        }
    }
}

// ---------------------------------------------------------------------------
// WipBlock: a block under construction that does not yet have a terminal
// ---------------------------------------------------------------------------

pub struct WipBlock {
    pub id: BlockId,
    pub instructions: Vec<InstructionId>,
    pub kind: BlockKind,
}

fn new_block(id: BlockId, kind: BlockKind) -> WipBlock {
    WipBlock {
        id,
        kind,
        instructions: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// HirBuilder: helper struct for constructing a CFG
// ---------------------------------------------------------------------------

pub struct HirBuilder<'a> {
    completed: IndexMap<BlockId, BasicBlock>,
    current: WipBlock,
    entry: BlockId,
    scopes: Vec<Scope>,
    /// Context identifiers: variables captured from an outer scope.
    /// Maps the outer scope's SymbolId to the source location where it was referenced.
    context: IndexMap<SymbolId, Option<SourceLocation>>,
    /// Resolved bindings: maps a SymbolId to the HIR IdentifierId created for it.
    bindings: IndexMap<SymbolId, IdentifierId>,
    /// Names already used by bindings, for collision avoidance.
    used_names: IndexMap<String, SymbolId>,
    env: &'a mut Environment,
    /// oxc semantic model — the direct source of all scope/binding queries.
    semantic: &'a Semantic<'a>,
    /// The full source text (for span -> SourceLocation conversion).
    source_text: &'a str,
    exception_handler_stack: Vec<BlockId>,
    /// Flat instruction table being built up.
    instruction_table: Vec<Instruction>,
    /// Traversal context: counts the number of `fbt` tag parents
    /// of the current babel node.
    pub fbt_depth: u32,
    /// The scope of the function being compiled (for context identifier checks).
    function_scope: ScopeId,
    /// The scope of the outermost component/hook function (for gather_captured_context).
    component_scope: ScopeId,
    /// Set of SymbolIds for variables declared in scopes between component_scope
    /// and any inner function scope, that are referenced from an inner function scope.
    context_identifiers: std::collections::HashSet<SymbolId>,
    /// Set of ScopeIds that have been matched to synthetic blocks/functions.
    claimed_synthetic_scopes: std::collections::HashSet<ScopeId>,
}

impl<'a> HirBuilder<'a> {
    /// Create a new HirBuilder over the oxc semantic model.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        env: &'a mut Environment,
        semantic: &'a Semantic<'a>,
        source_text: &'a str,
        function_scope: ScopeId,
        component_scope: ScopeId,
        context_identifiers: std::collections::HashSet<SymbolId>,
        bindings: Option<IndexMap<SymbolId, IdentifierId>>,
        context: Option<IndexMap<SymbolId, Option<SourceLocation>>>,
        entry_block_kind: Option<BlockKind>,
        used_names: Option<IndexMap<String, SymbolId>>,
    ) -> Self {
        let entry = env.next_block_id();
        let kind = entry_block_kind.unwrap_or(BlockKind::Block);
        HirBuilder {
            completed: IndexMap::new(),
            current: new_block(entry, kind),
            entry,
            scopes: Vec::new(),
            context: context.unwrap_or_default(),
            bindings: bindings.unwrap_or_default(),
            used_names: used_names.unwrap_or_default(),
            env,
            semantic,
            source_text,
            exception_handler_stack: Vec::new(),
            instruction_table: Vec::new(),
            fbt_depth: 0,
            function_scope,
            component_scope,
            context_identifiers,
            claimed_synthetic_scopes: std::collections::HashSet::new(),
        }
    }

    /// Check if a scope is the component scope or a descendant of it.
    fn is_scope_within_compiled_function(&self, scope_id: ScopeId) -> bool {
        let mut current = Some(scope_id);
        while let Some(id) = current {
            if id == self.component_scope {
                return true;
            }
            current = sq::scope_parent(self.semantic, id);
        }
        false
    }

    /// Access the environment.
    pub fn environment(&self) -> &Environment {
        self.env
    }

    /// Access the environment mutably.
    pub fn environment_mut(&mut self) -> &mut Environment {
        self.env
    }

    /// Access the oxc semantic model.
    pub fn semantic(&self) -> &'a Semantic<'a> {
        self.semantic
    }

    /// Access the source text.
    pub fn source_text(&self) -> &'a str {
        self.source_text
    }

    /// Create a new unique TypeVar type, allocated from the environment's type arena.
    pub fn make_type(&mut self) -> Type {
        let type_id = self.env.make_type();
        Type::TypeVar { id: type_id }
    }

    /// Access the function scope (the scope of the function being compiled).
    pub fn function_scope(&self) -> ScopeId {
        self.function_scope
    }

    /// Access the component scope.
    pub fn component_scope(&self) -> ScopeId {
        self.component_scope
    }

    /// Access the context map.
    pub fn context(&self) -> &IndexMap<SymbolId, Option<SourceLocation>> {
        &self.context
    }

    /// Access the pre-computed context identifiers set.
    pub fn context_identifiers(&self) -> &std::collections::HashSet<SymbolId> {
        &self.context_identifiers
    }

    /// Add a binding to the context identifiers set (used by hoisting).
    pub fn add_context_identifier(&mut self, symbol_id: SymbolId) {
        self.context_identifiers.insert(symbol_id);
    }

    pub fn claim_synthetic_scope(&mut self, scope_id: ScopeId) {
        self.claimed_synthetic_scopes.insert(scope_id);
    }

    pub fn is_synthetic_scope_claimed(&self, scope_id: ScopeId) -> bool {
        self.claimed_synthetic_scopes.contains(&scope_id)
    }

    /// Access the bindings map.
    pub fn bindings(&self) -> &IndexMap<SymbolId, IdentifierId> {
        &self.bindings
    }

    /// Access the used names map.
    pub fn used_names(&self) -> &IndexMap<String, SymbolId> {
        &self.used_names
    }

    /// Merge used names from a child builder back into this builder.
    pub fn merge_used_names(&mut self, child_used_names: IndexMap<String, SymbolId>) {
        for (name, symbol_id) in child_used_names {
            self.used_names.entry(name).or_insert(symbol_id);
        }
    }

    /// Merge bindings (symbol_id -> IdentifierId) from a child builder back into this builder.
    pub fn merge_bindings(&mut self, child_bindings: IndexMap<SymbolId, IdentifierId>) {
        for (symbol_id, identifier_id) in child_bindings {
            self.bindings.entry(symbol_id).or_insert(identifier_id);
        }
    }

    /// Convert an oxc span to an HIR SourceLocation using the source text.
    pub fn loc_of_span(&self, span: oxc_span::Span) -> SourceLocation {
        crate::build_hir::span_to_location(self.source_text, span)
    }

    /// Push an instruction onto the current block.
    pub fn push(&mut self, instruction: Instruction) {
        let loc = instruction.loc;
        let instr_id = InstructionId(self.instruction_table.len() as u32);
        self.instruction_table.push(instruction);
        self.current.instructions.push(instr_id);

        if let Some(&handler) = self.exception_handler_stack.last() {
            let continuation = self.reserve(self.current_block_kind());
            self.terminate_with_continuation(
                Terminal::MaybeThrow {
                    continuation: continuation.id,
                    handler: Some(handler),
                    id: EvaluationOrder(0),
                    loc,
                    effects: None,
                },
                continuation,
            );
        }
    }

    /// Terminate the current block with the given terminal and start a new block.
    pub fn terminate(&mut self, terminal: Terminal, next_block_kind: Option<BlockKind>) -> BlockId {
        let wip = std::mem::replace(
            &mut self.current,
            new_block(BlockId(u32::MAX), BlockKind::Block),
        );
        let block_id = wip.id;

        self.completed.insert(
            block_id,
            BasicBlock {
                kind: wip.kind,
                id: block_id,
                instructions: wip.instructions,
                terminal,
                preds: IndexSet::new(),
                phis: Vec::new(),
            },
        );

        if let Some(kind) = next_block_kind {
            let next_id = self.env.next_block_id();
            self.current = new_block(next_id, kind);
        }
        block_id
    }

    /// Terminate the current block with the given terminal, and set
    /// a previously reserved block as the new current block.
    pub fn terminate_with_continuation(&mut self, terminal: Terminal, continuation: WipBlock) {
        let wip = std::mem::replace(&mut self.current, continuation);
        let block_id = wip.id;
        self.completed.insert(
            block_id,
            BasicBlock {
                kind: wip.kind,
                id: block_id,
                instructions: wip.instructions,
                terminal,
                preds: IndexSet::new(),
                phis: Vec::new(),
            },
        );
    }

    /// Reserve a new block so it can be referenced before construction.
    pub fn reserve(&mut self, kind: BlockKind) -> WipBlock {
        let id = self.env.next_block_id();
        new_block(id, kind)
    }

    /// Save a previously reserved block as completed with the given terminal.
    pub fn complete(&mut self, block: WipBlock, terminal: Terminal) {
        let block_id = block.id;
        self.completed.insert(
            block_id,
            BasicBlock {
                kind: block.kind,
                id: block_id,
                instructions: block.instructions,
                terminal,
                preds: IndexSet::new(),
                phis: Vec::new(),
            },
        );
    }

    /// Sets the given wip block as current, executes the closure to populate
    /// it and obtain its terminal, then completes the block and restores the
    /// previous current block.
    pub fn enter_reserved(&mut self, wip: WipBlock, f: impl FnOnce(&mut Self) -> Terminal) {
        let prev = std::mem::replace(&mut self.current, wip);
        let terminal = f(self);
        let completed_wip = std::mem::replace(&mut self.current, prev);
        self.completed.insert(
            completed_wip.id,
            BasicBlock {
                kind: completed_wip.kind,
                id: completed_wip.id,
                instructions: completed_wip.instructions,
                terminal,
                preds: IndexSet::new(),
                phis: Vec::new(),
            },
        );
    }

    /// Like `enter_reserved`, but the closure returns a `Result<Terminal, CompilerDiagnostic>`.
    pub fn try_enter_reserved(
        &mut self,
        wip: WipBlock,
        f: impl FnOnce(&mut Self) -> Result<Terminal, CompilerDiagnostic>,
    ) -> Result<(), CompilerDiagnostic> {
        let prev = std::mem::replace(&mut self.current, wip);
        let terminal = f(self)?;
        let completed_wip = std::mem::replace(&mut self.current, prev);
        self.completed.insert(
            completed_wip.id,
            BasicBlock {
                kind: completed_wip.kind,
                id: completed_wip.id,
                instructions: completed_wip.instructions,
                terminal,
                preds: IndexSet::new(),
                phis: Vec::new(),
            },
        );
        Ok(())
    }

    /// Create a new block, set it as current, run the closure, complete it, restore.
    pub fn enter(
        &mut self,
        kind: BlockKind,
        f: impl FnOnce(&mut Self, BlockId) -> Terminal,
    ) -> BlockId {
        let wip = self.reserve(kind);
        let wip_id = wip.id;
        self.enter_reserved(wip, |this| f(this, wip_id));
        wip_id
    }

    /// Like `enter`, but the closure returns a `Result<Terminal, CompilerDiagnostic>`.
    pub fn try_enter(
        &mut self,
        kind: BlockKind,
        f: impl FnOnce(&mut Self, BlockId) -> Result<Terminal, CompilerDiagnostic>,
    ) -> Result<BlockId, CompilerDiagnostic> {
        let wip = self.reserve(kind);
        let wip_id = wip.id;
        self.try_enter_reserved(wip, |this| f(this, wip_id))?;
        Ok(wip_id)
    }

    /// Push an exception handler, run the closure, then pop the handler.
    pub fn enter_try_catch(&mut self, handler: BlockId, f: impl FnOnce(&mut Self)) {
        self.exception_handler_stack.push(handler);
        f(self);
        self.exception_handler_stack.pop();
    }

    /// Like `enter_try_catch`, but the closure returns a `Result`.
    pub fn try_enter_try_catch(
        &mut self,
        handler: BlockId,
        f: impl FnOnce(&mut Self) -> Result<(), CompilerDiagnostic>,
    ) -> Result<(), CompilerDiagnostic> {
        self.exception_handler_stack.push(handler);
        let result = f(self);
        self.exception_handler_stack.pop();
        result
    }

    /// Return the top of the exception handler stack, or None.
    pub fn resolve_throw_handler(&self) -> Option<BlockId> {
        self.exception_handler_stack.last().copied()
    }

    /// Push a Loop scope, run the closure, pop and verify.
    pub fn loop_scope<T>(
        &mut self,
        label: Option<String>,
        continue_block: BlockId,
        break_block: BlockId,
        f: impl FnOnce(&mut Self) -> Result<T, CompilerDiagnostic>,
    ) -> Result<T, CompilerDiagnostic> {
        self.scopes.push(Scope::Loop {
            label: label.clone(),
            continue_block,
            break_block,
        });
        let value = f(self)?;
        let last = self
            .scopes
            .pop()
            .expect("Mismatched loop scope: stack empty");
        match &last {
            Scope::Loop {
                label: l,
                continue_block: c,
                break_block: b,
            } => {
                assert!(
                    *l == label && *c == continue_block && *b == break_block,
                    "Mismatched loop scope"
                );
            }
            _ => {
                return Err(CompilerDiagnostic::new(
                    ErrorCategory::Invariant,
                    "Mismatched loop scope: expected Loop, got other",
                    None,
                ));
            }
        }
        Ok(value)
    }

    /// Push a Label scope, run the closure, pop and verify.
    pub fn label_scope<T>(
        &mut self,
        label: String,
        break_block: BlockId,
        f: impl FnOnce(&mut Self) -> Result<T, CompilerDiagnostic>,
    ) -> Result<T, CompilerDiagnostic> {
        self.scopes.push(Scope::Label {
            label: label.clone(),
            break_block,
        });
        let value = f(self)?;
        let last = self
            .scopes
            .pop()
            .expect("Mismatched label scope: stack empty");
        match &last {
            Scope::Label {
                label: l,
                break_block: b,
            } => {
                assert!(*l == label && *b == break_block, "Mismatched label scope");
            }
            _ => {
                return Err(CompilerDiagnostic::new(
                    ErrorCategory::Invariant,
                    "Mismatched label scope: expected Label, got other",
                    None,
                ));
            }
        }
        Ok(value)
    }

    /// Push a Switch scope, run the closure, pop and verify.
    pub fn switch_scope<T>(
        &mut self,
        label: Option<String>,
        break_block: BlockId,
        f: impl FnOnce(&mut Self) -> Result<T, CompilerDiagnostic>,
    ) -> Result<T, CompilerDiagnostic> {
        self.scopes.push(Scope::Switch {
            label: label.clone(),
            break_block,
        });
        let value = f(self)?;
        let last = self
            .scopes
            .pop()
            .expect("Mismatched switch scope: stack empty");
        match &last {
            Scope::Switch {
                label: l,
                break_block: b,
            } => {
                assert!(*l == label && *b == break_block, "Mismatched switch scope");
            }
            _ => {
                return Err(CompilerDiagnostic::new(
                    ErrorCategory::Invariant,
                    "Mismatched switch scope: expected Switch, got other",
                    None,
                ));
            }
        }
        Ok(value)
    }

    /// Look up the break target for the given label.
    pub fn lookup_break(&self, label: Option<&str>) -> Result<BlockId, CompilerDiagnostic> {
        for scope in self.scopes.iter().rev() {
            match scope {
                Scope::Loop { .. } | Scope::Switch { .. } if label.is_none() => {
                    return Ok(scope.break_block());
                }
                _ if label.is_some() && scope.label() == label => {
                    return Ok(scope.break_block());
                }
                _ => continue,
            }
        }
        Err(CompilerDiagnostic::new(
            ErrorCategory::Invariant,
            "Expected a loop or switch to be in scope for break",
            None,
        ))
    }

    /// Look up the continue target for the given label.
    pub fn lookup_continue(&self, label: Option<&str>) -> Result<BlockId, CompilerDiagnostic> {
        for scope in self.scopes.iter().rev() {
            match scope {
                Scope::Loop {
                    label: scope_label,
                    continue_block,
                    ..
                } => {
                    if label.is_none() || label == scope_label.as_deref() {
                        return Ok(*continue_block);
                    }
                }
                _ => {
                    if label.is_some() && scope.label() == label {
                        return Err(CompilerDiagnostic::new(
                            ErrorCategory::Invariant,
                            "Continue may only refer to a labeled loop",
                            None,
                        ));
                    }
                }
            }
        }
        Err(CompilerDiagnostic::new(
            ErrorCategory::Invariant,
            "Expected a loop to be in scope for continue",
            None,
        ))
    }

    /// Create a temporary identifier with a fresh id, returning its IdentifierId.
    pub fn make_temporary(&mut self, loc: Option<SourceLocation>) -> IdentifierId {
        let id = self.env.next_identifier_id();
        self.env.identifiers[id.0 as usize].loc = loc;
        id
    }

    /// Set the source location for an identifier.
    pub fn set_identifier_loc(&mut self, id: IdentifierId, loc: Option<SourceLocation>) {
        self.env.identifiers[id.0 as usize].loc = loc;
    }

    /// Record an error on the environment.
    pub fn record_error(&mut self, error: CompilerErrorDetail) -> Result<(), CompilerError> {
        self.env.record_error(error)
    }

    /// Record a diagnostic on the environment.
    pub fn record_diagnostic(&mut self, diagnostic: CompilerDiagnostic) {
        self.env.record_diagnostic(diagnostic);
    }

    /// Check if a name has a local (non-module-level) binding within the
    /// compiled function. Used for fbt/fbs JSX tag checks.
    pub fn has_local_binding(&self, name: &str) -> bool {
        if let Some(symbol_id) = sq::get_binding(self.semantic, self.component_scope, name) {
            let scope = self.semantic.scoping().symbol_scope_id(symbol_id);
            return scope != sq::program_scope(self.semantic);
        }
        false
    }

    /// Return the kind of the current block.
    pub fn current_block_kind(&self) -> BlockKind {
        self.current.kind
    }

    /// Construct the final HIR and instruction table from the completed blocks.
    pub fn build(
        mut self,
    ) -> Result<
        (
            HIR,
            Vec<Instruction>,
            IndexMap<String, SymbolId>,
            IndexMap<SymbolId, IdentifierId>,
        ),
        CompilerError,
    > {
        let mut hir = HIR {
            blocks: std::mem::take(&mut self.completed),
            entry: self.entry,
        };

        let mut instructions = std::mem::take(&mut self.instruction_table);

        let rpo_blocks = get_reverse_postordered_blocks(&hir, &instructions);

        for (id, block) in &hir.blocks {
            if !rpo_blocks.contains_key(id) {
                let has_function_expr = block.instructions.iter().any(|&instr_id| {
                    matches!(
                        instructions[instr_id.0 as usize].value,
                        InstructionValue::FunctionExpression { .. }
                    )
                });
                if has_function_expr {
                    let loc = block
                        .instructions
                        .first()
                        .and_then(|&i| instructions[i.0 as usize].loc)
                        .or_else(|| block.terminal.loc().copied());
                    self.env.record_error(CompilerErrorDetail {
                        category: ErrorCategory::Todo,
                        reason: "Support functions with unreachable code that may contain hoisted declarations".to_string(),
                        description: None,
                        loc,
                        suggestions: None,
                    })?;
                }
            }
        }

        hir.blocks = rpo_blocks;

        remove_unreachable_for_updates(&mut hir);
        remove_dead_do_while_statements(&mut hir);
        remove_unnecessary_try_catch(&mut hir);
        mark_instruction_ids(&mut hir, &mut instructions);
        mark_predecessors(&mut hir);

        let used_names = self.used_names;
        let bindings = self.bindings;
        Ok((hir, instructions, used_names, bindings))
    }

    // -----------------------------------------------------------------------
    // Binding resolution methods (oxc SymbolId-keyed)
    // -----------------------------------------------------------------------

    /// Map a SymbolId to an HIR IdentifierId.
    pub fn resolve_binding(
        &mut self,
        name: &str,
        symbol_id: SymbolId,
    ) -> Result<IdentifierId, CompilerError> {
        self.resolve_binding_with_loc(name, symbol_id, None)
    }

    /// Map a SymbolId to an HIR IdentifierId, with an optional source location.
    pub fn resolve_binding_with_loc(
        &mut self,
        name: &str,
        symbol_id: SymbolId,
        loc: Option<SourceLocation>,
    ) -> Result<IdentifierId, CompilerError> {
        if name == "fbt" {
            let should_record_fbt_error =
                if let Some(&identifier_id) = self.bindings.get(&symbol_id) {
                    match &self.env.identifiers[identifier_id.0 as usize].name {
                        Some(IdentifierName::Named(resolved_name)) => resolved_name == "fbt",
                        _ => false,
                    }
                } else {
                    true
                };
            if should_record_fbt_error {
                let decl_span = sq::declaration_span(self.semantic, symbol_id);
                let error_loc = Some(self.loc_of_span(decl_span)).or(loc);
                self.env.record_error(CompilerErrorDetail {
                    category: ErrorCategory::Todo,
                    reason: "Support local variables named `fbt`".to_string(),
                    description: Some(
                        "Local variables named `fbt` may conflict with the fbt plugin and are not yet supported".to_string(),
                    ),
                    loc: error_loc,
                    suggestions: None,
                })?;
            }
        }

        if let Some(&identifier_id) = self.bindings.get(&symbol_id) {
            return Ok(identifier_id);
        }

        if is_always_reserved_word(name) {
            return Err(CompilerError::from(reserved_identifier_diagnostic(name)));
        }

        // Find a unique name: start with the original name, then name_0, name_1, ...
        let mut candidate = name.to_string();
        let mut index = 0u32;
        loop {
            if let Some(&existing_symbol_id) = self.used_names.get(&candidate) {
                if existing_symbol_id == symbol_id {
                    break;
                }
                candidate = format!("{}_{}", name, index);
                index += 1;
            } else {
                break;
            }
        }

        let decl_span = sq::declaration_span(self.semantic, symbol_id);
        if candidate != name {
            self.env
                .renames
                .push(react_compiler_hir::environment::BindingRename {
                    original: name.to_string(),
                    renamed: candidate.clone(),
                    declaration_start: decl_span.start,
                });
        }

        let id = self.env.next_identifier_id();
        self.env.identifiers[id.0 as usize].name = Some(IdentifierName::Named(candidate.clone()));
        // Prefer the binding's declaration loc over the reference loc.
        let decl_loc = self.loc_of_span(decl_span);
        self.env.identifiers[id.0 as usize].loc = Some(decl_loc);

        self.used_names.insert(candidate, symbol_id);
        self.bindings.insert(symbol_id, id);
        Ok(id)
    }

    /// Set the loc on an identifier to the declaration-site loc.
    pub fn set_identifier_declaration_loc(
        &mut self,
        id: IdentifierId,
        loc: &Option<SourceLocation>,
    ) {
        if let Some(loc_val) = loc {
            self.env.identifiers[id.0 as usize].loc = Some(*loc_val);
        }
    }

    /// Resolve an identifier reference (by its resolved SymbolId) to a VariableBinding.
    pub fn resolve_identifier_symbol(
        &mut self,
        name: &str,
        symbol_id: Option<SymbolId>,
        loc: Option<SourceLocation>,
    ) -> Result<VariableBinding, CompilerError> {
        match symbol_id {
            None => Ok(VariableBinding::Global {
                name: name.to_string(),
            }),
            Some(symbol_id) => {
                let scoping = self.semantic.scoping();
                let binding_scope = scoping.symbol_scope_id(symbol_id);
                let program_scope = sq::program_scope(self.semantic);

                // Type-only declarations are treated as globals.
                let kind = sq::binding_kind(self.semantic, symbol_id);
                let decl_node = self.semantic.symbol_declaration(symbol_id);
                use oxc_ast::AstKind;
                if matches!(
                    decl_node.kind(),
                    AstKind::TSTypeAliasDeclaration(_)
                        | AstKind::TSInterfaceDeclaration(_)
                        | AstKind::TSEnumDeclaration(_)
                        | AstKind::TSModuleDeclaration(_)
                ) {
                    return Ok(VariableBinding::Global {
                        name: name.to_string(),
                    });
                }

                if binding_scope == program_scope {
                    Ok(match sq::import_info(self.semantic, symbol_id) {
                        Some(import_info) => match import_info.kind {
                            sq::ImportBindingKind::Default => VariableBinding::ImportDefault {
                                name: name.to_string(),
                                module: import_info.source,
                            },
                            sq::ImportBindingKind::Named => VariableBinding::ImportSpecifier {
                                name: name.to_string(),
                                module: import_info.source,
                                imported: import_info
                                    .imported
                                    .unwrap_or_else(|| name.to_string()),
                            },
                            sq::ImportBindingKind::Namespace => VariableBinding::ImportNamespace {
                                name: name.to_string(),
                                module: import_info.source,
                            },
                        },
                        None => VariableBinding::ModuleLocal {
                            name: name.to_string(),
                        },
                    })
                } else if !self.is_scope_within_compiled_function(binding_scope) {
                    Ok(VariableBinding::ModuleLocal {
                        name: name.to_string(),
                    })
                } else {
                    let binding_kind = crate::convert_binding_kind(&kind);
                    let identifier_id = self.resolve_binding_with_loc(name, symbol_id, loc)?;
                    Ok(VariableBinding::Identifier {
                        identifier: identifier_id,
                        binding_kind,
                    })
                }
            }
        }
    }

    /// Whether a resolved symbol is a captured context identifier.
    pub fn is_context_symbol(&self, symbol_id: Option<SymbolId>) -> bool {
        match symbol_id {
            None => false,
            Some(symbol_id) => {
                let scope = self.semantic.scoping().symbol_scope_id(symbol_id);
                if scope == sq::program_scope(self.semantic) {
                    return false;
                }
                self.context_identifiers.contains(&symbol_id)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Post-build helper functions (CFG-only; unchanged from the bridge version)
// ---------------------------------------------------------------------------

/// Compute a reverse-postorder of blocks reachable from the entry.
pub fn get_reverse_postordered_blocks(
    hir: &HIR,
    _instructions: &[Instruction],
) -> IndexMap<BlockId, BasicBlock> {
    let mut visited: IndexSet<BlockId> = IndexSet::new();
    let mut used: IndexSet<BlockId> = IndexSet::new();
    let mut used_fallthroughs: IndexSet<BlockId> = IndexSet::new();
    let mut postorder: Vec<BlockId> = Vec::new();

    fn visit(
        hir: &HIR,
        block_id: BlockId,
        is_used: bool,
        visited: &mut IndexSet<BlockId>,
        used: &mut IndexSet<BlockId>,
        used_fallthroughs: &mut IndexSet<BlockId>,
        postorder: &mut Vec<BlockId>,
    ) {
        let was_used = used.contains(&block_id);
        let was_visited = visited.contains(&block_id);
        visited.insert(block_id);
        if is_used {
            used.insert(block_id);
        }
        if was_visited && (was_used || !is_used) {
            return;
        }

        let block = hir
            .blocks
            .get(&block_id)
            .unwrap_or_else(|| panic!("[HIRBuilder] expected block {:?} to exist", block_id));

        let mut successors = each_terminal_successor(&block.terminal);
        successors.reverse();

        let fallthrough = terminal_fallthrough(&block.terminal);

        if let Some(ft) = fallthrough {
            if is_used {
                used_fallthroughs.insert(ft);
            }
            visit(hir, ft, false, visited, used, used_fallthroughs, postorder);
        }
        for successor in successors {
            visit(
                hir,
                successor,
                is_used,
                visited,
                used,
                used_fallthroughs,
                postorder,
            );
        }

        if !was_visited {
            postorder.push(block_id);
        }
    }

    visit(
        hir,
        hir.entry,
        true,
        &mut visited,
        &mut used,
        &mut used_fallthroughs,
        &mut postorder,
    );

    let mut blocks = IndexMap::new();
    for block_id in postorder.into_iter().rev() {
        let block = hir.blocks.get(&block_id).unwrap();
        if used.contains(&block_id) {
            blocks.insert(block_id, block.clone());
        } else if used_fallthroughs.contains(&block_id) {
            blocks.insert(
                block_id,
                BasicBlock {
                    kind: block.kind,
                    id: block_id,
                    instructions: Vec::new(),
                    terminal: Terminal::Unreachable {
                        id: block.terminal.evaluation_order(),
                        loc: block.terminal.loc().copied(),
                    },
                    preds: block.preds.clone(),
                    phis: Vec::new(),
                },
            );
        }
    }

    blocks
}

/// For each block with a `For` terminal whose update block is gone, drop update.
pub fn remove_unreachable_for_updates(hir: &mut HIR) {
    let block_ids: IndexSet<BlockId> = hir.blocks.keys().copied().collect();
    for block in hir.blocks.values_mut() {
        if let Terminal::For { update, .. } = &mut block.terminal
            && let Some(update_id) = *update
                && !block_ids.contains(&update_id) {
                    *update = None;
                }
    }
}

/// For each block with a `DoWhile` terminal whose test block is gone, replace with Goto.
pub fn remove_dead_do_while_statements(hir: &mut HIR) {
    let block_ids: IndexSet<BlockId> = hir.blocks.keys().copied().collect();
    for block in hir.blocks.values_mut() {
        let should_replace = if let Terminal::DoWhile { test, .. } = &block.terminal {
            !block_ids.contains(test)
        } else {
            false
        };
        if should_replace
            && let Terminal::DoWhile {
                loop_block,
                id,
                loc,
                ..
            } = std::mem::replace(
                &mut block.terminal,
                Terminal::Unreachable {
                    id: EvaluationOrder(0),
                    loc: None,
                },
            ) {
                block.terminal = Terminal::Goto {
                    block: loop_block,
                    variant: GotoVariant::Break,
                    id,
                    loc,
                };
            }
    }
}

/// For each block with a `Try` terminal whose handler block is gone, replace with Goto.
pub fn remove_unnecessary_try_catch(hir: &mut HIR) {
    let block_ids: IndexSet<BlockId> = hir.blocks.keys().copied().collect();

    let replacements: Vec<(BlockId, BlockId, BlockId, BlockId, Option<SourceLocation>)> = hir
        .blocks
        .iter()
        .filter_map(|(&block_id, block)| {
            if let Terminal::Try {
                block: try_block,
                handler,
                fallthrough,
                loc,
                ..
            } = &block.terminal
                && !block_ids.contains(handler) {
                    return Some((block_id, *try_block, *handler, *fallthrough, *loc));
                }
            None
        })
        .collect();

    for (block_id, try_block, handler_id, fallthrough_id, loc) in replacements {
        if let Some(block) = hir.blocks.get_mut(&block_id) {
            block.terminal = Terminal::Goto {
                block: try_block,
                id: EvaluationOrder(0),
                loc,
                variant: GotoVariant::Break,
            };
        }

        if let Some(fallthrough) = hir.blocks.get_mut(&fallthrough_id) {
            if fallthrough.preds.len() == 1 && fallthrough.preds.contains(&handler_id) {
                hir.blocks.shift_remove(&fallthrough_id);
            } else {
                fallthrough.preds.shift_remove(&handler_id);
            }
        }
    }
}

/// Sequentially number all instructions and terminals starting from 1.
pub fn mark_instruction_ids(hir: &mut HIR, instructions: &mut [Instruction]) {
    let mut order: u32 = 0;
    for block in hir.blocks.values_mut() {
        for &instr_id in &block.instructions {
            order += 1;
            instructions[instr_id.0 as usize].id = EvaluationOrder(order);
        }
        order += 1;
        block.terminal.set_evaluation_order(EvaluationOrder(order));
    }
}

/// DFS from entry, populating predecessor sets.
pub fn mark_predecessors(hir: &mut HIR) {
    for block in hir.blocks.values_mut() {
        block.preds.clear();
    }

    let mut visited: IndexSet<BlockId> = IndexSet::new();

    fn visit(
        hir: &mut HIR,
        block_id: BlockId,
        prev_block_id: Option<BlockId>,
        visited: &mut IndexSet<BlockId>,
    ) {
        if let Some(prev_id) = prev_block_id {
            if let Some(block) = hir.blocks.get_mut(&block_id) {
                block.preds.insert(prev_id);
            } else {
                return;
            }
        }

        if visited.contains(&block_id) {
            return;
        }
        visited.insert(block_id);

        let successors = if let Some(block) = hir.blocks.get(&block_id) {
            each_terminal_successor(&block.terminal)
        } else {
            return;
        };

        for successor in successors {
            visit(hir, successor, Some(block_id), visited);
        }
    }

    visit(hir, hir.entry, None, &mut visited);
}

// ---------------------------------------------------------------------------
// Public helper functions
// ---------------------------------------------------------------------------

/// Create a temporary Place with a fresh identifier allocated in the arena.
pub fn create_temporary_place(env: &mut Environment, loc: Option<SourceLocation>) -> Place {
    let id = env.next_identifier_id();
    env.identifiers[id.0 as usize].loc = loc;
    Place {
        identifier: id,
        reactive: false,
        effect: Effect::Unknown,
        loc: None,
    }
}
