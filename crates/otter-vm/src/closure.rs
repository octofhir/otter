//! GC body for closure values.
//!
//! A closure carries:
//!
//! - the bytecode function id it executes,
//! - the one context it was created over ([`crate::context`]), from which
//!   every outer binding its body reaches is a fixed hop,
//! - an optional bound `this` (arrow closures capture their receiver
//!   lexically; non-arrow closures take `this` from the call site),
//! - an optional bound `new.target` for arrow closures,
//! - a handle to its out-of-line [`ClosureRareBody`] once it has an own
//!   property or a `[[Prototype]]` override.
//!
//! # Contents
//!
//! - [`ClosureCallHeader`] — stable machine-facing call ABI prefix.
//! - [`ClosureCallState`] — allocation-neutral VM call metadata.
//! - [`JsClosureBody`] — GC body: the ABI prefix, the rare handle, the
//!   alignment padding, then the bound values.
//! - [`JsClosure`] — 8-byte handle plus cached function id.
//! - [`alloc_closure`] / [`alloc_closure_with_roots`] — allocators.
//! - [`JS_CLOSURE_BODY_TYPE_TAG`] — reserved
//!   [`otter_gc::Traceable::TYPE_TAG`].
//!
//! # Invariants
//!
//! - The machine-facing prefix is `#[repr(C)]`: native linkage reads
//!   [`ClosureCallHeader`], the bound `this` word and traced rare handle;
//!   the final four bytes are alignment padding.
//! - [`ClosureCallHeader::context`] is a full 8-byte `Value` word holding a
//!   context or `undefined`, fixed at creation. Generated code loads it with
//!   one instruction and follows it without cage-base arithmetic.
//! - The fixed body is 24 bytes. Bound `this` and `new.target` are trailing
//!   words present only when their flag is set: a closure with a bound
//!   `new.target` carries two words (the first `undefined` without a bound
//!   `this`), one with only a bound `this` carries one, and a plain closure
//!   none. Generated code reads the `this` word only after testing its flag.
//! - Per-instance `name`/`length` deletion, non-extensibility and the
//!   named-lookup summary are bits of [`ClosureCallHeader::flags`]; the
//!   named-lookup summary is its most significant byte.
//! - The context, the rare handle and the bound values are traced; the
//!   pending body carries the context and rare handle, and the allocator
//!   roots the bound values until they are copied into the cell. The
//!   constructor layout families live in the rare record and contain no receiver.
//!
//! # See also
//!
//! - [`crate::native_abi::Frame`] — fixed-width native activation ABI.
//! - [`crate::jit::JitCompileSnapshot`] — publishes the closure byte offsets
//!   to native backends.
//! - [`crate::closure_construct`] — the rare record.
//!
//! # Spec
//!
//! - ECMA-262 §10.2.3 OrdinaryFunctionCreate — `[[Environment]]`.
//! - ECMA-262 §13.3.6 — `[[Call]]` for ordinary functions / closures.
//! - ECMA-262 §10.2.1.1 — `[[ThisMode]]` for arrow functions.

use crate::Value;
use crate::closure_construct::{ClosureRareBody, ClosureRareHandle};
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
/// [`ClosureCallHeader::flags`] bit: the intrinsic `name` metadata property
/// was deleted from this instance (`delete f.name`). Sibling closures of the
/// same template keep their own copies, so the marker cannot live in a table
/// keyed by the bytecode function id.
const CLOSURE_FLAG_NAME_DELETED: u32 = 1 << 8;
/// As [`CLOSURE_FLAG_NAME_DELETED`] for `length`.
const CLOSURE_FLAG_LENGTH_DELETED: u32 = 1 << 9;
/// [`ClosureCallHeader::flags`] bit: §10.1.4 `[[Extensible]]` is false for
/// this instance. Sibling closures of the same bytecode template are distinct
/// function objects, so `Object.preventExtensions(f)` must not seal them.
const CLOSURE_FLAG_NON_EXTENSIBLE: u32 = 1 << 10;
/// Shift of the named-lookup summary byte (the `CLOSURE_LOOKUP_*` bits)
/// inside [`ClosureCallHeader::flags`].
const CLOSURE_NAMED_LOOKUP_SHIFT: u32 = 24;

/// Stable machine-facing closure call metadata.
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClosureCallHeader {
    /// Index into [`otter_bytecode::BytecodeModule::functions`].
    pub function_id: u32,
    /// Presence and call-setup routing flags, per-instance state bits and
    /// the named-lookup summary byte.
    pub flags: u32,
    /// The context this closure was created over, or `undefined`.
    pub context: Value,
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

    /// The named-lookup summary byte.
    #[inline]
    const fn named_lookup(self) -> u8 {
        (self.flags >> CLOSURE_NAMED_LOOKUP_SHIFT) as u8
    }

    /// Number of trailing bound-value words a body with these flags carries.
    #[inline]
    const fn bound_words(self) -> usize {
        if self.flags & CLOSURE_CALL_FLAG_BOUND_NEW_TARGET != 0 {
            2
        } else if self.flags & CLOSURE_CALL_FLAG_BOUND_THIS != 0 {
            1
        } else {
            0
        }
    }
}

/// GC body backing every closure value.
///
/// The fixed body is part of the stable call/allocation ABI; the bound values
/// trail it (see the module invariants).
#[repr(C, align(8))]
#[derive(Debug)]
pub struct JsClosureBody {
    /// Fixed-layout metadata read by native linkage.
    pub call_header: ClosureCallHeader,
    /// Out-of-line state; null until the closure needs it.
    rare: ClosureRareHandle,
}

/// [`ClosureCallHeader::flags`] named-lookup bit: the function kind's default
/// `[[Prototype]]` is `%Function.prototype%` (not a generator or async kind).
pub const CLOSURE_LOOKUP_ORDINARY: u8 = 1 << 0;
/// Generated proof that a closure's named lookup is ordinary: its
/// named-lookup byte reads exactly [`CLOSURE_LOOKUP_ORDINARY`].
#[must_use]
pub(crate) fn ordinary_named_lookup_guard() -> crate::jit::JitBodyGuard {
    crate::jit::JitBodyGuard {
        byte: CLOSURE_NAMED_LOOKUP_BYTE,
        width: crate::jit::JitGuardWidth::Byte,
        expect: u32::from(CLOSURE_LOOKUP_ORDINARY),
    }
}

/// [`ClosureCallHeader::flags`] word of a fresh closure whose function kind
/// has the ordinary `%Function.prototype%` lookup.
pub(crate) const CLOSURE_FLAGS_ORDINARY_LOOKUP: u32 =
    (CLOSURE_LOOKUP_ORDINARY as u32) << CLOSURE_NAMED_LOOKUP_SHIFT;

/// Named-lookup bit: an own-property bag exists.
pub const CLOSURE_LOOKUP_OWN_PROPS: u8 = 1 << 1;
/// Named-lookup bit: a `[[Prototype]]` override is installed.
pub const CLOSURE_LOOKUP_PROTO_OVERRIDE: u8 = 1 << 2;

impl otter_gc::SafeTraceable for JsClosureBody {
    const TYPE_TAG: u8 = JS_CLOSURE_BODY_TYPE_TAG;

    fn trace_slots_safe(&mut self, visitor: &mut SlotVisitor<'_>) {
        use crate::pelt::PeltField as _;
        self.trace_fixed_fields(visitor);
        let words = self.call_header.bound_words();
        let base = self.bound_words_ptr();
        for index in 0..words {
            // SAFETY: an allocated body carries `bound_words` initialized
            // trailing words.
            unsafe { (*base.add(index)).pelt_trace(visitor) };
        }
    }

    /// The pending payload has no trailing words yet: its bound values are
    /// the allocator's rooted locals, copied in by the initializer.
    fn trace_pending_slots_safe(&mut self, visitor: &mut SlotVisitor<'_>) {
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
/// Byte offset of the rare-record handle in [`JsClosureBody`]'s payload.
pub const CLOSURE_BODY_RARE_OFFSET: usize = std::mem::offset_of!(JsClosureBody, rare);
/// Byte offset of the trailing bound `this` word, valid only when
/// [`CLOSURE_CALL_FLAG_BOUND_THIS`] is set.
pub const CLOSURE_BODY_BOUND_THIS_OFFSET: usize = std::mem::size_of::<JsClosureBody>();
/// Byte offset of the trailing bound `new.target` word, valid only when
/// [`CLOSURE_CALL_FLAG_BOUND_NEW_TARGET`] is set.
pub const CLOSURE_BODY_BOUND_NEW_TARGET_OFFSET: usize =
    CLOSURE_BODY_BOUND_THIS_OFFSET + std::mem::size_of::<Value>();

/// Byte offset of the named-lookup summary byte from the cell header, as
/// generated code addresses it (the most significant byte of the flags word).
pub const CLOSURE_NAMED_LOOKUP_BYTE: u32 = (otter_gc::header::HEADER_SIZE
    + CLOSURE_BODY_CALL_FLAGS_OFFSET
    + (CLOSURE_NAMED_LOOKUP_SHIFT / 8) as usize) as u32;

const _: [(); 16] = [(); std::mem::size_of::<ClosureCallHeader>()];
const _: [(); 8] = [(); std::mem::align_of::<ClosureCallHeader>()];
const _: [(); 0] = [(); CLOSURE_CALL_HEADER_FUNCTION_ID_OFFSET];
const _: [(); 4] = [(); CLOSURE_CALL_HEADER_FLAGS_OFFSET];
const _: [(); 8] = [(); CLOSURE_CALL_HEADER_CONTEXT_OFFSET];
const _: [(); 0] = [(); CLOSURE_BODY_CALL_HEADER_OFFSET];
const _: [(); 16] = [(); CLOSURE_BODY_RARE_OFFSET];
const _: [(); 24] = [(); CLOSURE_BODY_BOUND_THIS_OFFSET];
const _: [(); 24] = [(); std::mem::size_of::<JsClosureBody>()];
// The named-lookup byte is the flags word's top byte on a little-endian host.
const _: () = assert!(cfg!(target_endian = "little"));

impl JsClosureBody {
    pub(crate) fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        visitor(self.call_header.function_id);
        crate::code_liveness::visit_value(&self.call_header.context, visitor);
        let words = self.call_header.bound_words();
        let base = self.bound_words_ptr();
        for index in 0..words {
            // SAFETY: an allocated body carries `bound_words` trailing words.
            crate::code_liveness::visit_value(unsafe { &*base.add(index) }, visitor);
        }
    }

    fn new(function_id: u32, context: Value, bound_this: bool, bound_new_target: bool) -> Self {
        debug_assert!(context.is_undefined() || context.as_context().is_some());
        Self {
            call_header: ClosureCallHeader::new(function_id, context, bound_this, bound_new_target),
            rare: ClosureRareHandle::null(),
        }
    }

    /// Base of the trailing bound-value words.
    #[inline]
    fn bound_words_ptr(&self) -> *mut Value {
        // SAFETY: computes the tail address only; dereferenced solely for the
        // words the flags say the allocation reserved.
        unsafe { (self as *const Self).add(1).cast_mut().cast::<Value>() }
    }

    fn trace_fixed_fields(&mut self, visitor: &mut SlotVisitor<'_>) {
        use crate::pelt::PeltField as _;
        self.call_header.context.pelt_trace(visitor);
        if !self.rare.is_null() {
            visitor(&mut self.rare as *mut ClosureRareHandle as *mut RawGc);
        }
    }

    #[inline]
    pub(crate) fn bound_this_option(&self) -> Option<Value> {
        self.call_header
            .has_flag(CLOSURE_CALL_FLAG_BOUND_THIS)
            // SAFETY: the flag guarantees the first trailing word exists.
            .then(|| unsafe { *self.bound_words_ptr() })
    }

    #[inline]
    pub(crate) fn bound_new_target_option(&self) -> Option<Value> {
        self.call_header
            .has_flag(CLOSURE_CALL_FLAG_BOUND_NEW_TARGET)
            // SAFETY: the flag guarantees both trailing words exist.
            .then(|| unsafe { *self.bound_words_ptr().add(1) })
    }

    /// The rare record, if allocated.
    #[inline]
    pub(crate) fn rare(&self) -> Option<ClosureRareHandle> {
        (!self.rare.is_null()).then_some(self.rare)
    }

    /// The named-lookup summary byte.
    #[inline]
    pub(crate) fn named_lookup(&self) -> u8 {
        self.call_header.named_lookup()
    }

    #[inline]
    fn set_named_lookup_bits(&mut self, bits: u8, on: bool) {
        let shifted = u32::from(bits) << CLOSURE_NAMED_LOOKUP_SHIFT;
        if on {
            self.call_header.flags |= shifted;
        } else {
            self.call_header.flags &= !shifted;
        }
    }

    #[inline]
    fn set_state_flag(&mut self, flag: u32, on: bool) {
        if on {
            self.call_header.flags |= flag;
        } else {
            self.call_header.flags &= !flag;
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
        let flag = match key {
            "name" => CLOSURE_FLAG_NAME_DELETED,
            "length" => CLOSURE_FLAG_LENGTH_DELETED,
            _ => return false,
        };
        heap.read_payload(self.handle(), |body| body.call_header.has_flag(flag))
    }

    /// Record (or clear) the per-instance deleted-metadata flag.
    pub(crate) fn set_metadata_deleted(
        self,
        heap: &mut otter_gc::GcHeap,
        key: &str,
        deleted: bool,
    ) {
        let flag = match key {
            "name" => CLOSURE_FLAG_NAME_DELETED,
            "length" => CLOSURE_FLAG_LENGTH_DELETED,
            _ => return,
        };
        heap.with_payload(self.handle(), |body| body.set_state_flag(flag, deleted));
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
    /// The named-lookup summary byte (`CLOSURE_LOOKUP_*` bits).
    #[must_use]
    pub(crate) fn named_lookup(self, heap: &GcHeap) -> u8 {
        heap.read_payload(self.handle, JsClosureBody::named_lookup)
    }

    pub(crate) fn mark_ordinary_lookup(self, heap: &mut GcHeap) {
        heap.with_payload(self.handle, |body| {
            body.set_named_lookup_bits(CLOSURE_LOOKUP_ORDINARY, true);
        });
    }

    /// This closure's rare record, if allocated.
    #[must_use]
    pub(crate) fn rare(self, heap: &GcHeap) -> Option<ClosureRareHandle> {
        heap.read_payload(self.handle, JsClosureBody::rare)
    }

    /// Install a freshly allocated rare record. The closure may be older than
    /// the record, so the edge is barriered.
    pub(crate) fn install_rare(self, heap: &mut GcHeap, rare: ClosureRareHandle) {
        heap.with_payload(self.handle, |body| {
            debug_assert!(body.rare.is_null(), "a closure keeps its first rare record");
            body.rare = rare;
        });
        heap.write_barrier(self.handle, rare);
    }

    /// Read the rare record with `read`, or `None` without one.
    fn with_rare<R>(self, heap: &GcHeap, read: impl FnOnce(&ClosureRareBody) -> R) -> Option<R> {
        let rare = self.rare(heap)?;
        Some(heap.read_payload(rare, read))
    }

    /// This closure instance's own-property bag, if it has been
    /// materialized. The zero compressed handle denotes an absent bag.
    #[must_use]
    pub fn own_props(self, heap: &GcHeap) -> Option<JsObject> {
        self.with_rare(heap, |rare| rare.own_props)
            .filter(|bag| !bag.is_null())
    }

    /// Install the per-instance own-property bag into the rare record the
    /// caller already allocated. Records the rare→bag edge with the GC write
    /// barrier (the bag may be younger).
    pub fn set_own_props(self, heap: &mut GcHeap, bag: JsObject) {
        let rare = self
            .rare(heap)
            .expect("an own-property bag is installed into an allocated rare record");
        heap.with_payload(rare, |rare| rare.own_props = bag);
        heap.write_barrier(rare, bag);
        heap.with_payload(self.handle, |body| {
            body.set_named_lookup_bits(CLOSURE_LOOKUP_OWN_PROPS, true);
        });
    }

    /// Release an own-property bag a delete left empty and extensible, so
    /// the closure reads as one that never had own properties again (V8
    /// rolls a delete of the last-added property back to the parent map).
    /// Generated code proving the ordinary lookup then stays valid.
    pub(crate) fn release_empty_own_props(self, heap: &mut GcHeap) {
        let Some(bag) = self.own_props(heap) else {
            return;
        };
        let empty = crate::object::with_properties(bag, heap, |properties| {
            properties.keys().next().is_none() && properties.symbol_keys().next().is_none()
        });
        if !empty || !crate::object::is_extensible(bag, heap) {
            return;
        }
        let Some(rare) = self.rare(heap) else {
            return;
        };
        heap.with_payload(rare, |rare| rare.own_props = JsObject::null());
        heap.with_payload(self.handle, |body| {
            body.set_named_lookup_bits(CLOSURE_LOOKUP_OWN_PROPS, false);
        });
    }

    /// The function's `prototype` slot: its value (the hole until the default
    /// object is allocated) and whether it is writable. `None` without a
    /// rare record: unallocated and writable.
    #[must_use]
    pub(crate) fn prototype_slot(self, heap: &GcHeap) -> Option<(Value, bool)> {
        self.with_rare(heap, |rare| (rare.prototype, rare.prototype_writable))
    }

    /// Store the function's `prototype` value into the rare record the caller
    /// already allocated. The value may be younger than the record, so the
    /// edge is barriered. A different value retires every constructor family
    /// of this closure: each was selected for the replaced prototype (V8's
    /// initial-map change), so the head never names a stale family.
    pub(crate) fn set_prototype_value(self, heap: &mut GcHeap, value: Value) {
        use crate::pelt::PeltField as _;
        let rare = self
            .rare(heap)
            .expect("a prototype value is stored into an allocated rare record");
        let (previous, head) =
            heap.read_payload(rare, |rare| (rare.prototype, rare.constructor_layouts));
        if previous != value && !head.is_null() {
            crate::constructor_layout::detach_families(heap, head);
            heap.with_payload(rare, |rare| {
                rare.constructor_layouts = crate::constructor_layout::ConstructorLayout::null();
            });
        }
        heap.with_payload(rare, |rare| {
            rare.prototype = value;
            rare.prototype_ordinary = value.as_object().is_some();
        });
        let mut child = value;
        let mut visit = |slot: *mut RawGc| {
            // SAFETY: `pelt_trace` hands out pointers into the local copy of
            // the value; the slot is read to record its edge only.
            let raw = unsafe { *slot };
            heap.record_write_edge(rare, raw);
        };
        child.pelt_trace(&mut visit);
    }

    /// Whether a generated `instanceof` site's cell ever cached this closure.
    #[must_use]
    pub(crate) fn instanceof_cached(self, heap: &GcHeap) -> bool {
        self.with_rare(heap, |rare| rare.instanceof_cached)
            .unwrap_or(false)
    }

    /// Clear the `prototype` property's writable attribute in the rare record
    /// the caller already allocated.
    pub(crate) fn freeze_prototype(self, heap: &mut GcHeap) {
        let rare = self
            .rare(heap)
            .expect("a prototype attribute is stored into an allocated rare record");
        heap.with_payload(rare, |rare| rare.prototype_writable = false);
    }

    /// Actual function-object constructor families. Sibling closures of the
    /// same template own independent heads.
    pub(crate) fn constructor_layouts(
        self,
        heap: &GcHeap,
    ) -> crate::constructor_layout::ConstructorLayout {
        self.with_rare(heap, |rare| rare.constructor_layouts)
            .unwrap_or_else(crate::constructor_layout::ConstructorLayout::null)
    }
    pub(crate) fn set_constructor_layouts(
        self,
        heap: &mut GcHeap,
        layouts: crate::constructor_layout::ConstructorLayout,
    ) {
        let rare = self
            .rare(heap)
            .expect("constructor layout owner has a rare record");
        heap.with_payload(rare, |body| body.constructor_layouts = layouts);
        heap.record_write(rare, &layouts);
    }

    /// §10.1.3 `[[IsExtensible]]` for this closure instance.
    #[must_use]
    pub fn is_extensible(self, heap: &GcHeap) -> bool {
        !heap.read_payload(self.handle, |body| {
            body.call_header.has_flag(CLOSURE_FLAG_NON_EXTENSIBLE)
        })
    }

    /// §10.1.4 `[[PreventExtensions]]` for this closure instance.
    pub fn prevent_extensions(self, heap: &mut GcHeap) {
        heap.with_payload(self.handle, |body| {
            body.set_state_flag(CLOSURE_FLAG_NON_EXTENSIBLE, true);
        });
    }

    /// The `[[Prototype]]` override installed on this closure instance,
    /// if any. `None` means the closure still walks the realm's
    /// `%Function.prototype%`; a stored `Value::null()` is an explicit null
    /// prototype.
    #[must_use]
    pub fn proto_override(self, heap: &GcHeap) -> Option<Value> {
        let installed = heap.read_payload(self.handle, |body| {
            body.named_lookup() & CLOSURE_LOOKUP_PROTO_OVERRIDE != 0
        });
        if !installed {
            return None;
        }
        self.with_rare(heap, |rare| rare.proto_override)
    }

    /// Drop the per-instance `[[Prototype]]` override, restoring the
    /// intrinsic `%Function.prototype%` walk.
    pub fn clear_proto_override(self, heap: &mut GcHeap) {
        if let Some(rare) = self.rare(heap) {
            heap.with_payload(rare, |rare| rare.proto_override = Value::undefined());
        }
        heap.with_payload(self.handle, |body| {
            body.set_named_lookup_bits(CLOSURE_LOOKUP_PROTO_OVERRIDE, false);
        });
    }

    /// Install the per-instance `[[Prototype]]` override into the rare record
    /// the caller already allocated. The new prototype may be younger than
    /// the record, so every heap slot the value carries is recorded in the
    /// remembered set.
    pub fn set_proto_override(self, heap: &mut GcHeap, proto: Value) {
        use crate::pelt::PeltField as _;

        let rare = self
            .rare(heap)
            .expect("a prototype override is installed into an allocated rare record");
        heap.with_payload(rare, |rare| rare.proto_override = proto);
        heap.with_payload(self.handle, |body| {
            body.set_named_lookup_bits(CLOSURE_LOOKUP_PROTO_OVERRIDE, true);
        });
        let mut child = proto;
        let mut visit = |slot: *mut RawGc| {
            // SAFETY: `pelt_trace` hands out pointers into the local copy
            // of the value; the slot is read to record its edge only.
            let raw = unsafe { *slot };
            heap.record_write_edge(rare, raw);
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

/// Allocate one closure over `context` (a context value or `undefined`).
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
/// The context rides in the pending body, which the allocator traces; the
/// bound values are rooted here and copied into their trailing words once
/// the cell exists, so a collection rewrites them before the copy.
/// `external_visit` covers any other young value the caller holds in a Rust
/// local across this call (per the [`GcHeap::alloc_with_roots`] contract).
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
    let body = JsClosureBody::new(
        function_id,
        context,
        bound_this.is_some(),
        bound_new_target.is_some(),
    );
    let words = body.call_header.bound_words();
    let mut bound = [
        bound_this.unwrap_or_else(Value::undefined),
        bound_new_target.unwrap_or_else(Value::undefined),
    ];
    let bound_slot = bound.as_mut_ptr();
    let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
        external_visit(visitor);
        for index in 0..words {
            // SAFETY: `index < 2`; the local array outlives the allocation.
            unsafe { (*bound_slot.add(index)).trace_value_slot_mut(visitor) };
        }
    };
    let handle = heap.alloc_trailing_with_roots_initialized(
        body,
        words * std::mem::size_of::<Value>(),
        &mut visit,
        |body| {
            let base = body.bound_words_ptr();
            for index in 0..words {
                // SAFETY: the cell reserved `words` trailing words; the local
                // array was rewritten by any collection the allocation ran.
                unsafe { *base.add(index) = *bound_slot.add(index) };
            }
        },
    )?;
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
        let this_object = crate::object::alloc_fixture_object_with_roots(&mut heap, &mut |_| {})
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
                    has_extension: false,
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
                crate::object::alloc_fixture_object_with_roots(&mut heap, &mut roots)
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
            assert!(body.rare().is_none());
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
        assert!(
            closure
                .call_header(&heap)
                .has_flag(CLOSURE_CALL_FLAG_BOUND_THIS)
        );
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
        assert_eq!(CLOSURE_BODY_RARE_OFFSET, 16);
        assert_eq!(
            std::mem::size_of::<JsClosureBody>(),
            CLOSURE_BODY_RARE_OFFSET + 8,
            "the rare handle is followed only by immutable alignment padding"
        );
        assert_eq!(CLOSURE_BODY_BOUND_THIS_OFFSET, 24);
        assert_eq!(CLOSURE_BODY_BOUND_NEW_TARGET_OFFSET, 32);
    }

    #[test]
    fn plain_closure_is_a_32_byte_cell() {
        assert_eq!(std::mem::size_of::<JsClosureBody>(), 24);
        let mut heap = GcHeap::new().expect("heap");
        let plain = alloc_closure(&mut heap, 3, Value::undefined(), None, None).expect("alloc");
        assert_eq!(
            heap.debug_header_tag(plain.handle()),
            Some(JS_CLOSURE_BODY_TYPE_TAG)
        );
        let arrow = alloc_closure(&mut heap, 3, Value::undefined(), Some(Value::null()), None)
            .expect("arrow");
        let lexical = alloc_closure(
            &mut heap,
            3,
            Value::undefined(),
            None,
            Some(Value::number_i32(5)),
        )
        .expect("lexical new.target");
        assert_eq!(arrow.bound_this(&heap), Some(Value::null()));
        assert_eq!(lexical.bound_this(&heap), None);
        assert_eq!(lexical.bound_new_target(&heap), Some(Value::number_i32(5)));
        let sizes = [plain, arrow, lexical].map(|closure| {
            // SAFETY: the handle names a live closure cell.
            unsafe { (*closure.handle().as_header_ptr()).size_bytes() }
        });
        assert_eq!(sizes, [32, 40, 48]);
    }

    #[test]
    fn per_instance_state_lives_in_the_flags_word() {
        let mut heap = GcHeap::new().expect("heap");
        let closure = alloc_closure(&mut heap, 4, Value::undefined(), None, None).expect("alloc");
        closure.mark_ordinary_lookup(&mut heap);
        closure.set_metadata_deleted(&mut heap, "length", true);
        closure.prevent_extensions(&mut heap);
        assert!(closure.metadata_deleted(&heap, "length"));
        assert!(!closure.metadata_deleted(&heap, "name"));
        assert!(!closure.is_extensible(&heap));
        let header = closure.call_header(&heap);
        assert!(!header.requires_runtime_setup());
        // SAFETY: the handle names a live closure cell.
        let lookup = unsafe {
            *(closure.handle().as_header_ptr() as *const u8).add(CLOSURE_NAMED_LOOKUP_BYTE as usize)
        };
        assert_eq!(lookup, CLOSURE_LOOKUP_ORDINARY);
    }

    #[test]
    fn type_tag_matches_traceable_const() {
        assert_eq!(
            <JsClosureBody as otter_gc::SafeTraceable>::TYPE_TAG,
            JS_CLOSURE_BODY_TYPE_TAG
        );
    }
}
