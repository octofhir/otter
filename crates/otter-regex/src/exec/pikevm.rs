//! Linear-time PikeVM backend (deferred to a later phase).
//!
//! A Thompson-NFA simulation carrying capture threads, giving linear-time
//! matching **with** submatch tracking and immunity to catastrophic
//! backtracking. It cannot model backreferences, so [`super::select_engine`]
//! only routes backref-free (and, for now, lookbehind-free) programs here.
//!
//! # Contents
//! - [`PikeVm`] — the executor stub; implements [`super::Engine`].
//!
//! # Invariants
//! - When present, this backend runs in time linear in `input.len()` regardless
//!   of pattern shape — no step budget is required for the patterns routed here.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-pattern-matching>

use super::{Engine, ExecConfig};
use crate::api::Match;
use crate::cursor::Input;
use crate::error::ExecError;
use crate::program::Program;

/// The linear-time PikeVM matcher (not yet implemented).
#[derive(Debug, Default)]
pub(crate) struct PikeVm;

impl Engine for PikeVm {
    fn run(
        &self,
        program: &Program,
        input: &Input<'_>,
        start: usize,
        config: ExecConfig,
    ) -> Result<Option<Match>, ExecError> {
        let _ = (program, input, start, config);
        todo!("Milestone 3+: linear-time PikeVM backend")
    }
}
