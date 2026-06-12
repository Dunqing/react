use std::collections::HashSet;

use react_compiler_ast::scope::BindingId;
use react_compiler_ast::scope::ScopeInfo;
use react_compiler_ast::scope::ScopeKind;
use react_compiler_diagnostics::CompilerDiagnostic;
use react_compiler_diagnostics::CompilerError;
use react_compiler_diagnostics::CompilerErrorDetail;
use react_compiler_diagnostics::ErrorCategory;
use react_compiler_hir::*;

use crate::hir_builder::HirBuilder;

#[allow(unused_imports)]
use super::*;

/// Check if a binding's declaration is a direct statement of the block
/// (not inside a nested control flow block like if/for/while).
/// Uses the binding's declaration_start position to check if it falls within
/// one of the block's direct VariableDeclaration, FunctionDeclaration, or
/// ClassDeclaration statements. This avoids false positives when two bindings
/// share the same name but are declared in different scopes (e.g., `const x`
/// inside an if-branch and `const x` after it).
pub(crate) fn is_binding_in_block_direct_statements(
    binding: &react_compiler_ast::scope::BindingData,
    stmts: &[react_compiler_ast::statements::Statement],
) -> bool {
    use react_compiler_ast::statements::Statement;
    let decl_start = match binding.declaration_start {
        Some(pos) => pos,
        None => return false,
    };
    for stmt in stmts {
        match stmt {
            Statement::VariableDeclaration(vd) => {
                let start = vd.base.start.unwrap_or(0);
                let end = vd.base.end.unwrap_or(u32::MAX);
                if decl_start >= start && decl_start < end {
                    return true;
                }
            }
            Statement::FunctionDeclaration(fd) => {
                let start = fd.base.start.unwrap_or(0);
                let end = fd.base.end.unwrap_or(u32::MAX);
                if decl_start >= start && decl_start < end {
                    return true;
                }
            }
            Statement::ClassDeclaration(cd) => {
                let start = cd.base.start.unwrap_or(0);
                let end = cd.base.end.unwrap_or(u32::MAX);
                if decl_start >= start && decl_start < end {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

#[allow(dead_code)]
pub(crate) fn pattern_declares_name(pattern: &react_compiler_ast::patterns::PatternLike, name: &str) -> bool {
    use react_compiler_ast::patterns::PatternLike;
    match pattern {
        PatternLike::Identifier(id) => id.name == name,
        PatternLike::ObjectPattern(op) => op.properties.iter().any(|prop| match prop {
            react_compiler_ast::patterns::ObjectPatternProperty::ObjectProperty(p) => {
                pattern_declares_name(&p.value, name)
            }
            react_compiler_ast::patterns::ObjectPatternProperty::RestElement(r) => {
                pattern_declares_name(&r.argument, name)
            }
        }),
        PatternLike::ArrayPattern(ap) => ap.elements.iter().any(|el| {
            el.as_ref()
                .map_or(false, |e| pattern_declares_name(e, name))
        }),
        PatternLike::AssignmentPattern(ap) => pattern_declares_name(&ap.left, name),
        PatternLike::RestElement(r) => pattern_declares_name(&r.argument, name),
        PatternLike::MemberExpression(_) => false,
        PatternLike::TSAsExpression(_)
        | PatternLike::TSSatisfiesExpression(_)
        | PatternLike::TSNonNullExpression(_)
        | PatternLike::TSTypeAssertion(_)
        | PatternLike::TypeCastExpression(_) => false,
    }
}

// =============================================================================
// Statement position helpers
// =============================================================================

pub(crate) fn statement_start(stmt: &react_compiler_ast::statements::Statement) -> Option<u32> {
    use react_compiler_ast::statements::Statement;
    match stmt {
        Statement::BlockStatement(s) => s.base.start,
        Statement::ReturnStatement(s) => s.base.start,
        Statement::IfStatement(s) => s.base.start,
        Statement::ForStatement(s) => s.base.start,
        Statement::WhileStatement(s) => s.base.start,
        Statement::DoWhileStatement(s) => s.base.start,
        Statement::ForInStatement(s) => s.base.start,
        Statement::ForOfStatement(s) => s.base.start,
        Statement::SwitchStatement(s) => s.base.start,
        Statement::ThrowStatement(s) => s.base.start,
        Statement::TryStatement(s) => s.base.start,
        Statement::BreakStatement(s) => s.base.start,
        Statement::ContinueStatement(s) => s.base.start,
        Statement::LabeledStatement(s) => s.base.start,
        Statement::ExpressionStatement(s) => s.base.start,
        Statement::EmptyStatement(s) => s.base.start,
        Statement::DebuggerStatement(s) => s.base.start,
        Statement::WithStatement(s) => s.base.start,
        Statement::VariableDeclaration(s) => s.base.start,
        Statement::FunctionDeclaration(s) => s.base.start,
        Statement::ClassDeclaration(s) => s.base.start,
        Statement::ImportDeclaration(s) => s.base.start,
        Statement::ExportNamedDeclaration(s) => s.base.start,
        Statement::ExportDefaultDeclaration(s) => s.base.start,
        Statement::ExportAllDeclaration(s) => s.base.start,
        Statement::TSTypeAliasDeclaration(s) => s.base.start,
        Statement::TSInterfaceDeclaration(s) => s.base.start,
        Statement::TSEnumDeclaration(s) => s.base.start,
        Statement::TSModuleDeclaration(s) => s.base.start,
        Statement::TSDeclareFunction(s) => s.base.start,
        Statement::TypeAlias(s) => s.base.start,
        Statement::OpaqueType(s) => s.base.start,
        Statement::InterfaceDeclaration(s) => s.base.start,
        Statement::DeclareVariable(s) => s.base.start,
        Statement::DeclareFunction(s) => s.base.start,
        Statement::DeclareClass(s) => s.base.start,
        Statement::DeclareModule(s) => s.base.start,
        Statement::DeclareModuleExports(s) => s.base.start,
        Statement::DeclareExportDeclaration(s) => s.base.start,
        Statement::DeclareExportAllDeclaration(s) => s.base.start,
        Statement::DeclareInterface(s) => s.base.start,
        Statement::DeclareTypeAlias(s) => s.base.start,
        Statement::DeclareOpaqueType(s) => s.base.start,
        Statement::EnumDeclaration(s) => s.base.start,
        Statement::Unknown(s) => s.base().start,
    }
}

pub(crate) fn statement_end(stmt: &react_compiler_ast::statements::Statement) -> Option<u32> {
    use react_compiler_ast::statements::Statement;
    match stmt {
        Statement::BlockStatement(s) => s.base.end,
        Statement::ReturnStatement(s) => s.base.end,
        Statement::IfStatement(s) => s.base.end,
        Statement::ForStatement(s) => s.base.end,
        Statement::WhileStatement(s) => s.base.end,
        Statement::DoWhileStatement(s) => s.base.end,
        Statement::ForInStatement(s) => s.base.end,
        Statement::ForOfStatement(s) => s.base.end,
        Statement::SwitchStatement(s) => s.base.end,
        Statement::ThrowStatement(s) => s.base.end,
        Statement::TryStatement(s) => s.base.end,
        Statement::BreakStatement(s) => s.base.end,
        Statement::ContinueStatement(s) => s.base.end,
        Statement::LabeledStatement(s) => s.base.end,
        Statement::ExpressionStatement(s) => s.base.end,
        Statement::EmptyStatement(s) => s.base.end,
        Statement::DebuggerStatement(s) => s.base.end,
        Statement::WithStatement(s) => s.base.end,
        Statement::VariableDeclaration(s) => s.base.end,
        Statement::FunctionDeclaration(s) => s.base.end,
        Statement::ClassDeclaration(s) => s.base.end,
        Statement::ImportDeclaration(s) => s.base.end,
        Statement::ExportNamedDeclaration(s) => s.base.end,
        Statement::ExportDefaultDeclaration(s) => s.base.end,
        Statement::ExportAllDeclaration(s) => s.base.end,
        Statement::TSTypeAliasDeclaration(s) => s.base.end,
        Statement::TSInterfaceDeclaration(s) => s.base.end,
        Statement::TSEnumDeclaration(s) => s.base.end,
        Statement::TSModuleDeclaration(s) => s.base.end,
        Statement::TSDeclareFunction(s) => s.base.end,
        Statement::TypeAlias(s) => s.base.end,
        Statement::OpaqueType(s) => s.base.end,
        Statement::InterfaceDeclaration(s) => s.base.end,
        Statement::DeclareVariable(s) => s.base.end,
        Statement::DeclareFunction(s) => s.base.end,
        Statement::DeclareClass(s) => s.base.end,
        Statement::DeclareModule(s) => s.base.end,
        Statement::DeclareModuleExports(s) => s.base.end,
        Statement::DeclareExportDeclaration(s) => s.base.end,
        Statement::DeclareExportAllDeclaration(s) => s.base.end,
        Statement::DeclareInterface(s) => s.base.end,
        Statement::DeclareTypeAlias(s) => s.base.end,
        Statement::DeclareOpaqueType(s) => s.base.end,
        Statement::EnumDeclaration(s) => s.base.end,
        Statement::Unknown(s) => s.base().end,
    }
}

/// Extract the HIR SourceLocation from a Statement AST node.
pub(crate) fn statement_loc(stmt: &react_compiler_ast::statements::Statement) -> Option<SourceLocation> {
    use react_compiler_ast::statements::Statement;
    let loc = match stmt {
        Statement::BlockStatement(s) => s.base.loc.clone(),
        Statement::ReturnStatement(s) => s.base.loc.clone(),
        Statement::IfStatement(s) => s.base.loc.clone(),
        Statement::ForStatement(s) => s.base.loc.clone(),
        Statement::WhileStatement(s) => s.base.loc.clone(),
        Statement::DoWhileStatement(s) => s.base.loc.clone(),
        Statement::ForInStatement(s) => s.base.loc.clone(),
        Statement::ForOfStatement(s) => s.base.loc.clone(),
        Statement::SwitchStatement(s) => s.base.loc.clone(),
        Statement::ThrowStatement(s) => s.base.loc.clone(),
        Statement::TryStatement(s) => s.base.loc.clone(),
        Statement::BreakStatement(s) => s.base.loc.clone(),
        Statement::ContinueStatement(s) => s.base.loc.clone(),
        Statement::LabeledStatement(s) => s.base.loc.clone(),
        Statement::ExpressionStatement(s) => s.base.loc.clone(),
        Statement::EmptyStatement(s) => s.base.loc.clone(),
        Statement::DebuggerStatement(s) => s.base.loc.clone(),
        Statement::WithStatement(s) => s.base.loc.clone(),
        Statement::VariableDeclaration(s) => s.base.loc.clone(),
        Statement::FunctionDeclaration(s) => s.base.loc.clone(),
        Statement::ClassDeclaration(s) => s.base.loc.clone(),
        Statement::ImportDeclaration(s) => s.base.loc.clone(),
        Statement::ExportNamedDeclaration(s) => s.base.loc.clone(),
        Statement::ExportDefaultDeclaration(s) => s.base.loc.clone(),
        Statement::ExportAllDeclaration(s) => s.base.loc.clone(),
        Statement::TSTypeAliasDeclaration(s) => s.base.loc.clone(),
        Statement::TSInterfaceDeclaration(s) => s.base.loc.clone(),
        Statement::TSEnumDeclaration(s) => s.base.loc.clone(),
        Statement::TSModuleDeclaration(s) => s.base.loc.clone(),
        Statement::TSDeclareFunction(s) => s.base.loc.clone(),
        Statement::TypeAlias(s) => s.base.loc.clone(),
        Statement::OpaqueType(s) => s.base.loc.clone(),
        Statement::InterfaceDeclaration(s) => s.base.loc.clone(),
        Statement::DeclareVariable(s) => s.base.loc.clone(),
        Statement::DeclareFunction(s) => s.base.loc.clone(),
        Statement::DeclareClass(s) => s.base.loc.clone(),
        Statement::DeclareModule(s) => s.base.loc.clone(),
        Statement::DeclareModuleExports(s) => s.base.loc.clone(),
        Statement::DeclareExportDeclaration(s) => s.base.loc.clone(),
        Statement::DeclareExportAllDeclaration(s) => s.base.loc.clone(),
        Statement::DeclareInterface(s) => s.base.loc.clone(),
        Statement::DeclareTypeAlias(s) => s.base.loc.clone(),
        Statement::DeclareOpaqueType(s) => s.base.loc.clone(),
        Statement::EnumDeclaration(s) => s.base.loc.clone(),
        Statement::Unknown(s) => s.base().loc.clone(),
    };
    convert_opt_loc(&loc)
}

/// Collect binding names from a pattern that are declared in the given scope.
pub(crate) fn collect_binding_names_from_pattern(
    pattern: &react_compiler_ast::patterns::PatternLike,
    scope_id: react_compiler_ast::scope::ScopeId,
    scope_info: &ScopeInfo,
    out: &mut HashSet<BindingId>,
) {
    use react_compiler_ast::patterns::PatternLike;
    match pattern {
        PatternLike::Identifier(id) => {
            if let Some(&binding_id) = scope_info.scopes[scope_id.0 as usize]
                .bindings
                .get(&id.name)
            {
                out.insert(binding_id);
            }
        }
        PatternLike::ObjectPattern(obj) => {
            for prop in &obj.properties {
                match prop {
                    react_compiler_ast::patterns::ObjectPatternProperty::ObjectProperty(p) => {
                        collect_binding_names_from_pattern(&p.value, scope_id, scope_info, out);
                    }
                    react_compiler_ast::patterns::ObjectPatternProperty::RestElement(r) => {
                        collect_binding_names_from_pattern(&r.argument, scope_id, scope_info, out);
                    }
                }
            }
        }
        PatternLike::ArrayPattern(arr) => {
            for elem in &arr.elements {
                if let Some(e) = elem {
                    collect_binding_names_from_pattern(e, scope_id, scope_info, out);
                }
            }
        }
        PatternLike::AssignmentPattern(assign) => {
            collect_binding_names_from_pattern(&assign.left, scope_id, scope_info, out);
        }
        PatternLike::RestElement(rest) => {
            collect_binding_names_from_pattern(&rest.argument, scope_id, scope_info, out);
        }
        PatternLike::MemberExpression(_) => {}
        PatternLike::TSAsExpression(_)
        | PatternLike::TSSatisfiesExpression(_)
        | PatternLike::TSNonNullExpression(_)
        | PatternLike::TSTypeAssertion(_)
        | PatternLike::TypeCastExpression(_) => {}
    }
}

// =============================================================================
// lower_block_statement (with hoisting)
// =============================================================================

/// Lower a BlockStatement with hoisting support.
///
/// Implements the TS BlockStatement hoisting pass: identifies forward references to
/// block-scoped bindings and emits DeclareContext instructions to hoist them.
pub(crate) fn lower_block_statement(
    builder: &mut HirBuilder,
    block: &react_compiler_ast::statements::BlockStatement,
    parent_scope: Option<react_compiler_ast::scope::ScopeId>,
) -> Result<(), CompilerError> {
    let _ = lower_block_statement_inner(builder, block, None, parent_scope);
    Ok(())
}

pub(crate) fn lower_block_statement_with_scope(
    builder: &mut HirBuilder,
    block: &react_compiler_ast::statements::BlockStatement,
    scope_override: react_compiler_ast::scope::ScopeId,
) -> Result<(), CompilerError> {
    let _ = lower_block_statement_inner(builder, block, Some(scope_override), None);
    Ok(())
}

pub(crate) fn lower_block_statement_inner(
    builder: &mut HirBuilder,
    block: &react_compiler_ast::statements::BlockStatement,
    scope_override: Option<react_compiler_ast::scope::ScopeId>,
    parent_scope: Option<react_compiler_ast::scope::ScopeId>,
) -> Result<(), CompilerDiagnostic> {
    use react_compiler_ast::scope::BindingKind as AstBindingKind;
    use react_compiler_ast::statements::Statement;

    // Look up the block's scope to identify hoistable bindings.
    // Use the scope override if provided (for function body blocks that share the function's scope).
    let block_scope_id = scope_override.or_else(|| {
        let found = builder
            .scope_info()
            .resolve_scope_for_node(block.base.node_id);
        if found.is_some() {
            return found;
        }
        // Fallback for synthetic blocks (start=0 from Hermes match desugar):
        // find a descendant scope of the parent that contains the block's declarations.
        let mut decl_names = Vec::new();
        for stmt in &block.body {
            if let Statement::VariableDeclaration(vd) = stmt {
                for d in &vd.declarations {
                    if let react_compiler_ast::patterns::PatternLike::Identifier(id) = &d.id {
                        decl_names.push(id.name.as_str());
                    }
                }
            }
        }
        if decl_names.is_empty() {
            return None;
        }
        let search_parent = parent_scope.unwrap_or_else(|| builder.function_scope());
        let found =
            builder
                .scope_info()
                .find_block_scope_by_bindings(&decl_names, search_parent, |sid| {
                    builder.is_synthetic_scope_claimed(sid)
                });
        if let Some(sid) = found {
            builder.claim_synthetic_scope(sid);
        }
        found
    });

    let scope_id = match block_scope_id {
        Some(id) => id,
        None => {
            for body_stmt in &block.body {
                lower_statement(builder, body_stmt, None, parent_scope)?;
            }
            return Ok(());
        }
    };

    // Collect hoistable bindings from this scope AND direct child block scopes.
    // In Babel, a function body BlockStatement shares the function's scope, so
    // all bindings (var, const, let) are in one scope. But our scope extraction
    // may split them: function scope has params/var, child block scope has const/let.
    // Including child block scope bindings matches TS behavior where
    // stmt.scope.bindings includes all bindings accessible in the block.
    //
    // IMPORTANT: Only include bindings whose declaration falls within THIS block's
    // statement range. Bindings declared in nested blocks (e.g., inside an `if`
    // branch) should NOT be hoisted at the parent level — they'll be handled when
    // that nested block is recursively lowered. This prevents DeclareContext from
    // being emitted before an `if` terminal for variables declared within the branch.
    let hoistable: Vec<(
        BindingId,
        String,
        AstBindingKind,
        String,
        Option<u32>,
        Option<u32>,
    )> = builder
        .scope_info()
        .scope_bindings_with_children(scope_id)
        .filter(|b| {
            !matches!(b.kind, AstBindingKind::Param | AstBindingKind::Module)
                && b.declaration_type != "FunctionExpression"
                && b.declaration_type != "TypeAlias"
                && b.declaration_type != "OpaqueType"
                && b.declaration_type != "InterfaceDeclaration"
                && b.declaration_type != "TSTypeAliasDeclaration"
                && b.declaration_type != "TSInterfaceDeclaration"
                && b.declaration_type != "TSEnumDeclaration"
        })
        .map(|b| {
            (
                b.id,
                b.name.clone(),
                b.kind.clone(),
                b.declaration_type.clone(),
                b.declaration_start,
                b.declaration_node_id,
            )
        })
        .collect();

    if hoistable.is_empty() {
        // No hoistable bindings, just lower statements normally
        for body_stmt in &block.body {
            lower_statement(builder, body_stmt, None, Some(scope_id))?;
        }
        return Ok(());
    }

    // Track which bindings have been "declared" (their declaration statement has been seen)
    let mut declared: HashSet<BindingId> = HashSet::new();

    for body_stmt in &block.body {
        let stmt_start = statement_start(body_stmt).unwrap_or(0);
        let stmt_end = statement_end(body_stmt).unwrap_or(u32::MAX);
        let is_function_decl = matches!(body_stmt, Statement::FunctionDeclaration(_));

        // Collect ranges of nested function scopes within this statement.
        // Used to check per-reference whether a reference is inside a nested function,
        // rather than checking once per-statement.
        let nested_function_ranges: Vec<(u32, u32)> = if is_function_decl {
            // For function declarations, fnDepth starts at 1 (all refs are inside)
            vec![(stmt_start, stmt_end)]
        } else {
            let scope_info = builder.scope_info();
            scope_info
                .node_to_scope
                .iter()
                .filter(|&(&pos, &sid)| {
                    pos > stmt_start
                        && pos < stmt_end
                        && matches!(scope_info.scopes[sid.0 as usize].kind, ScopeKind::Function)
                })
                .filter_map(|(&pos, _)| {
                    scope_info
                        .node_to_scope_end
                        .get(&pos)
                        .map(|&end| (pos, end))
                })
                .collect()
        };

        // Find references to not-yet-declared hoistable bindings within this statement
        struct HoistInfo {
            binding_id: BindingId,
            name: String,
            kind: AstBindingKind,
            declaration_type: String,
            first_ref_pos: u32,
            first_ref_nid: u32,
        }
        let mut will_hoist: Vec<HoistInfo> = Vec::new();

        for (binding_id, name, kind, decl_type, _decl_start, decl_node_id) in &hoistable {
            if declared.contains(binding_id) {
                continue;
            }

            // Find the first reference (not declaration) to this binding in the statement's range.
            // Exclude JSX identifier references: while Babel's scope system links JSX
            // tag names to local bindings (and the context capture pass includes them),
            // the TS hoisting analysis does NOT traverse JSX elements. This mismatch
            // is intentional — it matches the TS behavior where <colgroup> adds
            // "colgroup" to the context but does NOT trigger hoisting, causing
            // EnterSSA to error with "Expected identifier to be defined before use".
            //
            // The decl_start filter excludes the binding's own declaration position from
            // counting as a reference. For hoisted bindings (function declarations), this
            // filter is only applied when the current statement IS a FunctionDeclaration,
            // since that's the only statement type where decl_start is a declaration, not
            // a reference.
            let apply_decl_filter = !matches!(kind, AstBindingKind::Hoisted) || is_function_decl;
            let refs_in_stmt: Vec<(u32, u32)> = builder
                .scope_info()
                .ref_node_id_to_binding
                .iter()
                .filter_map(|(&ref_nid, &ref_bid)| {
                    if ref_bid != *binding_id {
                        return None;
                    }
                    let entry = builder.identifier_locs().get(&ref_nid)?;
                    let ref_start = entry.start;
                    if ref_start < stmt_start || ref_start >= stmt_end {
                        return None;
                    }
                    if apply_decl_filter && *decl_node_id == Some(ref_nid) {
                        return None;
                    }
                    if entry.is_jsx {
                        return None;
                    }
                    Some((ref_start, ref_nid))
                })
                .collect();

            if refs_in_stmt.is_empty() {
                continue;
            }

            let (first_ref_pos, first_ref_nid) =
                *refs_in_stmt.iter().min_by_key(|(pos, _)| *pos).unwrap();

            // Hoist if: (1) binding is "hoisted" kind (function declaration), or
            // (2) any reference to this binding is inside a nested function scope.
            // Check per-reference rather than per-statement to correctly handle
            // statements that contain both nested functions and top-level code.
            let is_hoisted_kind = matches!(kind, AstBindingKind::Hoisted);
            let refs_in_nested_fn: Vec<(u32, u32)> = refs_in_stmt
                .iter()
                .copied()
                .filter(|&(ref_pos, _)| {
                    nested_function_ranges
                        .iter()
                        .any(|&(fn_start, fn_end)| ref_pos >= fn_start && ref_pos < fn_end)
                })
                .collect();
            let should_hoist = is_hoisted_kind || !refs_in_nested_fn.is_empty();
            if should_hoist {
                // Bindings pulled in from CHILD block scopes (the
                // scope_bindings_with_children descent compensates for scope
                // splitting) only hoist when declared as a direct statement of
                // THIS block; ones declared inside nested control-flow blocks
                // are handled when those blocks are recursively lowered. TS
                // never sees child-block bindings here (Babel's
                // stmt.scope.bindings holds only the block's own scope), so the
                // guard must NOT apply to own-scope bindings: catch params and
                // for-in/for-of head vars belong to the block's scope without
                // being declared by any direct statement, and TS hoists them.
                let binding_data = &builder.scope_info().bindings[binding_id.0 as usize];
                if binding_data.scope != scope_id
                    && !is_binding_in_block_direct_statements(binding_data, &block.body)
                {
                    continue;
                }
                // For hoisted bindings (function declarations), use the first reference
                // overall. For non-hoisted bindings, use the first reference inside a
                // nested function.
                let (hoist_ref_pos, hoist_ref_nid) = if is_hoisted_kind {
                    (first_ref_pos, first_ref_nid)
                } else {
                    *refs_in_nested_fn
                        .iter()
                        .min_by_key(|(pos, _)| *pos)
                        .unwrap()
                };
                will_hoist.push(HoistInfo {
                    binding_id: *binding_id,
                    name: name.clone(),
                    kind: kind.clone(),
                    declaration_type: decl_type.clone(),
                    first_ref_pos: hoist_ref_pos,
                    first_ref_nid: hoist_ref_nid,
                });
            }
        }

        // Sort by first reference position to match TS traversal order
        will_hoist.sort_by_key(|h| h.first_ref_pos);

        // Emit DeclareContext for hoisted bindings
        for info in &will_hoist {
            if builder
                .environment()
                .is_hoisted_identifier(info.binding_id.0)
            {
                continue;
            }

            let hoist_kind = match info.kind {
                AstBindingKind::Const | AstBindingKind::Var => InstructionKind::HoistedConst,
                AstBindingKind::Let => InstructionKind::HoistedLet,
                AstBindingKind::Hoisted => InstructionKind::HoistedFunction,
                _ => {
                    if info.declaration_type == "FunctionDeclaration" {
                        InstructionKind::HoistedFunction
                    } else if info.declaration_type == "VariableDeclarator" {
                        // Unsupported hoisting for this declaration kind
                        builder.record_error(CompilerErrorDetail {
                            category: ErrorCategory::Todo,
                            reason: "Handle non-const declarations for hoisting".to_string(),
                            description: Some(format!(
                                "variable \"{}\" declared with {:?}",
                                info.name, info.kind
                            )),
                            loc: None,
                            suggestions: None,
                        })?;
                        continue;
                    } else {
                        builder.record_error(CompilerErrorDetail {
                            category: ErrorCategory::Todo,
                            reason: "Unsupported declaration type for hoisting".to_string(),
                            description: Some(format!(
                                "variable \"{}\" declared with {}",
                                info.name, info.declaration_type
                            )),
                            loc: None,
                            suggestions: None,
                        })?;
                        continue;
                    }
                }
            };

            // Look up the reference location for the DeclareContext instruction.
            let ref_loc = builder
                .identifier_locs()
                .get(&info.first_ref_nid)
                .map(|e| e.loc.clone());
            let identifier = builder.resolve_binding(&info.name, info.binding_id)?;
            let place = Place {
                effect: Effect::Unknown,
                identifier,
                reactive: false,
                loc: ref_loc.clone(),
            };
            lower_value_to_temporary(
                builder,
                InstructionValue::DeclareContext {
                    lvalue: LValue {
                        kind: hoist_kind,
                        place,
                    },
                    loc: ref_loc,
                },
            )?;
            builder
                .environment_mut()
                .add_hoisted_identifier(info.binding_id.0);
            // Hoisted identifiers also become context identifiers (matching TS addHoistedIdentifier)
            builder.add_context_identifier(info.binding_id);
        }

        // After processing the statement, mark any bindings it declares as "seen".
        // This must cover all statement types that can introduce bindings.
        match body_stmt {
            Statement::FunctionDeclaration(func) => {
                if let Some(id) = &func.id {
                    if let Some(&binding_id) = builder.scope_info().scopes[scope_id.0 as usize]
                        .bindings
                        .get(&id.name)
                    {
                        declared.insert(binding_id);
                    }
                }
            }
            Statement::VariableDeclaration(var_decl) => {
                for decl in &var_decl.declarations {
                    collect_binding_names_from_pattern(
                        &decl.id,
                        scope_id,
                        builder.scope_info(),
                        &mut declared,
                    );
                }
            }
            Statement::ClassDeclaration(cls) => {
                if let Some(id) = &cls.id {
                    if let Some(&binding_id) = builder.scope_info().scopes[scope_id.0 as usize]
                        .bindings
                        .get(&id.name)
                    {
                        declared.insert(binding_id);
                    }
                }
            }
            _ => {
                // For other statement types (e.g. ForStatement with VariableDeclaration in init),
                // we rely on the reference_to_binding check for forward references.
                // Any bindings declared by child scopes won't be in this block's scope anyway.
            }
        }

        lower_statement(builder, body_stmt, None, Some(scope_id))?;
    }
    Ok(())
}

// =============================================================================
// lower_statement
// =============================================================================

pub(crate) fn lower_statement(
    builder: &mut HirBuilder,
    stmt: &react_compiler_ast::statements::Statement,
    label: Option<&str>,
    parent_scope: Option<react_compiler_ast::scope::ScopeId>,
) -> Result<(), CompilerDiagnostic> {
    use react_compiler_ast::statements::Statement;

    match stmt {
        Statement::EmptyStatement(_) => {
            // no-op
        }
        Statement::DebuggerStatement(dbg) => {
            let loc = convert_opt_loc(&dbg.base.loc);
            let value = InstructionValue::Debugger { loc };
            lower_value_to_temporary(builder, value)?;
        }
        Statement::ExpressionStatement(expr_stmt) => {
            lower_expression_to_temporary(builder, &expr_stmt.expression)?;
        }
        Statement::ReturnStatement(ret) => {
            let loc = convert_opt_loc(&ret.base.loc);
            let value = if let Some(arg) = &ret.argument {
                lower_expression_to_temporary(builder, arg)?
            } else {
                let undefined_value = InstructionValue::Primitive {
                    value: PrimitiveValue::Undefined,
                    loc: None,
                };
                lower_value_to_temporary(builder, undefined_value)?
            };
            let fallthrough = builder.reserve(BlockKind::Block);
            builder.terminate_with_continuation(
                Terminal::Return {
                    value,
                    return_variant: ReturnVariant::Explicit,
                    id: EvaluationOrder(0),
                    loc,
                    effects: None,
                },
                fallthrough,
            );
        }
        Statement::ThrowStatement(throw) => {
            let loc = convert_opt_loc(&throw.base.loc);
            let value = lower_expression_to_temporary(builder, &throw.argument)?;

            // Check for throw handler (try/catch)
            if let Some(_handler) = builder.resolve_throw_handler() {
                builder.record_error(CompilerErrorDetail {
                    category: ErrorCategory::Todo,
                    reason: "(BuildHIR::lowerStatement) Support ThrowStatement inside of try/catch"
                        .to_string(),
                    description: None,
                    loc: loc.clone(),
                    suggestions: None,
                })?;
            }

            let fallthrough = builder.reserve(BlockKind::Block);
            builder.terminate_with_continuation(
                Terminal::Throw {
                    value,
                    id: EvaluationOrder(0),
                    loc,
                },
                fallthrough,
            );
        }
        Statement::BlockStatement(block) => {
            lower_block_statement(builder, block, parent_scope)?;
        }
        Statement::VariableDeclaration(var_decl) => {
            use react_compiler_ast::patterns::PatternLike;
            use react_compiler_ast::statements::VariableDeclarationKind;
            if matches!(var_decl.kind, VariableDeclarationKind::Var) {
                builder.record_error(CompilerErrorDetail {
                    reason: "(BuildHIR::lowerStatement) Handle var kinds in VariableDeclaration"
                        .to_string(),
                    category: ErrorCategory::Todo,
                    loc: convert_opt_loc(&var_decl.base.loc),
                    description: None,
                    suggestions: None,
                })?;
                // Treat `var` as `let` so references to the variable don't break
            }
            let kind = match var_decl.kind {
                VariableDeclarationKind::Let | VariableDeclarationKind::Var => InstructionKind::Let,
                VariableDeclarationKind::Const | VariableDeclarationKind::Using => {
                    InstructionKind::Const
                }
            };
            for declarator in &var_decl.declarations {
                let stmt_loc = convert_opt_loc(&var_decl.base.loc);
                if let Some(init) = &declarator.init {
                    let value = lower_expression_to_temporary(builder, init)?;
                    let assign_style = match &declarator.id {
                        PatternLike::ObjectPattern(_) | PatternLike::ArrayPattern(_) => {
                            AssignmentStyle::Destructure
                        }
                        _ => AssignmentStyle::Assignment,
                    };
                    lower_assignment(builder, stmt_loc, kind, &declarator.id, value, assign_style)?;
                } else if let PatternLike::Identifier(id) = &declarator.id {
                    // No init: emit DeclareLocal or DeclareContext
                    let id_loc = convert_opt_loc(&id.base.loc);
                    let mut binding = builder.resolve_identifier(
                        &id.name,
                        id.base.start.unwrap_or(0),
                        id_loc.clone(),
                        id.base.node_id,
                    )?;
                    if !matches!(binding, VariableBinding::Identifier { .. }) {
                        // Position-based resolution failed (synthetic $$gen vars
                        // at position 0). Try scope lookup including descendants.
                        if let Some((binding_id, binding_data)) = builder
                            .scope_info()
                            .find_binding_id_in_descendants(&id.name, builder.function_scope())
                        {
                            let binding_kind = crate::convert_binding_kind(&binding_data.kind);
                            let identifier = builder.resolve_binding_with_loc(
                                &id.name,
                                binding_id,
                                id_loc.clone(),
                            )?;
                            binding = VariableBinding::Identifier {
                                identifier,
                                binding_kind,
                            };
                        }
                    }
                    match binding {
                        VariableBinding::Identifier { identifier, .. } => {
                            // Update the identifier's loc to the declaration site
                            // (it may have been first created at a reference site during hoisting)
                            builder.set_identifier_declaration_loc(identifier, &id_loc);
                            let place = Place {
                                identifier,
                                effect: Effect::Unknown,
                                reactive: false,
                                loc: id_loc.clone(),
                            };
                            if builder.is_context_identifier(
                                &id.name,
                                id.base.start.unwrap_or(0),
                                id.base.node_id,
                            ) {
                                if kind == InstructionKind::Const {
                                    builder.record_error(CompilerErrorDetail {
                                        reason: "Expect `const` declaration not to be reassigned"
                                            .to_string(),
                                        category: ErrorCategory::Syntax,
                                        loc: id_loc.clone(),
                                        description: None,
                                        suggestions: None,
                                    })?;
                                }
                                lower_value_to_temporary(
                                    builder,
                                    InstructionValue::DeclareContext {
                                        lvalue: LValue {
                                            kind: InstructionKind::Let,
                                            place,
                                        },
                                        loc: id_loc,
                                    },
                                )?;
                            } else {
                                let type_annotation =
                                    extract_type_annotation_name(&id.type_annotation);
                                lower_value_to_temporary(
                                    builder,
                                    InstructionValue::DeclareLocal {
                                        lvalue: LValue { kind, place },
                                        type_annotation,
                                        loc: id_loc,
                                    },
                                )?;
                            }
                        }
                        _ => {
                            builder.record_error(CompilerErrorDetail {
                                reason: "Could not find binding for declaration".to_string(),
                                category: ErrorCategory::Invariant,
                                loc: id_loc,
                                description: None,
                                suggestions: None,
                            })?;
                        }
                    }
                } else {
                    builder.record_error(CompilerErrorDetail {
                        reason: "Expected variable declaration to be an identifier if no initializer was provided".to_string(),
                        category: ErrorCategory::Syntax,
                        loc: convert_opt_loc(&declarator.base.loc),
                        description: None,
                        suggestions: None,
                    })?;
                }
            }
        }
        Statement::BreakStatement(brk) => {
            let loc = convert_opt_loc(&brk.base.loc);
            let label_name = brk.label.as_ref().map(|l| l.name.as_str());
            let target = builder.lookup_break(label_name)?;
            let fallthrough = builder.reserve(BlockKind::Block);
            builder.terminate_with_continuation(
                Terminal::Goto {
                    block: target,
                    variant: GotoVariant::Break,
                    id: EvaluationOrder(0),
                    loc,
                },
                fallthrough,
            );
        }
        Statement::ContinueStatement(cont) => {
            let loc = convert_opt_loc(&cont.base.loc);
            let label_name = cont.label.as_ref().map(|l| l.name.as_str());
            let target = builder.lookup_continue(label_name)?;
            let fallthrough = builder.reserve(BlockKind::Block);
            builder.terminate_with_continuation(
                Terminal::Goto {
                    block: target,
                    variant: GotoVariant::Continue,
                    id: EvaluationOrder(0),
                    loc,
                },
                fallthrough,
            );
        }
        Statement::IfStatement(if_stmt) => {
            let loc = convert_opt_loc(&if_stmt.base.loc);
            // Block for code following the if
            let continuation_block = builder.reserve(BlockKind::Block);
            let continuation_id = continuation_block.id;

            // Block for the consequent (if the test is truthy)
            let consequent_loc = statement_loc(&if_stmt.consequent);
            let consequent_block = builder.try_enter(BlockKind::Block, |builder, _block_id| {
                lower_statement(builder, &if_stmt.consequent, None, parent_scope)?;
                Ok(Terminal::Goto {
                    block: continuation_id,
                    variant: GotoVariant::Break,
                    id: EvaluationOrder(0),
                    loc: consequent_loc,
                })
            })?;

            // Block for the alternate (if the test is not truthy)
            let alternate_block = if let Some(alternate) = &if_stmt.alternate {
                let alternate_loc = statement_loc(alternate);
                builder.try_enter(BlockKind::Block, |builder, _block_id| {
                    lower_statement(builder, alternate, None, parent_scope)?;
                    Ok(Terminal::Goto {
                        block: continuation_id,
                        variant: GotoVariant::Break,
                        id: EvaluationOrder(0),
                        loc: alternate_loc,
                    })
                })?
            } else {
                // If there is no else clause, use the continuation directly
                continuation_id
            };

            let test = lower_expression_to_temporary(builder, &if_stmt.test)?;
            builder.terminate_with_continuation(
                Terminal::If {
                    test,
                    consequent: consequent_block,
                    alternate: alternate_block,
                    fallthrough: continuation_id,
                    id: EvaluationOrder(0),
                    loc,
                },
                continuation_block,
            );
        }
        Statement::ForStatement(for_stmt) => {
            let loc = convert_opt_loc(&for_stmt.base.loc);

            let test_block = builder.reserve(BlockKind::Loop);
            let test_block_id = test_block.id;
            // Block for code following the loop
            let continuation_block = builder.reserve(BlockKind::Block);
            let continuation_id = continuation_block.id;

            // Init block: lower init expression/declaration, then goto test
            let init_block = builder.try_enter(BlockKind::Loop, |builder, _block_id| {
                let init_loc = match &for_stmt.init {
                    None => {
                        // No init expression (e.g., `for (; ...)`), add a placeholder
                        let placeholder = InstructionValue::Primitive {
                            value: PrimitiveValue::Undefined,
                            loc: loc.clone(),
                        };
                        lower_value_to_temporary(builder, placeholder)?;
                        loc.clone()
                    }
                    Some(init) => {
                        match init.as_ref() {
                            react_compiler_ast::statements::ForInit::VariableDeclaration(var_decl) => {
                                let init_loc = convert_opt_loc(&var_decl.base.loc);
                                lower_statement(builder, &Statement::VariableDeclaration(var_decl.clone()), None, parent_scope)?;
                                init_loc
                            }
                            react_compiler_ast::statements::ForInit::Expression(expr) => {
                                let init_loc = expression_loc(expr);
                                builder.record_error(CompilerErrorDetail {
                                    category: ErrorCategory::Todo,
                                    reason: "(BuildHIR::lowerStatement) Handle non-variable initialization in ForStatement".to_string(),
                                    description: None,
                                    loc: loc.clone(),
                                    suggestions: None,
                                })?;
                                lower_expression_to_temporary(builder, expr)?;
                                init_loc
                            }
                        }
                    }
                };
                Ok(Terminal::Goto {
                    block: test_block_id,
                    variant: GotoVariant::Break,
                    id: EvaluationOrder(0),
                    loc: init_loc,
                })
            })?;

            // Update block (optional)
            let update_block_id = if let Some(update) = &for_stmt.update {
                let update_loc = expression_loc(update);
                Some(builder.try_enter(BlockKind::Loop, |builder, _block_id| {
                    lower_expression_to_temporary(builder, update)?;
                    Ok(Terminal::Goto {
                        block: test_block_id,
                        variant: GotoVariant::Break,
                        id: EvaluationOrder(0),
                        loc: update_loc,
                    })
                })?)
            } else {
                None
            };

            // Loop body block
            let continue_target = update_block_id.unwrap_or(test_block_id);
            let body_loc = statement_loc(&for_stmt.body);
            let body_block = builder.try_enter(BlockKind::Block, |builder, _block_id| {
                builder.loop_scope(
                    label.map(|s| s.to_string()),
                    continue_target,
                    continuation_id,
                    |builder| {
                        lower_statement(builder, &for_stmt.body, None, parent_scope)?;
                        Ok(Terminal::Goto {
                            block: continue_target,
                            variant: GotoVariant::Continue,
                            id: EvaluationOrder(0),
                            loc: body_loc,
                        })
                    },
                )
            })?;

            // Emit For terminal, then fill in the test block
            builder.terminate_with_continuation(
                Terminal::For {
                    init: init_block,
                    test: test_block_id,
                    update: update_block_id,
                    loop_block: body_block,
                    fallthrough: continuation_id,
                    id: EvaluationOrder(0),
                    loc: loc.clone(),
                },
                test_block,
            );

            // Fill in the test block
            if let Some(test_expr) = &for_stmt.test {
                let test = lower_expression_to_temporary(builder, test_expr)?;
                builder.terminate_with_continuation(
                    Terminal::Branch {
                        test,
                        consequent: body_block,
                        alternate: continuation_id,
                        fallthrough: continuation_id,
                        id: EvaluationOrder(0),
                        loc: loc.clone(),
                    },
                    continuation_block,
                );
            } else {
                builder.record_error(CompilerErrorDetail {
                    category: ErrorCategory::Todo,
                    reason: "(BuildHIR::lowerStatement) Handle empty test in ForStatement"
                        .to_string(),
                    description: None,
                    loc: loc.clone(),
                    suggestions: None,
                })?;
                // Treat `for(;;)` as `while(true)` to keep the builder state consistent
                let true_val = InstructionValue::Primitive {
                    value: PrimitiveValue::Boolean(true),
                    loc: loc.clone(),
                };
                let test = lower_value_to_temporary(builder, true_val)?;
                builder.terminate_with_continuation(
                    Terminal::Branch {
                        test,
                        consequent: body_block,
                        alternate: continuation_id,
                        fallthrough: continuation_id,
                        id: EvaluationOrder(0),
                        loc,
                    },
                    continuation_block,
                );
            }
        }
        Statement::WhileStatement(while_stmt) => {
            let loc = convert_opt_loc(&while_stmt.base.loc);
            // Block used to evaluate whether to (re)enter or exit the loop
            let conditional_block = builder.reserve(BlockKind::Loop);
            let conditional_id = conditional_block.id;
            // Block for code following the loop
            let continuation_block = builder.reserve(BlockKind::Block);
            let continuation_id = continuation_block.id;

            // Loop body
            let body_loc = statement_loc(&while_stmt.body);
            let loop_block = builder.try_enter(BlockKind::Block, |builder, _block_id| {
                builder.loop_scope(
                    label.map(|s| s.to_string()),
                    conditional_id,
                    continuation_id,
                    |builder| {
                        lower_statement(builder, &while_stmt.body, None, parent_scope)?;
                        Ok(Terminal::Goto {
                            block: conditional_id,
                            variant: GotoVariant::Continue,
                            id: EvaluationOrder(0),
                            loc: body_loc,
                        })
                    },
                )
            })?;

            // Emit While terminal, jumping to the conditional block
            builder.terminate_with_continuation(
                Terminal::While {
                    test: conditional_id,
                    loop_block,
                    fallthrough: continuation_id,
                    id: EvaluationOrder(0),
                    loc: loc.clone(),
                },
                conditional_block,
            );

            // Fill in the conditional block: lower test, branch
            let test = lower_expression_to_temporary(builder, &while_stmt.test)?;
            builder.terminate_with_continuation(
                Terminal::Branch {
                    test,
                    consequent: loop_block,
                    alternate: continuation_id,
                    fallthrough: conditional_id,
                    id: EvaluationOrder(0),
                    loc,
                },
                continuation_block,
            );
        }
        Statement::DoWhileStatement(do_while_stmt) => {
            let loc = convert_opt_loc(&do_while_stmt.base.loc);
            // Block used to evaluate whether to (re)enter or exit the loop
            let conditional_block = builder.reserve(BlockKind::Loop);
            let conditional_id = conditional_block.id;
            // Block for code following the loop
            let continuation_block = builder.reserve(BlockKind::Block);
            let continuation_id = continuation_block.id;

            // Loop body, executed at least once unconditionally prior to exit
            let body_loc = statement_loc(&do_while_stmt.body);
            let loop_block = builder.try_enter(BlockKind::Block, |builder, _block_id| {
                builder.loop_scope(
                    label.map(|s| s.to_string()),
                    conditional_id,
                    continuation_id,
                    |builder| {
                        lower_statement(builder, &do_while_stmt.body, None, parent_scope)?;
                        Ok(Terminal::Goto {
                            block: conditional_id,
                            variant: GotoVariant::Continue,
                            id: EvaluationOrder(0),
                            loc: body_loc,
                        })
                    },
                )
            })?;

            // Jump to the conditional block
            builder.terminate_with_continuation(
                Terminal::DoWhile {
                    loop_block,
                    test: conditional_id,
                    fallthrough: continuation_id,
                    id: EvaluationOrder(0),
                    loc: loc.clone(),
                },
                conditional_block,
            );

            // Fill in the conditional block: lower test, branch
            let test = lower_expression_to_temporary(builder, &do_while_stmt.test)?;
            builder.terminate_with_continuation(
                Terminal::Branch {
                    test,
                    consequent: loop_block,
                    alternate: continuation_id,
                    fallthrough: conditional_id,
                    id: EvaluationOrder(0),
                    loc,
                },
                continuation_block,
            );
        }
        Statement::ForInStatement(for_in) => {
            let loc = convert_opt_loc(&for_in.base.loc);
            let continuation_block = builder.reserve(BlockKind::Block);
            let continuation_id = continuation_block.id;
            let init_block = builder.reserve(BlockKind::Loop);
            let init_block_id = init_block.id;

            let body_loc = statement_loc(&for_in.body);
            let loop_block = builder.try_enter(BlockKind::Block, |builder, _block_id| {
                builder.loop_scope(
                    label.map(|s| s.to_string()),
                    init_block_id,
                    continuation_id,
                    |builder| {
                        lower_statement(builder, &for_in.body, None, parent_scope)?;
                        Ok(Terminal::Goto {
                            block: init_block_id,
                            variant: GotoVariant::Continue,
                            id: EvaluationOrder(0),
                            loc: body_loc,
                        })
                    },
                )
            })?;

            let value = lower_expression_to_temporary(builder, &for_in.right)?;
            builder.terminate_with_continuation(
                Terminal::ForIn {
                    init: init_block_id,
                    loop_block,
                    fallthrough: continuation_id,
                    id: EvaluationOrder(0),
                    loc: loc.clone(),
                },
                init_block,
            );

            // Lower the init: NextPropertyOf + assignment
            let left_loc = match for_in.left.as_ref() {
                react_compiler_ast::statements::ForInOfLeft::VariableDeclaration(var_decl) => {
                    convert_opt_loc(&var_decl.base.loc).or(loc.clone())
                }
                react_compiler_ast::statements::ForInOfLeft::Pattern(pat) => {
                    pattern_like_hir_loc(pat).or(loc.clone())
                }
            };
            let next_property = lower_value_to_temporary(
                builder,
                InstructionValue::NextPropertyOf {
                    value,
                    loc: left_loc.clone(),
                },
            )?;

            let assign_result = match for_in.left.as_ref() {
                react_compiler_ast::statements::ForInOfLeft::VariableDeclaration(var_decl) => {
                    if var_decl.declarations.len() != 1 {
                        builder.record_error(CompilerErrorDetail {
                            category: ErrorCategory::Invariant,
                            reason: format!(
                                "Expected only one declaration in ForInStatement init, got {}",
                                var_decl.declarations.len()
                            ),
                            description: None,
                            loc: left_loc.clone(),
                            suggestions: None,
                        })?;
                    }
                    if let Some(declarator) = var_decl.declarations.first() {
                        lower_assignment(
                            builder,
                            left_loc.clone(),
                            InstructionKind::Let,
                            &declarator.id,
                            next_property.clone(),
                            AssignmentStyle::Assignment,
                        )?
                    } else {
                        None
                    }
                }
                react_compiler_ast::statements::ForInOfLeft::Pattern(pattern) => lower_assignment(
                    builder,
                    left_loc.clone(),
                    InstructionKind::Reassign,
                    pattern,
                    next_property.clone(),
                    AssignmentStyle::Assignment,
                )?,
            };
            // Use the assign result (StoreLocal temp) as the test, matching TS behavior
            let test_value = assign_result.unwrap_or(next_property);
            let test = lower_value_to_temporary(
                builder,
                InstructionValue::LoadLocal {
                    place: test_value,
                    loc: left_loc.clone(),
                },
            )?;
            builder.terminate_with_continuation(
                Terminal::Branch {
                    test,
                    consequent: loop_block,
                    alternate: continuation_id,
                    fallthrough: continuation_id,
                    id: EvaluationOrder(0),
                    loc: loc.clone(),
                },
                continuation_block,
            );
        }
        Statement::ForOfStatement(for_of) => {
            let loc = convert_opt_loc(&for_of.base.loc);
            let continuation_block = builder.reserve(BlockKind::Block);
            let continuation_id = continuation_block.id;
            let init_block = builder.reserve(BlockKind::Loop);
            let init_block_id = init_block.id;
            let test_block = builder.reserve(BlockKind::Loop);
            let test_block_id = test_block.id;

            if for_of.is_await {
                builder.record_error(CompilerErrorDetail {
                    category: ErrorCategory::Todo,
                    reason: "(BuildHIR::lowerStatement) Handle for-await loops".to_string(),
                    description: None,
                    loc: loc.clone(),
                    suggestions: None,
                })?;
                return Ok(());
            }

            let body_loc = statement_loc(&for_of.body);
            let loop_block = builder.try_enter(BlockKind::Block, |builder, _block_id| {
                builder.loop_scope(
                    label.map(|s| s.to_string()),
                    init_block_id,
                    continuation_id,
                    |builder| {
                        lower_statement(builder, &for_of.body, None, parent_scope)?;
                        Ok(Terminal::Goto {
                            block: init_block_id,
                            variant: GotoVariant::Continue,
                            id: EvaluationOrder(0),
                            loc: body_loc,
                        })
                    },
                )
            })?;

            let value = lower_expression_to_temporary(builder, &for_of.right)?;
            builder.terminate_with_continuation(
                Terminal::ForOf {
                    init: init_block_id,
                    test: test_block_id,
                    loop_block,
                    fallthrough: continuation_id,
                    id: EvaluationOrder(0),
                    loc: loc.clone(),
                },
                init_block,
            );

            // Init block: GetIterator, goto test
            let iterator = lower_value_to_temporary(
                builder,
                InstructionValue::GetIterator {
                    collection: value.clone(),
                    loc: value.loc.clone(),
                },
            )?;
            builder.terminate_with_continuation(
                Terminal::Goto {
                    block: test_block_id,
                    variant: GotoVariant::Break,
                    id: EvaluationOrder(0),
                    loc: loc.clone(),
                },
                test_block,
            );

            // Test block: IteratorNext, assign, branch
            let left_loc = match for_of.left.as_ref() {
                react_compiler_ast::statements::ForInOfLeft::VariableDeclaration(var_decl) => {
                    convert_opt_loc(&var_decl.base.loc).or(loc.clone())
                }
                react_compiler_ast::statements::ForInOfLeft::Pattern(pat) => {
                    pattern_like_hir_loc(pat).or(loc.clone())
                }
            };
            let advance_iterator = lower_value_to_temporary(
                builder,
                InstructionValue::IteratorNext {
                    iterator: iterator.clone(),
                    collection: value.clone(),
                    loc: left_loc.clone(),
                },
            )?;

            let assign_result = match for_of.left.as_ref() {
                react_compiler_ast::statements::ForInOfLeft::VariableDeclaration(var_decl) => {
                    if var_decl.declarations.len() != 1 {
                        builder.record_error(CompilerErrorDetail {
                            category: ErrorCategory::Invariant,
                            reason: format!(
                                "Expected only one declaration in ForOfStatement init, got {}",
                                var_decl.declarations.len()
                            ),
                            description: None,
                            loc: left_loc.clone(),
                            suggestions: None,
                        })?;
                    }
                    if let Some(declarator) = var_decl.declarations.first() {
                        lower_assignment(
                            builder,
                            left_loc.clone(),
                            InstructionKind::Let,
                            &declarator.id,
                            advance_iterator.clone(),
                            AssignmentStyle::Assignment,
                        )?
                    } else {
                        None
                    }
                }
                react_compiler_ast::statements::ForInOfLeft::Pattern(pattern) => lower_assignment(
                    builder,
                    left_loc.clone(),
                    InstructionKind::Reassign,
                    pattern,
                    advance_iterator.clone(),
                    AssignmentStyle::Assignment,
                )?,
            };
            // Use the assign result (StoreLocal temp) as the test, matching TS behavior
            let test_value = assign_result.unwrap_or(advance_iterator);
            let test = lower_value_to_temporary(
                builder,
                InstructionValue::LoadLocal {
                    place: test_value,
                    loc: left_loc.clone(),
                },
            )?;
            builder.terminate_with_continuation(
                Terminal::Branch {
                    test,
                    consequent: loop_block,
                    alternate: continuation_id,
                    fallthrough: continuation_id,
                    id: EvaluationOrder(0),
                    loc: loc.clone(),
                },
                continuation_block,
            );
        }
        Statement::SwitchStatement(switch_stmt) => {
            let loc = convert_opt_loc(&switch_stmt.base.loc);
            let continuation_block = builder.reserve(BlockKind::Block);
            let continuation_id = continuation_block.id;

            // Iterate through cases in reverse order so that previous blocks can
            // fallthrough to successors
            let mut fallthrough = continuation_id;
            let mut cases: Vec<Case> = Vec::new();
            let mut has_default = false;

            for ii in (0..switch_stmt.cases.len()).rev() {
                let case = &switch_stmt.cases[ii];
                let case_loc = convert_opt_loc(&case.base.loc);

                if case.test.is_none() {
                    if has_default {
                        builder.record_error(CompilerErrorDetail {
                            category: ErrorCategory::Syntax,
                            reason: "Expected at most one `default` branch in a switch statement"
                                .to_string(),
                            description: None,
                            loc: case_loc.clone(),
                            suggestions: None,
                        })?;
                        break;
                    }
                    has_default = true;
                }

                let fallthrough_target = fallthrough;
                let block = builder.try_enter(BlockKind::Block, |builder, _block_id| {
                    builder.switch_scope(label.map(|s| s.to_string()), continuation_id, |builder| {
                        for consequent in &case.consequent {
                            lower_statement(builder, consequent, None, parent_scope)?;
                        }
                        Ok(Terminal::Goto {
                            block: fallthrough_target,
                            variant: GotoVariant::Break,
                            id: EvaluationOrder(0),
                            loc: case_loc.clone(),
                        })
                    })
                })?;

                let test = if let Some(test_expr) = &case.test {
                    Some(lower_reorderable_expression(builder, test_expr)?)
                } else {
                    None
                };

                cases.push(Case { test, block });
                fallthrough = block;
            }

            // Reverse back to original order
            cases.reverse();

            // If no default case, add one that jumps to continuation
            if !has_default {
                cases.push(Case {
                    test: None,
                    block: continuation_id,
                });
            }

            let test = lower_expression_to_temporary(builder, &switch_stmt.discriminant)?;
            builder.terminate_with_continuation(
                Terminal::Switch {
                    test,
                    cases,
                    fallthrough: continuation_id,
                    id: EvaluationOrder(0),
                    loc,
                },
                continuation_block,
            );
        }
        Statement::TryStatement(try_stmt) => {
            let loc = convert_opt_loc(&try_stmt.base.loc);
            let continuation_block = builder.reserve(BlockKind::Block);
            let continuation_id = continuation_block.id;

            let handler_clause = match &try_stmt.handler {
                Some(h) => h,
                None => {
                    builder.record_error(CompilerErrorDetail {
                        category: ErrorCategory::Todo,
                        reason:
                            "(BuildHIR::lowerStatement) Handle TryStatement without a catch clause"
                                .to_string(),
                        description: None,
                        loc: loc.clone(),
                        suggestions: None,
                    })?;
                    return Ok(());
                }
            };

            if try_stmt.finalizer.is_some() {
                builder.record_error(CompilerErrorDetail {
                    category: ErrorCategory::Todo,
                    reason: "(BuildHIR::lowerStatement) Handle TryStatement with a finalizer ('finally') clause".to_string(),
                    description: None,
                    loc: loc.clone(),
                    suggestions: None,
                })?;
            }

            // Set up handler binding if catch has a param
            let handler_binding_info: Option<(Place, react_compiler_ast::patterns::PatternLike)> =
                if let Some(param) = &handler_clause.param {
                    // Check for destructuring in catch clause params.
                    // Match TS behavior: Babel doesn't register destructured catch bindings
                    // in its scope, so resolveIdentifier fails and records an invariant error.
                    let is_destructuring = matches!(
                        param,
                        react_compiler_ast::patterns::PatternLike::ObjectPattern(_)
                            | react_compiler_ast::patterns::PatternLike::ArrayPattern(_)
                    );
                    if is_destructuring {
                        // Iterate the pattern to find all identifier locs for error reporting
                        fn collect_identifier_locs(
                            pat: &react_compiler_ast::patterns::PatternLike,
                            locs: &mut Vec<Option<SourceLocation>>,
                        ) {
                            match pat {
                                react_compiler_ast::patterns::PatternLike::Identifier(id) => {
                                    locs.push(convert_opt_loc(&id.base.loc));
                                }
                                react_compiler_ast::patterns::PatternLike::ObjectPattern(obj) => {
                                    for prop in &obj.properties {
                                        match prop {
                                            react_compiler_ast::patterns::ObjectPatternProperty::ObjectProperty(p) => {
                                                collect_identifier_locs(&p.value, locs);
                                            }
                                            react_compiler_ast::patterns::ObjectPatternProperty::RestElement(r) => {
                                                collect_identifier_locs(&r.argument, locs);
                                            }
                                        }
                                    }
                                }
                                react_compiler_ast::patterns::PatternLike::ArrayPattern(arr) => {
                                    for elem in &arr.elements {
                                        if let Some(e) = elem {
                                            collect_identifier_locs(e, locs);
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                        let mut id_locs = Vec::new();
                        collect_identifier_locs(param, &mut id_locs);
                        for id_loc in id_locs {
                            builder.record_error(CompilerErrorDetail {
                                reason: "(BuildHIR::lowerAssignment) Could not find binding for declaration.".to_string(),
                                category: ErrorCategory::Invariant,
                                loc: id_loc,
                                description: None,
                                suggestions: None,
                            })?;
                        }
                        None
                    } else {
                        let param_loc = convert_opt_loc(&pattern_like_loc(param));
                        let id = builder.make_temporary(param_loc.clone());
                        promote_temporary(builder, id);
                        let place = Place {
                            identifier: id,
                            effect: Effect::Unknown,
                            reactive: false,
                            loc: param_loc.clone(),
                        };
                        // Emit DeclareLocal for the catch binding
                        lower_value_to_temporary(
                            builder,
                            InstructionValue::DeclareLocal {
                                lvalue: LValue {
                                    kind: InstructionKind::Catch,
                                    place: place.clone(),
                                },
                                type_annotation: None,
                                loc: param_loc,
                            },
                        )?;
                        Some((place, param.clone()))
                    }
                } else {
                    None
                };

            // Create the handler (catch) block
            let handler_binding_for_block = handler_binding_info.clone();
            let handler_loc = convert_opt_loc(&handler_clause.base.loc);
            // Use the catch param's loc for the assignment, matching TS: handlerBinding.path.node.loc
            let handler_param_loc = handler_clause
                .param
                .as_ref()
                .and_then(|p| convert_opt_loc(&pattern_like_loc(p)));
            let handler_block = builder.try_enter(BlockKind::Catch, |builder, _block_id| {
                if let Some((ref place, ref pattern)) = handler_binding_for_block {
                    lower_assignment(
                        builder,
                        handler_param_loc.clone().or_else(|| handler_loc.clone()),
                        InstructionKind::Catch,
                        pattern,
                        place.clone(),
                        AssignmentStyle::Assignment,
                    )?;
                }
                // Lower the catch body using lower_block_statement to get hoisting support.
                // Match TS behavior where `lowerStatement(builder, handlerPath.get('body'))`
                // processes the catch body as a BlockStatement (with hoisting).
                // Use the catch clause's scope since the catch body block shares
                // the CatchClause scope in Babel (contains the catch param binding).
                // Use the catch clause's scope (which contains the catch param binding).
                // Fall back to the body block's own scope if the catch clause scope is missing.
                let catch_scope = builder
                    .scope_info()
                    .resolve_scope_for_node(handler_clause.base.node_id)
                    .or_else(|| {
                        builder
                            .scope_info()
                            .resolve_scope_for_node(handler_clause.body.base.node_id)
                    });
                if let Some(scope_id) = catch_scope {
                    lower_block_statement_with_scope(builder, &handler_clause.body, scope_id)?;
                } else {
                    // No scope found — this shouldn't happen with well-formed Babel output.
                    // Fall back to plain block lowering (no hoisting) rather than panicking,
                    // since this is a non-critical degradation.
                    lower_block_statement(builder, &handler_clause.body, parent_scope)?;
                }
                Ok(Terminal::Goto {
                    block: continuation_id,
                    variant: GotoVariant::Break,
                    id: EvaluationOrder(0),
                    loc: handler_loc.clone(),
                })
            })?;

            // Create the try block
            // Use lower_block_statement to get hoisting support for bindings
            // declared inside the try body. This matches the catch block's use of
            // lower_block_statement_with_scope and ensures self-referencing function
            // declarations (e.g., `const loop = () => { loop(); }`) inside try blocks
            // are correctly promoted to context variables.
            let try_body_loc = convert_opt_loc(&try_stmt.block.base.loc);
            let try_block = builder.try_enter(BlockKind::Block, |builder, _block_id| {
                builder.try_enter_try_catch(handler_block, |builder| {
                    lower_block_statement(builder, &try_stmt.block, parent_scope)?;
                    Ok(())
                })?;
                Ok(Terminal::Goto {
                    block: continuation_id,
                    variant: GotoVariant::Try,
                    id: EvaluationOrder(0),
                    loc: try_body_loc.clone(),
                })
            })?;

            builder.terminate_with_continuation(
                Terminal::Try {
                    block: try_block,
                    handler_binding: handler_binding_info.map(|(place, _)| place),
                    handler: handler_block,
                    fallthrough: continuation_id,
                    id: EvaluationOrder(0),
                    loc,
                },
                continuation_block,
            );
        }
        Statement::LabeledStatement(labeled_stmt) => {
            let label_name = &labeled_stmt.label.name;
            let loc = convert_opt_loc(&labeled_stmt.base.loc);

            // Check if the body is a loop statement - if so, delegate with label
            match labeled_stmt.body.as_ref() {
                Statement::ForStatement(_)
                | Statement::WhileStatement(_)
                | Statement::DoWhileStatement(_)
                | Statement::ForInStatement(_)
                | Statement::ForOfStatement(_) => {
                    // Labeled loops are special because of continue, push the label down
                    lower_statement(builder, &labeled_stmt.body, Some(label_name), parent_scope)?;
                }
                _ => {
                    // All other statements create a continuation block to allow `break`
                    let continuation_block = builder.reserve(BlockKind::Block);
                    let continuation_id = continuation_block.id;
                    let body_loc = statement_loc(&labeled_stmt.body);

                    let block = builder.try_enter(BlockKind::Block, |builder, _block_id| {
                        builder.label_scope(label_name.clone(), continuation_id, |builder| {
                            lower_statement(builder, &labeled_stmt.body, None, parent_scope)?;
                            Ok(())
                        })?;
                        Ok(Terminal::Goto {
                            block: continuation_id,
                            variant: GotoVariant::Break,
                            id: EvaluationOrder(0),
                            loc: body_loc,
                        })
                    })?;

                    builder.terminate_with_continuation(
                        Terminal::Label {
                            block,
                            fallthrough: continuation_id,
                            id: EvaluationOrder(0),
                            loc,
                        },
                        continuation_block,
                    );
                }
            }
        }
        Statement::WithStatement(with_stmt) => {
            let loc = convert_opt_loc(&with_stmt.base.loc);
            builder.record_error(CompilerErrorDetail {
                category: ErrorCategory::UnsupportedSyntax,
                reason: "JavaScript 'with' syntax is not supported".to_string(),
                description: Some("'with' syntax is considered deprecated and removed from JavaScript standards, consider alternatives".to_string()),
                loc: loc.clone(),
                suggestions: None,
            })?;
            lower_value_to_temporary(
                builder,
                InstructionValue::UnsupportedNode {
                    node_type: Some("WithStatement".to_string()),
                    original_node: serialize_statement(stmt),
                    loc,
                },
            )?;
        }
        Statement::FunctionDeclaration(func_decl) => {
            lower_function_declaration(builder, func_decl)?;
        }
        Statement::ClassDeclaration(cls) => {
            let loc = convert_opt_loc(&cls.base.loc);
            builder.record_error(CompilerErrorDetail {
                category: ErrorCategory::UnsupportedSyntax,
                reason: "Inline `class` declarations are not supported".to_string(),
                description: Some(
                    "Move class declarations outside of components/hooks".to_string(),
                ),
                loc: loc.clone(),
                suggestions: None,
            })?;
            lower_value_to_temporary(
                builder,
                InstructionValue::UnsupportedNode {
                    node_type: Some("ClassDeclaration".to_string()),
                    original_node: serialize_statement(stmt),
                    loc,
                },
            )?;
        }
        Statement::ImportDeclaration(_)
        | Statement::ExportNamedDeclaration(_)
        | Statement::ExportDefaultDeclaration(_)
        | Statement::ExportAllDeclaration(_) => {
            let (loc, node_type_name) = match stmt {
                Statement::ImportDeclaration(s) => {
                    (convert_opt_loc(&s.base.loc), "ImportDeclaration")
                }
                Statement::ExportNamedDeclaration(s) => {
                    (convert_opt_loc(&s.base.loc), "ExportNamedDeclaration")
                }
                Statement::ExportDefaultDeclaration(s) => {
                    (convert_opt_loc(&s.base.loc), "ExportDefaultDeclaration")
                }
                Statement::ExportAllDeclaration(s) => {
                    (convert_opt_loc(&s.base.loc), "ExportAllDeclaration")
                }
                _ => unreachable!(),
            };
            builder.record_error(CompilerErrorDetail {
                category: ErrorCategory::Syntax,
                reason: "JavaScript `import` and `export` statements may only appear at the top level of a module".to_string(),
                description: None,
                loc: loc.clone(),
                suggestions: None,
            })?;
            lower_value_to_temporary(
                builder,
                InstructionValue::UnsupportedNode {
                    node_type: Some(node_type_name.to_string()),
                    original_node: serialize_statement(stmt),
                    loc,
                },
            )?;
        }
        // TypeScript/Flow declarations are type-only, skip them
        Statement::TSEnumDeclaration(e) => {
            let loc = convert_opt_loc(&e.base.loc);
            let original_node = serde_json::to_value(
                &react_compiler_ast::statements::Statement::TSEnumDeclaration(e.clone()),
            )
            .ok();
            lower_value_to_temporary(
                builder,
                InstructionValue::UnsupportedNode {
                    node_type: Some("TSEnumDeclaration".to_string()),
                    original_node,
                    loc,
                },
            )?;
        }
        Statement::EnumDeclaration(e) => {
            let loc = convert_opt_loc(&e.base.loc);
            let original_node = serde_json::to_value(
                &react_compiler_ast::statements::Statement::EnumDeclaration(e.clone()),
            )
            .ok();
            lower_value_to_temporary(
                builder,
                InstructionValue::UnsupportedNode {
                    node_type: Some("EnumDeclaration".to_string()),
                    original_node,
                    loc,
                },
            )?;
        }
        // TypeScript/Flow type declarations are type-only, skip them
        Statement::TSTypeAliasDeclaration(_)
        | Statement::TSInterfaceDeclaration(_)
        | Statement::TSModuleDeclaration(_)
        | Statement::TSDeclareFunction(_)
        | Statement::TypeAlias(_)
        | Statement::OpaqueType(_)
        | Statement::InterfaceDeclaration(_)
        | Statement::DeclareVariable(_)
        | Statement::DeclareFunction(_)
        | Statement::DeclareClass(_)
        | Statement::DeclareModule(_)
        | Statement::DeclareModuleExports(_)
        | Statement::DeclareExportDeclaration(_)
        | Statement::DeclareExportAllDeclaration(_)
        | Statement::DeclareInterface(_)
        | Statement::DeclareTypeAlias(_)
        | Statement::DeclareOpaqueType(_) => {}
        // The TS reference can only reach its equivalent default case via
        // assertExhaustive (Babel's closed Statement type), so it crashes;
        // here unmodeled syntax is reachable by construction and degrades
        // like the other unsupported-statement arms instead.
        Statement::Unknown(unknown) => {
            let loc = convert_opt_loc(&unknown.base().loc);
            let node_type = unknown.node_type().to_string();
            builder.record_error(CompilerErrorDetail {
                category: ErrorCategory::UnsupportedSyntax,
                reason: format!("Unsupported statement kind '{node_type}'"),
                description: None,
                loc: loc.clone(),
                suggestions: None,
            })?;
            lower_value_to_temporary(
                builder,
                InstructionValue::UnsupportedNode {
                    node_type: Some(node_type),
                    original_node: Some(unknown.raw().parse_value()),
                    loc,
                },
            )?;
        }
    }
    Ok(())
}

// =============================================================================
// lower() entry point
// =============================================================================
