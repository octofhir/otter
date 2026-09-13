//! Semantic admission for shared generated call linkage.
//!
//! # Contents
//! - One CodeBlock policy for ordinary, guarded-method and constructor targets.
//!
//! # Invariants
//! - Admission describes function semantics, not generation liveness or callable
//!   identity. Native entry selection and generated guards prove those separately.
//! - Ordinary calls can consume complete actual windows; guarded methods retain
//!   their current narrower argument contract. Constructors never admit methods.
//! - Dynamic target selection and compile-time baking must use this same policy.
//!
//! # See also
//! - [`crate::jit_registry`] — current native generation and frame metadata.
//! - [`crate::interp::jit_compile`] — source-site planning and inline candidates.

use crate::{CodeBlock, jit::JitDirectCallKind};

impl CodeBlock {
    pub(crate) fn admits_generated_call(&self, kind: JitDirectCallKind) -> bool {
        if self.is_generator
            || self.is_async
            || self.is_async_generator
            || self.has_rest
            || self.contains_direct_eval
        {
            return false;
        }
        match kind {
            JitDirectCallKind::Plain => !self.is_derived_constructor,
            JitDirectCallKind::Method => !self.is_derived_constructor && !self.needs_arguments,
            JitDirectCallKind::Construct
            | JitDirectCallKind::DerivedConstruct
            | JitDirectCallKind::SuperConstruct
            | JitDirectCallKind::DerivedSuperConstruct => {
                !self.is_method && !(self.is_derived_constructor && self.makes_function)
            }
        }
    }
}
