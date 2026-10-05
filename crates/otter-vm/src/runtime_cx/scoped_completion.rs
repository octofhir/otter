//! Rooted async-context and pending-throw handoff at native completion boundaries.
//!
//! # Contents
//! - Scoped async-context reads, installation and synchronous restoration.
//! - Pending exception transfer directly between the current owner and handles.
//! - Persistent retention through the existing isolate root table.
//! - Fresh native error extents shared with HostCompletionJob admission.
//!
//! # Invariants
//! Every moving value enters or leaves the existing handle arena without an
//! intervening collecting operation. Async restoration rereads its saved
//! handle after every ordinary callback result, including typed fatal errors.
//! The callback receives the canonical NativeCtx while all outer handles stay
//! traced. NativeScope::finish still consumes the scope; no raw value or private
//! context accessor is exposed. This module owns no queue, exception carrier,
//! root registry or additional completion state.
//!
//! # See also
//! - super::NativeScope owns the handle extent and scoped high-level calls.
//! - crate::host_completion owns the shared fresh completion reset.

use super::{Local, NativeCtx, NativeScope};

impl NativeCtx<'_> {
    /// Begin a fresh synchronous native error extent. Retire prior throw,
    /// frames, detail and rejection-report origin before executing the new
    /// operation. This does not run JavaScript or cancel any queued work.
    pub fn clear_pending_error(&mut self) {
        crate::host_completion::clear_pending_error(self.cx.interp);
    }
}

impl<'scope, 'rt> NativeScope<'scope, 'rt> {
    /// Root the current async-context value in this scope.
    #[must_use]
    pub fn async_context(&mut self) -> Local<'scope> {
        let value = self.ctx.async_context();
        self.value(value)
    }

    /// Install the current value of a rooted async context. This method does
    /// not collect; the interpreter's existing async-context slot owns it next.
    pub fn set_async_context(&mut self, value: Local<'_>) {
        let value = self.raw(value);
        self.ctx.set_async_context(value);
    }

    /// Invoke a synchronous native operation in a rooted async extent, then
    /// restore the collector-updated ambient value before returning its result.
    ///
    /// All outer Local operands remain traced while the callback uses the
    /// canonical NativeCtx or opens child scopes. Both success and typed error
    /// results restore the ambient value. As with other native callbacks, the
    /// operation must not unwind through an external native ABI.
    pub fn with_async_context<R>(
        &mut self,
        value: Local<'_>,
        body: impl FnOnce(&mut NativeCtx<'rt>) -> R,
    ) -> R {
        let ambient = self.async_context();
        self.set_async_context(value);
        let result = body(self.ctx);
        self.set_async_context(ambient);
        result
    }

    /// Remove the isolate's pending throw directly into a traced handle. An
    /// absent pending value remains None; a thrown undefined remains Some.
    #[must_use]
    pub fn take_pending_uncaught_throw(&mut self) -> Option<Local<'scope>> {
        let value = self.ctx.cx.interp.take_pending_uncaught_throw()?;
        Some(self.value(value))
    }

    /// Restore the current value of a scoped exception to its existing pending
    /// owner. No allocation, conversion or exception-object synthesis occurs.
    pub fn set_pending_uncaught_throw(&mut self, value: Local<'_>) {
        let value = self.raw(value);
        self.ctx.cx.interp.set_pending_uncaught_throw(value);
    }

    /// Consume the scalar origin tag for the pending rejection report.
    #[must_use]
    pub fn take_uncaught_from_promise_rejection(&mut self) -> bool {
        self.ctx.cx.interp.take_uncaught_from_promise_rejection()
    }

    /// Retain a scoped value in the existing persistent-root table. The host
    /// must remove the returned id through its current cleanup owner.
    #[must_use]
    pub fn persistent_root_insert(&mut self, value: Local<'_>) -> crate::PersistentRootId {
        let value = self.raw(value);
        self.ctx.persistent_root_insert(value)
    }
}

#[cfg(test)]
mod tests;
