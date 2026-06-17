// Copyright (c) Meta Platforms, Inc. and affiliates.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the root directory of this source tree.

//! Apply lowering-time binding renames to an output AST.
//!
//! When lowering resolves a name collision (an inner binding whose name shadows
//! an outer binding that is also referenced), it allocates a fresh name
//! (`name_0`, `name_1`, …) for the inner binding. The reference (Babel) compiler
//! records these via `scope.rename`, which mutates the shared source AST in
//! place — so even an UNCOMPILED passthrough function prints the renamed name.
//!
//! The native port records the same renames on the `Environment` (keyed by the
//! binding's declaration start offset) but prints uncompiled functions straight
//! from the original oxc AST, which still carries the original name. This module
//! is the native analogue of Babel's `scope.rename`: given the rename list and
//! an output `Program`, it rewrites the binding + all of its in-scope references
//! to the new name.
//!
//! Renames are keyed by the binding's declaration start offset. Because the
//! output program is re-parsed from the SAME source text, source spans line up
//! with the offsets recorded during lowering.

use std::collections::HashMap;

use oxc_allocator::Allocator;
use oxc_allocator::FromIn;
use oxc_ast::ast::Program;
use oxc_ast_visit::VisitMut;
use oxc_semantic::SemanticBuilder;
use oxc_span::GetSpan;
use oxc_str::Ident;

use react_compiler::entrypoint::compile_result::BindingRenameInfo;

/// Rewrite each renamed binding (and its references) in `program` to its new
/// name, mirroring the reference compiler's `scope.rename`. No-op when `renames`
/// is empty. `allocator` is the arena that owns `program` (new name atoms are
/// allocated into it).
pub fn apply_renames_to_program<'a>(
    program: &mut Program<'a>,
    allocator: &'a Allocator,
    renames: &[BindingRenameInfo],
) {
    if renames.is_empty() {
        return;
    }

    // Index renames by the binding's declaration start offset.
    let by_decl_start: HashMap<u32, &BindingRenameInfo> =
        renames.iter().map(|r| (r.declaration_start, r)).collect();

    // Collect the set of identifier-node start offsets (binding site + every
    // resolved reference) that must be rewritten, mapped to their new name.
    // We build a throwaway semantic model to resolve the symbol-reference graph,
    // collect the offsets (plain `u32`s), then drop semantic before mutating the
    // program (semantic borrows it immutably).
    let occurrences: HashMap<u32, String> = {
        let semantic = SemanticBuilder::new()
            .with_build_nodes(true)
            .build(program)
            .semantic;
        let scoping = semantic.scoping();
        let mut out: HashMap<u32, String> = HashMap::new();
        for symbol_id in scoping.symbol_ids() {
            let decl_span = semantic.symbol_declaration(symbol_id).kind().span();
            let Some(rename) = by_decl_start.get(&decl_span.start) else {
                continue;
            };
            // Only rename a symbol whose current name still matches the recorded
            // `original` — guards against renaming an unrelated binding that
            // happens to start at the same offset.
            if scoping.symbol_name(symbol_id) != rename.original {
                continue;
            }
            // The binding identifier itself.
            out.insert(decl_span.start, rename.renamed.clone());
            // Every resolved reference to this symbol.
            for reference in scoping.get_resolved_references(symbol_id) {
                let ref_span = semantic.nodes().get_node(reference.node_id()).kind().span();
                out.insert(ref_span.start, rename.renamed.clone());
            }
        }
        out
    };

    if occurrences.is_empty() {
        return;
    }

    let mut renamer = Renamer {
        allocator,
        occurrences,
    };
    renamer.visit_program(program);
}

struct Renamer<'a> {
    allocator: &'a Allocator,
    occurrences: HashMap<u32, String>,
}

impl<'a> Renamer<'a> {
    fn rewrite(&self, span_start: u32, name: &mut Ident<'a>) {
        if let Some(new_name) = self.occurrences.get(&span_start) {
            *name = Ident::from_in(new_name.as_str(), self.allocator);
        }
    }
}

impl<'a> VisitMut<'a> for Renamer<'a> {
    fn visit_binding_identifier(&mut self, it: &mut oxc_ast::ast::BindingIdentifier<'a>) {
        self.rewrite(it.span.start, &mut it.name);
    }

    fn visit_identifier_reference(&mut self, it: &mut oxc_ast::ast::IdentifierReference<'a>) {
        self.rewrite(it.span.start, &mut it.name);
    }
}
