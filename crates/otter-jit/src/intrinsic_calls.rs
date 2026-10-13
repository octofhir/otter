//! Proven ordinary calls whose effects have a native operation.
//!
//! # Contents
//! - [`IntrinsicCallForm`] selects the operation shared by both generated tiers.
//! - [`proof_form`] validates the source-owned callable proof and call form.
//! - [`IntrinsicMethodForm`] / [`method_form`] select a builtin method the
//!   optimizing tier calls through its runtime entry without a native frame.
//!
//! # Invariants
//! - Selection follows an evaluated callable, never a source identifier.
//! - The held identity cell and source realm precede every specialized effect.
//! - Only ordinary fixed calls and constructions enter this admission boundary.
//!   Other forms retain their existing call linkage and canonical operation.
//! - Arguments are evaluated before selection, including ignored extra arguments.
//!
//! # See also
//! - `otter_vm::jit::intrinsic_calls` owns the traced callable proof.
//! - [`crate::template::plan`] and [`crate::graph::builder`] consume these forms.

use otter_vm::{IntrinsicCallKind, JitIntrinsicCallProof};

/// An intrinsic operation admitted at an ordinary `Call` or `New` site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntrinsicCallForm {
    ArrayEmpty,
    ArrayLength,
    Boolean,
    NumberZero,
    ObjectEmpty,
}

/// An intrinsic operation admitted at a `receiver.method(argument)` site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntrinsicMethodForm {
    /// `%RegExp.prototype.exec%` with one argument.
    RegExpExec,
    /// `%RegExp.prototype.test%` with one argument.
    RegExpTest,
}

/// Validate one baked method proof in the owning source realm.
pub(crate) fn method_form(
    proof: JitIntrinsicCallProof,
    source_realm: u32,
    argc: usize,
) -> Option<IntrinsicMethodForm> {
    if proof.identity_cell == 0 || proof.realm_id != source_realm || argc != 1 {
        return None;
    }
    match proof.kind {
        IntrinsicCallKind::RegExpExec => Some(IntrinsicMethodForm::RegExpExec),
        IntrinsicCallKind::RegExpTest => Some(IntrinsicMethodForm::RegExpTest),
        _ => None,
    }
}

/// Select the semantic operation without broadening its runtime domain.
pub(crate) fn admitted_form(
    kind: IntrinsicCallKind,
    construct: bool,
    argc: usize,
) -> Option<IntrinsicCallForm> {
    use IntrinsicCallForm as Form;
    use IntrinsicCallKind as Kind;
    match (kind, construct, argc) {
        (Kind::Array, _, 0) => Some(Form::ArrayEmpty),
        (Kind::Array, _, 1) => Some(Form::ArrayLength),
        (Kind::Boolean, false, _) => Some(Form::Boolean),
        (Kind::Number, false, 0) => Some(Form::NumberZero),
        (Kind::Object, _, 0) => Some(Form::ObjectEmpty),
        // Method intrinsics specialize only `receiver.method(...)` sites.
        (Kind::RegExpExec | Kind::RegExpTest, _, _) => None,
        _ => None,
    }
}

/// Validate one baked proof in the owning source realm before admission.
pub(crate) fn proof_form(
    proof: JitIntrinsicCallProof,
    source_realm: u32,
    construct: bool,
    argc: usize,
) -> Option<IntrinsicCallForm> {
    if proof.identity_cell == 0 || proof.realm_id != source_realm {
        return None;
    }
    admitted_form(proof.kind, construct, argc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_forms_admit_one_argument_regexp_methods_in_their_realm() {
        let proof = |kind| JitIntrinsicCallProof {
            kind,
            identity_cell: 8,
            realm_id: 3,
        };
        assert_eq!(
            method_form(proof(IntrinsicCallKind::RegExpExec), 3, 1),
            Some(IntrinsicMethodForm::RegExpExec)
        );
        assert_eq!(
            method_form(proof(IntrinsicCallKind::RegExpTest), 3, 1),
            Some(IntrinsicMethodForm::RegExpTest)
        );
        assert_eq!(
            method_form(proof(IntrinsicCallKind::RegExpExec), 3, 0),
            None
        );
        assert_eq!(
            method_form(proof(IntrinsicCallKind::RegExpExec), 4, 1),
            None
        );
        assert_eq!(method_form(proof(IntrinsicCallKind::Array), 3, 1), None);
        for construct in [false, true] {
            assert_eq!(
                admitted_form(IntrinsicCallKind::RegExpExec, construct, 1),
                None
            );
        }
    }

    #[test]
    fn admission_preserves_constructor_and_coercion_domains() {
        use IntrinsicCallForm as Form;
        use IntrinsicCallKind as Kind;
        for construct in [false, true] {
            assert_eq!(
                admitted_form(Kind::Array, construct, 0),
                Some(Form::ArrayEmpty)
            );
            assert_eq!(
                admitted_form(Kind::Array, construct, 1),
                Some(Form::ArrayLength)
            );
            assert_eq!(admitted_form(Kind::Array, construct, 2), None);
            assert_eq!(
                admitted_form(Kind::Object, construct, 0),
                Some(Form::ObjectEmpty)
            );
            assert_eq!(admitted_form(Kind::Object, construct, 1), None);
        }
        for argc in [0, 1, 7] {
            assert_eq!(
                admitted_form(Kind::Boolean, false, argc),
                Some(Form::Boolean)
            );
            assert_eq!(admitted_form(Kind::Boolean, true, argc), None);
        }
        assert_eq!(
            admitted_form(Kind::Number, false, 0),
            Some(Form::NumberZero)
        );
        for (construct, argc) in [(false, 1), (true, 0), (true, 1)] {
            assert_eq!(admitted_form(Kind::Number, construct, argc), None);
        }
    }

    #[test]
    fn proof_requires_a_held_identity_in_the_source_realm() {
        let proof = JitIntrinsicCallProof {
            kind: IntrinsicCallKind::Array,
            identity_cell: 16,
            realm_id: 7,
        };
        assert_eq!(
            proof_form(proof, 7, true, 0),
            Some(IntrinsicCallForm::ArrayEmpty)
        );
        assert_eq!(proof_form(proof, 8, true, 0), None);
        assert_eq!(
            proof_form(
                JitIntrinsicCallProof {
                    identity_cell: 0,
                    ..proof
                },
                7,
                true,
                0
            ),
            None
        );
    }
}
