//! Execution layer: the [`Engine`] abstraction and per-execution config.
//!
//! # Contents
//! - [`ExecConfig`] — the ReDoS step budget for one match attempt.
//! - [`Engine`] — the executor trait; the matcher backends implement it.
//! - [`select_engine`] — the pattern-to-engine routing rule (§2.4 of the
//!   research note).
//!
//! # Invariants
//! - Every [`Engine::run`] honours [`ExecConfig::step_limit`]: exceeding it
//!   returns [`crate::ExecError::StepLimitExceeded`] rather than looping.
//! - All positions an engine reports are UTF-16 code-unit offsets.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-pattern-matching> (§22.2.2)

pub(crate) mod backtrack;
pub(crate) mod pikevm;

use crate::api::Match;
use crate::cursor::Input;
use crate::error::ExecError;
use crate::program::Program;

/// Per-execution tuning for a single match attempt.
///
/// The default is unbounded (no ReDoS guard); hosts running untrusted patterns
/// should set a [`step_limit`](ExecConfig::step_limit).
#[derive(Debug, Clone, Copy, Default)]
pub struct ExecConfig {
    /// Maximum number of matching "steps" (instruction dispatches plus
    /// backtrack-stack operations) one match attempt may perform. `None` means
    /// unbounded. A budget around `10_000_000` cuts pathological backtracking
    /// within milliseconds while leaving realistic patterns untouched.
    pub step_limit: Option<u64>,
}

/// A matcher backend.
///
/// Backends differ in their time/space guarantees and feature coverage (the
/// bounded backtracker handles every ECMAScript feature; the PikeVM trades
/// backreferences for a true linear-time guarantee). [`select_engine`] routes a
/// compiled [`Program`] to the appropriate backend.
pub(crate) trait Engine {
    /// Attempt to match `program` against `input`, beginning the search at
    /// code-unit offset `start`. Returns the leftmost match (honouring
    /// quantifier greediness), `Ok(None)` for no match, or
    /// [`ExecError`] if a [`ExecConfig`] constraint was hit.
    fn run(
        &self,
        program: &Program,
        input: &Input<'_>,
        start: usize,
        config: ExecConfig,
    ) -> Result<Option<Match>, ExecError>;
}

/// Which backend kind a [`Program`] routes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EngineKind {
    /// Bounded backtracking — full feature coverage; ReDoS-guarded by budget.
    Backtrack,
    /// Linear-time PikeVM — backref-free, lookaround-limited (later phase).
    PikeVm,
}

/// The engine-selection rule (research note §2.4).
///
/// Milestone 2 always selects the bounded backtracker. Once the PikeVM lands,
/// backref-free / capturing-lookbehind-free programs route to it for a true
/// linear-time guarantee.
#[must_use]
pub(crate) fn select_engine(program: &Program) -> EngineKind {
    let _ = program;
    EngineKind::Backtrack
}
