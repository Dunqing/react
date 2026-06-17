/**
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the MIT license found in the
 * LICENSE file in the root directory of this source tree.
 */
use react_compiler_diagnostics::{
    CompilerDiagnostic, CompilerDiagnosticDetail, CompilerError, CompilerSuggestion,
    CompilerSuggestionOperation, ErrorCategory,
};

/// A 1-based line / 0-based column source position, with an optional byte
/// `index`. Local to suppression parsing (mirrors Babel's `t.SourceLocation`
/// position shape).
#[derive(Debug, Clone)]
pub struct Position {
    pub line: u32,
    pub column: u32,
    pub index: Option<u32>,
}

/// A source-location span (start/end positions). `filename`/`identifier_name`
/// are kept for shape-parity with the diagnostic location but are unused here.
#[derive(Debug, Clone)]
pub struct SourceLocation {
    pub start: Position,
    pub end: Position,
    #[allow(dead_code)]
    pub filename: Option<String>,
    #[allow(dead_code)]
    pub identifier_name: Option<String>,
}

/// The inner data of a comment: its (delimiter-stripped) text, full span
/// offsets, and source location. Mirrors Babel's `t.Comment`.
#[derive(Debug, Clone)]
pub struct CommentData {
    pub value: String,
    pub start: Option<u32>,
    pub end: Option<u32>,
    pub loc: Option<SourceLocation>,
}

/// A program comment — line (`//`) or block (`/* */`).
#[derive(Debug, Clone)]
pub enum Comment {
    CommentLine(CommentData),
    CommentBlock(CommentData),
}

/// Convert oxc program comments into the local [`Comment`] shape used by
/// [`find_program_suppressions`]. Mirrors Babel's `t.Comment`:
/// - `value` is the comment's inner text (delimiters stripped), matching
///   Babel's `comment.value`.
/// - `start`/`end` are the *full* comment span (including `/* */` or `//`),
///   matching Babel's `comment.start`/`comment.end` used for range checks and
///   the removal suggestion.
pub fn oxc_comments_to_ast_comments(
    comments: &[oxc_ast::ast::Comment],
    source_text: &str,
) -> Vec<Comment> {
    comments
        .iter()
        .map(|comment| {
            let full_span = comment.span;
            let content_span = comment.content_span();
            let value = source_text
                .get(content_span.start as usize..content_span.end as usize)
                .unwrap_or("")
                .to_string();
            let loc = SourceLocation {
                start: position_of_offset(source_text, full_span.start),
                end: position_of_offset(source_text, full_span.end),
                filename: None,
                identifier_name: None,
            };
            let data = CommentData {
                value,
                start: Some(full_span.start),
                end: Some(full_span.end),
                loc: Some(loc),
            };
            if comment.is_line() {
                Comment::CommentLine(data)
            } else {
                Comment::CommentBlock(data)
            }
        })
        .collect()
}

/// Compute a 1-based line / 0-based column `Position` from a byte offset.
fn position_of_offset(source: &str, offset: u32) -> Position {
    let off = offset as usize;
    let mut line: u32 = 1;
    let mut line_start: usize = 0;
    for (i, b) in source.as_bytes().iter().enumerate() {
        if i >= off {
            break;
        }
        if *b == b'\n' {
            line += 1;
            line_start = i + 1;
        }
    }
    Position {
        line,
        column: (off.saturating_sub(line_start)) as u32,
        index: Some(offset),
    }
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
#[derive(Debug, Clone)]
pub struct SuppressionRange {
    pub disable_comment: CommentData,
    pub enable_comment: Option<CommentData>,
    pub source: SuppressionSource,
}

fn comment_data(comment: &Comment) -> &CommentData {
    match comment {
        Comment::CommentBlock(data) | Comment::CommentLine(data) => data,
    }
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
/// Equivalent to findProgramSuppressions in Suppression.ts
pub fn find_program_suppressions(
    comments: &[Comment],
    rule_names: Option<&[String]>,
    flow_suppressions: bool,
) -> Vec<SuppressionRange> {
    let mut suppression_ranges: Vec<SuppressionRange> = Vec::new();
    let mut disable_comment: Option<CommentData> = None;
    let mut enable_comment: Option<CommentData> = None;
    let mut source: Option<SuppressionSource> = None;

    let has_rules = matches!(rule_names, Some(names) if !names.is_empty());

    for comment in comments {
        let data = comment_data(comment);

        if data.start.is_none() || data.end.is_none() {
            continue;
        }

        // Check for eslint-disable-next-line (only if not already within a block)
        if disable_comment.is_none() && has_rules
            && let Some(names) = rule_names
                && matches_eslint_disable_next_line(&data.value, names) {
                    disable_comment = Some(data.clone());
                    enable_comment = Some(data.clone());
                    source = Some(SuppressionSource::Eslint);
                }

        // Check for Flow suppression (only if not already within a block)
        if flow_suppressions && disable_comment.is_none() && matches_flow_suppression(&data.value) {
            disable_comment = Some(data.clone());
            enable_comment = Some(data.clone());
            source = Some(SuppressionSource::Flow);
        }

        // Check for eslint-disable (block start)
        if has_rules
            && let Some(names) = rule_names
                && matches_eslint_disable(&data.value, names) {
                    disable_comment = Some(data.clone());
                    source = Some(SuppressionSource::Eslint);
                }

        // Check for eslint-enable (block end)
        if has_rules
            && let Some(names) = rule_names
                && matches_eslint_enable(&data.value, names)
                    && matches!(source, Some(SuppressionSource::Eslint)) {
                        enable_comment = Some(data.clone());
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
        let disable_start = match suppression.disable_comment.start {
            Some(s) => s,
            None => continue,
        };

        // The suppression is within the function
        if disable_start > fn_start
            && (suppression.enable_comment.is_none()
                || suppression
                    .enable_comment
                    .as_ref()
                    .and_then(|c| c.end)
                    .is_some_and(|end| end < fn_end))
        {
            suppressions_in_scope.push(suppression);
        }

        // The suppression wraps the function
        if disable_start < fn_start
            && (suppression.enable_comment.is_none()
                || suppression
                    .enable_comment
                    .as_ref()
                    .and_then(|c| c.end)
                    .is_some_and(|end| end > fn_end))
        {
            suppressions_in_scope.push(suppression);
        }
    }

    suppressions_in_scope
}

/// Convert suppression ranges to a CompilerError.
pub fn suppressions_to_compiler_error(suppressions: &[SuppressionRange]) -> CompilerError {
    assert!(
        !suppressions.is_empty(),
        "Expected at least one suppression comment source range"
    );

    let mut error = CompilerError::new();

    for suppression in suppressions {
        let (disable_start, disable_end) = match (
            suppression.disable_comment.start,
            suppression.disable_comment.end,
        ) {
            (Some(s), Some(e)) => (s, e),
            _ => continue,
        };

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
            suppression.disable_comment.value.trim()
        );

        let mut diagnostic =
            CompilerDiagnostic::new(ErrorCategory::Suppression, reason, Some(description));

        diagnostic.suggestions = Some(vec![CompilerSuggestion {
            description: suggestion.to_string(),
            range: (disable_start as usize, disable_end as usize),
            op: CompilerSuggestionOperation::Remove,
            text: None,
        }]);

        // Add error detail with location info
        let loc = suppression.disable_comment.loc.as_ref().map(|l| {
            react_compiler_diagnostics::SourceLocation {
                start: react_compiler_diagnostics::Position {
                    line: l.start.line,
                    column: l.start.column,
                    index: l.start.index,
                },
                end: react_compiler_diagnostics::Position {
                    line: l.end.line,
                    column: l.end.column,
                    index: l.end.index,
                },
            }
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
