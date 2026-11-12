//! Pattern validation through the matcher's one parser.
//!
//! # Contents
//! - [`validate_utf16`] checks syntax and early errors without lowering or matching.
//!
//! # Invariants
//! - Validation and runtime compilation use the same grammar and depth limit.
//! - No executable program, native tier or matcher scratch is created here.
//!
//! # See also
//! - `crate::parser` owns pattern parsing; `crate::api` adds runtime compilation.

use crate::{Flags, RegexError};

/// Validate a UTF-16 pattern without constructing a matcher.
///
/// Stateful JS flags and conflicting `u`/`v` flags are checked by the host,
/// just as for runtime compilation.
///
/// # Errors
/// Returns the same syntax, early-error and depth-limit errors as compilation.
pub fn validate_utf16(pattern: &[u16], flags: Flags) -> Result<(), RegexError> {
    crate::parser::parse(pattern, flags).map(|_| ())
}
