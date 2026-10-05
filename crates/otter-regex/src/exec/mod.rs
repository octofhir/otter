//! Execution layer: the backtracking matcher and its per-search config.
//!
//! # Contents
//! - [`ExecConfig`] — the host's step budget for one search.
//! - [`backtrack`] — the matcher backend; every ECMAScript feature is matched
//!   here.
//!
//! # Invariants
//! - A search explores until it decides whether a match exists, as ECMAScript
//!   requires; only a host-set [`ExecConfig::step_limit`] ends it early, with
//!   [`crate::ExecError::StepLimitExceeded`], never with a false no-match.
//! - All positions the matcher reports are UTF-16 code-unit offsets.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-pattern-matching> (§22.2.2)

pub(crate) mod backtrack;

/// Per-execution tuning for one search, shared across all candidate starts.
#[derive(Debug, Clone, Copy)]
pub struct ExecConfig {
    /// Maximum number of backtrack points the complete search may explore;
    /// unbounded by default.
    pub step_limit: u64,
}

impl Default for ExecConfig {
    fn default() -> Self {
        Self {
            step_limit: u64::MAX,
        }
    }
}
