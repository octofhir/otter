//! Structured syntax diagnostics for the OXC frontend boundary.
//!
//! # Contents
//! - [`SyntaxDiagnostic`] — owned parser diagnostic data.
//! - [`SyntaxError`] — parse error batch returned by this crate.
//!
//! # Invariants
//! - Diagnostics are owned and serializable; no OXC or miette handles cross the
//!   crate boundary.
//! - Ranges are byte offsets into the original source text.
//!
//! # See also
//! - [`crate::parse`]

use oxc_diagnostics::OxcDiagnostic;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Owned parser diagnostic emitted by `otter-syntax`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyntaxDiagnostic {
    /// Stable Otter diagnostic code for parse failures.
    pub code: String,
    /// Human-readable parser message.
    pub message: String,
    /// Optional byte range in the original source.
    pub range: Option<(u32, u32)>,
    /// Optional parser help text.
    pub help: Option<String>,
}

impl SyntaxDiagnostic {
    /// Build a diagnostic from an OXC parser diagnostic.
    #[must_use]
    pub fn from_oxc(diagnostic: &OxcDiagnostic) -> Self {
        let range = diagnostic.labels.as_ref().and_then(|labels| {
            labels
                .iter()
                .find(|label| label.primary())
                .or_else(|| labels.first())
                .map(|label| {
                    let start = usize_to_u32(label.offset());
                    let end = usize_to_u32(label.offset().saturating_add(label.len()));
                    (start, end)
                })
        });
        Self {
            code: "SYNTAX_ERROR".to_string(),
            message: diagnostic.to_string(),
            range,
            help: diagnostic.help.as_ref().map(ToString::to_string),
        }
    }

    /// Build an unspanned diagnostic from a plain message.
    #[must_use]
    pub fn from_message(message: impl Into<String>) -> Self {
        Self {
            code: "SYNTAX_ERROR".to_string(),
            message: message.into(),
            range: None,
            help: None,
        }
    }
}

/// Errors produced by the OXC frontend.
#[derive(Debug, Clone, Error, Serialize, Deserialize)]
#[error("syntax error: {}", .messages.join("; "))]
pub struct SyntaxError {
    /// One message per parser diagnostic.
    pub messages: Vec<String>,
    /// Structured parser diagnostics.
    pub diagnostics: Vec<SyntaxDiagnostic>,
}

impl SyntaxError {
    /// Build a syntax error from OXC parser diagnostics.
    #[must_use]
    pub fn from_oxc(errors: &[OxcDiagnostic]) -> Self {
        let diagnostics: Vec<_> = errors.iter().map(SyntaxDiagnostic::from_oxc).collect();
        let messages = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.clone())
            .collect();
        Self {
            messages,
            diagnostics,
        }
    }

    /// Build a syntax error from plain messages.
    #[must_use]
    pub fn from_messages(messages: Vec<String>) -> Self {
        let diagnostics = messages
            .iter()
            .map(|message| SyntaxDiagnostic::from_message(message.clone()))
            .collect();
        Self {
            messages,
            diagnostics,
        }
    }
}

fn usize_to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}
