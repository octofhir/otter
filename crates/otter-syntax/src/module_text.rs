//! Checked UTF-8 text at the host module-resolution boundary.
//!
//! # Contents
//! - Literal module specifiers and import attributes shared by loader and compiler.
//!
//! # Invariants
//! - Runtime string literals retain UTF-16; only host module requests require UTF-8.
//! - Only the `type` key is supported; reject unknown static attributes before
//!   compiler/loader erase the AST into a request containing just `attr_type`.
//! - Unpaired surrogates are rejected, never replaced or used as diagnostic spellings
//!   in module identity keys.
//!
//! # See also
//! - `otter-compiler::ImportRequest` owns the resulting host request.

use oxc_ast::ast::{StringLiteral, WithClause};
use oxc_span::{GetSpan, Span};

use crate::{SyntaxDiagnostic, SyntaxError};

/// Read a module-boundary literal as UTF-8 without replacing lone surrogates.
///
/// # Errors
/// Returns a spanned diagnostic when the host cannot represent the string.
pub fn module_literal_text<'a>(literal: &StringLiteral<'a>) -> Result<&'a str, SyntaxError> {
    literal.value.as_str().ok_or_else(|| {
        module_text_error(
            "module request text contains an unpaired surrogate",
            literal.span,
        )
    })
}

/// Read the `type` attribute shared by dependency scanning and lowering.
///
/// # Errors
/// Returns a spanned diagnostic for an unsupported key or an unrepresentable
/// `type` value. Check every key before extracting `type`, including keys after
/// it: unsupported attributes must not disappear from the module identity.
/// Static request rejection is a SyntaxError; ImportCall options use TypeError.
pub fn module_attribute_type(
    clause: Option<&WithClause<'_>>,
) -> Result<Option<String>, SyntaxError> {
    let Some(clause) = clause else {
        return Ok(None);
    };
    for entry in &clause.with_entries {
        if entry.key.as_js_str() != "type" {
            return Err(module_text_error(
                "unsupported import attribute; only \"type\" is supported",
                entry.key.span(),
            ));
        }
    }
    clause
        .with_entries
        .first()
        .map(|entry| module_literal_text(&entry.value).map(str::to_owned))
        .transpose()
}

fn module_text_error(message: &str, span: Span) -> SyntaxError {
    SyntaxError {
        messages: vec![message.to_owned()],
        diagnostics: vec![SyntaxDiagnostic {
            range: Some((span.start, span.end)),
            ..SyntaxDiagnostic::from_message(message)
        }],
    }
}
