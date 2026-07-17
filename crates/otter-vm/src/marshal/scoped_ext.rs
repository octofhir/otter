//! Scope-arena builders backing the marshalling layer.
//!
//! [`crate::Interpreter`] additions that mint the JS values
//! [`super::IntoJs`] implementations need — binary buffers, typed-array
//! views, settled promises — plus the iterable drain that
//! [`super::Sequence`] extraction rides. Each builder follows the
//! handle-scope rooting contract: the scope publishes the interpreter's direct
//! runtime-root provider, caller-local values that must survive an allocation
//! are traced explicitly, and each result is parked in the caller's scope
//! before return.
//!
//! # Contents
//! - [`Interpreter::scoped_array_buffer_from_bytes`]
//! - [`Interpreter::scoped_typed_array_from_bytes`]
//! - [`Interpreter::scoped_promise_fulfilled`] /
//!   [`Interpreter::scoped_promise_rejected`]
//! - [`Interpreter::scoped_iterate_to_handles`]
//!
//! # Invariants
//! - `ArrayBuffer` and typed-array bodies allocate in old space
//!   (non-moving), so a buffer handle passed by value into the view
//!   allocation cannot be invalidated by that allocation.
//! - The iterable drain parks every produced element in the scope
//!   before any further allocation; the raw `Vec<Value>` the iterator
//!   walk returns is never held across one.
//!
//! # See also
//! - [`crate::handles`] — the sibling scoped creation methods.
//! - [`crate::binary::typed_array`] — view representation + kinds.

use otter_gc::raw::RawGc;

use crate::binary::typed_array::{JsTypedArray, TypedArrayKind};
use crate::handles::{HandleScope, Local};
use crate::promise::JsPromiseHandle;
use crate::{ExecutionContext, Interpreter, Value, VmError};

impl Interpreter {
    /// Allocate a fixed-length `ArrayBuffer` owning `bytes` and park it
    /// in the current scope. The backing store is accounted as external
    /// memory; the scope's direct root provider keeps prior handles live
    /// through the reservation.
    pub(crate) fn scoped_array_buffer_from_bytes<'s>(
        &mut self,
        scope: &'s HandleScope,
        bytes: Vec<u8>,
    ) -> Result<Local<'s>, VmError> {
        let _runtime_roots_guard = self.scope_runtime_roots_guard();
        let mut external_visit = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        let buffer = crate::binary::JsArrayBuffer::from_bytes_with_roots(
            bytes,
            &mut self.gc_heap,
            &mut external_visit,
        )?;
        Ok(self.scoped_value(scope, Value::array_buffer(buffer)))
    }

    /// Allocate a typed array of `kind` viewing a fresh `ArrayBuffer`
    /// that owns `bytes`, and park it in the current scope. `bytes`
    /// must be an exact multiple of the element width.
    pub(crate) fn scoped_typed_array_from_bytes<'s>(
        &mut self,
        scope: &'s HandleScope,
        kind: TypedArrayKind,
        bytes: Vec<u8>,
    ) -> Result<Local<'s>, VmError> {
        let width = kind.bytes_per_element();
        if !bytes.len().is_multiple_of(width) {
            return Err(self.err_type(
                format!(
                    "byte length {} is not a multiple of {} element width {}",
                    bytes.len(),
                    kind.name(),
                    width
                )
                .into(),
            ));
        }
        let length = bytes.len() / width;
        let _runtime_roots_guard = self.scope_runtime_roots_guard();
        let mut external_visit = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
        let buffer = crate::binary::JsArrayBuffer::from_bytes_with_roots(
            bytes,
            &mut self.gc_heap,
            &mut external_visit,
        )?;
        // Both the buffer body and the view body live in old space, so
        // `buffer` cannot be moved by the view allocation.
        let view = JsTypedArray::new(&mut self.gc_heap, buffer, kind, 0, length)?;
        Ok(self.scoped_value(scope, Value::typed_array(view)))
    }

    /// Allocate a pre-fulfilled promise whose value is the handle
    /// `value`, and park it in the current scope.
    pub(crate) fn scoped_promise_fulfilled<'s>(
        &mut self,
        scope: &'s HandleScope,
        value: Local<'_>,
    ) -> Result<Local<'s>, VmError> {
        let value = self.handle_arena.get(value.index());
        self.scoped_promise_settled(scope, value, true)
    }

    /// Allocate a pre-rejected promise whose reason is the handle
    /// `reason`, and park it in the current scope.
    pub(crate) fn scoped_promise_rejected<'s>(
        &mut self,
        scope: &'s HandleScope,
        reason: Local<'_>,
    ) -> Result<Local<'s>, VmError> {
        let reason = self.handle_arena.get(reason.index());
        self.scoped_promise_settled(scope, reason, false)
    }

    fn scoped_promise_settled<'s>(
        &mut self,
        scope: &'s HandleScope,
        payload: Value,
        fulfilled: bool,
    ) -> Result<Local<'s>, VmError> {
        let _runtime_roots_guard = self.scope_runtime_roots_guard();
        // The payload local is stored into the promise body by the same
        // allocation that can move it; tracing it rewrites this frame's
        // copy in place, so the stored value is current.
        let payload_root = payload;
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            payload_root.trace_value_slots(visitor);
        };
        let promise = if fulfilled {
            JsPromiseHandle::fulfilled_with_roots(
                &mut self.gc_heap,
                payload_root,
                &mut external_visit,
            )?
        } else {
            JsPromiseHandle::rejected_with_roots(
                &mut self.gc_heap,
                payload_root,
                &mut external_visit,
            )?
        };
        Ok(self.scoped_value(scope, Value::promise(promise)))
    }

    /// Drain an iterable to completion (§7.4.13 IteratorToList) and
    /// park every produced element in the current scope.
    ///
    /// Arrays take the dense fast path with no context; every other
    /// iterable drives the user-visible `Symbol.iterator` protocol and
    /// therefore needs the call's execution context. A context-free
    /// call on a non-array reports a `TypeError` instead of guessing.
    pub(crate) fn scoped_iterate_to_handles<'s>(
        &mut self,
        stack: &mut crate::ActivationStack,
        scope: &'s HandleScope,
        context: Option<&ExecutionContext>,
        iterable: Local<'_>,
    ) -> Result<Vec<Local<'s>>, VmError> {
        let value = self.handle_arena.get(iterable.index());
        let elements = if let Some(array) = value.as_array() {
            crate::array::with_elements(array, &self.gc_heap, <[Value]>::to_vec)
        } else if let Some(context) = context {
            self.iterator_to_list_sync(context, stack, &value)?
        } else {
            return Err(self.err_type(
                "cannot iterate a non-array iterable without an execution context"
                    .to_string()
                    .into(),
            ));
        };
        // Park immediately: nothing allocates between the walk's return
        // and these pushes, so the raw values are still current.
        Ok(elements
            .into_iter()
            .map(|element| self.scoped_value(scope, element))
            .collect())
    }
}
