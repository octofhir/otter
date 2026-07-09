//! The marshalling context: a borrowed view over a native call.
//!
//! [`MarshalCx`] bundles the two things every conversion needs — the
//! call's [`crate::NativeCtx`] and an open [`HandleScope`] — and
//! exposes the marshalling primitives on top of them: scoped value
//! creation, spec coercions, binary builders, promise builders, host
//! data access, and callable re-entry. [`super::FromJs`] /
//! [`super::IntoJs`] implementations and generated binding glue see
//! this type; they never touch the interpreter directly.
//!
//! # Contents
//! - [`MarshalCx`] — construction ([`MarshalCx::new`]) + primitives.
//!
//! # Invariants
//! - Every JS value this type hands out or accepts is a
//!   [`Scoped`] handle parked in the ambient scope; raw
//!   [`Value`]s appear only at the [`MarshalCx::park`] /
//!   [`MarshalCx::escape`] boundary.
//! - Coercions that can re-enter user JS (`to_string` / `to_number` on
//!   objects, iteration, callback invocation) require the call's
//!   execution context and report [`JsError::Type`] without one.
//!
//! # See also
//! - [`crate::runtime_cx`] — the underlying native call context.
//! - [`super::scoped_ext`] — the interpreter builders wrapped here.

use crate::binary::typed_array::TypedArrayKind;
use crate::handles::{HandleScope, Scoped};
use crate::{ExecutionContext, NativeCtx, Value, VmError};

use super::error::JsError;

/// Borrowed conversion context over (`&mut NativeCtx`, `&HandleScope`).
///
/// `'rt` is the mutator turn, `'cx` the borrow of the native context,
/// `'s` the ambient handle scope every minted handle is pinned to.
pub struct MarshalCx<'rt, 'cx, 's> {
    ctx: &'cx mut NativeCtx<'rt>,
    scope: &'s HandleScope,
}

impl<'rt, 'cx, 's> MarshalCx<'rt, 'cx, 's> {
    /// Wrap a native call context and an open scope.
    #[must_use]
    pub fn new(ctx: &'cx mut NativeCtx<'rt>, scope: &'s HandleScope) -> Self {
        Self { ctx, scope }
    }

    /// Borrow the underlying native context (the manual escape hatch).
    pub fn ctx(&mut self) -> &mut NativeCtx<'rt> {
        self.ctx
    }

    /// The ambient handle scope.
    #[must_use]
    pub fn scope(&self) -> &'s HandleScope {
        self.scope
    }

    fn interp(&mut self) -> &mut crate::Interpreter {
        self.ctx.interp_mut()
    }

    fn context(&self) -> Option<&'rt ExecutionContext> {
        self.ctx.context_ref()
    }

    fn vm_err(&mut self, err: VmError) -> JsError {
        JsError::from_vm(self.ctx.interp_mut(), err)
    }

    // ---- parking and reading ------------------------------------------------

    /// Park an incoming raw `Value` (an argument, a receiver) in the
    /// scope. Do this before the first allocation.
    #[must_use]
    pub fn park(&mut self, value: Value) -> Scoped<'s> {
        let scope = self.scope;
        self.interp().scoped_value(scope, value)
    }

    /// Read the current raw `Value` behind a handle for immediate
    /// hand-off (a return to the VM, a store into a rooted object).
    /// Valid only until the next allocation.
    #[must_use]
    pub fn escape(&self, v: Scoped<'_>) -> Value {
        self.ctx.escape(v)
    }

    /// Whether the handle currently holds `undefined`.
    #[must_use]
    pub fn is_undefined(&self, v: Scoped<'_>) -> bool {
        self.ctx.escape(v).is_undefined()
    }

    /// Whether the handle currently holds `null`.
    #[must_use]
    pub fn is_null(&self, v: Scoped<'_>) -> bool {
        self.ctx.escape(v).is_null()
    }

    /// Whether the handle currently holds `undefined` or `null`.
    #[must_use]
    pub fn is_nullish(&self, v: Scoped<'_>) -> bool {
        let raw = self.ctx.escape(v);
        raw.is_undefined() || raw.is_null()
    }

    /// Whether the handle currently holds an ordinary object.
    #[must_use]
    pub fn is_object(&self, v: Scoped<'_>) -> bool {
        self.ctx.escape(v).as_object().is_some()
    }

    /// Non-coercing number read.
    #[must_use]
    pub fn as_f64(&self, v: Scoped<'_>) -> Option<f64> {
        self.ctx.escape(v).as_f64()
    }

    /// Non-coercing string read (lossy Rust rendering).
    #[must_use]
    pub fn as_string_lossy(&self, v: Scoped<'_>) -> Option<String> {
        let raw = self.ctx.escape(v);
        raw.as_string(self.ctx.heap())
            .map(|s| s.to_lossy_string(self.ctx.heap()))
    }

    // ---- creation -----------------------------------------------------------

    /// Park the `undefined` immediate.
    #[must_use]
    pub fn undefined(&mut self) -> Scoped<'s> {
        let scope = self.scope;
        self.interp().scoped_undefined(scope)
    }

    /// Park the `null` immediate.
    #[must_use]
    pub fn null(&mut self) -> Scoped<'s> {
        let scope = self.scope;
        self.interp().scoped_null(scope)
    }

    /// Park a number immediate.
    #[must_use]
    pub fn number(&mut self, n: f64) -> Scoped<'s> {
        let scope = self.scope;
        self.interp().scoped_number(scope, n)
    }

    /// Park a boolean immediate.
    #[must_use]
    pub fn boolean(&mut self, b: bool) -> Scoped<'s> {
        let scope = self.scope;
        self.interp().scoped_boolean(scope, b)
    }

    /// Allocate a JS string from UTF-8 text.
    pub fn string(&mut self, text: &str) -> Result<Scoped<'s>, JsError> {
        let scope = self.scope;
        self.interp()
            .scoped_string(scope, text)
            .map_err(|err| self.vm_err(err))
    }

    /// Allocate a JS string from WTF-16 code units (lone surrogates
    /// preserved).
    pub fn string_from_units(&mut self, units: &[u16]) -> Result<Scoped<'s>, JsError> {
        let scope = self.scope;
        let interp = self.ctx.interp_mut();
        let string = crate::string::JsString::from_utf16_units(units, interp.gc_heap_mut())
            .map_err(VmError::from);
        match string {
            Ok(string) => Ok(interp.scoped_value(scope, Value::string(string))),
            Err(err) => Err(self.vm_err(err)),
        }
    }

    /// Allocate an ordinary object (`%Object.prototype%`).
    pub fn object(&mut self) -> Result<Scoped<'s>, JsError> {
        let scope = self.scope;
        self.interp()
            .scoped_object(scope)
            .map_err(|err| self.vm_err(err))
    }

    /// Allocate an array of `len` holes.
    pub fn array(&mut self, len: usize) -> Result<Scoped<'s>, JsError> {
        let scope = self.scope;
        self.interp()
            .scoped_array(scope, len)
            .map_err(|err| self.vm_err(err))
    }

    /// Allocate a fixed-length `ArrayBuffer` owning `bytes`.
    pub fn array_buffer_from_bytes(&mut self, bytes: Vec<u8>) -> Result<Scoped<'s>, JsError> {
        let scope = self.scope;
        self.interp()
            .scoped_array_buffer_from_bytes(scope, bytes)
            .map_err(|err| self.vm_err(err))
    }

    /// Allocate a typed array of `kind` over a fresh buffer owning
    /// `bytes` (`bytes.len()` must be a multiple of the element width).
    pub fn typed_array_from_bytes(
        &mut self,
        kind: TypedArrayKind,
        bytes: Vec<u8>,
    ) -> Result<Scoped<'s>, JsError> {
        let scope = self.scope;
        self.interp()
            .scoped_typed_array_from_bytes(scope, kind, bytes)
            .map_err(|err| self.vm_err(err))
    }

    /// Allocate a `Uint8Array` over a fresh buffer owning `bytes`.
    pub fn uint8_array_from_bytes(&mut self, bytes: Vec<u8>) -> Result<Scoped<'s>, JsError> {
        self.typed_array_from_bytes(TypedArrayKind::Uint8, bytes)
    }

    /// Allocate a pre-fulfilled promise carrying `value`.
    pub fn promise_fulfilled(&mut self, value: Scoped<'_>) -> Result<Scoped<'s>, JsError> {
        let scope = self.scope;
        self.interp()
            .scoped_promise_fulfilled(scope, value)
            .map_err(|err| self.vm_err(err))
    }

    /// Allocate a pre-rejected promise carrying `reason`.
    pub fn promise_rejected(&mut self, reason: Scoped<'_>) -> Result<Scoped<'s>, JsError> {
        let scope = self.scope;
        self.interp()
            .scoped_promise_rejected(scope, reason)
            .map_err(|err| self.vm_err(err))
    }

    // ---- object access ------------------------------------------------------

    /// Read property `key` from the object handle `obj` (absent reads
    /// as `undefined`).
    pub fn get(&mut self, obj: Scoped<'_>, key: &str) -> Result<Scoped<'s>, JsError> {
        let scope = self.scope;
        self.interp()
            .scoped_get(scope, obj, key)
            .map_err(|err| self.vm_err(err))
    }

    /// Write `value` to property `key` on the object handle `obj`.
    pub fn set(&mut self, obj: Scoped<'_>, key: &str, value: Scoped<'_>) -> Result<(), JsError> {
        let scope = self.scope;
        self.interp()
            .scoped_set(scope, obj, key, value)
            .map_err(|err| self.vm_err(err))
    }

    /// Store `value` at array index `index` on the array handle `arr`.
    pub fn set_index(
        &mut self,
        arr: Scoped<'_>,
        index: usize,
        value: Scoped<'_>,
    ) -> Result<(), JsError> {
        let scope = self.scope;
        self.interp()
            .scoped_set_index(scope, arr, index, value)
            .map_err(|err| self.vm_err(err))
    }

    // ---- spec coercions -----------------------------------------------------

    /// §7.1.17 `ToString`, returning the lossy Rust rendering (lone
    /// surrogates replaced — USVString semantics). Objects re-enter
    /// user JS and need the call's execution context.
    pub fn to_string_spec(&mut self, v: Scoped<'_>) -> Result<String, JsError> {
        let value = self.ctx.escape(v);
        if crate::abstract_ops::is_primitive(&value) {
            let interp = self.ctx.interp_mut();
            return crate::coerce::primitive_to_string_lossy(interp, &value)
                .map_err(|err| self.vm_err(err));
        }
        let Some(context) = self.context() else {
            return Err(JsError::Type(
                "cannot coerce an object to a string without an execution context".to_string(),
            ));
        };
        let value = self.ctx.escape(v);
        let interp = self.ctx.interp_mut();
        interp
            .coerce_to_string(context, &value)
            .map_err(|err| self.vm_err(err))
    }

    /// §7.1.17 `ToString`, returning WTF-16 code units (lone
    /// surrogates preserved — DOMString semantics).
    pub fn to_string_units(&mut self, v: Scoped<'_>) -> Result<Vec<u16>, JsError> {
        let value = self.ctx.escape(v);
        if !crate::abstract_ops::is_primitive(&value) && self.context().is_none() {
            return Err(JsError::Type(
                "cannot coerce an object to a string without an execution context".to_string(),
            ));
        }
        let context = self.context();
        let value = self.ctx.escape(v);
        let interp = self.ctx.interp_mut();
        let string = crate::coerce::to_js_string_units(interp, context, &value);
        match string {
            Ok(units) => Ok(units),
            Err(err) => Err(self.vm_err(err)),
        }
    }

    /// §7.1.4 `ToNumber`. Objects re-enter user JS and need the call's
    /// execution context.
    pub fn to_number_spec(&mut self, v: Scoped<'_>) -> Result<f64, JsError> {
        let value = self.ctx.escape(v);
        if crate::abstract_ops::is_primitive(&value) {
            let interp = self.ctx.interp_mut();
            return crate::coerce::primitive_to_number(interp, &value)
                .map(crate::number::NumberValue::as_f64)
                .map_err(|err| self.vm_err(err));
        }
        let Some(context) = self.context() else {
            return Err(JsError::Type(
                "cannot coerce an object to a number without an execution context".to_string(),
            ));
        };
        let value = self.ctx.escape(v);
        let interp = self.ctx.interp_mut();
        interp
            .coerce_to_number(context, &value)
            .map(crate::number::NumberValue::as_f64)
            .map_err(|err| self.vm_err(err))
    }

    /// §7.1.2 `ToBoolean` (never re-enters).
    #[must_use]
    pub fn to_boolean(&self, v: Scoped<'_>) -> bool {
        self.ctx.escape(v).to_boolean(self.ctx.heap())
    }

    /// Copy the live byte range out of a `BufferSource` handle — an
    /// `ArrayBuffer` or any typed-array view. `None` when the handle
    /// holds neither; a detached buffer reads as empty.
    #[must_use]
    pub fn buffer_source_bytes(&self, v: Scoped<'_>) -> Option<Vec<u8>> {
        let raw = self.ctx.escape(v);
        let heap = self.ctx.heap();
        if let Some(view) = raw.as_typed_array(heap) {
            let offset = view.byte_offset(heap);
            let length = view.byte_length(heap);
            let bytes = view
                .buffer(heap)
                .with_bytes(heap, |bytes| {
                    bytes.get(offset..offset + length).map(<[u8]>::to_vec)
                })
                .unwrap_or_default();
            return Some(bytes);
        }
        if let Some(buffer) = raw.as_array_buffer() {
            return Some(buffer.with_bytes(heap, <[u8]>::to_vec));
        }
        None
    }

    // ---- iteration / host data / callables ----------------------------------

    /// Drain an iterable (§7.4.13) and park every element in the
    /// scope. Arrays take a dense fast path; anything else drives the
    /// `Symbol.iterator` protocol and needs the execution context.
    pub fn iterate_to_handles(&mut self, v: Scoped<'_>) -> Result<Vec<Scoped<'s>>, JsError> {
        let context = self.context();
        let scope = self.scope;
        let interp = self.ctx.interp_mut();
        let handles = interp.scoped_iterate_to_handles(scope, context, v);
        handles.map_err(|err| self.vm_err(err))
    }

    /// Borrow the host data of a branded host object. Reports a
    /// `TypeError` when the handle is not an object, carries no host
    /// data, or the data is of a different type.
    pub fn with_host_data<T: std::any::Any, R>(
        &self,
        v: Scoped<'_>,
        f: impl FnOnce(&T) -> R,
    ) -> Result<R, JsError> {
        let raw = self.ctx.escape(v);
        let Some(object) = raw.as_object() else {
            return Err(JsError::Type("value is not an object".to_string()));
        };
        crate::object::with_host_data::<T, R>(object, self.ctx.heap(), f)
            .map_err(|err| JsError::Type(err.to_string()))
    }

    /// Whether the handle currently holds a callable value.
    #[must_use]
    pub fn is_callable(&mut self, v: Scoped<'_>) -> bool {
        let raw = self.ctx.escape(v);
        self.ctx.interp_mut().is_callable_runtime(&raw)
    }

    /// Synchronously invoke the callable handle `callee` with
    /// `this_value` and `args`, parking the completion value in the
    /// scope. Needs the call's execution context.
    pub fn call(
        &mut self,
        callee: Scoped<'_>,
        this_value: Scoped<'_>,
        args: &[Scoped<'_>],
    ) -> Result<Scoped<'s>, JsError> {
        let Some(context) = self.context() else {
            return Err(JsError::Type(
                "cannot invoke a callback without an execution context".to_string(),
            ));
        };
        let scope = self.scope;
        let interp = self.ctx.interp_mut();
        // Re-resolve every handle through the arena immediately before
        // the call; run_callable_sync roots its own frame from there.
        let callee = interp.escape_scoped(callee);
        let this_value = interp.escape_scoped(this_value);
        let argv: smallvec::SmallVec<[Value; 8]> =
            args.iter().map(|a| interp.escape_scoped(*a)).collect();
        let result = interp.run_callable_sync(context, &callee, this_value, argv);
        match result {
            Ok(value) => Ok(interp.scoped_value(scope, value)),
            Err(err) => Err(self.vm_err(err)),
        }
    }
}
