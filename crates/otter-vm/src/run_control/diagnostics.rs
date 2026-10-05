//! Typed diagnostic rendering for the VM's one dynamic error-detail slot.
//!
//! # Contents
//! - One renderer shared by in-flight VM errors and owned RunError snapshots.
//! - Regressions for stale handled detail and exact structured OOM diagnostics.
//!
//! # Invariants
//! - Only the corresponding error family can read a dynamic detail payload.
//! - Structural errors, termination and allocation refusal render their own
//!   discriminant/scalars, even when an earlier error left detail in the slot.
//! - Rendering does not mutate exception roots or create JavaScript values.
//!
//! # See also
//! - [`super::RunError`] owns host-visible error snapshots.
//! - `crate::error_ops::throwable` owns fallible exception materialization.

use super::{ErrorDetail, VmError};

impl VmError {
    /// Render current detail only when its family belongs to this failure.
    pub(crate) fn render_with_detail(&self, detail: Option<&ErrorDetail>) -> String {
        let err = self;
        match err {
            VmError::TypeError
            | VmError::RangeError
            | VmError::SyntaxError
            | VmError::URIError
            | VmError::BudgetExceeded
            | VmError::ThisUninitialized
            | VmError::InvalidRegExp => match detail {
                Some(ErrorDetail::Message(m)) => m.to_string(),
                _ => err.to_string(),
            },
            VmError::ResourceLimit => match detail {
                Some(ErrorDetail::Resource(error)) => error.to_string(),
                _ => err.to_string(),
            },
            VmError::UndefinedIdentifier => match detail {
                Some(ErrorDetail::Name(n)) => format!("{n} is not defined"),
                _ => err.to_string(),
            },
            VmError::UnknownIntrinsic => match detail {
                Some(ErrorDetail::Name(n)) => format!("unknown intrinsic method `{n}`"),
                _ => err.to_string(),
            },
            VmError::Uncaught => match detail {
                Some(ErrorDetail::Uncaught(v)) => format!("uncaught exception: {v}"),
                _ => err.to_string(),
            },
            VmError::TypeMismatchAt => match detail {
                Some(ErrorDetail::Mismatch(p)) => {
                    format!("{}: cannot operate on a value of type {}", p.op, p.kind)
                }
                _ => err.to_string(),
            },
            VmError::JsonError => match detail {
                Some(ErrorDetail::Json(p)) => p.message.clone(),
                _ => err.to_string(),
            },
            VmError::Coded => match detail {
                Some(ErrorDetail::Coded(p)) => p.message.clone(),
                Some(ErrorDetail::Syscall(p)) => p.message.clone(),
                _ => err.to_string(),
            },
            other => other.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_control::{
        RunError, VmCodedError, VmJsonError, VmSyscallError, VmTypeMismatchAt,
    };

    fn stale_details() -> Vec<ErrorDetail> {
        vec![
            ErrorDetail::Message("previous syntax payload".into()),
            ErrorDetail::Name("previousIdentifier".into()),
            ErrorDetail::Uncaught("previous rejection object".into()),
            ErrorDetail::Mismatch(VmTypeMismatchAt {
                op: "previous coercion".into(),
                kind: "Symbol".into(),
            }),
            ErrorDetail::Json(VmJsonError {
                code: "JSON_PARSE",
                message: "previous JSON syntax".into(),
            }),
            ErrorDetail::Coded(VmCodedError {
                kind: crate::ErrorKind::SyntaxError,
                code: "ERR_PREVIOUS",
                message: "previous coded error".into(),
            }),
            ErrorDetail::Syscall(VmSyscallError {
                code: "ENOENT",
                message: "previous syscall".into(),
                syscall: "open",
                path: None,
                dest: None,
                errno: -2,
            }),
        ]
    }

    #[test]
    fn stale_handled_detail_cannot_replace_structural_control_or_oom_diagnostics() {
        for error in [
            VmError::MissingReturn,
            VmError::InvalidOperand,
            VmError::Interrupted,
            VmError::Exit { code: 27 },
            VmError::OutOfMemory {
                requested_bytes: 131072,
                heap_limit_bytes: 32768,
            },
            VmError::StackOverflow { limit: 128 },
            VmError::TypeMismatch,
            VmError::NotCallable,
        ] {
            for detail in stale_details() {
                let expected = error.to_string();
                assert_eq!(error.render_with_detail(Some(&detail)), expected);
                assert_eq!(
                    RunError {
                        error,
                        detail: Some(detail),
                        frames: Vec::new()
                    }
                    .message(),
                    expected
                );
            }
        }
    }

    #[test]
    fn only_a_matching_dynamic_family_survives_the_owned_run_error_boundary() {
        let pairs = [
            (
                VmError::SyntaxError,
                ErrorDetail::Message("exact parser payload".into()),
                "exact parser payload",
            ),
            (
                VmError::UndefinedIdentifier,
                ErrorDetail::Name("missingName".into()),
                "missingName is not defined",
            ),
            (
                VmError::Uncaught,
                ErrorDetail::Uncaught("exact thrown text".into()),
                "uncaught exception: exact thrown text",
            ),
            (
                VmError::JsonError,
                ErrorDetail::Json(VmJsonError {
                    code: "JSON_PARSE",
                    message: "exact JSON offset".into(),
                }),
                "exact JSON offset",
            ),
            (
                VmError::Coded,
                ErrorDetail::Coded(VmCodedError {
                    kind: crate::ErrorKind::RangeError,
                    code: "ERR_RANGE",
                    message: "exact coded range".into(),
                }),
                "exact coded range",
            ),
        ];
        for (error, detail, expected) in pairs {
            assert_eq!(error.render_with_detail(Some(&detail)), expected);
            assert_eq!(
                RunError {
                    error,
                    detail: Some(detail),
                    frames: Vec::new()
                }
                .message(),
                expected
            );
        }
        let wrong_detail = ErrorDetail::Uncaught("older caught rejection".into());
        assert_eq!(
            RunError {
                error: VmError::SyntaxError,
                detail: Some(wrong_detail),
                frames: Vec::new()
            }
            .message(),
            "SyntaxError"
        );
    }
}
