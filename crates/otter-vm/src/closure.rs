//! GC body for closure values.
//!
//! A closure carries:
//!
//! - the bytecode function id it executes,
//! - the one context it was created over ([`crate::context`]), from which
//!   every outer binding its body reaches is a fixed hop,
//! - an optional bound `this` (arrow closures capture their receiver
//!   lexically; non-arrow closures take `this` from the call site),
//! - an optional bound `new.target` for arrow closures.
//!
//! # Contents
//!
//! - [`ClosureCallHeader`] — stable machine-facing call ABI prefix.
//! - [`ClosureCallState`] — allocation-neutral VM call metadata.
//! - [`JsClosureBody`] — GC body holding the ABI prefix, canonical
//!   bound values, and per-instance property state.
//! - [`JsClosure`] — 8-byte handle plus cached function id.
//! - [`alloc_closure`] / [`alloc_closure_with_roots`] — allocators.
//! - [`JS_CLOSURE_BODY_TYPE_TAG`] — reserved
//!   [`otter_gc::Traceable::TYPE_TAG`].
//!
//! # Invariants
//!
//! - The machine-facing prefix is `#[repr(C)]`: native linkage may read
//!   [`ClosureCallHeader`], bound values and the constructor header only.
//! - [`ClosureCallHeader::context`] is a full 8-byte `Value` word holding a
//!   context or `undefined`, fixed at creation. Generated code loads it with
//!   one instruction and follows it without cage-base arithmetic.
//! - Canonical `Value` fields are always traced, in the pending body too, so
//!   an allocation-triggered collection rewrites the context and bound values
//!   before they are copied into the cell. Presence flags distinguish `None`
//!   from `Some(undefined)` while [`JsClosure`] keeps the ergonomic
//!   `Option<Value>` API.
//! - Closures are allocated in old space and never move; a bound
//!   `new.target` requires the call-setup runtime stub.
//!
//! # See also
//!
//! - [`crate::native_abi::NativeFrame`] — fixed-width native activation ABI.
//! - [`crate::jit::JitCompileSnapshot`] — publishes the closure byte offsets
//!   to native backends.
//!
//! # Spec
//!
//! - ECMA-262 §10.2.3 OrdinaryFunctionCreate — `[[Environment]]`.
//! - ECMA-262 §13.3.6 — `[[Call]]` for ordinary functions / closures.
//! - ECMA-262 §10.2.1.1 — `[[ThisMode]]` for arrow functions.

use crate::Value;
use crate::object::JsObject;
use otter_gc::GcHeap;
use otter_gc::OutOfMemory;
use otter_gc::heap::RootSlotVisitor;
use otter_gc::raw::{RawGc, SlotVisitor};

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`JsClosureBody`].
pub const JS_CLOSURE_BODY_TYPE_TAG: u8 = 0x23;

/// [`ClosureCallHeader::flags`] bit: `bound_this` is semantically present.
pub const CLOSURE_CALL_FLAG_BOUND_THIS: u32 = 1 << 0;
/// [`ClosureCallHeader::flags`] bit: `bound_new_target` is semantically present.
pub const CLOSURE_CALL_FLAG_BOUND_NEW_TARGET: u32 = 1 << 1;
/// Flags whose semantics require the call-setup runtime stub.
///
/// Native linkage handles lexical `this` and the context inline. A lexical
/// `new.target` routes through setup before control returns to the compiled
/// callee in the same native activation.
pub const CLOSURE_CALL_RUNTIME_SETUP_FLAGS: u32 = CLOSURE_CALL_FLAG_BOUND_NEW_TARGET;

/// Stable machine-facing closure call metadata.
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClosureCallHeader {
    /// Index into [`otter_bytecode::BytecodeModule::functions`].
    pub function_id: u32,
    /// Presence and call-setup routing flags.
    pub flags: u32,
    /// The context this closure was created over, or `undefined`.
    pub context: Value,
}

/// Allocation-neutral closure state consumed by call preparation.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ClosureCallState {
    pub(crate) bound_this: Option<Value>,
    pub(crate) bound_new_target: Option<Value>,
}

impl ClosureCallHeader {
    fn new(function_id: u32, context: Value, bound_this: bool, bound_new_target: bool) -> Self {
        let mut flags = 0;
        if bound_this {
            flags |= CLOSURE_CALL_FLAG_BOUND_THIS;
        }
        if bound_new_target {
            flags |= CLOSURE_CALL_FLAG_BOUND_NEW_TARGET;
        }
        Self {
            function_id,
            flags,
            context,
        }
    }

    /// Whether every bit in `flag` is present.
    #[inline]
    #[must_use]
    pub const fn has_flag(self, flag: u32) -> bool {
        self.flags & flag == flag
    }

    /// Whether native linkage must run the call-setup runtime stub.
    ///
    /// `false` means all closure call state can be installed inline. `true`
    /// still remains in the current compiled activation: the setup stub
    /// establishes the complex state, then dispatch resumes in compiled code.
    #[inline]
    #[must_use]
    pub const fn requires_runtime_setup(self) -> bool {
        self.flags & CLOSURE_CALL_RUNTIME_SETUP_FLAGS != 0
    }
}

/// GC body backing every closure value.
///
/// The prefix through `construct` is part of the stable call/allocation ABI.
/// Everything after it is a traced implementation detail.
#[repr(C, align(8))]
#[derive(Debug)]
pub struct JsClosureBody {
    /// Fixed-layout metadata read by native linkage.
    pub call_header: ClosureCallHeader,
    /// Canonical traced lexical `this`; consult the header flag for presence.
    pub bound_this: Value,
    /// Canonical traced lexical `new.target`; consult the header flag for presence.
    pub bound_new_target: Value,
    /// Canonical property and weak constructor state in the fixed prefix.
    pub(crate) construct: crate::closure_construct::ClosureConstructHeader,
    /// Per-instance deletion of the intrinsic `name` metadata property
    /// (`delete f.name`). Sibling closures of the same template keep
    /// their own copies, so the marker cannot live in a table keyed by
    /// the bytecode function id.
    pub name_deleted: bool,
    /// As [`Self::name_deleted`] for `length`.
    pub length_deleted: bool,
    /// §10.1.4 `[[Extensible]]` for this closure instance. Sibling
    /// closures of the same bytecode template are distinct function
    /// objects, so `Object.preventExtensions(f)` must not seal them.
    pub non_extensible: bool,
    /// §10.1.2 `[[Prototype]]` override installed by
    /// `Object.setPrototypeOf` / `f.__proto__ = p`. `None` means the
    /// closure still walks the realm's `%Function.prototype%`; a
    /// stored `Value::null()` is an explicit null prototype.
    pub proto_override: Option<Value>,
    /// Named-lookup summary read by generated property guards: the
    /// `CLOSURE_LOOKUP_*` bits. Exactly [`CLOSURE_LOOKUP_ORDINARY`] means a
    /// name the closure does not own virtually resolves on the active realm's
    /// `%Function.prototype%`.
    pub(crate) named_lookup: u8,
}

/// [`JsClosureBody::named_lookup`] bit: the function kind's default
/// `[[Prototype]]` is `%Function.prototype%` (not a generator or async kind).
pub const CLOSURE_LOOKUP_ORDINARY: u8 = 1 << 0;
/// Generated proof that a closure's named lookup is ordinary: its
/// [`JsClosureBody::named_lookup`] byte reads exactly
/// [`CLOSURE_LOOKUP_ORDINARY`].
#[must_use]
pub(crate) fn ordinary_named_lookup_guard() -> crate::jit::JitBodyGuard {
    crate::jit::JitBodyGuard {
        byte: CLOSURE_NAMED_LOOKUP_BYTE,
        width: crate::jit::JitGuardWidth::Byte,
        expect: u32::from(CLOSURE_LOOKUP_ORDINARY),
    }
}

/// [`JsClosureBody::named_lookup`] bit: an own-property bag exists.
pub const CLOSURE_LOOKUP_OWN_PROPS: u8 = 1 << 1;
/// [`JsClosureBody::named_lookup`] bit: a `[[Prototype]]` override is installed.
pub const CLOSURE_LOOKUP_PROTO_OVERRIDE: u8 = 1 << 2;

impl otter_gc::SafeTraceable for JsClosureBody {
    const TYPE_TAG: u8 = JS_CLOSURE_BODY_TYPE_TAG;

    fn trace_slots_safe(&mut self, visitor: &mut SlotVisitor<'_>) {
        self.trace_fixed_fields(visitor);
    }
}

/// Byte offset of `function_id` inside [`ClosureCallHeader`].
pub const CLOSURE_CALL_HEADER_FUNCTION_ID_OFFSET: usize =
    std::mem::offset_of!(ClosureCallHeader, function_id);
/// Byte offset of `flags` inside [`ClosureCallHeader`].
pub const CLOSURE_CALL_HEADER_FLAGS_OFFSET: usize = std::mem::offset_of!(ClosureCallHeader, flags);
/// Byte offset of `context` inside [`ClosureCallHeader`].
pub const CLOSURE_CALL_HEADER_CONTEXT_OFFSET: usize =
    std::mem::offset_of!(ClosureCallHeader, context);

/// Byte offset of the nested call header in [`JsClosureBody`]'s payload.
pub const CLOSURE_BODY_CALL_HEADER_OFFSET: usize = std::mem::offset_of!(JsClosureBody, call_header);
/// Byte offset of the nested function id in [`JsClosureBody`]'s payload.
pub const CLOSURE_BODY_FUNCTION_ID_OFFSET: usize =
    CLOSURE_BODY_CALL_HEADER_OFFSET + CLOSURE_CALL_HEADER_FUNCTION_ID_OFFSET;
/// Byte offset of the nested call flags in [`JsClosureBody`]'s payload.
pub const CLOSURE_BODY_CALL_FLAGS_OFFSET: usize =
    CLOSURE_BODY_CALL_HEADER_OFFSET + CLOSURE_CALL_HEADER_FLAGS_OFFSET;
/// Byte offset of the nested context word in [`JsClosureBody`]'s payload.
pub const CLOSURE_BODY_CONTEXT_OFFSET: usize =
    CLOSURE_BODY_CALL_HEADER_OFFSET + CLOSURE_CALL_HEADER_CONTEXT_OFFSET;
/// Byte offset of canonical `bound_this` in [`JsClosureBody`]'s payload.
pub const CLOSURE_BODY_BOUND_THIS_OFFSET: usize = std::mem::offset_of!(JsClosureBody, bound_this);
/// Byte offset of canonical `bound_new_target` in [`JsClosureBody`]'s payload.
pub const CLOSURE_BODY_BOUND_NEW_TARGET_OFFSET: usize =
    std::mem::offset_of!(JsClosureBody, bound_new_target);

/// Byte offset of [`JsClosureBody::named_lookup`] in the payload.
pub const CLOSURE_BODY_NAMED_LOOKUP_OFFSET: usize =
    std::mem::offset_of!(JsClosureBody, named_lookup);
/// Byte offset of [`JsClosureBody::named_lookup`] from the cell header, as
/// generated code addresses it.
pub const CLOSURE_NAMED_LOOKUP_BYTE: u32 =
    (otter_gc::header::HEADER_SIZE + CLOSURE_BODY_NAMED_LOOKUP_OFFSET) as u32;

/// Byte offset of canonical constructor own_props state.
pub const CLOSURE_BODY_OWN_PROPS_OFFSET: usize = std::mem::offset_of!(JsClosureBody, construct)
    + std::mem::offset_of!(crate::closure_construct::ClosureConstructHeader, own_props);
/// Byte offset of canonical constructor prototype_shape state.
pub const CLOSURE_BODY_PROTOTYPE_SHAPE_OFFSET: usize =
    std::mem::offset_of!(JsClosureBody, construct)
        + std::mem::offset_of!(
            crate::closure_construct::ClosureConstructHeader,
            prototype_shape
        );
/// Byte offset of canonical constructor prototype_slot state.
pub const CLOSURE_BODY_PROTOTYPE_SLOT_OFFSET: usize =
    std::mem::offset_of!(JsClosureBody, construct)
        + std::mem::offset_of!(
            crate::closure_construct::ClosureConstructHeader,
            prototype_slot
        );
/// Byte offset of canonical constructor learned_instance_fields state.
pub const CLOSURE_BODY_LEARNED_INSTANCE_FIELDS_OFFSET: usize =
    std::mem::offset_of!(JsClosureBody, construct)
        + std::mem::offset_of!(
            crate::closure_construct::ClosureConstructHeader,
            learned_instance_fields
        );
/// Byte offset of canonical constructor last_instance state.
pub const CLOSURE_BODY_LAST_INSTANCE_OFFSET: usize = std::mem::offset_of!(JsClosureBody, construct)
    + std::mem::offset_of!(
        crate::closure_construct::ClosureConstructHeader,
        last_instance
    );

const _: [(); 16] = [(); std::mem::size_of::<ClosureCallHeader>()];
const _: [(); 8] = [(); std::mem::align_of::<ClosureCallHeader>()];
const _: [(); 0] = [(); CLOSURE_CALL_HEADER_FUNCTION_ID_OFFSET];
const _: [(); 4] = [(); CLOSURE_CALL_HEADER_FLAGS_OFFSET];
const _: [(); 8] = [(); CLOSURE_CALL_HEADER_CONTEXT_OFFSET];
const _: [(); 0] = [(); CLOSURE_BODY_CALL_HEADER_OFFSET];
const _: [(); 16] = [(); CLOSURE_BODY_BOUND_THIS_OFFSET];
const _: [(); 24] = [(); CLOSURE_BODY_BOUND_NEW_TARGET_OFFSET];
const _: [(); 32] = [(); CLOSURE_BODY_OWN_PROPS_OFFSET];

impl JsClosureBody {
    pub(crate) fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        visitor(self.call_header.function_id);
        crate::code_liveness::visit_value(&self.call_header.context, visitor);
        crate::code_liveness::visit_value(&self.bound_this, visitor);
        crate::code_liveness::visit_value(&self.bound_new_target, visitor);
        if let Some(value) = &self.proto_override {
            crate::code_liveness::visit_value(value, visitor);
        }
    }

    fn new(
        function_id: u32,
        context: Value,
        bound_this: Option<Value>,
        bound_new_target: Option<Value>,
    ) -> Self {
        debug_assert!(context.is_undefined() || context.as_context().is_some());
        let call_header = ClosureCallHeader::new(
            function_id,
            context,
            bound_this.is_some(),
            bound_new_target.is_some(),
        );
        Self {
            call_header,
            bound_this: bound_this.unwrap_or_else(Value::undefined),
            bound_new_target: bound_new_target.unwrap_or_else(Value::undefined),
            construct: crate::closure_construct::ClosureConstructHeader::default(),
            name_deleted: false,
            length_deleted: false,
            non_extensible: false,
            proto_override: None,
            named_lookup: 0,
        }
    }

    fn trace_fixed_fields(&mut self, visitor: &mut SlotVisitor<'_>) {
        use crate::pelt::PeltField as _;
        self.construct.last_instance.set(JsObject::null());
        self.call_header.context.pelt_trace(visitor);
        self.bound_this.pelt_trace(visitor);
        self.bound_new_target.pelt_trace(visitor);
        self.construct.own_props.pelt_trace(visitor);
        self.proto_override.pelt_trace(visitor);
    }

    #[inline]
    pub(crate) fn bound_this_option(&self) -> Option<Value> {
        self.call_header
            .has_flag(CLOSURE_CALL_FLAG_BOUND_THIS)
            .then_some(self.bound_this)
    }

    #[inline]
    pub(crate) fn bound_new_target_option(&self) -> Option<Value> {
        self.call_header
            .has_flag(CLOSURE_CALL_FLAG_BOUND_NEW_TARGET)
            .then_some(self.bound_new_target)
    }

    fn call_state(&self) -> ClosureCallState {
        ClosureCallState {
            bound_this: self.bound_this_option(),
            bound_new_target: self.bound_new_target_option(),
        }
    }
}

/// 4-byte compressed `Gc<JsClosureBody>` handle to the underlying
/// body cell.
pub type JsClosureHandle = otter_gc::Gc<JsClosureBody>;

/// 8-byte `Copy` closure value: 4-byte GC handle to the body plus
/// a 4-byte cached `function_id` so the call path can dispatch
/// without a heap touch. Identity (`===`) is handle-offset equality.
///
/// Matches V8 / JSC `JSFunction` cell-with-cached-code-entry layout.
/// Packs into [`crate::Value`] under `TAG_PTR_FUNCTION`.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct JsClosure {
    /// GC handle to the body cell. Field is `pub` so call-site
    /// pattern matches can bind it alongside the cached function id;
    /// mutation should still happen through dedicated helpers.
    pub handle: JsClosureHandle,
    /// Function-id cache. Mirrors [`ClosureCallHeader::function_id`];
    /// kept on the wrapper so the call path stays heap-free.
    pub cached_function_id: u32,
}

impl JsClosure {
    /// Per-instance deleted-metadata flag for `name`/`length`.
    pub(crate) fn metadata_deleted(self, heap: &otter_gc::GcHeap, key: &str) -> bool {
        heap.read_payload(self.handle(), |body| match key {
            "name" => body.name_deleted,
            "length" => body.length_deleted,
            _ => false,
        })
    }

    /// Record (or clear) the per-instance deleted-metadata flag.
    pub(crate) fn set_metadata_deleted(
        self,
        heap: &mut otter_gc::GcHeap,
        key: &str,
        deleted: bool,
    ) {
        heap.with_payload(self.handle(), |body| match key {
            "name" => body.name_deleted = deleted,
            "length" => body.length_deleted = deleted,
            _ => {}
        });
    }

    /// Construct from a raw handle + the function id stored inside
    /// it. Mirrors `from_handle` constructors on the other GC
    /// wrappers; callers that already hold both fields skip the
    /// `heap.read_payload` re-read.
    #[must_use]
    pub fn from_parts(handle: JsClosureHandle, function_id: u32) -> Self {
        Self {
            handle,
            cached_function_id: function_id,
        }
    }

    /// Underlying GC handle.
    #[must_use]
    pub fn handle(self) -> JsClosureHandle {
        self.handle
    }

    /// Underlying type-erased GC pointer; used by the tagged-value
    /// packer.
    #[must_use]
    pub fn raw(self) -> otter_gc::raw::RawGc {
        self.handle.raw()
    }

    /// Bytecode function id. Cached on the wrapper for heap-free
    /// hot-path access.
    #[must_use]
    pub fn function_id(self) -> u32 {
        self.cached_function_id
    }

    /// Copy the stable machine-facing call header.
    #[must_use]
    pub fn call_header(self, heap: &GcHeap) -> ClosureCallHeader {
        heap.read_payload(self.handle, |body| body.call_header)
    }

    /// Copy the bound values in one payload read.
    #[must_use]
    pub(crate) fn call_state(self, heap: &GcHeap) -> ClosureCallState {
        heap.read_payload(self.handle, JsClosureBody::call_state)
    }

    /// Whether native linkage must use the call-setup runtime stub before
    /// entering this closure's compiled body.
    #[must_use]
    pub fn requires_runtime_setup(self, heap: &GcHeap) -> bool {
        self.call_header(heap).requires_runtime_setup()
    }

    /// `Some(this)` for arrow closures, `None` otherwise. Reads the
    /// body once.
    #[must_use]
    pub fn bound_this(self, heap: &GcHeap) -> Option<Value> {
        heap.read_payload(self.handle, JsClosureBody::bound_this_option)
    }

    /// Lexical `new.target` captured for arrow closures.
    #[must_use]
    pub fn bound_new_target(self, heap: &GcHeap) -> Option<Value> {
        heap.read_payload(self.handle, JsClosureBody::bound_new_target_option)
    }

    /// The context this closure was created over, or `undefined`.
    #[must_use]
    pub fn context(self, heap: &GcHeap) -> Value {
        heap.read_payload(self.handle, |body| body.call_header.context)
    }

    /// Record that this closure's function kind defaults its
    /// `[[Prototype]]` to `%Function.prototype%`. Set once at creation.
    pub(crate) fn mark_ordinary_lookup(self, heap: &mut GcHeap) {
        heap.with_payload(self.handle, |body| {
            body.named_lookup |= CLOSURE_LOOKUP_ORDINARY;
        });
    }

    /// This closure instance's own-property bag, if it has been
    /// materialized. The zero compressed handle denotes an absent bag.
    #[must_use]
    pub fn own_props(self, heap: &GcHeap) -> Option<JsObject> {
        heap.read_payload(self.handle, |body| {
            let bag = body.construct.own_props;
            (!bag.is_null()).then_some(bag)
        })
    }

    /// Install the per-instance own-property bag. Records the
    /// closure→bag edge with the GC write barrier (the body lives in
    /// old space; the bag may be younger).
    pub fn set_own_props(self, heap: &mut GcHeap, bag: JsObject) {
        heap.with_payload(self.handle, |body| {
            body.construct.own_props = bag;
            body.named_lookup |= CLOSURE_LOOKUP_OWN_PROPS;
        });
        heap.write_barrier(self.handle, bag);
    }

    /// §10.1.3 `[[IsExtensible]]` for this closure instance.
    #[must_use]
    pub fn is_extensible(self, heap: &GcHeap) -> bool {
        !heap.read_payload(self.handle, |body| body.non_extensible)
    }

    /// §10.1.4 `[[PreventExtensions]]` for this closure instance.
    pub fn prevent_extensions(self, heap: &mut GcHeap) {
        heap.with_payload(self.handle, |body| body.non_extensible = true);
    }

    /// The `[[Prototype]]` override installed on this closure instance,
    /// if any. See [`JsClosureBody::proto_override`].
    #[must_use]
    pub fn proto_override(self, heap: &GcHeap) -> Option<Value> {
        heap.read_payload(self.handle, |body| body.proto_override)
    }

    /// Drop the per-instance `[[Prototype]]` override, restoring the
    /// intrinsic `%Function.prototype%` walk.
    pub fn clear_proto_override(self, heap: &mut GcHeap) {
        heap.with_payload(self.handle, |body| {
            body.proto_override = None;
            body.named_lookup &= !CLOSURE_LOOKUP_PROTO_OVERRIDE;
        });
    }

    /// Install the per-instance `[[Prototype]]` override. The body lives
    /// in old space and the new prototype may be younger, so every heap
    /// slot the value carries is recorded in the remembered set.
    pub fn set_proto_override(self, heap: &mut GcHeap, proto: Value) {
        use crate::pelt::PeltField as _;

        heap.with_payload(self.handle, |body| {
            body.proto_override = Some(proto);
            body.named_lookup |= CLOSURE_LOOKUP_PROTO_OVERRIDE;
        });
        let mut child = proto;
        let handle = self.handle;
        let mut visit = |slot: *mut RawGc| {
            // SAFETY: `pelt_trace` hands out pointers into the local copy
            // of the value; the slot is read to record its edge only.
            let raw = unsafe { *slot };
            heap.record_write_edge(handle, raw);
        };
        child.pelt_trace(&mut visit);
    }

    /// Identity comparison via GC handle offset.
    #[must_use]
    pub fn ptr_eq(self, other: Self) -> bool {
        self.handle == other.handle
    }

    /// Backing-pointer for cycle / identity sets.
    #[must_use]
    pub fn identity_addr(self) -> *const () {
        self.handle.offset() as usize as *const ()
    }

    /// Visit the embedded GC handle slot during root tracing.
    pub fn trace_value_slots(&self, visitor: &mut SlotVisitor<'_>) {
        let p = &self.handle as *const JsClosureHandle as *mut RawGc;
        visitor(p);
    }
}

/// Allocate one old-space closure over `context` (a context value or
/// `undefined`).
///
/// # Errors
/// Surfaces [`OutOfMemory`] verbatim.
pub fn alloc_closure(
    heap: &mut GcHeap,
    function_id: u32,
    context: Value,
    bound_this: Option<Value>,
    bound_new_target: Option<Value>,
) -> Result<JsClosure, OutOfMemory> {
    alloc_closure_with_roots(
        heap,
        function_id,
        context,
        bound_this,
        bound_new_target,
        &mut |_| {},
    )
}

/// Allocate a closure body while exposing caller-owned roots across
/// any allocation-triggered collection.
///
/// The context and bound values ride in the pending body, which the
/// allocator traces, so a collection rewrites them before the copy into the
/// cell. `external_visit` covers any other young value the caller holds in a
/// Rust local across this call (per the [`GcHeap::alloc_with_roots`]
/// contract).
///
/// # Errors
///
/// Surfaces [`OutOfMemory`] verbatim.
pub fn alloc_closure_with_roots(
    heap: &mut GcHeap,
    function_id: u32,
    context: Value,
    bound_this: Option<Value>,
    bound_new_target: Option<Value>,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsClosure, OutOfMemory> {
    let body = JsClosureBody::new(function_id, context, bound_this, bound_new_target);
    let handle = heap.alloc_old_with_roots(body, external_visit)?;
    Ok(JsClosure::from_parts(handle, function_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Allocate a closure while a capped heap forces a full collection, with
    /// its context and bound values young: the pending body must carry them.
    fn alloc_closure_across_forced_full_gc(with_external_roots: bool) {
        const HEAP_CAP: u64 = 4 * 1024;

        let mut heap = GcHeap::with_max_heap_bytes(HEAP_CAP).expect("heap");
        let this_object = crate::object::alloc_object_with_roots(&mut heap, &mut |_| {})
            .expect("young bound this");
        let mut bound_this = Value::object(this_object);
        let context = {
            let slot: *mut Value = &mut bound_this;
            let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
                // SAFETY: the local outlives the allocation.
                unsafe { (*slot).trace_value_slot_mut(visitor) };
            };
            crate::context::alloc_context_with_roots(
                &mut heap,
                crate::context::ContextShape {
                    scope_function_id: 71,
                    scope_index: 0,
                    slot_count: 1,
                },
                Value::undefined(),
                |_| false,
                &mut roots,
            )
            .expect("young context")
        };
        assert!(crate::context::write_slot(
            &mut heap,
            context,
            0,
            Value::number_i32(404)
        ));
        let mut context_value = Value::context(context);
        let mut external = if with_external_roots {
            let this_slot: *mut Value = &mut bound_this;
            let context_slot: *mut Value = &mut context_value;
            let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
                // SAFETY: the locals outlive the allocation.
                unsafe {
                    (*this_slot).trace_value_slot_mut(visitor);
                    (*context_slot).trace_value_slot_mut(visitor);
                }
            };
            Some(Value::object(
                crate::object::alloc_object_with_roots(&mut heap, &mut roots)
                    .expect("young external root"),
            ))
        } else {
            None
        };
        let original = [bound_this.to_bits(), context_value.to_bits()];

        // Fill the capped heap without crossing it; the closure allocation
        // must overshoot, collect, and retry with rewritten pending fields.
        let fill_roots = |heap: &mut GcHeap,
                          bound_this: &mut Value,
                          context_value: &mut Value,
                          external: &mut Option<Value>| {
            let this_slot: *mut Value = bound_this;
            let context_slot: *mut Value = context_value;
            let external_slot: *mut Option<Value> = external;
            let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
                use crate::pelt::PeltField as _;
                // SAFETY: the locals outlive the allocation.
                unsafe {
                    (*this_slot).trace_value_slot_mut(visitor);
                    (*context_slot).trace_value_slot_mut(visitor);
                    (*external_slot).pelt_trace(visitor);
                }
            };
            heap.alloc_old_with_roots(
                crate::upvalue::UpvalueCellBody {
                    value: Value::undefined(),
                },
                &mut roots,
            )
            .expect("filler");
        };
        let before_filler = heap.tracked_bytes();
        fill_roots(
            &mut heap,
            &mut bound_this,
            &mut context_value,
            &mut external,
        );
        let filler_bytes = heap.tracked_bytes() - before_filler;
        while heap.tracked_bytes().saturating_add(filler_bytes) <= HEAP_CAP {
            fill_roots(
                &mut heap,
                &mut bound_this,
                &mut context_value,
                &mut external,
            );
        }
        let collections_before = heap.gc_stats().gc_cycles;

        let closure = if with_external_roots {
            let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                use crate::pelt::PeltField as _;
                external.pelt_trace(visitor);
            };
            alloc_closure_with_roots(
                &mut heap,
                71,
                context_value,
                Some(bound_this),
                None,
                &mut external_visit,
            )
            .expect("closure after forced collection")
        } else {
            alloc_closure(&mut heap, 71, context_value, Some(bound_this), None)
                .expect("closure after forced collection")
        };
        assert!(heap.gc_stats().gc_cycles > collections_before);
        let stored_this = closure.bound_this(&heap).expect("stored this");
        let stored_context = closure.context(&heap);
        assert!(
            [stored_this.to_bits(), stored_context.to_bits()]
                .iter()
                .zip(original)
                .any(|(after, before)| *after != before),
            "forced full GC must relocate at least one young field"
        );
        let context = stored_context.as_context().expect("context survives");
        assert_eq!(
            crate::context::read_slot(&heap, context, 0),
            Some(Value::number_i32(404))
        );
        assert_eq!(
            heap.debug_header_tag(stored_this.as_object().expect("this object")),
            Some(crate::object::OBJECT_BODY_TYPE_TAG)
        );
        if let Some(value) = external {
            assert_eq!(
                heap.debug_header_tag(value.as_object().expect("external object")),
                Some(crate::object::OBJECT_BODY_TYPE_TAG)
            );
        }
    }

    #[test]
    fn closure_allocator_roots_pending_call_fields_across_forced_full_gc() {
        alloc_closure_across_forced_full_gc(false);
    }

    #[test]
    fn closure_allocator_composes_external_roots_across_forced_full_gc() {
        alloc_closure_across_forced_full_gc(true);
    }

    #[test]
    fn allocates_closure_without_a_context() {
        let mut heap = GcHeap::new().expect("heap");
        let closure = alloc_closure(&mut heap, 7, Value::undefined(), None, None).expect("alloc");
        assert_eq!(closure.function_id(), 7);
        assert_eq!(closure.bound_this(&heap), None);
        assert_eq!(closure.bound_new_target(&heap), None);
        assert!(closure.context(&heap).is_undefined());
        assert!(!closure.requires_runtime_setup(&heap));
        heap.read_payload(closure.handle(), |body| {
            assert_eq!(body.call_header.function_id, 7);
            assert_eq!(body.call_header.flags, 0);
            assert!(body.bound_this.is_undefined());
            assert!(body.bound_new_target.is_undefined());
        });
    }

    #[test]
    fn presence_flags_distinguish_some_undefined_from_none() {
        let mut heap = GcHeap::new().expect("heap");
        let closure = alloc_closure(
            &mut heap,
            9,
            Value::undefined(),
            Some(Value::undefined()),
            None,
        )
        .expect("alloc");
        assert_eq!(closure.bound_this(&heap), Some(Value::undefined()));
        assert_eq!(closure.bound_new_target(&heap), None);
        let state = closure.call_state(&heap);
        assert_eq!(state.bound_this, Some(Value::undefined()));
        assert!(closure.context(&heap).is_undefined());
    }

    #[test]
    fn only_a_bound_new_target_needs_runtime_setup() {
        let mut heap = GcHeap::new().expect("heap");
        let lexical_this =
            alloc_closure(&mut heap, 1, Value::undefined(), Some(Value::null()), None)
                .expect("closure");
        assert!(!lexical_this.requires_runtime_setup(&heap));
        let lexical_new_target =
            alloc_closure(&mut heap, 1, Value::undefined(), None, Some(Value::null()))
                .expect("closure");
        let header = lexical_new_target.call_header(&heap);
        assert!(header.has_flag(CLOSURE_CALL_FLAG_BOUND_NEW_TARGET));
        assert!(header.requires_runtime_setup());
    }

    #[test]
    fn closure_call_abi_layout_is_stable() {
        assert_eq!(std::mem::size_of::<ClosureCallHeader>(), 16);
        assert_eq!(std::mem::align_of::<ClosureCallHeader>(), 8);
        assert_eq!(CLOSURE_BODY_FUNCTION_ID_OFFSET, 0);
        assert_eq!(CLOSURE_BODY_CALL_FLAGS_OFFSET, 4);
        assert_eq!(CLOSURE_BODY_CONTEXT_OFFSET, 8);
        assert_eq!(CLOSURE_BODY_BOUND_THIS_OFFSET, 16);
        assert_eq!(CLOSURE_BODY_BOUND_NEW_TARGET_OFFSET, 24);
        assert_eq!(CLOSURE_BODY_OWN_PROPS_OFFSET, 32);
    }

    #[test]
    fn closure_body_carries_no_capture_tail() {
        // Fixed-size body: the context word replaced the capture tail, the
        // absolute spine base, and the eval-environment handle.
        assert_eq!(std::mem::size_of::<JsClosureBody>(), 80);
        let mut heap = GcHeap::new().expect("heap");
        let closure = alloc_closure(&mut heap, 3, Value::undefined(), None, None).expect("alloc");
        let tag = heap.debug_header_tag(closure.handle());
        assert_eq!(tag, Some(JS_CLOSURE_BODY_TYPE_TAG));
    }

    #[test]
    fn type_tag_matches_traceable_const() {
        assert_eq!(
            <JsClosureBody as otter_gc::SafeTraceable>::TYPE_TAG,
            JS_CLOSURE_BODY_TYPE_TAG
        );
    }
}
