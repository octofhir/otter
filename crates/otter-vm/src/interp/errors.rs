//! `VmError` raising with attached `ErrorDetail`.
//!
//! # Contents
//! `raise` plus the `err_*` constructors (`err_type`, `err_range`, …)
//! and error-detail take/render accessors.
//!
//! # Invariants
//! One error detail is in flight per isolate at a time; the slot is
//! overwritten by the next raise and consumed at surfacing boundaries.
#![allow(unused_imports)]
use crate::*;

impl Interpreter {
    /// Stash `detail` for the in-flight error and return the matching `Copy`
    /// [`VmError`]. One error is in flight per isolate at a time, so the slot is
    /// overwritten by the next raise and read at the surfacing boundary.
    #[inline]
    pub(crate) fn raise(&self, detail: run_control::ErrorDetail, err: VmError) -> VmError {
        *self.pending_error_detail.borrow_mut() = Some(detail);
        err
    }

    /// Publish the exact admitted-resource refusal before its fatal scalar.
    pub(crate) fn err_resource(&self, error: otter_resource::ResourceError) -> VmError {
        self.raise(
            run_control::ErrorDetail::Resource(error),
            VmError::ResourceLimit,
        )
    }

    /// Owned UTF-8 source admission has only resource refusal or a source
    /// length that cannot fit the supported ledger geometry. Actual resource
    /// cause is published atomically; malformed unsupported geometry is a
    /// structural boundary, never a fabricated resource/OOM/JavaScript error.
    pub(crate) fn err_owned_source(&self, error: otter_resource::SharedSourceError) -> VmError {
        match error {
            otter_resource::SharedSourceError::Resource(error) => self.err_resource(error),
            _ => VmError::InvalidOperand,
        }
    }

    /// Preserve ledger refusals from the single code-space publication owner.
    pub(crate) fn err_link(&self, error: crate::BytecodeLinkError) -> VmError {
        match error {
            crate::BytecodeLinkError::RetainedBytes(error) => self.err_resource(error),
            _ => VmError::InvalidOperand,
        }
    }

    /// Raise a `TypeError` carrying `message`.
    pub(crate) fn err_type(&self, message: Box<str>) -> VmError {
        self.raise(
            run_control::ErrorDetail::Message(message),
            VmError::TypeError,
        )
    }

    /// Raise a `RangeError` carrying `message`.
    pub(crate) fn err_range(&self, message: Box<str>) -> VmError {
        self.raise(
            run_control::ErrorDetail::Message(message),
            VmError::RangeError,
        )
    }

    /// Raise a `SyntaxError` carrying `message`.
    pub(crate) fn err_syntax(&self, message: Box<str>) -> VmError {
        self.raise(
            run_control::ErrorDetail::Message(message),
            VmError::SyntaxError,
        )
    }

    /// Raise a budget-exceeded error carrying `message`.
    pub(crate) fn err_budget(&self, message: Box<str>) -> VmError {
        self.raise(
            run_control::ErrorDetail::Message(message),
            VmError::BudgetExceeded,
        )
    }

    /// Raise a derived-`this`-uninitialized `ReferenceError` carrying `message`.
    pub(crate) fn err_this_uninit(&self, message: Box<str>) -> VmError {
        self.raise(
            run_control::ErrorDetail::Message(message),
            VmError::ThisUninitialized,
        )
    }

    /// Raise an invalid-regexp error carrying the backend `message`.
    pub(crate) fn err_invalid_regexp(&self, message: Box<str>) -> VmError {
        self.raise(
            run_control::ErrorDetail::Message(message),
            VmError::InvalidRegExp,
        )
    }

    /// Raise an unresolved-identifier `ReferenceError` carrying `name`.
    pub(crate) fn err_undefined_ident(&self, name: Box<str>) -> VmError {
        self.raise(
            run_control::ErrorDetail::Name(name),
            VmError::UndefinedIdentifier,
        )
    }

    /// Raise an unknown-intrinsic `TypeError` carrying the method `name`.
    pub(crate) fn err_unknown_intrinsic(&self, name: Box<str>) -> VmError {
        self.raise(
            run_control::ErrorDetail::Name(name),
            VmError::UnknownIntrinsic,
        )
    }

    /// Raise an uncaught-exception error carrying the thrown value's `display`.
    pub(crate) fn err_uncaught(&self, display: Box<str>) -> VmError {
        self.raise(
            run_control::ErrorDetail::Uncaught(display),
            VmError::Uncaught,
        )
    }

    /// Raise a `TypeError` with operation context (`<op>: cannot operate on
    /// a value of type <kind>`).
    pub(crate) fn err_type_mismatch_at(
        &self,
        op: impl Into<String>,
        kind: impl Into<String>,
    ) -> VmError {
        self.raise(
            run_control::ErrorDetail::Mismatch(run_control::VmTypeMismatchAt {
                op: op.into(),
                kind: kind.into(),
            }),
            VmError::TypeMismatchAt,
        )
    }

    /// Raise a Node-style coded error.
    pub(crate) fn err_coded(
        &self,
        kind: crate::error_classes::ErrorKind,
        code: &'static str,
        message: impl Into<String>,
    ) -> VmError {
        self.raise(
            run_control::ErrorDetail::Coded(run_control::VmCodedError {
                kind,
                code,
                message: message.into(),
            }),
            VmError::Coded,
        )
    }

    /// Raise a failed system call, carrying the properties Node stamps on it.
    pub(crate) fn err_syscall(&self, payload: run_control::VmSyscallError) -> VmError {
        self.raise(run_control::ErrorDetail::Syscall(payload), VmError::Coded)
    }

    /// Clone the in-flight error's dynamic detail, set by the `err_*` helpers.
    /// Read at the surfacing boundary paired with the `Copy` [`VmError`].
    pub fn error_detail(&self) -> Option<run_control::ErrorDetail> {
        self.pending_error_detail.borrow().clone()
    }

    /// Take the in-flight error's dynamic detail.
    pub fn take_error_detail(&self) -> Option<run_control::ErrorDetail> {
        self.pending_error_detail.borrow_mut().take()
    }

    /// Render the full, dynamic user-facing message for `err`, pairing the
    /// `Copy` discriminant with the in-flight [`run_control::ErrorDetail`].
    /// `VmError`'s own `Display` is intentionally lossy (no isolate access), so
    /// any site that needs the dynamic message must route through here.
    pub(crate) fn render_vm_error(&self, err: &VmError) -> String {
        err.render_with_detail(self.pending_error_detail.borrow().as_ref())
    }
}
