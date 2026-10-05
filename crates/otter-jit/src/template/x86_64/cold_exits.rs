//! Compile-time reachability of whole-function Template cold exits.
//!
//! # Contents
//! - The exit bits consumed by the existing operation exit-label carrier.
//! - Conservative operation classification before whole-function emission.
//!
//! # Invariants
//! - Only operations whose encoder names a proven subset reduce the mask.
//!   Every other operation retains all exits, including committed exceptions.
//! - The function compiler retains all exits when it emitted JS-call
//!   call-source relays: each relay names every destination, even unused ones.
//! - Return reaches its own source completion, then the shared pair epilogue.
//!   Constructor completion, terminal-pair errors and entry admission use that
//!   frame owner; they do not branch back to operation exit labels.
//! - This mask is compiler state only, never an installed ABI or root carrier.
//!
//! # See also
//! - `super::operation::emit_operation` owns each actual outgoing branch.
//! - `crate::x86_64::frame` retains every activation completion path.

use super::TemplateOp;

pub(super) const TYPE_MISMATCH: u16 = 1 << 0;
pub(super) const UNSUPPORTED: u16 = 1 << 1;
pub(super) const RUNTIME_TRANSITION: u16 = 1 << 2;
pub(super) const ALLOCATION_MISS: u16 = 1 << 3;
pub(super) const BACKEDGE_RELINK: u16 = 1 << 4;
pub(super) const RETURNED: u16 = 1 << 5;
pub(super) const COMMITTED_THROW: u16 = 1 << 6;
pub(super) const THREW: u16 = 1 << 7;
pub(super) const FATAL: u16 = 1 << 8;
pub(super) const ALL: u16 = (1 << 9) - 1;
const POLL: u16 = BACKEDGE_RELINK | THREW | FATAL;

/// Every external exit label referenced by this operation's actual encoder.
pub(super) fn required(op: TemplateOp) -> u16 {
    match op {
        TemplateOp::LoadImmediate { .. }
        | TemplateOp::Move { .. }
        | TemplateOp::LoadSelfClosure { .. }
        | TemplateOp::LoadClosureContext { .. }
        | TemplateOp::LoadContextSlot { .. }
        | TemplateOp::StoreContextSlot { .. }
        | TemplateOp::FusedNumericChain { .. } => 0,
        TemplateOp::Jump { back_edge, .. } | TemplateOp::BranchNullish { back_edge, .. } => {
            if back_edge {
                POLL
            } else {
                0
            }
        }
        TemplateOp::Branch { back_edge, .. } => TYPE_MISMATCH | if back_edge { POLL } else { 0 },
        TemplateOp::Return { .. } | TemplateOp::ReturnUndefined => RETURNED,
        TemplateOp::ReturnDerived { .. } => RUNTIME_TRANSITION | RETURNED,
        _ => ALL,
    }
}

#[cfg(test)]
#[path = "cold_exits/tests.rs"]
mod tests;
