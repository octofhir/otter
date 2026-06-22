//! Execution layer: the bounded-backtracking matcher and its per-execution
//! config.
//!
//! # Contents
//! - [`ExecConfig`] — the ReDoS step budget for one match attempt.
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

/// Per-execution tuning for a single match attempt.
///
/// The default is unbounded (no ReDoS guard); hosts running untrusted patterns
/// should set a [`step_limit`](ExecConfig::step_limit).
#[derive(Debug, Clone, Copy, Default)]
pub struct ExecConfig {
    /// Maximum number of backtrack points one match attempt may explore. `None`
    /// means unbounded. A budget around `10_000_000` cuts pathological
    /// backtracking within milliseconds while leaving realistic patterns
    /// untouched.
    pub step_limit: Option<u64>,
}
