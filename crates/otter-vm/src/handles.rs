//! Scope-based GC handle rooting.
//!
//! Native Rust code building JS values holds raw [`Value`] cage offsets. The
//! young generation is a moving Cheney semispace, so any allocation can
//! relocate any young object and turn a copy held in a Rust local into a
//! dangling offset. The handle scope replaces the ad-hoc "thread `value_roots`
//! into every allocating call and re-read afterwards" contract with a single
//! rooted store — the [`HandleArena`] — that the collector rewrites in place.
//!
//! A [`Scoped`] handle carries only an index into that store, never a cached
//! payload, so every read resolves through the current slot and can never be
//! stale. Handles are minted inside [`Interpreter::with_handle_scope`], which
//! owns the arena range it opened and truncates back to it on return, so a
//! `Scoped` cannot outlive the scope that created it (the `'s` lifetime pins
//! it) and can never dangle.
//!
//! # Contents
//!
//! - [`HandleArena`] — contiguous, collector-traced handle storage; one per
//!   [`Interpreter`].
//! - [`HandleScope`] — a scope token owning an arena range `[base, len)`.
//! - [`Scoped`] — a `Copy` index handle whose lifetime pins it to its scope.
//!
//! # Invariants
//!
//! - **Every collection entry point that can run while native code is on the
//!   Rust stack must trace the arena.** There are exactly two: the extra-roots
//!   provider path walked during dispatch and the per-allocation snapshot path
//!   (`collect_runtime_roots`). Both reach
//!   [`crate::runtime_state::RuntimeState::trace_roots`], which walks the
//!   arena, so a slot is always current after any collection. A handle read
//!   through a live arena is therefore never stale.
//! - The arena is a strict stack: [`HandleScope`] truncation may only drop the
//!   range the scope opened, never slots an outer scope owns. Nesting is free —
//!   an inner scope's `base` is the current arena length, and its truncate
//!   leaves every outer slot in place.
//! - A `Scoped` never caches a payload; it is only an arena index. Reads go
//!   through [`HandleArena::get`], which the collector keeps live.
//! - A scoped write may box a wide number inside the callee (`object::set` and
//!   its symbol/define variants), and that box allocation traces only the
//!   receiver it is handed. On the host-side path no dispatch provider is
//!   registered, so a scope installs the interpreter's runtime-root provider
//!   for its lifetime (see [`Interpreter::push_scope_runtime_roots`]) — the
//!   arena is then traced by any collection the box drives, so sibling handles
//!   never go stale. The registration is gated on there being no provider
//!   already, so the dispatch hot path pays nothing.
//!
//! # See also
//!
//! - [`crate::runtime_state`] — the root walker that traces the arena.
//! - [`crate::allocation_ops`] — the snapshot root path used by host-side
//!   allocations.

use std::marker::PhantomData;

use otter_gc::raw::RawGc;

use crate::{Interpreter, JsString, Value, VmError};

/// Contiguous scope-handle storage. One per [`crate::Interpreter`].
///
/// Every live slot is traced — and rewritten in place — by the runtime root
/// walk, so a parked [`Value`] always reflects the object's current location.
#[derive(Debug, Default)]
pub struct HandleArena {
    slots: Vec<Value>,
}

impl HandleArena {
    /// A fresh, empty arena.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self { slots: Vec::new() }
    }

    /// Number of live handle slots. Used as the truncation base when a new
    /// scope opens.
    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }

    /// Park `v` and return its stable slot index for the lifetime of the
    /// owning scope.
    pub(crate) fn push(&mut self, v: Value) -> u32 {
        let idx = self.slots.len() as u32;
        self.slots.push(v);
        idx
    }

    /// Read the (possibly relocated) value parked at `idx`.
    pub(crate) fn get(&self, idx: u32) -> Value {
        self.slots[idx as usize]
    }

    /// Overwrite the value parked at `idx`. Test-only: production writes never
    /// re-park a handle, because the only way a parked object relocates is a
    /// collection that rewrites its slot in place (see [`Interpreter::scoped_set`]).
    #[cfg(test)]
    pub(crate) fn set(&mut self, idx: u32, v: Value) {
        self.slots[idx as usize] = v;
    }

    /// Drop every slot at or above `base`, restoring the arena to the state a
    /// scope found on entry.
    pub(crate) fn truncate(&mut self, base: usize) {
        self.slots.truncate(base);
    }

    /// Visit every live slot as a GC root. Called from the runtime root walk;
    /// the collector rewrites each slot to the relocated offset on a move.
    pub(crate) fn trace(&self, visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)) {
        for slot in &self.slots {
            slot.trace_value_slots(visitor);
        }
    }
}

/// Scope token. Created only by [`crate::Interpreter::with_handle_scope`]; owns
/// the arena range `[base, len)`, which is truncated when the scope exits.
///
/// Kept private-constructible so user code cannot forge one and hand a
/// [`Scoped`] a range it does not own.
pub struct HandleScope {
    // Read only by the test-only `base()` accessor today; the scope wrappers
    // truncate from the `base` value they captured on entry, not the field.
    #[allow(dead_code)]
    base: usize,
}

impl HandleScope {
    /// Open a scope over the arena range starting at `base`.
    pub(crate) fn new(base: usize) -> Self {
        Self { base }
    }

    /// The arena length captured when this scope opened. Test-only today: the
    /// scope wrappers truncate from the `base` they captured directly, never
    /// through the token.
    #[allow(dead_code)]
    pub(crate) fn base(&self) -> usize {
        self.base
    }
}

/// A rooted, always-current handle into the [`HandleArena`].
///
/// `Copy` and cheap: it carries only the arena index, never a payload. The `'s`
/// lifetime pins it inside the [`HandleScope`] that created it, so it cannot
/// escape the `with_handle_scope` closure and can never dangle.
///
/// The lifetime makes escape a compile error. A `Scoped` cannot be returned out
/// of the scope closure — the closure result type cannot name the scope's
/// higher-ranked lifetime:
///
/// ```compile_fail
/// use otter_vm::{Interpreter, NativeCallInfo, NativeCtx};
///
/// let mut interp = Interpreter::new();
/// let mut ctx = NativeCtx::new_with_call_info_and_context(
///     &mut interp,
///     NativeCallInfo::default_call(),
///     None,
/// );
/// // Returning the handle from the closure fails to compile: `scope`'s result
/// // type is fixed and cannot capture the `&HandleScope` token's lifetime.
/// let escaped = ctx.scope(|ctx, s| ctx.scoped_object(s).unwrap());
/// let _ = escaped;
/// ```
///
/// Nor can it be stashed into a binding that outlives the scope:
///
/// ```compile_fail
/// use otter_vm::{Interpreter, NativeCallInfo, NativeCtx};
///
/// let mut interp = Interpreter::new();
/// let mut ctx = NativeCtx::new_with_call_info_and_context(
///     &mut interp,
///     NativeCallInfo::default_call(),
///     None,
/// );
/// let mut leaked = None;
/// ctx.scope(|ctx, s| {
///     // Storing the handle into a binding declared outside the scope would let
///     // it outlive the arena range it indexes — the borrow checker rejects it.
///     leaked = Some(ctx.scoped_object(s).unwrap());
/// });
/// let _ = leaked;
/// ```
#[derive(Debug, Clone, Copy)]
pub struct Scoped<'s> {
    idx: u32,
    _scope: PhantomData<&'s HandleScope>,
}

impl<'s> Scoped<'s> {
    /// Wrap an arena index as a handle pinned to scope `'s`.
    pub(crate) fn new(idx: u32) -> Self {
        Self {
            idx,
            _scope: PhantomData,
        }
    }

    /// The arena slot index this handle resolves through.
    pub(crate) fn index(self) -> u32 {
        self.idx
    }
}

/// Scope entry point and scoped allocation/access built on the arena.
///
/// These are the surface VM-internal callers use to build JS values without
/// hand-threading `value_roots`. They back the native-context surface
/// ([`crate::NativeCtx::scope`] and its `scoped_*` methods) and
/// interpreter-internal adoption.
impl Interpreter {
    /// Open a handle scope, run `f`, then truncate the arena back to the length
    /// it had on entry.
    ///
    /// Handles minted inside `f` (via `scoped_*`) borrow the `&HandleScope`
    /// token, not the interpreter, so allocating calls interleave freely with
    /// live handles. The `'s` lifetime pins every [`Scoped`] to the closure, so
    /// none can escape. Truncation happens before the closure's result is
    /// returned; an early `?` inside `f` propagates through the returned `R`,
    /// so the truncation still runs on the normal return path here.
    ///
    /// Interpreter-internal reflection paths (e.g.
    /// `Object.getOwnPropertyDescriptors`) drive this variant directly; the
    /// native-context [`crate::NativeCtx::scope`] wrapper is the surface feature
    /// authors use.
    pub(crate) fn with_handle_scope<R>(
        &mut self,
        f: impl FnOnce(&mut Interpreter, &HandleScope) -> R,
    ) -> R {
        let base = self.handle_arena.len();
        let scope = HandleScope::new(base);
        let roots_depth = self.push_scope_runtime_roots();
        let r = f(self, &scope);
        self.pop_scope_runtime_roots(roots_depth);
        self.handle_arena.truncate(base);
        r
    }

    /// Register this interpreter's full runtime root set (which includes the
    /// handle arena) as an extra-roots provider for a scope's lifetime, but only
    /// when no provider is already installed.
    ///
    /// A scoped write can box a wide number inside `object::set` (and the
    /// symbol/define variants); that box allocation traces only the receiver it
    /// is handed. On the dispatch path the loop already installs a provider over
    /// [`crate::runtime_state::RuntimeState::trace_roots`], so a collection
    /// driven by the box sees the arena and sibling handles stay live. On the
    /// host-side path — module init, timer/worker dispatch — no provider is
    /// registered, so that same collection would walk only the receiver and
    /// strand every sibling parked in the arena. Installing the provider for the
    /// scope closes that hole for *every* scoped op uniformly, without threading
    /// a root snapshot through each write.
    ///
    /// Gated on [`otter_gc::GcHeap::has_extra_roots`] so the dispatch hot path
    /// pays nothing: it already has a provider, so this is a no-op there. Returns
    /// the registration depth to unwind, or `None` when a provider was already
    /// present. Pair with [`Self::pop_scope_runtime_roots`].
    pub(crate) fn push_scope_runtime_roots(&mut self) -> Option<usize> {
        if self.gc_heap.has_extra_roots() {
            return None;
        }
        // `ExtraRoots::new` records a raw pointer to `self`; the registration
        // lives only until the paired pop below, and the interpreter outlives
        // it, so the closure's `&mut self` reborrow is sound (mirrors
        // `Interpreter::run_callable_sync`).
        let extra = otter_gc::ExtraRoots::new(self as &Interpreter);
        Some(self.gc_heap.push_extra_roots(extra))
    }

    /// Unwind the registration [`Self::push_scope_runtime_roots`] installed, if
    /// any.
    pub(crate) fn pop_scope_runtime_roots(&mut self, depth: Option<usize>) {
        if let Some(depth) = depth {
            self.gc_heap.pop_extra_roots_to(depth - 1);
        }
    }

    /// Root an incoming raw `Value` in the current scope and hand back a
    /// [`Scoped`] handle to it.
    pub(crate) fn scoped_value<'s>(&mut self, _scope: &'s HandleScope, value: Value) -> Scoped<'s> {
        let idx = self.handle_arena.push(value);
        Scoped::new(idx)
    }

    /// Allocate a string and park it in the current scope.
    ///
    /// `JsString::from_str` can relocate young objects; every prior handle in
    /// the arena is traced across that allocation, and the freshly built string
    /// is parked immediately, so nothing goes stale.
    pub(crate) fn scoped_string<'s>(
        &mut self,
        scope: &'s HandleScope,
        text: &str,
    ) -> Result<Scoped<'s>, VmError> {
        let string = JsString::from_str(text, &mut self.gc_heap)?;
        Ok(self.scoped_value(scope, Value::string(string)))
    }

    /// Build interned `JsString` values for `keys`, rooting each in a handle
    /// scope so an earlier key is never stranded by a later string allocation,
    /// and return them as plain `Value`s for immediate hand-off to an array
    /// allocator.
    ///
    /// Key-list reflection (`Object.keys`, `Object.getOwnPropertyNames`,
    /// for-in) allocates one key string at a time; each allocation can drive a
    /// full sweep that reclaims a previously built, still-unrooted key. Parking
    /// every key in the arena as it is built lets the collector keep them live,
    /// and each is read back out once building finishes. The returned `Value`s
    /// are handed straight to the caller's single array allocation — which
    /// traces the pending element vector itself — with no intervening
    /// allocation, so the post-scope reads are current.
    pub(crate) fn scoped_key_strings(&mut self, keys: &[String]) -> Result<Vec<Value>, VmError> {
        self.with_handle_scope(|interp, scope| {
            let mut handles = Vec::with_capacity(keys.len());
            for key in keys {
                handles.push(interp.scoped_string(scope, key)?);
            }
            Ok(handles
                .into_iter()
                .map(|handle| interp.escape_scoped(handle))
                .collect())
        })
    }

    /// Allocate an ordinary object with `%Object.prototype%` installed (the
    /// prototype an object-literal `{}` resolves to) and park it in the current
    /// scope. The allocation snapshots the runtime roots (including the arena),
    /// so prior handles survive any collection it drives.
    pub(crate) fn scoped_object<'s>(
        &mut self,
        scope: &'s HandleScope,
    ) -> Result<Scoped<'s>, VmError> {
        let object = self.alloc_runtime_rooted_object_with_roots(&[], &[])?;
        // Install the prototype only after the allocation: the object alloc can
        // drive a scavenge that relocates the realm prototype while it is still
        // young, so reading the handle beforehand could bake a stale offset
        // (mirrors `run_new_object_reg`). The realm-intrinsic table is always
        // traced, so a post-alloc read yields the relocated handle.
        if let Some(proto) = self.object_prototype_object_opt() {
            crate::object::set_prototype(object, &mut self.gc_heap, Some(proto));
        }
        Ok(self.scoped_value(scope, Value::object(object)))
    }

    /// Allocate a bare (null-prototype) object and park it in the current
    /// scope. Same rooting contract as [`Self::scoped_object`], without the
    /// prototype install.
    pub(crate) fn scoped_object_bare<'s>(
        &mut self,
        scope: &'s HandleScope,
    ) -> Result<Scoped<'s>, VmError> {
        let object = self.alloc_runtime_rooted_object_with_roots(&[], &[])?;
        Ok(self.scoped_value(scope, Value::object(object)))
    }

    /// Allocate an array whose `length` is `len` (elements start as holes) and
    /// park it in the current scope. The runtime-root snapshot keeps prior
    /// handles live across the allocation and the length reservation.
    pub(crate) fn scoped_array<'s>(
        &mut self,
        scope: &'s HandleScope,
        len: usize,
    ) -> Result<Scoped<'s>, VmError> {
        let roots = self.collect_runtime_roots();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
        };
        let array = crate::array::alloc_array_with_roots(&mut self.gc_heap, &mut external_visit)
            .map_err(VmError::from)?;
        // Park before growing so the handle survives any allocation the length
        // reservation drives; then resolve the (possibly relocated) handle from
        // the arena to set the length.
        let handle = self.scoped_value(scope, Value::array(array));
        if len > 0 {
            let array = self
                .handle_arena
                .get(handle.index())
                .as_array()
                .ok_or(VmError::TypeMismatch)?;
            crate::array::set_length(array, &mut self.gc_heap, len).map_err(VmError::from)?;
        }
        Ok(handle)
    }

    /// Park an `f64` number in the current scope. Numbers are NaN-boxed
    /// immediates, so this never allocates; it exists so number construction
    /// reads the same as every other scoped creation.
    pub(crate) fn scoped_number<'s>(&mut self, scope: &'s HandleScope, n: f64) -> Scoped<'s> {
        self.scoped_value(scope, Value::number_f64(n))
    }

    /// Park a boolean immediate in the current scope.
    pub(crate) fn scoped_boolean<'s>(&mut self, scope: &'s HandleScope, b: bool) -> Scoped<'s> {
        self.scoped_value(scope, Value::boolean(b))
    }

    /// Park the `undefined` immediate in the current scope.
    pub(crate) fn scoped_undefined<'s>(&mut self, scope: &'s HandleScope) -> Scoped<'s> {
        self.scoped_value(scope, Value::undefined())
    }

    /// Park the `null` immediate in the current scope.
    pub(crate) fn scoped_null<'s>(&mut self, scope: &'s HandleScope) -> Scoped<'s> {
        self.scoped_value(scope, Value::null())
    }

    /// Allocate a static builtin native function and park it in the current
    /// scope. Mirrors the object-builder `builtin_method` path: a builtin-tagged
    /// function backed by the static fast-call `call`. The allocation snapshots
    /// the runtime roots (including the arena), so prior handles survive any
    /// collection it drives; the fresh function is parked immediately.
    pub(crate) fn scoped_native_static<'s>(
        &mut self,
        scope: &'s HandleScope,
        name: &'static str,
        length: u8,
        call: crate::native_function::NativeFastFn,
    ) -> Result<Scoped<'s>, VmError> {
        let roots = self.collect_runtime_roots();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
        };
        let function = crate::native_function::NativeFunction::new_static_with_roots(
            &mut self.gc_heap,
            name,
            length,
            call,
            &mut external_visit,
        )?;
        Ok(self.scoped_value(scope, Value::native_function(function)))
    }

    /// Read property `key` from the object handle `obj`, resolving `obj`
    /// through the arena at call time, and park the result in the current
    /// scope. Absent properties read back as `undefined`.
    pub(crate) fn scoped_get<'s>(
        &mut self,
        scope: &'s HandleScope,
        obj: Scoped<'_>,
        key: &str,
    ) -> Result<Scoped<'s>, VmError> {
        let object = self
            .handle_arena
            .get(obj.index())
            .as_object()
            .ok_or(VmError::TypeMismatch)?;
        let value = crate::object::get(object, &self.gc_heap, key).unwrap_or_else(Value::undefined);
        Ok(self.scoped_value(scope, value))
    }

    /// Write `value` to property `key` on the object handle `obj`, resolving
    /// both handles through the arena at call time.
    ///
    /// `object::set` never reassigns the `JsObject` handle it is given; the only
    /// way the object relocates is a moving collection driven by the write's own
    /// allocation, and that collection rewrites the arena slot in place. The
    /// slot is therefore authoritative on return — parking the (now-stale) local
    /// back would clobber the collector's fix-up, so the write intentionally
    /// does not re-park.
    pub(crate) fn scoped_set(
        &mut self,
        _scope: &HandleScope,
        obj: Scoped<'_>,
        key: &str,
        value: Scoped<'_>,
    ) -> Result<(), VmError> {
        let mut object = self
            .handle_arena
            .get(obj.index())
            .as_object()
            .ok_or(VmError::TypeMismatch)?;
        let stored = self.handle_arena.get(value.index());
        crate::object::set(&mut object, &mut self.gc_heap, key, stored);
        Ok(())
    }

    /// Write `value` to the symbol-keyed property `key` on the object handle
    /// `obj`, resolving both handles through the arena at call time. Same
    /// authoritative-slot contract as [`Self::scoped_set`]. A rejected write
    /// (non-extensible object) surfaces as [`VmError::TypeMismatch`].
    pub(crate) fn scoped_set_symbol(
        &mut self,
        _scope: &HandleScope,
        obj: Scoped<'_>,
        key: crate::symbol::JsSymbol,
        value: Scoped<'_>,
    ) -> Result<(), VmError> {
        let object = self
            .handle_arena
            .get(obj.index())
            .as_object()
            .ok_or(VmError::TypeMismatch)?;
        let stored = self.handle_arena.get(value.index());
        if crate::object::set_symbol(object, &mut self.gc_heap, key, stored) {
            Ok(())
        } else {
            Err(VmError::TypeMismatch)
        }
    }

    /// Allocate an ordinary object whose prototype is the object held by the
    /// `proto` handle, and park it in the current scope. Same rooting contract
    /// as [`Self::scoped_object`]: the object is allocated first (the alloc may
    /// relocate the still-young prototype), then the prototype is read back
    /// through the arena and installed, so no stale offset can be baked in. A
    /// `proto` handle that does not hold an object installs a `null` prototype.
    pub(crate) fn scoped_object_with_proto<'s>(
        &mut self,
        scope: &'s HandleScope,
        proto: Scoped<'_>,
    ) -> Result<Scoped<'s>, VmError> {
        let object = self.alloc_runtime_rooted_object_with_roots(&[], &[])?;
        let handle = self.scoped_value(scope, Value::object(object));
        let proto_obj = self.handle_arena.get(proto.index()).as_object();
        let object = self
            .handle_arena
            .get(handle.index())
            .as_object()
            .ok_or(VmError::TypeMismatch)?;
        crate::object::set_prototype(object, &mut self.gc_heap, proto_obj);
        Ok(handle)
    }

    /// Define the symbol-keyed data property `key` on the object handle `obj`
    /// with explicit attribute `flags`. The symbol is carried in a scope handle
    /// (`key`) so it stays live across the earlier allocations that built the
    /// value being stored; all three handles resolve through the arena at call
    /// time. A `key` handle that does not hold a symbol, or a rejected define
    /// (non-extensible object), surfaces as [`VmError::TypeMismatch`].
    pub(crate) fn scoped_define_symbol(
        &mut self,
        _scope: &HandleScope,
        obj: Scoped<'_>,
        key: Scoped<'_>,
        value: Scoped<'_>,
        flags: crate::object::PropertyFlags,
    ) -> Result<(), VmError> {
        let object = self
            .handle_arena
            .get(obj.index())
            .as_object()
            .ok_or(VmError::TypeMismatch)?;
        let symbol = self
            .handle_arena
            .get(key.index())
            .as_symbol(&self.gc_heap)
            .ok_or(VmError::TypeMismatch)?;
        let stored = self.handle_arena.get(value.index());
        let descriptor = crate::object::PropertyDescriptor {
            kind: crate::object::DescriptorKind::Data { value: stored },
            flags,
        };
        if crate::object::define_own_symbol_property(object, &mut self.gc_heap, symbol, descriptor)
        {
            Ok(())
        } else {
            Err(VmError::TypeMismatch)
        }
    }

    /// Build a §6.2.5.4 FromPropertyDescriptor result object for `desc` and park
    /// it in the current scope.
    ///
    /// The descriptor's own `Value` fields (data value, accessor get/set) are
    /// parked *before* the result object is allocated, so the allocation and
    /// each subsequent field write cannot strand them; every write then resolves
    /// the result through the arena, so an intermediate collection can never
    /// leave a half-built descriptor. Field order matches the spec:
    /// `value`/`writable` (data) or `get`/`set` (accessor), then `enumerable`
    /// and `configurable`.
    pub(crate) fn scoped_descriptor_object<'s>(
        &mut self,
        scope: &'s HandleScope,
        desc: &crate::object::PropertyDescriptor,
    ) -> Result<Scoped<'s>, VmError> {
        use crate::object::DescriptorKind;
        let (value_h, get_h, set_h) = match &desc.kind {
            DescriptorKind::Data { value } => (Some(self.scoped_value(scope, *value)), None, None),
            DescriptorKind::Accessor { getter, setter } => (
                None,
                Some(self.scoped_value(scope, getter.unwrap_or_else(Value::undefined))),
                Some(self.scoped_value(scope, setter.unwrap_or_else(Value::undefined))),
            ),
        };
        let result = self.scoped_object(scope)?;
        match &desc.kind {
            DescriptorKind::Data { .. } => {
                self.scoped_set(scope, result, "value", value_h.expect("data value parked"))?;
                let writable = self.scoped_boolean(scope, desc.writable());
                self.scoped_set(scope, result, "writable", writable)?;
            }
            DescriptorKind::Accessor { .. } => {
                self.scoped_set(scope, result, "get", get_h.expect("accessor getter parked"))?;
                self.scoped_set(scope, result, "set", set_h.expect("accessor setter parked"))?;
            }
        }
        let enumerable = self.scoped_boolean(scope, desc.enumerable());
        self.scoped_set(scope, result, "enumerable", enumerable)?;
        let configurable = self.scoped_boolean(scope, desc.configurable());
        self.scoped_set(scope, result, "configurable", configurable)?;
        Ok(result)
    }

    /// Define data property `key` on the object handle `obj` with explicit
    /// attribute `flags`, resolving both handles through the arena at call
    /// time. A rejected define (non-extensible object, non-configurable
    /// redefinition) surfaces as [`VmError::TypeMismatch`].
    pub(crate) fn scoped_define_data(
        &mut self,
        _scope: &HandleScope,
        obj: Scoped<'_>,
        key: &str,
        value: Scoped<'_>,
        flags: crate::object::PropertyFlags,
    ) -> Result<(), VmError> {
        let object = self
            .handle_arena
            .get(obj.index())
            .as_object()
            .ok_or(VmError::TypeMismatch)?;
        let stored = self.handle_arena.get(value.index());
        let descriptor = crate::object::PropertyDescriptor {
            kind: crate::object::DescriptorKind::Data { value: stored },
            flags,
        };
        if crate::object::define_own_property(object, &mut self.gc_heap, key, descriptor) {
            Ok(())
        } else {
            Err(VmError::TypeMismatch)
        }
    }

    /// Store `value` at array index `index` on the array handle `arr`,
    /// resolving both handles through the arena at call time. The array shell
    /// keeps its identity across the store, so the collector-tracked arena slot
    /// stays current without a re-park.
    pub(crate) fn scoped_set_index(
        &mut self,
        _scope: &HandleScope,
        arr: Scoped<'_>,
        index: usize,
        value: Scoped<'_>,
    ) -> Result<(), VmError> {
        let array = self
            .handle_arena
            .get(arr.index())
            .as_array()
            .ok_or(VmError::TypeMismatch)?;
        let stored = self.handle_arena.get(value.index());
        let roots = self.collect_runtime_roots();
        let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            for &slot in &roots {
                visitor(slot);
            }
        };
        crate::array::set_with_roots(array, &mut self.gc_heap, index, stored, &mut external_visit)
            .map_err(VmError::from)
    }

    /// Current scope-handle arena length — the truncation base a fresh scope
    /// captures on entry. Used by the native-context [`crate::NativeCtx::scope`]
    /// wrapper, which opens and closes a scope without threading through
    /// [`Self::with_handle_scope`] (its closure hands back the interpreter, not
    /// the native context).
    pub(crate) fn handle_arena_len(&self) -> usize {
        self.handle_arena.len()
    }

    /// Truncate the scope-handle arena back to `base`, dropping every slot a
    /// scope opened. Paired with [`Self::handle_arena_len`] by the native-context
    /// scope wrapper.
    pub(crate) fn handle_arena_truncate(&mut self, base: usize) {
        self.handle_arena.truncate(base);
    }

    /// Read the current raw `Value` behind a scope handle for immediate
    /// hand-off across the scope boundary (returning to the VM, or storing into
    /// an already-rooted object). Valid until the next allocation.
    pub(crate) fn escape_scoped(&self, handle: Scoped<'_>) -> Value {
        self.handle_arena.get(handle.index())
    }
}

#[cfg(test)]
impl Interpreter {
    /// Current scope-handle arena length.
    fn handle_arena_len_for_test(&self) -> usize {
        self.handle_arena.len()
    }

    /// Force a minor collection while tracing the full runtime root set
    /// (including the handle arena), mirroring the host-side snapshot path so
    /// tests can drive a relocation with handles live.
    ///
    /// `pub(crate)` so the native-context test module can drive a scavenge from
    /// inside a `NativeCtx::scope` closure with handles live.
    pub(crate) fn collect_minor_tracing_runtime_roots(&mut self) {
        let roots = self.collect_runtime_roots();
        self.gc_heap.collect_minor_with_roots(&mut |visitor| {
            for &slot in &roots {
                visitor(slot);
            }
        });
    }

    /// Force a minor collection that roots *only* `receiver`, exactly as
    /// `object::set`'s internal box allocation does (it hands `compress` a
    /// visitor over the receiver alone). Any sibling that survives does so only
    /// through a registered extra-roots provider — the handle arena — which the
    /// host-side scope is responsible for installing. Used to prove the
    /// set-boxing hole is closed at the scope boundary.
    pub(crate) fn collect_minor_rooting_only_receiver(&mut self, receiver: Scoped<'_>) {
        let mut raw = self
            .handle_arena
            .get(receiver.index())
            .as_raw_gc()
            .expect("receiver is a heap cell");
        self.gc_heap.collect_minor_with_roots(&mut |visitor| {
            visitor(&mut raw as *mut RawGc);
        });
    }

    /// Force a full (mark-sweep) collection while tracing the full runtime root
    /// set. Old-space objects (e.g. string bodies) do not move, but anything
    /// the root walk cannot reach is swept — so surviving one proves the arena
    /// keeps a handle live.
    fn collect_full_tracing_runtime_roots(&mut self) {
        let roots = self.collect_runtime_roots();
        self.gc_heap.collect_full(&mut |visitor| {
            for &slot in &roots {
                visitor(slot);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn imm(i: i32) -> Value {
        Value::number_i32(i)
    }

    #[test]
    fn push_read_truncate() {
        let mut arena = HandleArena::new();
        assert_eq!(arena.len(), 0);

        let a = arena.push(imm(10));
        let b = arena.push(imm(20));
        let c = arena.push(imm(30));
        assert_eq!((a, b, c), (0, 1, 2));
        assert_eq!(arena.len(), 3);

        assert_eq!(arena.get(a).as_i32(), Some(10));
        assert_eq!(arena.get(c).as_i32(), Some(30));

        arena.set(b, imm(99));
        assert_eq!(arena.get(b).as_i32(), Some(99));

        arena.truncate(1);
        assert_eq!(arena.len(), 1);
        assert_eq!(arena.get(a).as_i32(), Some(10));
    }

    #[test]
    fn nested_truncate_leaves_outer_slots() {
        let mut arena = HandleArena::new();
        let outer = arena.push(imm(1));

        // An inner scope opens at the current length and only owns what it
        // pushes; truncating back to that base must not touch `outer`.
        let inner_base = arena.len();
        arena.push(imm(2));
        arena.push(imm(3));
        assert_eq!(arena.len(), 3);

        arena.truncate(inner_base);
        assert_eq!(arena.len(), 1);
        assert_eq!(arena.get(outer).as_i32(), Some(1));
    }

    #[test]
    fn scope_truncates_on_return() {
        let mut interp = Interpreter::new();
        let base = interp.handle_arena_len_for_test();
        let out = interp.with_handle_scope(|interp, s| {
            let v = interp.scoped_value(s, imm(7));
            interp.escape_scoped(v).as_i32()
        });
        assert_eq!(out, Some(7));
        // The scope range is gone once the wrapper returns.
        assert_eq!(interp.handle_arena_len_for_test(), base);
    }

    #[test]
    fn scoped_object_survives_and_moves_under_minor_gc() {
        let mut interp = Interpreter::new();
        let (moved, content) = interp.with_handle_scope(|interp, s| {
            // Objects are young-space, so a minor scavenge relocates a survivor
            // — the case that turns a raw held offset stale. Park a string
            // property so we can prove the whole object is intact post-move.
            let obj = interp.scoped_object(s).unwrap();
            let value = interp.scoped_string(s, "payload").unwrap();
            interp.scoped_set(s, obj, "k", value).unwrap();
            let before = interp
                .escape_scoped(obj)
                .as_raw_gc()
                .expect("object is a heap cell")
                .0;

            // Force minor collections until the survivor is evacuated to the
            // other semispace (its offset changes), proving the arena slot was
            // rewritten in place rather than left dangling. Cheney evacuation
            // moves a young survivor on the first flip; the bounded loop guards
            // against promotion having already parked it in old space.
            let mut after = before;
            let mut moved = false;
            for _ in 0..8 {
                // Churn young space so a collection has something to evacuate.
                let _ = interp.scoped_object(s).unwrap();
                interp.collect_minor_tracing_runtime_roots();
                after = interp
                    .escape_scoped(obj)
                    .as_raw_gc()
                    .expect("object still a heap cell after gc")
                    .0;
                if after != before {
                    moved = true;
                    break;
                }
            }
            assert!(
                moved,
                "scoped object never relocated across a minor GC (before={before}, after={after}); \
                 the move test did not exercise a relocation",
            );

            // Read the property back through the relocated handle: the arena
            // rewrote the object slot, and the object's own slots were fixed up
            // by the scavenge.
            let read_back = interp.scoped_get(s, obj, "k").unwrap();
            let content = interp
                .escape_scoped(read_back)
                .as_string(interp.gc_heap())
                .expect("property value still a string")
                .to_lossy_string(interp.gc_heap());
            (moved, content)
        });
        assert!(moved);
        assert_eq!(content, "payload");
    }

    #[test]
    fn scoped_string_survives_full_gc() {
        let mut interp = Interpreter::new();
        // String bodies live in old space (mark-sweep, non-moving), so the
        // relocation proof uses an object above. Here a full GC would sweep any
        // unreachable old-space body; surviving one proves the arena roots the
        // string. Only the arena keeps it live.
        let content = interp.with_handle_scope(|interp, s| {
            let a = interp.scoped_string(s, "hello handle scope").unwrap();
            interp.collect_full_tracing_runtime_roots();
            interp
                .escape_scoped(a)
                .as_string(interp.gc_heap())
                .expect("slot still holds a string after full gc")
                .to_lossy_string(interp.gc_heap())
        });
        assert_eq!(content, "hello handle scope");
    }

    #[test]
    fn inner_scope_truncation_leaves_outer_handle_valid() {
        let mut interp = Interpreter::new();
        let content = interp.with_handle_scope(|interp, outer| {
            let outer_str = interp.scoped_string(outer, "outer").unwrap();

            // An inner scope allocates and forces a collection, then exits and
            // truncates its own range. The outer handle must still resolve.
            interp.with_handle_scope(|interp, inner| {
                let _tmp = interp.scoped_string(inner, "inner-temp").unwrap();
                interp.collect_minor_tracing_runtime_roots();
            });

            interp
                .escape_scoped(outer_str)
                .as_string(interp.gc_heap())
                .expect("outer handle valid after inner scope closed")
                .to_lossy_string(interp.gc_heap())
        });
        assert_eq!(content, "outer");
    }

    #[test]
    fn scoped_wide_number_write_keeps_sibling_handle_live() {
        // The set-boxing hole: `object::set` boxes a wide double into a
        // `HeapNumber`, and that box allocation traces only the receiver it is
        // handed — never the runtime root snapshot. On the host-side path (no
        // dispatch provider) a collection the box drives would strand every
        // sibling parked in the arena. `with_handle_scope` closes the hole by
        // registering the runtime-root provider (which traces the arena) for the
        // scope, so any such collection keeps siblings live.
        let mut interp = Interpreter::new();
        // No dispatch loop is running, so the scope must install the provider
        // itself — the exact condition the fix targets.
        assert!(
            !interp.gc_heap.has_extra_roots(),
            "test must start on the host-side path (no extra-roots provider)",
        );

        let (moved, sibling_content, target_number) = interp.with_handle_scope(|interp, s| {
            // Park a sibling with a distinctive string property, then the write
            // target. Only the arena keeps the sibling reachable.
            let sibling = interp.scoped_object(s).unwrap();
            let marker = interp.scoped_string(s, "sibling-payload").unwrap();
            interp.scoped_set(s, sibling, "k", marker).unwrap();
            let target = interp.scoped_object(s).unwrap();

            // Exercise the real boxing path: store a wide double through the
            // scoped write. This is the allocation whose internal collection
            // roots only `target`.
            let wide = interp.scoped_number(s, 12345.6789);
            interp.scoped_set(s, target, "n", wide).unwrap();

            let before = interp
                .escape_scoped(sibling)
                .as_raw_gc()
                .expect("sibling is a heap cell")
                .0;

            // Model `object::set`'s box allocation precisely: force a minor
            // collection that roots ONLY the receiver, exactly as `compress`
            // does. The sibling survives solely through the scope-registered
            // runtime-root provider (the arena). Churn young space so the
            // scavenge has a survivor to relocate.
            let mut after = before;
            let mut moved = false;
            for _ in 0..8 {
                let _ = interp.scoped_object(s).unwrap();
                interp.collect_minor_rooting_only_receiver(target);
                after = interp
                    .escape_scoped(sibling)
                    .as_raw_gc()
                    .expect("sibling still a heap cell after gc")
                    .0;
                if after != before {
                    moved = true;
                    break;
                }
            }
            assert!(
                moved,
                "sibling never relocated (before={before}, after={after}); the receiver-only \
                 collection did not exercise a move",
            );

            // Read the sibling's property back through its (relocated) handle:
            // intact only if the arena slot was traced and rewritten.
            let read_back = interp.scoped_get(s, sibling, "k").unwrap();
            let sibling_content = interp
                .escape_scoped(read_back)
                .as_string(interp.gc_heap())
                .expect("sibling property value still a string")
                .to_lossy_string(interp.gc_heap());
            let target_number = interp.scoped_get(s, target, "n").unwrap();
            let target_number = interp.escape_scoped(target_number).as_f64();
            (moved, sibling_content, target_number)
        });

        assert!(moved);
        assert_eq!(sibling_content, "sibling-payload");
        assert_eq!(target_number, Some(12345.6789));
    }

    #[test]
    fn stress_scoped_creation_with_interleaved_scavenges() {
        let mut interp = Interpreter::new();
        interp.with_handle_scope(|interp, s| {
            for i in 0..1000 {
                interp.with_handle_scope(|interp, inner| {
                    let text = format!("value-{i}");
                    let str_handle = interp.scoped_string(inner, &text).unwrap();
                    let obj = interp.scoped_object(inner).unwrap();
                    interp.scoped_set(inner, obj, "k", str_handle).unwrap();

                    // Force a scavenge with the handles live, then verify both
                    // the string content and the property read survived the
                    // relocation.
                    interp.collect_minor_tracing_runtime_roots();

                    let read_back = interp.scoped_get(inner, obj, "k").unwrap();
                    let content = interp
                        .escape_scoped(read_back)
                        .as_string(interp.gc_heap())
                        .expect("property value still a string")
                        .to_lossy_string(interp.gc_heap());
                    assert_eq!(content, text, "iteration {i} lost its string across gc");
                });
            }
            // The outer scope only holds the single opening frame after all
            // inner scopes truncated.
            assert_eq!(interp.handle_arena_len_for_test(), s.base());
        });
    }
}
