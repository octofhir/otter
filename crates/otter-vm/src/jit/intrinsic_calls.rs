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
    /// The realm's original `%RegExp.prototype.exec%`, called as a method.
    RegExpExec,
    /// The realm's original `%RegExp.prototype.test%`, called as a method.
    RegExpTest,
    /// The realm's original `%String.prototype.replace%`, called as a method.
    StringReplace,
    /// The realm's original `%String.prototype.split%`, called as a method.
    StringSplit,
    /// The realm's original `%String.prototype.match%`, called as a method.
    StringMatch,
    /// The realm's original `%String.prototype.search%`, called as a method.
    StringSearch,
}

impl IntrinsicCallKind {
    /// Number of kinds.
    pub const COUNT: usize = 10;

    /// Every kind, in feedback-payload order.
    pub const ALL: [Self; Self::COUNT] = [
        Self::Array,
        Self::Boolean,
        Self::Number,
        Self::Object,
        Self::RegExpExec,
        Self::RegExpTest,
        Self::StringReplace,
        Self::StringSplit,
        Self::StringMatch,
        Self::StringSearch,
    ];

    /// The scalar code feedback and runtime entries name this kind by.
    #[must_use]
    pub const fn feedback_payload(self) -> u32 {
        self as u32
    }

    pub(crate) const fn from_feedback_payload(payload: u32) -> Option<Self> {
        if (payload as usize) < Self::COUNT {
            Some(Self::ALL[payload as usize])
        } else {
            None
        }
    }

    /// Whether a method call (`receiver.method(...)`) is this callable's
    /// specialized form, rather than a plain call or construction.
    #[must_use]
    pub const fn is_method(self) -> bool {
        matches!(
            self,
            Self::RegExpExec
                | Self::RegExpTest
                | Self::StringReplace
                | Self::StringSplit
                | Self::StringMatch
                | Self::StringSearch
        )
    }

    /// The prototype a method kind's original callable lives on, and its
    /// property name there.
    pub(crate) const fn method_home(self) -> Option<(MethodHome, &'static str)> {
        Some(match self {
            Self::RegExpExec => (MethodHome::RegExpPrototype, "exec"),
            Self::RegExpTest => (MethodHome::RegExpPrototype, "test"),
            Self::StringReplace => (MethodHome::StringPrototype, "replace"),
            Self::StringSplit => (MethodHome::StringPrototype, "split"),
            Self::StringMatch => (MethodHome::StringPrototype, "match"),
            Self::StringSearch => (MethodHome::StringPrototype, "search"),
            Self::Array | Self::Boolean | Self::Number | Self::Object => return None,
        })
    }
}

/// The intrinsic prototype a method kind's original callable is read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MethodHome {
    RegExpPrototype,
    StringPrototype,
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
        for invalid in [IntrinsicCallKind::COUNT as u32, 256, u32::MAX] {
            assert_eq!(IntrinsicCallKind::from_feedback_payload(invalid), None);
        }
    }
}
