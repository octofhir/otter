//! Proven ordinary calls whose effects have a native operation.
//!
//! # Contents
//! - [`IntrinsicCallForm`] selects the operation shared by both generated tiers.
//! - [`proof_form`] validates the source-owned callable proof and call form.
//! - [`IntrinsicMethodForm`] / [`method_form`] select a builtin method the
//!   optimizing tier calls through its runtime entry without a native frame:
//!   RegExp `exec` / `test` and String `replace` / `split` / `match` /
//!   `search`.
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

/// An intrinsic operation admitted at a `receiver.method(...arguments)` site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntrinsicMethodForm {
    /// `%RegExp.prototype.exec%`.
    RegExpExec,
    /// `%RegExp.prototype.test%`.
    RegExpTest,
    /// `%String.prototype.replace%`.
    StringReplace,
    /// `%String.prototype.split%`.
    StringSplit,
    /// `%String.prototype.match%`.
    StringMatch,
    /// `%String.prototype.search%`.
    StringSearch,
}

impl IntrinsicMethodForm {
    /// The arguments the method reads; a call passing fewer reads the rest
    /// as `undefined`, which is what the method receives for them.
    pub(crate) const fn arity(self) -> usize {
        match self {
            Self::RegExpExec | Self::RegExpTest | Self::StringMatch | Self::StringSearch => 1,
            Self::StringReplace | Self::StringSplit => 2,
        }
    }

    /// The canonical callable kind this form specializes.
    pub(crate) const fn kind(self) -> IntrinsicCallKind {
        match self {
            Self::RegExpExec => IntrinsicCallKind::RegExpExec,
            Self::RegExpTest => IntrinsicCallKind::RegExpTest,
            Self::StringReplace => IntrinsicCallKind::StringReplace,
            Self::StringSplit => IntrinsicCallKind::StringSplit,
            Self::StringMatch => IntrinsicCallKind::StringMatch,
            Self::StringSearch => IntrinsicCallKind::StringSearch,
        }
    }

    /// The committed runtime entry running the method.
    pub(crate) const fn stub(self) -> otter_vm::native_abi::RuntimeStubDescriptor {
        use otter_vm::native_abi as abi;
        match self {
            Self::RegExpExec => abi::STUB_JIT_REGEXP_EXEC,
            Self::RegExpTest => abi::STUB_JIT_REGEXP_TEST,
            Self::StringReplace | Self::StringSplit | Self::StringMatch | Self::StringSearch => {
                abi::STUB_JIT_STRING_METHOD
            }
        }
    }
}

/// Validate one baked method proof in the owning source realm for a call
/// passing `argc` arguments: no more than the method reads, so none
/// evaluated beyond them is dropped from the call.
pub(crate) fn method_form(
    proof: JitIntrinsicCallProof,
    source_realm: u32,
    argc: usize,
) -> Option<IntrinsicMethodForm> {
    if proof.identity_cell == 0 || proof.realm_id != source_realm {
        return None;
    }
    let form = match proof.kind {
        IntrinsicCallKind::RegExpExec => IntrinsicMethodForm::RegExpExec,
        IntrinsicCallKind::RegExpTest => IntrinsicMethodForm::RegExpTest,
        IntrinsicCallKind::StringReplace => IntrinsicMethodForm::StringReplace,
        IntrinsicCallKind::StringSplit => IntrinsicMethodForm::StringSplit,
        IntrinsicCallKind::StringMatch => IntrinsicMethodForm::StringMatch,
        IntrinsicCallKind::StringSearch => IntrinsicMethodForm::StringSearch,
        _ => return None,
    };
    (argc <= form.arity()).then_some(form)
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
    fn method_forms_admit_calls_up_to_the_arity_in_their_realm() {
        let proof = |kind| JitIntrinsicCallProof {
            kind,
            identity_cell: 8,
            realm_id: 3,
        };
        for (kind, form) in [
            (
                IntrinsicCallKind::RegExpExec,
                IntrinsicMethodForm::RegExpExec,
            ),
            (
                IntrinsicCallKind::RegExpTest,
                IntrinsicMethodForm::RegExpTest,
            ),
            (
                IntrinsicCallKind::StringReplace,
                IntrinsicMethodForm::StringReplace,
            ),
            (
                IntrinsicCallKind::StringSplit,
                IntrinsicMethodForm::StringSplit,
            ),
            (
                IntrinsicCallKind::StringMatch,
                IntrinsicMethodForm::StringMatch,
            ),
            (
                IntrinsicCallKind::StringSearch,
                IntrinsicMethodForm::StringSearch,
            ),
        ] {
            assert_eq!(form.kind(), kind);
            for argc in 0..=form.arity() {
                assert_eq!(method_form(proof(kind), 3, argc), Some(form));
            }
            // An extra argument is evaluated by the call and stays its own.
            assert_eq!(method_form(proof(kind), 3, form.arity() + 1), None);
            assert_eq!(method_form(proof(kind), 4, 1), None);
            for construct in [false, true] {
                assert_eq!(admitted_form(kind, construct, 1), None);
            }
        }
        assert_eq!(method_form(proof(IntrinsicCallKind::Array), 3, 1), None);
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
