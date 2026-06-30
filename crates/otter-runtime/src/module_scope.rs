//! High-level builder for native hosted-module exports.
//!
//! `ModuleScope` builds a module's `module.exports` value with correct moving-GC
//! rooting: every value produced is parked on the interpreter's module-root
//! (anchor) stack, which the GC traces and rewrites in place. Because the GC
//! moves objects, a raw [`Value`] held across an allocation is stale — so the
//! builder hands back a [`Rooted`] handle and re-reads the relocated value on
//! demand. This replaces the hand-rolled `roots: Vec<Value>` + manual
//! `native_function_from_call_host_rooted` pattern used by early modules.
//!
//! # Contents
//! - [`ModuleScope`] - the builder; pops everything it pushed on drop.
//! - [`Rooted`] - a handle to a value parked on the module-root stack.
//!
//! # Invariants
//! - Never hold a bare `Value` across an allocation; carry [`Rooted`] and
//!   re-read via [`ModuleScope::value`].
//! - Every `push_module_root` is balanced by the scope's `Drop`.
//! - The export returned by [`ModuleScope::finish`] must be rooted by the caller
//!   before the scope drops (the CommonJS loader stores it into `require.cache`
//!   under a fresh root).

use otter_vm::object::PartialPropertyDescriptor;
use otter_vm::{NativeCall, NativeCtx, NativeFastFn, Value, number::NumberValue, object};

fn oom(err: otter_gc::OutOfMemory) -> String {
    format!("out of memory: {err}")
}

/// A value parked on the module-root stack. Safe to hold across allocations
/// (the GC rewrites the slot in place); read the live handle with
/// [`ModuleScope::value`].
#[derive(Clone, Copy, Debug)]
pub struct Rooted(usize);

/// Builder for a native module's exports. Records the root-stack base on
/// construction and releases everything it pushed on drop.
pub struct ModuleScope<'a, 'rt> {
    ctx: &'a mut NativeCtx<'rt>,
    base: usize,
}

impl<'a, 'rt> ModuleScope<'a, 'rt> {
    /// Start a build scope over a native context.
    #[must_use]
    pub fn new(ctx: &'a mut NativeCtx<'rt>) -> Self {
        let base = ctx.interp_mut().module_root_depth();
        Self { ctx, base }
    }

    fn root(&mut self, value: Value) -> Rooted {
        let depth = self.ctx.interp_mut().push_module_root(value);
        Rooted(depth - 1)
    }

    /// Re-read a rooted value (returns the relocated handle).
    #[must_use]
    pub fn value(&mut self, rooted: Rooted) -> Value {
        self.ctx.interp_mut().module_root(rooted.0)
    }

    /// `undefined`.
    pub fn undefined(&mut self) -> Rooted {
        self.root(Value::undefined())
    }

    /// `null`.
    pub fn null(&mut self) -> Rooted {
        self.root(Value::null())
    }

    /// A boolean.
    pub fn boolean(&mut self, b: bool) -> Rooted {
        self.root(Value::boolean(b))
    }

    /// A number.
    pub fn number(&mut self, n: f64) -> Rooted {
        self.root(Value::number(NumberValue::from_f64(n)))
    }

    /// A string.
    ///
    /// # Errors
    /// Returns the error message on allocation failure.
    pub fn string(&mut self, s: &str) -> Result<Rooted, String> {
        let js = otter_vm::string::JsString::from_str(s, self.ctx.heap_mut()).map_err(oom)?;
        Ok(self.root(Value::string(js)))
    }

    /// Allocate an ordinary object (with `Object.prototype`).
    ///
    /// # Errors
    /// Returns the error message on allocation failure.
    pub fn object(&mut self) -> Result<Rooted, String> {
        let obj = self.ctx.alloc_object().map_err(oom)?;
        Ok(self.root(Value::object(obj)))
    }

    /// Allocate an ordinary object and seat it on `%Object.prototype%`.
    ///
    /// [`ModuleScope::object`] leaves a fresh host object with a null
    /// `[[Prototype]]`; use this when the result is handed back to user code as
    /// a plain object (e.g. `path.parse`), so `Object.getPrototypeOf(o) ===
    /// Object.prototype` and structural `deepStrictEqual` against an object
    /// literal holds.
    ///
    /// # Errors
    /// Returns the error message on allocation failure.
    pub fn ordinary_object(&mut self) -> Result<Rooted, String> {
        let rooted = self.object()?;
        if let Some(proto) = self.object_prototype() {
            let obj = self
                .value(rooted)
                .as_object()
                .expect("freshly allocated object");
            object::set_prototype(obj, self.ctx.heap_mut(), Some(proto));
        }
        Ok(rooted)
    }

    /// Resolve `%Object.prototype%` via `globalThis.Object.prototype`.
    fn object_prototype(&mut self) -> Option<object::JsObject> {
        let interp = self.ctx.interp_mut();
        let global = *interp.global_this();
        let ctor = object::get(global, interp.gc_heap(), "Object")?;
        if let Some(native) = ctor.as_native_function() {
            return native
                .own_property_descriptor(interp.gc_heap_mut(), "prototype")
                .ok()
                .flatten()
                .and_then(|desc| match desc.kind {
                    object::DescriptorKind::Data { value } => value.as_object(),
                    _ => None,
                });
        }
        ctor.as_object()
            .and_then(|o| object::get(o, interp.gc_heap(), "prototype"))
            .and_then(|p| p.as_object())
    }

    /// Build a native function from a static fast fn.
    ///
    /// # Errors
    /// Returns the error message on allocation failure.
    pub fn function(
        &mut self,
        name: &'static str,
        length: u8,
        call: NativeFastFn,
    ) -> Result<Rooted, String> {
        let value = self
            .ctx
            .interp_mut()
            .native_function_from_call_host_rooted(name, length, NativeCall::Static(call), &[], &[])
            .map_err(oom)?;
        Ok(self.root(value))
    }

    /// Build a callable object: its `[[Call]]` runs `call`, and it carries the
    /// given method properties (e.g. `assert(cond)` + `assert.strictEqual`).
    ///
    /// # Errors
    /// Returns the error message on allocation failure.
    pub fn callable(
        &mut self,
        name: &'static str,
        length: u8,
        call: NativeFastFn,
        methods: &[(&'static str, u8, NativeFastFn)],
    ) -> Result<Rooted, String> {
        let call_fn = self.function(name, length, call)?;
        let host = self
            .ctx
            .interp_mut()
            .alloc_host_object_with_roots(&[], &[])
            .map_err(oom)?;
        let obj = self.root(Value::object(host));
        let call_value = self.value(call_fn);
        let host = self
            .value(obj)
            .as_object()
            .expect("freshly allocated host object");
        object::set_call_native(host, self.ctx.heap_mut(), call_value);
        for (mname, mlen, mcall) in methods {
            let method = self.function(mname, *mlen, *mcall)?;
            self.set(obj, mname, method);
        }
        Ok(obj)
    }

    /// Set own property `key = value` on `obj`.
    pub fn set(&mut self, obj: Rooted, key: &str, value: Rooted) {
        let v = self.value(value);
        let mut target = self
            .value(obj)
            .as_object()
            .expect("ModuleScope::set target is not an object");
        object::set(&mut target, self.ctx.heap_mut(), key, v);
    }

    /// Define a method property on `obj`.
    ///
    /// # Errors
    /// Returns the error message on allocation failure.
    pub fn set_method(
        &mut self,
        obj: Rooted,
        name: &'static str,
        length: u8,
        call: NativeFastFn,
    ) -> Result<(), String> {
        let method = self.function(name, length, call)?;
        self.set(obj, name, method);
        Ok(())
    }

    /// Define an own data property on a native function built by this scope.
    ///
    /// # Errors
    /// Returns an error when `func` is not a native function or the descriptor
    /// cannot be defined.
    pub fn set_native_function_property(
        &mut self,
        func: Rooted,
        key: &str,
        value: Rooted,
    ) -> Result<(), String> {
        let target = self.value(func).as_native_function().ok_or_else(|| {
            "ModuleScope::set_native_function_property target is not native".to_string()
        })?;
        let desc = object::PropertyDescriptor::data(self.value(value), true, false, true);
        if target.define_own_property(self.ctx.heap_mut(), key, desc) {
            Ok(())
        } else {
            Err(format!("failed to define native function property {key}"))
        }
    }

    /// Define a string property on `obj`.
    ///
    /// # Errors
    /// Returns the error message on allocation failure.
    pub fn set_string(&mut self, obj: Rooted, key: &str, s: &str) -> Result<(), String> {
        let value = self.string(s)?;
        self.set(obj, key, value);
        Ok(())
    }

    /// Define a non-writable, configurable, enumerable string property
    /// (e.g. `os.EOL`, which must throw on assignment in strict mode yet stay
    /// redefinable via `Object.defineProperties`).
    ///
    /// # Errors
    /// Returns the error message on allocation failure.
    pub fn set_string_readonly(&mut self, obj: Rooted, key: &str, s: &str) -> Result<(), String> {
        let value = self.string(s)?;
        let v = self.value(value);
        let target = self
            .value(obj)
            .as_object()
            .expect("ModuleScope::set_string_readonly target is not an object");
        let descriptor = PartialPropertyDescriptor {
            value: Some(v),
            writable: Some(false),
            enumerable: Some(true),
            configurable: Some(true),
            ..PartialPropertyDescriptor::default()
        };
        object::define_own_property_partial(target, self.ctx.heap_mut(), key, descriptor);
        Ok(())
    }

    /// Define a number property on `obj`.
    pub fn set_number(&mut self, obj: Rooted, key: &str, n: f64) {
        let value = self.number(n);
        self.set(obj, key, value);
    }

    /// Build an array from already-rooted elements. Reads each element live
    /// from its root slot, so it is safe across the intermediate allocations.
    ///
    /// # Errors
    /// Returns the error message on allocation failure.
    pub fn array(&mut self, items: &[Rooted]) -> Result<Rooted, String> {
        let values: Vec<Value> = items.iter().map(|r| self.value(*r)).collect();
        let arr = self.ctx.array_from_elements(values).map_err(oom)?;
        Ok(self.root(Value::array(arr)))
    }

    /// Return the export value. It is read live from its root slot; the caller
    /// must root it before this scope drops (the CommonJS loader does so by
    /// storing it into `require.cache`).
    #[must_use]
    pub fn finish(&mut self, export: Rooted) -> Value {
        self.value(export)
    }
}

impl Drop for ModuleScope<'_, '_> {
    fn drop(&mut self) {
        self.ctx.interp_mut().pop_module_roots_to(self.base);
    }
}
