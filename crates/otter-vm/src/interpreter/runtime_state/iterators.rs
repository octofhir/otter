//! Iterator protocol + generator / async-generator resume machinery for
//! `yield*` delegation. Includes `create_iter_result`, `alloc_generator`,
//! `resume_generator`, async generator variants, and the
//! `call_iterator_next_with_value` / `throw` / `return` helpers.

use crate::descriptors::VmNativeCallError;
use crate::module::{FunctionIndex, Module};
use crate::object::{HeapValueKind, ObjectHandle};
use crate::value::RegisterValue;

use super::{Interpreter, InterpreterError, RuntimeState};

impl RuntimeState {
    // ─── Generator Support (§27.5) ────────────────────────────────────

    /// Creates a `{ value, done }` iterator result object.
    /// Convenience wrapper around `create_iter_result_object`.
    pub fn create_iter_result(
        &mut self,
        value: RegisterValue,
        done: bool,
    ) -> Result<ObjectHandle, VmNativeCallError> {
        let obj = self.alloc_object();
        let value_prop = self.intern_property_name("value");
        let done_prop = self.intern_property_name("done");
        self.objects
            .set_property(obj, value_prop, value)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
        self.objects
            .set_property(obj, done_prop, RegisterValue::from_bool(done))
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
        Ok(obj)
    }

    /// Allocates a generator object in SuspendedStart state.
    ///
    /// Called when a generator function is invoked — instead of executing the
    /// body, we create a generator object that will lazily execute on `.next()`.
    pub fn alloc_generator(
        &mut self,
        module: Module,
        function_index: FunctionIndex,
        closure_handle: Option<ObjectHandle>,
        arguments: Vec<RegisterValue>,
    ) -> ObjectHandle {
        let prototype = self.intrinsics().generator_prototype();
        self.objects.alloc_generator(
            Some(prototype),
            module,
            function_index,
            closure_handle,
            arguments,
        )
    }

    /// Resumes a suspended generator. Called by the native `.next()`, `.return()`,
    /// and `.throw()` methods on `%GeneratorPrototype%`.
    pub(crate) fn resume_generator(
        &mut self,
        generator: ObjectHandle,
        sent_value: RegisterValue,
        resume_kind: crate::intrinsics::GeneratorResumeKind,
    ) -> Result<RegisterValue, VmNativeCallError> {
        Interpreter::resume_generator_impl(self, generator, sent_value, resume_kind)
    }

    // ─── Async Generator Support (§27.6) ────────────────────────────────

    /// Allocates an async generator object in SuspendedStart state.
    ///
    /// Called when an `async function*` is invoked — instead of executing the
    /// body, we create an async generator object that lazily executes on `.next()`.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-asyncgeneratorstart>
    pub fn alloc_async_generator(
        &mut self,
        module: Module,
        function_index: FunctionIndex,
        closure_handle: Option<ObjectHandle>,
        arguments: Vec<RegisterValue>,
    ) -> ObjectHandle {
        let prototype = self.intrinsics().async_generator_prototype();
        self.objects.alloc_async_generator(
            Some(prototype),
            module,
            function_index,
            closure_handle,
            arguments,
        )
    }

    /// Resumes a suspended async generator. Dequeues the front request
    /// and runs the body until next yield/await/return/throw.
    ///
    /// §27.6.3.3 AsyncGeneratorResume
    /// Spec: <https://tc39.es/ecma262/#sec-asyncgeneratorresume>
    pub(crate) fn resume_async_generator(
        &mut self,
        generator: ObjectHandle,
    ) -> Result<(), VmNativeCallError> {
        Interpreter::resume_async_generator_impl(self, generator)
    }

    // ─── yield* delegation helpers (§14.4.4) ────────────────────────────

    /// Calls `iterator.next(value)` — tries the internal fast path first
    /// (ArrayIterator/StringIterator), then falls back to protocol-based `.next()`.
    /// Returns (done, value).
    /// Spec: <https://tc39.es/ecma262/#sec-iteratornext>
    pub(crate) fn call_iterator_next_with_value(
        &mut self,
        iterator: ObjectHandle,
        value: RegisterValue,
    ) -> Result<(bool, RegisterValue), InterpreterError> {
        // Fast path: internal array/string iterators (ignores sent value,
        // which is correct per spec — arrays/strings don't use it).
        match self.iterator_next(iterator) {
            Ok(step) => {
                return Ok((step.is_done(), step.value()));
            }
            Err(InterpreterError::InvalidHeapValueKind) => {
                // Not an internal fast-path iterator — fall through to protocol.
            }
            Err(e) => return Err(e),
        }

        // Slow path: protocol-based iterator — look up .next() and call it.
        let next_prop = self.intern_property_name("next");
        let iter_val = RegisterValue::from_object_handle(iterator.0);
        let next_fn = self
            .ordinary_get(iterator, next_prop, iter_val)
            .map_err(|e| match e {
                VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
            })?;
        let callable = next_fn
            .as_object_handle()
            .map(ObjectHandle)
            .filter(|h| self.objects.is_callable(*h))
            .ok_or_else(|| {
                InterpreterError::TypeError("Iterator .next is not a function".into())
            })?;
        let result_obj = self
            .call_callable(callable, iter_val, &[value])
            .map_err(|e| match e {
                VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
            })?;
        self.read_iter_result(result_obj)
    }

    /// Calls `iterator.throw(value)` if the method exists.
    /// Returns `Some((done, value))` if `.throw` exists, `None` if it doesn't.
    /// Internal array/string iterators don't have `.throw()` — returns `None`.
    /// Spec: <https://tc39.es/ecma262/#sec-generator-function-definitions-runtime-semantics-evaluation> step 7.b
    pub(crate) fn call_iterator_throw(
        &mut self,
        iterator: ObjectHandle,
        value: RegisterValue,
    ) -> Result<Option<(bool, RegisterValue)>, InterpreterError> {
        // Internal array/string iterators have no .throw() method.
        if self.is_internal_fast_path_iterator(iterator) {
            return Ok(None);
        }

        let throw_prop = self.intern_property_name("throw");
        let iter_val = RegisterValue::from_object_handle(iterator.0);
        let throw_fn = self
            .ordinary_get(iterator, throw_prop, iter_val)
            .map_err(|e| match e {
                VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
            })?;
        if throw_fn == RegisterValue::undefined() || throw_fn == RegisterValue::null() {
            return Ok(None);
        }
        let callable = throw_fn
            .as_object_handle()
            .map(ObjectHandle)
            .filter(|h| self.objects.is_callable(*h))
            .ok_or_else(|| {
                InterpreterError::TypeError("Iterator .throw is not a function".into())
            })?;
        let result_obj = self
            .call_callable(callable, iter_val, &[value])
            .map_err(|e| match e {
                VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
            })?;
        self.read_iter_result(result_obj).map(Some)
    }

    /// Calls `iterator.return(value)` if the method exists.
    /// Returns `Some((done, value))` if `.return` exists, `None` if it doesn't.
    /// Internal array/string iterators have no `.return()` — returns `None`.
    /// Spec: <https://tc39.es/ecma262/#sec-generator-function-definitions-runtime-semantics-evaluation> step 7.c
    pub(crate) fn call_iterator_return(
        &mut self,
        iterator: ObjectHandle,
        value: RegisterValue,
    ) -> Result<Option<(bool, RegisterValue)>, InterpreterError> {
        // Internal array/string iterators have no .return() method.
        if self.is_internal_fast_path_iterator(iterator) {
            return Ok(None);
        }

        let return_prop = self.intern_property_name("return");
        let iter_val = RegisterValue::from_object_handle(iterator.0);
        let return_fn =
            self.ordinary_get(iterator, return_prop, iter_val)
                .map_err(|e| match e {
                    VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                    VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
                })?;
        if return_fn == RegisterValue::undefined() || return_fn == RegisterValue::null() {
            return Ok(None);
        }
        let callable = return_fn
            .as_object_handle()
            .map(ObjectHandle)
            .filter(|h| self.objects.is_callable(*h))
            .ok_or_else(|| {
                InterpreterError::TypeError("Iterator .return is not a function".into())
            })?;
        let result_obj = self
            .call_callable(callable, iter_val, &[value])
            .map_err(|e| match e {
                VmNativeCallError::Thrown(v) => InterpreterError::UncaughtThrow(v),
                VmNativeCallError::Internal(m) => InterpreterError::NativeCall(m),
            })?;
        self.read_iter_result(result_obj).map(Some)
    }

    /// Returns `true` if the handle is an internal array (values-kind) or string iterator
    /// that uses the `iterator_next` fast path and has no protocol-level `.next()`/`.throw()`/`.return()`.
    fn is_internal_fast_path_iterator(&self, handle: ObjectHandle) -> bool {
        matches!(self.objects.kind(handle), Ok(HeapValueKind::Iterator))
    }

    /// Reads `done` and `value` from an iterator result object.
    fn read_iter_result(
        &mut self,
        result_obj: RegisterValue,
    ) -> Result<(bool, RegisterValue), InterpreterError> {
        let result_handle = result_obj
            .as_object_handle()
            .map(ObjectHandle)
            .ok_or_else(|| {
                InterpreterError::TypeError("Iterator result must be an object".into())
            })?;
        let done_prop = self.intern_property_name("done");
        let done_val = self
            .ordinary_get(result_handle, done_prop, result_obj)
            .unwrap_or_else(|_| RegisterValue::from_bool(false));
        let done = self.js_to_boolean(done_val).unwrap_or(false);
        let value_prop = self.intern_property_name("value");
        let value = self
            .ordinary_get(result_handle, value_prop, result_obj)
            .unwrap_or_else(|_| RegisterValue::undefined());
        Ok((done, value))
    }
}
