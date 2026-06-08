//! Error types for pattern compilation and execution.
//!
//! # Contents
//! - [`RegexError`] — the pattern could not be compiled (syntax / early error /
//!   recursion-limit). The host maps this to a JS `SyntaxError`.
//! - [`ExecError`] — a single match attempt could not complete under an
//!   [`crate::ExecConfig`] constraint (the ReDoS step budget).
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-patterns-static-semantics-early-errors>

use core::fmt;

/// A pattern failed to compile.
///
/// Covers grammar syntax errors, ECMAScript early errors (§22.2.1), and the
/// parser recursion-depth limit (so a pathological nested pattern raises this
/// instead of overflowing the stack).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RegexError {
    /// A syntax or early error in the pattern, with a human-readable reason and
    /// the code-unit offset where it was detected.
    Syntax {
        /// Human-readable diagnostic.
        message: String,
        /// Code-unit offset into the pattern, or `usize::MAX` if unpositioned.
        offset: usize,
    },
    /// The parser's recursion-depth limit was exceeded (deeply nested groups or
    /// character classes).
    TooDeep {
        /// The configured maximum nesting depth.
        limit: usize,
    },
}

impl fmt::Display for RegexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegexError::Syntax { message, offset } => {
                if *offset == usize::MAX {
                    write!(f, "invalid regular expression: {message}")
                } else {
                    write!(
                        f,
                        "invalid regular expression at offset {offset}: {message}"
                    )
                }
            }
            RegexError::TooDeep { limit } => {
                write!(f, "regular expression nesting exceeds limit {limit}")
            }
        }
    }
}

impl std::error::Error for RegexError {}

/// A match attempt could not complete under an [`crate::ExecConfig`] constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExecError {
    /// The [`crate::ExecConfig`] step budget was exhausted before the engine
    /// could decide whether a match exists. The host treats the input+pattern
    /// pair as untrusted: per Otter's contract, it surfaces no match and moves
    /// on rather than stalling.
    StepLimitExceeded,
}

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecError::StepLimitExceeded => {
                f.write_str("regex step limit exceeded (possible ReDoS input)")
            }
        }
    }
}

impl std::error::Error for ExecError {}
