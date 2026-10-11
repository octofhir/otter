//! Exit destinations and collector roots shared by reusable baseline
//! operation emitters.
//!
//! # Contents
//! - [`OperationExits`] names the caller-owned recovery and completion labels.
//! - [`AllocRoots`] / [`AllocPacket`] name the record and spill window an
//!   allocating stub call inside an operation publishes.
//!
//! # Invariants
//! - Both targets and both tiers use this one compile-time carrier.
//! - Target-specific labels exist only where that operation encoder consumes
//!   them. A Template JS-call operation receives private relay labels that
//!   publish its call source before reaching the shared exit.
//! - Labels contain no runtime state or result representation. The caller owns
//!   frame recovery and the physical exit implementation.
//! - A committed throw never enters an eager exit that would replay its effect.
//! - In a baseline body every live value sits in the register window, so an
//!   allocating call names its site's window record. Inside an optimized
//!   body, values live across the operation sit in the body's tagged homes, so
//!   the call names the boundary's record and publishes those homes — the same
//!   record the frame is stamped with — or a collection would miss them.
//!
//! # See also
//! - [`crate::frame`] for shared activation exit geometry.

use dynasmrt::DynamicLabel;
use otter_vm::native_abi::SafepointId;

/// Where allocating stub calls inside one operation publish their roots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AllocRoots {
    /// A baseline body: the call names the window record baked for its site.
    Window,
    /// A baseline operation inside an optimized body: every call names the
    /// boundary's record and publishes the body's tagged homes.
    Optimized {
        /// The boundary's record, rooting the homes live across it.
        safepoint: SafepointId,
        /// Tagged homes in the body's machine-root window.
        tagged_slots: u16,
    },
}

impl AllocRoots {
    /// The packet one allocating call publishes, given the window record
    /// baked for its site. A site without a record publishes none: its entry
    /// cannot collect.
    #[must_use]
    pub(crate) fn packet(self, site: SafepointId) -> AllocPacket {
        if site == otter_vm::native_abi::NO_SAFEPOINT {
            return AllocPacket {
                safepoint: site,
                tagged_slots: 0,
            };
        }
        match self {
            Self::Window => AllocPacket {
                safepoint: site,
                tagged_slots: 0,
            },
            Self::Optimized {
                safepoint,
                tagged_slots,
            } => AllocPacket {
                safepoint,
                tagged_slots,
            },
        }
    }
}

/// The record and spill window one allocating stub call publishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AllocPacket {
    /// The record the stub resolves its roots through.
    pub(crate) safepoint: SafepointId,
    /// Tagged homes the packet exposes from the frame's machine-root window;
    /// zero publishes no spill window.
    pub(crate) tagged_slots: u16,
}

/// Exit labels one baseline operation may branch to in either generated tier.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OperationExits {
    pub(crate) type_mismatch_exit: DynamicLabel,
    pub(crate) identity_guard_exit: DynamicLabel,
    pub(crate) allocation_miss_exit: DynamicLabel,
    pub(crate) unsupported_exit: DynamicLabel,
    pub(crate) runtime_transition_exit: DynamicLabel,
    pub(crate) backedge_relink_exit: DynamicLabel,
    /// AArch64 runtime-transition helpers' shared side exit.
    #[cfg(target_arch = "aarch64")]
    pub(crate) bail: DynamicLabel,
    pub(crate) returned: DynamicLabel,
    pub(crate) committed_throw: DynamicLabel,
    pub(crate) threw: DynamicLabel,
    #[cfg(target_arch = "aarch64")]
    pub(crate) propagate_throw: DynamicLabel,
    pub(crate) fatal: DynamicLabel,
}
