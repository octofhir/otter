//! Compile error types for parser and lowering failures.
//!
//! # Contents
//! - `CompileError` diagnostics
//! - syntax-error conversion
//!
//! # Invariants
//! - Diagnostics keep parser messages and structured spans intact.
//!
//! # See also
//! - `entry` for compiler entry points

use crate::{SyntaxDiagnostic, SyntaxError};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Concrete compiler errors.
#[derive(Debug, Clone, Error, Serialize, Deserialize)]
#[non_exhaustive]
pub enum CompileError {
    /// Parsing failed in `otter-syntax`.
    #[error("syntax: {}", .messages.join("; "))]
    Syntax {
        /// One message per OXC parser diagnostic.
        messages: Vec<String>,
        /// Structured parser diagnostics with byte ranges and help text.
        diagnostics: Vec<SyntaxDiagnostic>,
    },
    /// The AST node is recognized but not supported by this slice.
    #[error("unsupported {node} at offset {}-{}", .span.0, .span.1)]
    Unsupported {
        /// AST node kind name.
        node: String,
        /// Source span of the offending node.
        span: (u32, u32),
    },
    /// A TypeScript construct is intentionally rejected by the
    /// frontend policy (e.g., `enum`, runtime `namespace`,
    /// decorators).
    #[error("typescript construct {node} is not supported in foundation")]
    TypeScriptUnsupported {
        /// AST node kind name.
        node: String,
        /// Source span of the offending node.
        span: (u32, u32),
    },
    /// Compiler-owned runtime metadata exceeded its hard materialization
    /// budget. The bytecode itself may be valid, but duplicating its
    /// diagnostics payload would exceed the bounded compiler/runtime DTO.
    #[error("compiled metadata requires {requested_bytes} bytes, limit is {limit_bytes} bytes")]
    MetadataLimit {
        /// Complete checked materialization size required by the metadata.
        requested_bytes: usize,
        /// Hard metadata materialization budget.
        limit_bytes: usize,
    },
    /// A fallible metadata allocation failed below the hard budget.
    #[error("failed to reserve {requested_bytes} bytes for compiled metadata")]
    MetadataAllocation {
        /// Complete checked materialization size being reserved.
        requested_bytes: usize,
    },
}

impl From<SyntaxError> for CompileError {
    fn from(error: SyntaxError) -> Self {
        Self::Syntax {
            messages: error.messages,
            diagnostics: error.diagnostics,
        }
    }
}
