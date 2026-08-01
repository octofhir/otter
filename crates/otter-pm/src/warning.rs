//! Non-fatal findings raised during an install.
//!
//! Some security gates have two settings: refuse, or proceed and say so. The
//! "say so" half needs a carrier, because a message printed from deep inside
//! resolution would be lost in JSON output and unordered in text output. A
//! warning is therefore collected, returned with the install report, and
//! rendered by the caller.
//!
//! # Contents
//! - [`InstallWarning`] — one non-fatal finding, with a stable code.
//!
//! # Invariants
//! - A warning never changes what an install does; the gate that produced it
//!   has already decided to proceed.
//! - Codes are stable strings so output can be matched by scripts.
//!
//! # See also
//! - [`crate::InstallReport`] carries the collected warnings.

use serde::{Deserialize, Serialize};

/// One non-fatal finding raised during an install.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallWarning {
    /// Stable warning code.
    pub code: &'static str,
    /// Human-readable description.
    pub message: String,
}

impl InstallWarning {
    /// Build a warning.
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
