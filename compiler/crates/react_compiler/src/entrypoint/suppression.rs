/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */
use oxc_ast::ast::Comment;
use react_compiler_diagnostics::{
    CompilerDiagnostic, CompilerDiagnosticDetail, CompilerError, CompilerSuggestion,
    CompilerSuggestionOperation, ErrorCategory,
};

/// The inner (delimiter-stripped) text of an oxc comment — the equivalent of
/// Babel's `comment.value`, computed on demand by slicing the source over the
/// comment's content span.
fn comment_value<'a>(comment: &Comment, source_text: &'a str) -> &'a str {
    let content_span = comment.content_span();
    source_text
        .get(content_span.start as usize..content_span.end as usize)
        .unwrap_or("")
}

#[derive(Debug, Clone)]
pub enum SuppressionSource {
    Eslint,
    Flow,
}

/// Captures the start and end range of a pair of eslint-disable ... eslint-enable comments.
/// In the case of a CommentLine or a relevant Flow suppression, both the disable and enable
/// point to the same comment.
///
/// The enable comment can be missing in the case where only a disable block is present,
/// ie the rest of the file has potential React violations.
///
/// Comments are stored as oxc [`Comment`]s; the inner text is recomputed on
/// demand from the source via [`comment_value`], and `span.start`/`.end` give
/// the full comment range used for overlap checks and the removal suggestion.
#[derive(Debug, Clone)]
pub struct SuppressionRange {
    pub disable_comment: Comment,
    pub enable_comment: Option<Comment>,
    pub source: SuppressionSource,
}

/// Check if a comment value matches `eslint-disable-next-line <rule>` for any rule in `rule_names`.
fn matches_eslint_disable_next_line(value: &str, rule_names: &[String]) -> bool {
    if let Some(rest) = value.strip_prefix("eslint-disable-next-line ") {
        return rule_names
            .iter()
            .any(|name| rest.starts_with(name.as_str()));
    }
    // Also check with leading space (comment values often have leading whitespace)
    let trimmed = value.trim_start();
    if let Some(rest) = trimmed.strip_prefix("eslint-disable-next-line ") {
        return rule_names
            .iter()
            .any(|name| rest.starts_with(name.as_str()));
    }
    false
}

/// Check if a comment value matches `eslint-disable <rule>` for any rule in `rule_names`.
fn matches_eslint_disable(value: &str, rule_names: &[String]) -> bool {
    if let Some(rest) = value.strip_prefix("eslint-disable ") {
        return rule_names
            .iter()
            .any(|name| rest.starts_with(name.as_str()));
    }
    let trimmed = value.trim_start();
    if let Some(rest) = trimmed.strip_prefix("eslint-disable ") {
        return rule_names
            .iter()
            .any(|name| rest.starts_with(name.as_str()));
    }
    false
}

/// Check if a comment value matches `eslint-enable <rule>` for any rule in `rule_names`.
fn matches_eslint_enable(value: &str, rule_names: &[String]) -> bool {
    if let Some(rest) = value.strip_prefix("eslint-enable ") {
        return rule_names
            .iter()
            .any(|name| rest.starts_with(name.as_str()));
    }
    let trimmed = value.trim_start();
    if let Some(rest) = trimmed.strip_prefix("eslint-enable ") {
        return rule_names
            .iter()
            .any(|name| rest.starts_with(name.as_str()));
    }
    false
}

/// Check if a comment value matches a Flow suppression pattern.
/// Matches: $FlowFixMe[react-rule, $FlowFixMe_xxx[react-rule,
///          $FlowExpectedError[react-rule, $FlowIssue[react-rule
fn matches_flow_suppression(value: &str) -> bool {
    // Find "$Flow" anywhere in the value
    let Some(idx) = value.find("$Flow") else {
        return false;
    };
    let after_dollar_flow = &value[idx + "$Flow".len()..];

    // Match FlowFixMe (with optional word chars), FlowExpectedError, or FlowIssue
    let after_kind = if let Some(rest) = after_dollar_flow.strip_prefix("FixMe") {
        // Skip "FixMe" + any word characters
        let word_end = rest
            .find(|c: char| !c.is_alphanumeric() && c != '_')
            .unwrap_or(rest.len());
        &rest[word_end..]
    } else if let Some(rest) = after_dollar_flow.strip_prefix("ExpectedError") {
        rest
    } else if let Some(rest) = after_dollar_flow.strip_prefix("Issue") {
        rest
    } else {
        return false;
    };

    // Must be followed by "[react-rule"
    after_kind.starts_with("[react-rule")
}

/// Parse eslint-disable/enable and Flow suppression comments from program comments.
/// Equivalent to findProgramSuppressions in Suppression.ts.
///
/// Consumes oxc [`Comment`]s directly: each comment's inner text is sliced from
/// `source_text` on demand and its `span.start`/`.end` give the full range.
pub fn find_program_suppressions(
    comments: &[Comment],
    source_text: &str,
    rule_names: Option<&[String]>,
    flow_suppressions: bool,
) -> Vec<SuppressionRange> {
    let mut suppression_ranges: Vec<SuppressionRange> = Vec::new();
    let mut disable_comment: Option<Comment> = None;
    let mut enable_comment: Option<Comment> = None;
    let mut source: Option<SuppressionSource> = None;

    let has_rules = matches!(rule_names, Some(names) if !names.is_empty());

    for comment in comments {
        let value = comment_value(comment, source_text);

        // Check for eslint-disable-next-line (only if not already within a block)
        if disable_comment.is_none() && has_rules
            && let Some(names) = rule_names
                && matches_eslint_disable_next_line(value, names) {
                    disable_comment = Some(*comment);
                    enable_comment = Some(*comment);
                    source = Some(SuppressionSource::Eslint);
                }

        // Check for Flow suppression (only if not already within a block)
        if flow_suppressions && disable_comment.is_none() && matches_flow_suppression(value) {
            disable_comment = Some(*comment);
            enable_comment = Some(*comment);
            source = Some(SuppressionSource::Flow);
        }

        // Check for eslint-disable (block start)
        if has_rules
            && let Some(names) = rule_names
                && matches_eslint_disable(value, names) {
                    disable_comment = Some(*comment);
                    source = Some(SuppressionSource::Eslint);
                }

        // Check for eslint-enable (block end)
        if has_rules
            && let Some(names) = rule_names
                && matches_eslint_enable(value, names)
                    && matches!(source, Some(SuppressionSource::Eslint)) {
                        enable_comment = Some(*comment);
                    }

        // If we have a complete suppression, push it
        if disable_comment.is_some() && source.is_some() {
            suppression_ranges.push(SuppressionRange {
                disable_comment: disable_comment.take().unwrap(),
                enable_comment: enable_comment.take(),
                source: source.take().unwrap(),
            });
        }
    }

    suppression_ranges
}

/// Check if suppression ranges overlap with a function's source range.
/// A suppression affects a function if:
/// 1. The suppression is within the function's body
/// 2. The suppression wraps the function
pub fn filter_suppressions_that_affect_function(
    suppressions: &[SuppressionRange],
    fn_start: u32,
    fn_end: u32,
) -> Vec<&SuppressionRange> {
    let mut suppressions_in_scope: Vec<&SuppressionRange> = Vec::new();

    for suppression in suppressions {
        let disable_start = suppression.disable_comment.span.start;

        // The suppression is within the function
        if disable_start > fn_start
            && (suppression.enable_comment.is_none()
                || suppression
                    .enable_comment
                    .as_ref()
                    .is_some_and(|c| c.span.end < fn_end))
        {
            suppressions_in_scope.push(suppression);
        }

        // The suppression wraps the function
        if disable_start < fn_start
            && (suppression.enable_comment.is_none()
                || suppression
                    .enable_comment
                    .as_ref()
                    .is_some_and(|c| c.span.end > fn_end))
        {
            suppressions_in_scope.push(suppression);
        }
    }

    suppressions_in_scope
}

/// Convert suppression ranges to a CompilerError. The comment text and
/// diagnostic location are recomputed from `source_text` on demand.
pub fn suppressions_to_compiler_error(
    suppressions: &[SuppressionRange],
    source_text: &str,
) -> CompilerError {
    assert!(
        !suppressions.is_empty(),
        "Expected at least one suppression comment source range"
    );

    let mut error = CompilerError::new();

    for suppression in suppressions {
        let disable_start = suppression.disable_comment.span.start;
        let disable_end = suppression.disable_comment.span.end;

        let (reason, suggestion) = match suppression.source {
            SuppressionSource::Eslint => (
                "React Compiler has skipped optimizing this component because one or more React ESLint rules were disabled",
                "Remove the ESLint suppression and address the React error",
            ),
            SuppressionSource::Flow => (
                "React Compiler has skipped optimizing this component because one or more React rule violations were reported by Flow",
                "Remove the Flow suppression and address the React error",
            ),
        };

        let description = format!(
            "React Compiler only works when your components follow all the rules of React, disabling them may result in unexpected or incorrect behavior. Found suppression `{}`",
            comment_value(&suppression.disable_comment, source_text).trim()
        );

        let mut diagnostic =
            CompilerDiagnostic::new(ErrorCategory::Suppression, reason, Some(description));

        diagnostic.suggestions = Some(vec![CompilerSuggestion {
            description: suggestion.to_string(),
            range: (disable_start as usize, disable_end as usize),
            op: CompilerSuggestionOperation::Remove,
            text: None,
        }]);

        // Add error detail with location info, computed from the full comment span.
        let (start_line, start_column) =
            react_compiler_diagnostics::offset_to_line_column(source_text, disable_start);
        let (end_line, end_column) =
            react_compiler_diagnostics::offset_to_line_column(source_text, disable_end);
        let loc = Some(react_compiler_diagnostics::SourceLocation {
            start: react_compiler_diagnostics::Position {
                line: start_line,
                column: start_column,
                index: Some(disable_start),
            },
            end: react_compiler_diagnostics::Position {
                line: end_line,
                column: end_column,
                index: Some(disable_end),
            },
        });

        diagnostic = diagnostic.with_detail(CompilerDiagnosticDetail::Error {
            loc,
            message: Some("Found React rule suppression".to_string()),
            identifier_name: None,
        });

        error.push_diagnostic(diagnostic);
    }

    error
}
