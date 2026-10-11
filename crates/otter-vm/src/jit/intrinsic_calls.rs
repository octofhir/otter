//! Owned intrinsic identity and native-call proofs consumed by both tiers.
//!
//! # Contents
//! - [`IntrinsicCallKind`] names the canonical callable in one source realm.
//! - [`JitIntrinsicCallProof`] retains its traced identity-cell authority.
//! - [`JitNativeCall`] selects a declared leaf, intrinsic proof, or native kind.
//!
//! # Invariants
//! Feedback carries only a scalar kind. A compile proof names an address-stable
//! traced cell owned by its code generation, never a moving callable handle.
//! The source realm must be active when the VM bakes the proof. Unsupported
//! intrinsic forms retain the native-kind linkage and canonical semantics.
//!
//! # See also
//! - `crate::realm_intrinsics` owns the original callable roots.
//! - `crate::call_feedback` classifies the actual resolved callable.
//! - `super::JitCompileSnapshot` owns these borrow-free compile inputs.

use super::JitStaticNativeCall;

/// Canonical callables with specialized value or allocation semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntrinsicCallKind {
    /// The realm's original Array constructor and ordinary-call entry.
    Array,
    /// The realm's original Boolean callable.
    Boolean,
    /// The realm's original Number callable.
    Number,
    /// The realm's original Object constructor and ordinary-call entry.
    Object,
}

impl IntrinsicCallKind {
    pub(crate) const ALL: [Self; 4] = [Self::Array, Self::Boolean, Self::Number, Self::Object];

    pub(crate) const fn feedback_payload(self) -> u32 {
        match self {
            Self::Array => 0,
            Self::Boolean => 1,
            Self::Number => 2,
            Self::Object => 3,
        }
    }

    pub(crate) const fn from_feedback_payload(payload: u32) -> Option<Self> {
        match payload {
            0 => Some(Self::Array),
            1 => Some(Self::Boolean),
            2 => Some(Self::Number),
            3 => Some(Self::Object),
            _ => None,
        }
    }
}

/// Exact canonical callable identity retained by one compiled generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitIntrinsicCallProof {
    /// Canonical semantics selected by the retained callable identity.
    pub kind: IntrinsicCallKind,
    /// Address of the generation-owned, collector-rewritten callable cell.
    pub identity_cell: u64,
    /// Realm of the original source instruction and canonical callable.
    pub realm_id: u32,
}

/// Selected native entry for one source call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitNativeCall {
    /// An exact audited leaf declaration.
    Leaf(JitStaticNativeCall),
    /// Canonical callable identity whose eligible forms a tier may specialize.
    Intrinsic(JitIntrinsicCallProof),
    /// Any NativeFunction, entered through the private Native kind convention.
    Native,
}

impl JitNativeCall {
    /// Exact leaf payload, when this plan permits the declared leaf ABI.
    #[must_use]
    pub const fn leaf(&self) -> Option<&JitStaticNativeCall> {
        match self {
            Self::Leaf(leaf) => Some(leaf),
            Self::Intrinsic(_) | Self::Native => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intrinsic_feedback_has_one_checked_scalar_codec() {
        for kind in IntrinsicCallKind::ALL {
            assert_eq!(
                IntrinsicCallKind::from_feedback_payload(kind.feedback_payload()),
                Some(kind)
            );
        }
        for invalid in [4, 256, u32::MAX] {
            assert_eq!(IntrinsicCallKind::from_feedback_payload(invalid), None);
        }
    }
}
