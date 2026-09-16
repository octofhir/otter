//! Execution layer: the bounded-backtracking matcher and its per-search config.
//!
//! # Contents
//! - [`ExecConfig`] — the ReDoS step budget for one search.
//! - [`backtrack`] — the matcher backend; every ECMAScript feature is matched
//!   here, with a step budget bounding catastrophic backtracking.
//!
//! # Invariants
//! - The matcher honours [`ExecConfig::step_limit`]: exceeding it returns
//!   [`crate::ExecError::StepLimitExceeded`] rather than looping.
//! - All positions the matcher reports are UTF-16 code-unit offsets.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-pattern-matching> (§22.2.2)

pub(crate) mod backtrack;

/// Default maximum number of explored backtrack points per search.
///
/// The checked-in adversarial corpus reaches this ceiling while the ordinary
/// matching suite stays below it. Hosts may lower the bound, but no public
/// execution path is unbounded.
pub const DEFAULT_STEP_LIMIT: u64 = 1_000_000;

/// Per-execution tuning for one search, shared across all candidate starts.
#[derive(Debug, Clone, Copy)]
pub struct ExecConfig {
    /// Maximum number of backtrack points the complete search may explore.
    pub step_limit: u64,
}

impl Default for ExecConfig {
    fn default() -> Self {
        Self {
            step_limit: DEFAULT_STEP_LIMIT,
        }
    }
}
