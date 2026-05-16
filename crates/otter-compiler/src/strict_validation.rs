//! Strict-mode early-error validation pass.
//!
//! Walks the parsed program once and rejects ECMA-262 strict-mode
//! early errors that `oxc_parser` does not flag on its own. The pass
//! must run before the bytecode lowering pipeline so it can surface a
//! `SyntaxError` with phase `parse` to the runner.
//!
//! # Contents
//! - [`validate_strict_mode_early_errors`] — public entry called from
//!   `compile_program` / `compile_module_program`.
//!
//! # Invariants
//! - Strictness is tracked as a stack: source-level strict (force,
//!   module mode, or top-level `"use strict"`), function-level strict
//!   (inherited from outer or own-body directive), and class bodies
//!   (unconditionally strict per ECMA-262 §10.2.10).
//! - The walker emits owned [`SyntaxDiagnostic`] entries; no `oxc`
//!   handles cross the crate boundary.
//!
//! # See also
//! - ECMA-262 §12.9.3.1 Static Semantics: Early Errors for
//!   NumericLiteral (LegacyOctalIntegerLiteral and
//!   NonOctalDecimalIntegerLiteral are early errors in strict code):
//!   <https://tc39.es/ecma262/#sec-literals-numeric-literals-static-semantics-early-errors>
//! - ECMA-262 §10.2.10 ClassBody is always strict mode code:
//!   <https://tc39.es/ecma262/#sec-strict-mode-code>

use otter_syntax::SyntaxDiagnostic;
use oxc_ast::ast::{ArrowFunctionExpression, Class, Function, NumericLiteral, Program};
use oxc_ast_visit::{Visit, walk};
use oxc_syntax::scope::ScopeFlags;

use crate::CompileError;

/// Validate strict-mode early errors that `oxc_parser` does not raise.
///
/// Returns `Ok(())` when the program is well-formed under strict-mode
/// early-error rules, or [`CompileError::Syntax`] carrying one
/// [`SyntaxDiagnostic`] per violation (preserving order of appearance).
///
/// `force_strict` lets direct-eval callers inherit the caller's
/// strictness without rewriting the source.
pub fn validate_strict_mode_early_errors(
    program: &Program<'_>,
    force_strict: bool,
) -> Result<(), CompileError> {
    // Note: `program.source_type` is unreliable here. `otter-syntax`
    // calls `SourceType::default()` (which is `mjs()` in oxc) for all
    // script and module inputs alike; the script-vs-module routing is
    // performed separately by the host runtime. We therefore derive
    // initial strictness from the caller's `force_strict` (true for
    // module compilation entry and direct-eval inheritance) plus the
    // top-level `"use strict"` directive only.
    let source_strict = force_strict || program.has_use_strict_directive();
    let mut visitor = StrictValidator {
        strict_stack: vec![source_strict],
        diagnostics: Vec::new(),
    };
    visitor.visit_program(program);
    if visitor.diagnostics.is_empty() {
        return Ok(());
    }
    let messages = visitor
        .diagnostics
        .iter()
        .map(|d| d.message.clone())
        .collect();
    Err(CompileError::Syntax {
        messages,
        diagnostics: visitor.diagnostics,
    })
}

struct StrictValidator {
    strict_stack: Vec<bool>,
    diagnostics: Vec<SyntaxDiagnostic>,
}

impl StrictValidator {
    fn is_strict(&self) -> bool {
        self.strict_stack.last().copied().unwrap_or(false)
    }
}

impl<'a> Visit<'a> for StrictValidator {
    fn visit_function(&mut self, it: &Function<'a>, flags: ScopeFlags) {
        let body_strict = it
            .body
            .as_ref()
            .is_some_and(|b| b.has_use_strict_directive());
        let inner_strict = self.is_strict() || body_strict;
        self.strict_stack.push(inner_strict);
        walk::walk_function(self, it, flags);
        self.strict_stack.pop();
    }

    fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
        let inner_strict = self.is_strict() || it.body.has_use_strict_directive();
        self.strict_stack.push(inner_strict);
        walk::walk_arrow_function_expression(self, it);
        self.strict_stack.pop();
    }

    fn visit_class(&mut self, it: &Class<'a>) {
        // ECMA-262 §10.2.10 — class bodies are always strict mode code.
        self.strict_stack.push(true);
        walk::walk_class(self, it);
        self.strict_stack.pop();
    }

    fn visit_numeric_literal(&mut self, it: &NumericLiteral<'a>) {
        if !self.is_strict() {
            return;
        }
        let Some(raw) = it.raw else {
            return;
        };
        if is_legacy_numeric_form(raw.as_str()) {
            self.diagnostics.push(SyntaxDiagnostic {
                code: "STRICT_LEGACY_NUMERIC".to_string(),
                message: format!(
                    "SyntaxError: legacy octal or non-octal-decimal integer literal `{}` is not allowed in strict mode",
                    raw.as_str()
                ),
                range: Some((it.span.start, it.span.end)),
                help: Some(
                    "use the `0o` prefix for octal literals in strict mode code".to_string(),
                ),
            });
        }
    }
}

/// Detect `LegacyOctalIntegerLiteral` and `NonOctalDecimalIntegerLiteral`
/// raw source forms.
///
/// Both productions begin with `0` followed immediately by an ASCII
/// digit. Modern integer prefixes (`0x`, `0o`, `0b`), the `0n`
/// BigInt suffix, fractional / exponent forms (`0.5`, `0e1`), and
/// the bare `0` literal are excluded by checking that the second
/// character is in `0..=9`.
fn is_legacy_numeric_form(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'0' {
        return false;
    }
    bytes[1].is_ascii_digit()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_legacy_octal_forms() {
        assert!(is_legacy_numeric_form("00"));
        assert!(is_legacy_numeric_form("010"));
        assert!(is_legacy_numeric_form("0123"));
        // NonOctalDecimalIntegerLiteral
        assert!(is_legacy_numeric_form("08"));
        assert!(is_legacy_numeric_form("089"));
    }

    #[test]
    fn ignores_modern_numeric_forms() {
        assert!(!is_legacy_numeric_form("0"));
        assert!(!is_legacy_numeric_form("0x1F"));
        assert!(!is_legacy_numeric_form("0o17"));
        assert!(!is_legacy_numeric_form("0b101"));
        assert!(!is_legacy_numeric_form("0n"));
        assert!(!is_legacy_numeric_form("0.5"));
        assert!(!is_legacy_numeric_form("0e1"));
        assert!(!is_legacy_numeric_form("123"));
        assert!(!is_legacy_numeric_form(""));
    }
}
