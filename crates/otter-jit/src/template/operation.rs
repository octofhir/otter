//! Exit destinations shared by reusable baseline operation emitters.
//!
//! # Contents
//! - [`OperationExits`] names the caller-owned recovery and completion labels.
//!
//! # Invariants
//! - Both targets and both tiers use this one compile-time carrier.
//! - Target-specific labels exist only where that operation encoder consumes
//!   them. A Template JS-call operation receives private relay labels that
//!   publish its call source before reaching the shared exit.
//! - Labels contain no runtime state or result representation. The caller owns
//!   frame recovery and the physical exit implementation.
//! - A committed throw never enters an eager exit that would replay its effect.
//!
//! # See also
//! - [`crate::frame`] for shared activation exit geometry.

use dynasmrt::DynamicLabel;

/// Exit labels one baseline operation may branch to in either generated tier.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OperationExits {
    pub(crate) type_mismatch_exit: DynamicLabel,
    #[cfg(target_arch = "aarch64")]
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
