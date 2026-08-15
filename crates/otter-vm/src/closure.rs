//! GC body for closure values.
//!
//! A closure carries:
//!
//! - the bytecode function id it executes,
//! - the captured upvalue spine (one [`crate::UpvalueCell`] per
//!   binding, in declaration order),
//! - an optional bound `this` (arrow closures capture their receiver
//!   lexically; non-arrow closures take `this` from the call site),
//! - an optional bound `new.target` for arrow closures,
//! - an optional derived-constructor `this` cell for arrow
//!   `super()` calls that run after the original frame is off-stack,
//! - one nullable compressed direct-eval environment handle in the stable
//!   call header.
//!
//! # Contents
//!
//! - [`ClosureCallHeader`] — stable machine-facing call ABI prefix.
//! - [`ClosureCallState`] — allocation-neutral VM call metadata.
//! - [`JsClosureBody`] — GC body holding the ABI prefix, canonical
//!   bound values, and the traced tail.
//! - [`JsClosure`] — 8-byte handle plus cached function id.
//! - [`alloc_closure`] / [`alloc_closure_with_roots`] — allocators.
//! - [`JS_CLOSURE_BODY_TYPE_TAG`] — reserved
//!   [`otter_gc::Traceable::TYPE_TAG`].
//!
//! # Invariants
//!
//! - The machine-facing prefix is `#[repr(C)]`: native linkage may read
//!   [`ClosureCallHeader`], `bound_this`, and `bound_new_target` only. The
//!   nullable direct-eval handle has one representation and one traced owner:
//!   [`ClosureCallHeader::eval_env`]. Native linkage must never interpret the
//!   following Rust `Option` layout.
//! - The upvalue spine is built once at closure creation
//!   ([`Op::MakeClosure`](otter_bytecode::Op::MakeClosure)) and never
//!   resized. It is a [`crate::upvalue_spine::UpvalueSpineBody`] in old
//!   space, so its address matches the `upvalue_base` / `upvalue_count`
//!   pair for the closure's lifetime; native code must not retain that
//!   base beyond the live call. Per-cell mutation flows through
//!   [`crate::store_upvalue`] / [`crate::read_upvalue`].
//! - Canonical `Value` fields are always traced. Presence flags distinguish
//!   `None` from `Some(undefined)` while [`JsClosure`] keeps the ergonomic
//!   `Option<Value>` API.
//! - Bound `new.target` and derived-constructor `this` require the call-setup
//!   runtime stub. A direct-eval environment is copied directly into the
//!   callee's traced native frame and does not route through that stub.
//!
//! # See also
//!
//! - [`crate::native_abi::NativeFrame`] — fixed-width native activation ABI.
//! - [`crate::jit::JitCompileSnapshot`] — publishes the nested function-id
//!   byte offset to native backends.
//!
//! # Spec
//!
//! - ECMA-262 §15.2.5 — closure environment construction.
//! - ECMA-262 §13.3.6 — `[[Call]]` for ordinary functions / closures.
//! - ECMA-262 §10.2.1.1 — `[[ThisMode]]` for arrow functions.

use crate::object::JsObject;
use crate::upvalue_spine::UpvalueSpineHandle;
use crate::{UpvalueCell, Value, upvalue_source::UpvalueSource};
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
/// [`ClosureCallHeader::flags`] bit: the Rust tail carries a derived-`this` cell.
pub const CLOSURE_CALL_FLAG_BOUND_DERIVED_THIS: u32 = 1 << 2;
/// Flags whose semantics require the call-setup runtime stub.
///
/// Native linkage handles lexical `this` inline. Lexical `new.target`, shared
/// derived-constructor state route through setup before control returns to the
/// compiled callee in the same native activation. The direct-eval environment
/// has its own fixed header slot and is copied inline.
pub const CLOSURE_CALL_RUNTIME_SETUP_FLAGS: u32 =
    CLOSURE_CALL_FLAG_BOUND_NEW_TARGET | CLOSURE_CALL_FLAG_BOUND_DERIVED_THIS;

/// Stable machine-facing closure call metadata.
///
/// All addresses use fixed-width integers instead of Rust references. The
/// `upvalue_base` points into the closure's
/// [`crate::upvalue_spine::UpvalueSpineBody`] and is valid only while the
/// closure remains live; it is not a movable GC-object pointer and must
/// not be cached across calls.
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClosureCallHeader {
    /// Index into [`otter_bytecode::BytecodeModule::functions`].
    pub function_id: u32,
    /// Presence and call-setup routing flags.
    pub flags: u32,
    /// Process address of the first captured [`UpvalueCell`], or zero when empty.
    pub upvalue_base: u64,
    /// Number of captured [`UpvalueCell`] entries at `upvalue_base`.
    pub upvalue_count: u32,
    /// Nullable compressed direct-eval environment handle.
    pub eval_env: crate::eval_env::EvalEnvHandle,
}

/// Allocation-neutral closure state consumed by call preparation.
///
/// `upvalues` borrows the closure's spine without constructing a
/// `Vec`/`Box`. The exact closure value must remain rooted for every use
/// of this record that can cross a collection.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ClosureCallState {
    pub(crate) upvalues: UpvalueSource,
    pub(crate) bound_this: Option<Value>,
    pub(crate) bound_new_target: Option<Value>,
    pub(crate) bound_derived_this: Option<UpvalueCell>,
    pub(crate) eval_env: Option<crate::eval_env::EvalEnvHandle>,
}

impl ClosureCallHeader {
    fn new(
        function_id: u32,
        upvalue_count: u32,
        upvalue_base: u64,
        bound_this: bool,
        bound_new_target: bool,
        bound_derived_this: bool,
        eval_env: Option<crate::eval_env::EvalEnvHandle>,
    ) -> Self {
        let mut flags = 0;
        if bound_this {
            flags |= CLOSURE_CALL_FLAG_BOUND_THIS;
        }
        if bound_new_target {
            flags |= CLOSURE_CALL_FLAG_BOUND_NEW_TARGET;
        }
        if bound_derived_this {
            flags |= CLOSURE_CALL_FLAG_BOUND_DERIVED_THIS;
        }
        Self {
            function_id,
            flags,
            upvalue_base,
            upvalue_count,
            eval_env: eval_env.unwrap_or_else(crate::eval_env::EvalEnvHandle::null),
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
/// Only the prefix through `bound_new_target` is part of the stable call ABI.
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
    /// Captured upvalue spine in declaration order, or a null handle
    /// when the closure captures nothing. The cells live in the spine
    /// body's own cell, so the closure owns no storage outside the heap
    /// and a page image carries the whole of it. Per-cell mutation flows
    /// through [`crate::store_upvalue`] / [`crate::read_upvalue`]; the
    /// spine itself never resizes.
    pub spine: UpvalueSpineHandle,
    /// Arrow closures created inside derived constructors capture the
    /// constructor's shared `this` cell so `super()` can bind it even
    /// when the arrow is invoked through a nested sync dispatch.
    pub bound_derived_this: Option<UpvalueCell>,
    /// §10.2 — this closure instance's own-property bag. Each function
    /// object created by evaluating a function expression/declaration
    /// owns a DISTINCT property store (`f.foo = 1`, the materialized
    /// `f.prototype`, etc.), so it lives per-instance here rather than
    /// in a side table keyed by the bytecode template id (which every
    /// sibling closure of the same source would share). `None` until
    /// the first own property or `prototype` materialization.
    pub own_props: Option<JsObject>,
}

impl otter_gc::SafeTraceable for JsClosureBody {
    const TYPE_TAG: u8 = JS_CLOSURE_BODY_TYPE_TAG;

    /// Walk every outgoing reference, then republish the spine's address.
    ///
    /// The spine handle is visited first on purpose. A restore relocates
    /// it here, and `upvalue_base` — a raw process address compiled code
    /// loads directly out of the call header — has to be recomputed from
    /// the handle's post-relocation value. A scavenge never moves the
    /// spine (it is old-space), so outside a restore the refresh writes
    /// back what was already there.
    fn trace_slots_safe(&mut self, visitor: &mut SlotVisitor<'_>) {
        use crate::pelt::PeltField as _;

        if !self.spine.is_null() {
            let p = &mut self.spine as *mut UpvalueSpineHandle as *mut RawGc;
            visitor(p);
        }
        self.refresh_upvalue_base();

        self.bound_this.pelt_trace(visitor);
        self.bound_new_target.pelt_trace(visitor);
        self.bound_derived_this.pelt_trace(visitor);
        self.call_header.eval_env.pelt_trace(visitor);
        self.own_props.pelt_trace(visitor);
    }
}

/// Byte offset of `function_id` inside [`ClosureCallHeader`].
pub const CLOSURE_CALL_HEADER_FUNCTION_ID_OFFSET: usize =
    std::mem::offset_of!(ClosureCallHeader, function_id);
/// Byte offset of `flags` inside [`ClosureCallHeader`].
pub const CLOSURE_CALL_HEADER_FLAGS_OFFSET: usize = std::mem::offset_of!(ClosureCallHeader, flags);
/// Byte offset of `upvalue_base` inside [`ClosureCallHeader`].
pub const CLOSURE_CALL_HEADER_UPVALUE_BASE_OFFSET: usize =
    std::mem::offset_of!(ClosureCallHeader, upvalue_base);
/// Byte offset of `upvalue_count` inside [`ClosureCallHeader`].
pub const CLOSURE_CALL_HEADER_UPVALUE_COUNT_OFFSET: usize =
    std::mem::offset_of!(ClosureCallHeader, upvalue_count);
/// Byte offset of `eval_env` inside [`ClosureCallHeader`].
pub const CLOSURE_CALL_HEADER_EVAL_ENV_OFFSET: usize =
    std::mem::offset_of!(ClosureCallHeader, eval_env);

/// Byte offset of the nested call header in [`JsClosureBody`]'s payload.
pub const CLOSURE_BODY_CALL_HEADER_OFFSET: usize = std::mem::offset_of!(JsClosureBody, call_header);
/// Byte offset of the nested function id in [`JsClosureBody`]'s payload.
pub const CLOSURE_BODY_FUNCTION_ID_OFFSET: usize =
    CLOSURE_BODY_CALL_HEADER_OFFSET + CLOSURE_CALL_HEADER_FUNCTION_ID_OFFSET;
/// Byte offset of the nested call flags in [`JsClosureBody`]'s payload.
pub const CLOSURE_BODY_CALL_FLAGS_OFFSET: usize =
    CLOSURE_BODY_CALL_HEADER_OFFSET + CLOSURE_CALL_HEADER_FLAGS_OFFSET;
/// Byte offset of the nested upvalue base in [`JsClosureBody`]'s payload.
pub const CLOSURE_BODY_UPVALUE_BASE_OFFSET: usize =
    CLOSURE_BODY_CALL_HEADER_OFFSET + CLOSURE_CALL_HEADER_UPVALUE_BASE_OFFSET;
/// Byte offset of the nested upvalue count in [`JsClosureBody`]'s payload.
pub const CLOSURE_BODY_UPVALUE_COUNT_OFFSET: usize =
    CLOSURE_BODY_CALL_HEADER_OFFSET + CLOSURE_CALL_HEADER_UPVALUE_COUNT_OFFSET;
/// Byte offset of the nested nullable eval-environment handle in
/// [`JsClosureBody`]'s payload.
pub const CLOSURE_BODY_EVAL_ENV_OFFSET: usize =
    CLOSURE_BODY_CALL_HEADER_OFFSET + CLOSURE_CALL_HEADER_EVAL_ENV_OFFSET;
/// Byte offset of canonical `bound_this` in [`JsClosureBody`]'s payload.
pub const CLOSURE_BODY_BOUND_THIS_OFFSET: usize = std::mem::offset_of!(JsClosureBody, bound_this);
/// Byte offset of canonical `bound_new_target` in [`JsClosureBody`]'s payload.
pub const CLOSURE_BODY_BOUND_NEW_TARGET_OFFSET: usize =
    std::mem::offset_of!(JsClosureBody, bound_new_target);

const _: [(); 24] = [(); std::mem::size_of::<ClosureCallHeader>()];
const _: [(); 8] = [(); std::mem::align_of::<ClosureCallHeader>()];
const _: [(); 0] = [(); CLOSURE_CALL_HEADER_FUNCTION_ID_OFFSET];
const _: [(); 4] = [(); CLOSURE_CALL_HEADER_FLAGS_OFFSET];
const _: [(); 8] = [(); CLOSURE_CALL_HEADER_UPVALUE_BASE_OFFSET];
const _: [(); 16] = [(); CLOSURE_CALL_HEADER_UPVALUE_COUNT_OFFSET];
const _: [(); 20] = [(); CLOSURE_CALL_HEADER_EVAL_ENV_OFFSET];
const _: [(); 0] = [(); CLOSURE_BODY_CALL_HEADER_OFFSET];
const _: [(); 24] = [(); CLOSURE_BODY_BOUND_THIS_OFFSET];
const _: [(); 32] = [(); CLOSURE_BODY_BOUND_NEW_TARGET_OFFSET];

impl JsClosureBody {
    fn new(
        function_id: u32,
        spine: UpvalueSpineHandle,
        upvalue_count: u32,
        upvalue_base: u64,
        bound_this: Option<Value>,
        bound_new_target: Option<Value>,
        bound_derived_this: Option<UpvalueCell>,
        eval_env: Option<crate::eval_env::EvalEnvHandle>,
    ) -> Self {
        let call_header = ClosureCallHeader::new(
            function_id,
            upvalue_count,
            upvalue_base,
            bound_this.is_some(),
            bound_new_target.is_some(),
            bound_derived_this.is_some(),
            eval_env,
        );
        Self {
            call_header,
            bound_this: bound_this.unwrap_or_else(Value::undefined),
            bound_new_target: bound_new_target.unwrap_or_else(Value::undefined),
            spine,
            bound_derived_this,
            own_props: None,
        }
    }

    /// Recompute the raw spine address compiled code reads out of the
    /// call header. Cheap, and correct wherever the spine ended up.
    fn refresh_upvalue_base(&mut self) {
        self.call_header.upvalue_base = crate::upvalue_spine::cells_base_address(self.spine);
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

    #[inline]
    pub(crate) fn bound_derived_this_option(&self) -> Option<UpvalueCell> {
        debug_assert_eq!(
            self.call_header
                .has_flag(CLOSURE_CALL_FLAG_BOUND_DERIVED_THIS),
            self.bound_derived_this.is_some()
        );
        self.bound_derived_this
    }

    #[inline]
    pub(crate) fn eval_env_option(&self) -> Option<crate::eval_env::EvalEnvHandle> {
        (!self.call_header.eval_env.is_null()).then_some(self.call_header.eval_env)
    }

    /// Copy call metadata while borrowing the immutable upvalue allocation.
    fn call_state(&self) -> ClosureCallState {
        // SAFETY: the spine is built once, never resized, and lives in
        // old space, so the published base stays valid while the closure
        // is reachable. A consumer of ClosureCallState must root the
        // exact closure value for the record's live extent, as documented
        // on the record itself.
        let upvalues = unsafe {
            UpvalueSource::from_raw_parts(
                self.call_header.upvalue_base as usize as *mut UpvalueCell,
                self.call_header.upvalue_count,
            )
        }
        .expect("closure upvalue spine must fit the u32 call ABI");
        ClosureCallState {
            upvalues,
            bound_this: self.bound_this_option(),
            bound_new_target: self.bound_new_target_option(),
            bound_derived_this: self.bound_derived_this_option(),
            eval_env: self.eval_env_option(),
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

    /// Copy all dynamic call metadata without cloning the captured spine.
    ///
    /// The returned upvalue source remains valid while this exact closure is
    /// rooted; closure creation never resizes its external vector allocation.
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

    /// Shared derived-constructor `this` cell captured by arrow
    /// closures that may run `super()`.
    #[must_use]
    pub fn bound_derived_this(self, heap: &GcHeap) -> Option<UpvalueCell> {
        heap.read_payload(self.handle, JsClosureBody::bound_derived_this_option)
    }

    /// Captured direct-eval variable environment, if any.
    #[must_use]
    pub fn eval_env(self, heap: &GcHeap) -> Option<crate::eval_env::EvalEnvHandle> {
        heap.read_payload(self.handle, JsClosureBody::eval_env_option)
    }

    /// This closure instance's own-property bag, if it has been
    /// materialized. See [`JsClosureBody::own_props`].
    #[must_use]
    pub fn own_props(self, heap: &GcHeap) -> Option<JsObject> {
        heap.read_payload(self.handle, |body| body.own_props)
    }

    /// Install the per-instance own-property bag. Records the
    /// closure→bag edge with the GC write barrier (the body lives in
    /// old space; the bag may be younger).
    pub fn set_own_props(self, heap: &mut GcHeap, bag: JsObject) {
        heap.with_payload(self.handle, |body| body.own_props = Some(bag));
        heap.write_barrier(self.handle, bag);
    }

    /// Number of captured upvalue cells. Reads the body once.
    #[must_use]
    pub fn upvalue_count(self, heap: &GcHeap) -> usize {
        self.call_header(heap).upvalue_count as usize
    }

    /// Run `f` with the captured upvalue spine. The slice borrow
    /// never escapes the closure; callers that need to retain a
    /// cell beyond `f` should snapshot the `UpvalueCell` handle
    /// (it is itself a `Copy` GC handle).
    pub fn with_upvalues<F, R>(self, heap: &GcHeap, f: F) -> R
    where
        F: FnOnce(&[UpvalueCell]) -> R,
    {
        let spine = heap.read_payload(self.handle, |body| body.spine);
        if spine.is_null() {
            return f(&[]);
        }
        heap.read_payload(spine, |body| f(body.cells()))
    }

    /// Snapshot the captured upvalue spine into a fresh `Vec`.
    /// Use when the caller needs to return cells across a borrow
    /// boundary; otherwise prefer [`Self::with_upvalues`].
    #[must_use]
    pub fn upvalues_snapshot(self, heap: &GcHeap) -> Vec<UpvalueCell> {
        self.with_upvalues(heap, <[UpvalueCell]>::to_vec)
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

/// Allocate a closure body in old-space, consistent with
/// [`crate::alloc_upvalue`] and with the spine itself.
///
/// # Errors
///
/// Surfaces [`OutOfMemory`] verbatim.
pub fn alloc_closure(
    heap: &mut GcHeap,
    function_id: u32,
    upvalues: Vec<UpvalueCell>,
    mut bound_this: Option<Value>,
    mut bound_new_target: Option<Value>,
    mut bound_derived_this: Option<UpvalueCell>,
    mut eval_env: Option<crate::eval_env::EvalEnvHandle>,
) -> Result<JsClosure, OutOfMemory> {
    let mut upvalues = upvalues;
    let spine = {
        let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            trace_pending_call_fields(
                &mut bound_this,
                &mut bound_new_target,
                &mut bound_derived_this,
                &mut eval_env,
                visitor,
            );
        };
        alloc_spine_for(heap, &mut upvalues, &mut visit)?
    };
    let body = JsClosureBody::new(
        function_id,
        spine,
        upvalue_count_of(&upvalues),
        crate::upvalue_spine::cells_base_address(spine),
        bound_this,
        bound_new_target,
        bound_derived_this,
        eval_env,
    );
    let handle = heap.alloc_old(body)?;
    Ok(JsClosure::from_parts(handle, function_id))
}

/// Cell count as the call ABI expresses it.
fn upvalue_count_of(upvalues: &[UpvalueCell]) -> u32 {
    u32::try_from(upvalues.len()).expect("closure upvalue spine exceeds the u32 call ABI")
}

/// Allocate the spine a closure will own, or a null handle when it
/// captures nothing — a closure with no upvalues costs no second cell.
fn alloc_spine_for(
    heap: &mut GcHeap,
    upvalues: &mut [UpvalueCell],
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<crate::upvalue_spine::UpvalueSpineHandle, OutOfMemory> {
    if upvalues.is_empty() {
        return Ok(crate::upvalue_spine::UpvalueSpineHandle::null());
    }
    crate::upvalue_spine::alloc_upvalue_spine(heap, upvalues, external_visit)
}

/// Trace closure-call fields that remain in Rust locals while the captured
/// upvalue spine is allocated.
///
/// The spine allocation can trigger a moving full collection before the
/// closure body exists. These are therefore real pending-payload slots, not
/// copies that can be reconstructed from the eventual body.
fn trace_pending_call_fields(
    bound_this: &mut Option<Value>,
    bound_new_target: &mut Option<Value>,
    bound_derived_this: &mut Option<UpvalueCell>,
    eval_env: &mut Option<crate::eval_env::EvalEnvHandle>,
    visitor: &mut SlotVisitor<'_>,
) {
    use crate::pelt::PeltField as _;

    bound_this.pelt_trace(visitor);
    bound_new_target.pelt_trace(visitor);
    bound_derived_this.pelt_trace(visitor);
    eval_env.pelt_trace(visitor);
}

/// Allocate a closure body while exposing caller-owned roots across
/// any allocation-triggered collection.
///
/// Use this from interpreter call sites where the surrounding
/// `Value`s on the Rust stack must be preserved (per the
/// [`GcHeap::alloc_with_roots`] contract).
///
/// # Errors
///
/// Surfaces [`OutOfMemory`] verbatim.
pub fn alloc_closure_with_roots(
    heap: &mut GcHeap,
    function_id: u32,
    upvalues: Vec<UpvalueCell>,
    mut bound_this: Option<Value>,
    mut bound_new_target: Option<Value>,
    mut bound_derived_this: Option<UpvalueCell>,
    mut eval_env: Option<crate::eval_env::EvalEnvHandle>,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsClosure, OutOfMemory> {
    let mut upvalues = upvalues;
    let spine = {
        let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            trace_pending_call_fields(
                &mut bound_this,
                &mut bound_new_target,
                &mut bound_derived_this,
                &mut eval_env,
                visitor,
            );
        };
        alloc_spine_for(heap, &mut upvalues, &mut visit)?
    };
    let body = JsClosureBody::new(
        function_id,
        spine,
        upvalue_count_of(&upvalues),
        crate::upvalue_spine::cells_base_address(spine),
        bound_this,
        bound_new_target,
        bound_derived_this,
        eval_env,
    );
    let handle = heap.alloc_with_roots(body, external_visit)?;
    Ok(JsClosure::from_parts(handle, function_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pelt::PeltField as _;
    use crate::{Value, alloc_upvalue, eval_env::EvalEnvBody, upvalue::UpvalueCellBody};

    fn alloc_closure_across_forced_full_gc(with_external_roots: bool) {
        const HEAP_CAP: u64 = 4 * 1024;

        let mut heap = GcHeap::with_max_heap_bytes(HEAP_CAP).expect("heap");
        let mut bound_new_target = None;
        let mut bound_derived_this = None;
        let mut eval_env = None;

        let this_object = crate::object::alloc_object_with_roots(&mut heap, &mut |_| {})
            .expect("young bound this");
        let mut bound_this = Some(Value::object(this_object));

        let target_object = {
            let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
                trace_pending_call_fields(
                    &mut bound_this,
                    &mut bound_new_target,
                    &mut bound_derived_this,
                    &mut eval_env,
                    visitor,
                );
            };
            crate::object::alloc_object_with_roots(&mut heap, &mut roots)
                .expect("young bound new.target")
        };
        bound_new_target = Some(Value::object(target_object));

        let derived_cell = {
            let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
                trace_pending_call_fields(
                    &mut bound_this,
                    &mut bound_new_target,
                    &mut bound_derived_this,
                    &mut eval_env,
                    visitor,
                );
            };
            heap.alloc_with_roots(
                UpvalueCellBody {
                    value: Value::number_i32(303),
                },
                &mut roots,
            )
            .expect("young derived-this cell")
        };
        bound_derived_this = Some(derived_cell);

        let env = {
            let derived_cell = bound_derived_this.expect("derived cell");
            let body = EvalEnvBody {
                names: vec!["evalSentinel".to_string()],
                cells: vec![derived_cell],
                parent: None,
            };
            let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
                trace_pending_call_fields(
                    &mut bound_this,
                    &mut bound_new_target,
                    &mut bound_derived_this,
                    &mut eval_env,
                    visitor,
                );
            };
            heap.alloc_with_roots(body, &mut roots)
                .expect("young eval env")
        };
        eval_env = Some(env);

        let mut captured = {
            let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
                trace_pending_call_fields(
                    &mut bound_this,
                    &mut bound_new_target,
                    &mut bound_derived_this,
                    &mut eval_env,
                    visitor,
                );
            };
            heap.alloc_with_roots(
                UpvalueCellBody {
                    value: Value::number_i32(404),
                },
                &mut roots,
            )
            .expect("young captured cell")
        };

        let mut external = if with_external_roots {
            let object = {
                let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
                    trace_pending_call_fields(
                        &mut bound_this,
                        &mut bound_new_target,
                        &mut bound_derived_this,
                        &mut eval_env,
                        visitor,
                    );
                    visitor(std::ptr::addr_of_mut!(captured).cast::<RawGc>());
                };
                crate::object::alloc_object_with_roots(&mut heap, &mut roots)
                    .expect("young external root")
            };
            Some(Value::object(object))
        } else {
            None
        };

        let this_shape = crate::object::shape_id(
            bound_this
                .expect("bound this")
                .as_object()
                .expect("bound this object"),
            &heap,
        );
        let target_shape = crate::object::shape_id(
            bound_new_target
                .expect("bound new.target")
                .as_object()
                .expect("bound new.target object"),
            &heap,
        );
        let external_shape = external.map(|value| {
            crate::object::shape_id(value.as_object().expect("external object"), &heap)
        });
        let original_offsets = [
            bound_this
                .expect("bound this")
                .as_object()
                .expect("bound this object")
                .offset(),
            bound_new_target
                .expect("bound new.target")
                .as_object()
                .expect("bound new.target object")
                .offset(),
            bound_derived_this.expect("derived cell").offset(),
            eval_env.expect("eval env").offset(),
            captured.offset(),
        ];

        // Fill the capped heap without crossing it. The next spine allocation
        // must overshoot, collect the unrooted filler cells, and retry while
        // rewriting every pending closure-call field in place.
        let before_filler = heap.tracked_bytes();
        {
            let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
                trace_pending_call_fields(
                    &mut bound_this,
                    &mut bound_new_target,
                    &mut bound_derived_this,
                    &mut eval_env,
                    visitor,
                );
                visitor(std::ptr::addr_of_mut!(captured).cast::<RawGc>());
                external.pelt_trace(visitor);
            };
            let _ = heap
                .alloc_old_with_roots(
                    UpvalueCellBody {
                        value: Value::undefined(),
                    },
                    &mut roots,
                )
                .expect("first filler");
        }
        let filler_bytes = heap.tracked_bytes() - before_filler;
        assert!(filler_bytes > 0);
        while heap.tracked_bytes().saturating_add(filler_bytes) <= HEAP_CAP {
            let mut roots = |visitor: &mut dyn FnMut(*mut RawGc)| {
                trace_pending_call_fields(
                    &mut bound_this,
                    &mut bound_new_target,
                    &mut bound_derived_this,
                    &mut eval_env,
                    visitor,
                );
                visitor(std::ptr::addr_of_mut!(captured).cast::<RawGc>());
                external.pelt_trace(visitor);
            };
            let _ = heap
                .alloc_old_with_roots(
                    UpvalueCellBody {
                        value: Value::undefined(),
                    },
                    &mut roots,
                )
                .expect("filler");
        }
        assert!(HEAP_CAP - heap.tracked_bytes() < filler_bytes);
        let collections_before = heap.gc_stats().gc_cycles;

        let closure = if with_external_roots {
            let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                external.pelt_trace(visitor);
            };
            alloc_closure_with_roots(
                &mut heap,
                71,
                vec![captured],
                bound_this,
                bound_new_target,
                bound_derived_this,
                eval_env,
                &mut external_visit,
            )
            .expect("closure after forced collection")
        } else {
            alloc_closure(
                &mut heap,
                71,
                vec![captured],
                bound_this,
                bound_new_target,
                bound_derived_this,
                eval_env,
            )
            .expect("closure after forced collection")
        };

        assert!(heap.gc_stats().gc_cycles > collections_before);
        let stored_this = closure
            .bound_this(&heap)
            .expect("stored this")
            .as_object()
            .expect("stored this object");
        let stored_target = closure
            .bound_new_target(&heap)
            .expect("stored new.target")
            .as_object()
            .expect("stored new.target object");
        let stored_derived = closure
            .bound_derived_this(&heap)
            .expect("stored derived-this cell");
        let stored_env = closure.eval_env(&heap).expect("stored eval env");
        let stored_capture = closure.upvalues_snapshot(&heap)[0];

        assert_eq!(crate::object::shape_id(stored_this, &heap), this_shape);
        assert_eq!(crate::object::shape_id(stored_target, &heap), target_shape);
        assert_eq!(
            crate::read_upvalue(&heap, stored_derived),
            Value::number_i32(303)
        );
        assert_eq!(
            crate::read_upvalue(&heap, stored_capture),
            Value::number_i32(404)
        );
        heap.read_payload(stored_env, |body| {
            assert_eq!(body.names.len(), 1);
            assert_eq!(body.names[0], "evalSentinel");
            assert_eq!(body.cells.as_slice(), &[stored_derived]);
        });
        assert!(
            [
                stored_this.offset(),
                stored_target.offset(),
                stored_derived.offset(),
                stored_env.offset(),
                stored_capture.offset(),
            ]
            .iter()
            .zip(original_offsets)
            .any(|(after, before)| *after != before),
            "forced full GC must relocate at least one young capture"
        );

        if let (Some(value), Some(shape)) = (external, external_shape) {
            assert_eq!(
                crate::object::shape_id(value.as_object().expect("external object"), &heap),
                shape
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
    fn allocates_empty_closure() {
        let mut heap = GcHeap::new().expect("heap");
        let closure =
            alloc_closure(&mut heap, 7, Vec::new(), None, None, None, None).expect("alloc");
        assert_eq!(closure.function_id(), 7);
        assert_eq!(closure.bound_this(&heap), None);
        assert_eq!(closure.bound_new_target(&heap), None);
        assert!(!closure.requires_runtime_setup(&heap));
        heap.read_payload(closure.handle(), |body| {
            assert_eq!(body.call_header.function_id, 7);
            assert_eq!(body.call_header.flags, 0);
            assert_eq!(body.call_header.upvalue_base, 0);
            assert_eq!(body.call_header.upvalue_count, 0);
            assert!(body.call_header.eval_env.is_null());
            assert!(body.spine.is_null(), "no captures means no spine cell");
            assert!(body.bound_this.is_undefined());
            assert!(body.bound_new_target.is_undefined());
        });
    }

    #[test]
    fn allocates_closure_with_upvalues_and_bound_this() {
        let mut heap = GcHeap::new().expect("heap");
        let cell_a = alloc_upvalue(&mut heap, Value::undefined()).expect("cell");
        let cell_b = alloc_upvalue(&mut heap, Value::undefined()).expect("cell");
        let upvalues = vec![cell_a, cell_b];
        let closure = alloc_closure(
            &mut heap,
            42,
            upvalues,
            Some(Value::null()),
            None,
            None,
            None,
        )
        .expect("alloc");
        assert_eq!(closure.function_id(), 42);
        assert_eq!(closure.upvalue_count(&heap), 2);
        assert_eq!(closure.bound_this(&heap), Some(Value::null()));
        assert_eq!(closure.bound_new_target(&heap), None);
        let call_state = closure.call_state(&heap);
        assert_eq!(call_state.upvalues.len(), 2);
        assert_eq!(call_state.upvalues.read(0), Some(cell_a));
        assert_eq!(call_state.upvalues.read(1), Some(cell_b));
        let spine = heap.read_payload(closure.handle(), |body| {
            assert_eq!(body.call_header.function_id, 42);
            assert_eq!(body.call_header.upvalue_count, 2);
            assert!(body.call_header.has_flag(CLOSURE_CALL_FLAG_BOUND_THIS));
            assert!(
                !body
                    .call_header
                    .has_flag(CLOSURE_CALL_FLAG_BOUND_NEW_TARGET)
            );
            assert!(body.bound_this.is_null());
            assert!(body.bound_new_target.is_undefined());
            body.spine
        });
        // The published base is the spine's trailing array, so compiled
        // code and the call state read the same cells.
        let base = crate::upvalue_spine::cells_base_address(spine);
        assert_eq!(
            heap.read_payload(closure.handle(), |body| body.call_header.upvalue_base),
            base
        );
        assert_eq!(call_state.upvalues.base_ptr_or_null() as usize as u64, base);
        assert_eq!(
            heap.read_payload(spine, |body| body.cells().to_vec()),
            vec![cell_a, cell_b]
        );
    }

    #[test]
    fn presence_flags_distinguish_some_undefined_from_none() {
        let mut heap = GcHeap::new().expect("heap");
        let closure = alloc_closure(
            &mut heap,
            9,
            Vec::new(),
            Some(Value::undefined()),
            None,
            None,
            None,
        )
        .expect("alloc");

        assert_eq!(closure.bound_this(&heap), Some(Value::undefined()));
        assert_eq!(closure.bound_new_target(&heap), None);
        heap.read_payload(closure.handle(), |body| {
            assert!(body.bound_this.is_undefined());
            assert!(body.bound_new_target.is_undefined());
            assert!(body.call_header.has_flag(CLOSURE_CALL_FLAG_BOUND_THIS));
            assert!(
                !body
                    .call_header
                    .has_flag(CLOSURE_CALL_FLAG_BOUND_NEW_TARGET)
            );
        });
    }

    #[test]
    fn semantic_tail_flags_and_eval_env_use_distinct_call_paths() {
        let mut heap = GcHeap::new().expect("heap");
        let derived_this = alloc_upvalue(&mut heap, Value::hole()).expect("derived this");
        let eval_env = crate::eval_env::alloc_eval_env(&mut heap, None).expect("eval env");
        let closure = alloc_closure(
            &mut heap,
            1,
            Vec::new(),
            None,
            Some(Value::null()),
            Some(derived_this),
            Some(eval_env),
        )
        .expect("closure");
        let header = closure.call_header(&heap);
        assert!(header.has_flag(CLOSURE_CALL_FLAG_BOUND_NEW_TARGET));
        assert!(header.has_flag(CLOSURE_CALL_FLAG_BOUND_DERIVED_THIS));
        assert!(header.requires_runtime_setup());
        assert_eq!(header.eval_env, eval_env);
        assert_eq!(closure.bound_new_target(&heap), Some(Value::null()));
        assert_eq!(closure.bound_derived_this(&heap), Some(derived_this));
        assert_eq!(closure.eval_env(&heap), Some(eval_env));

        let lexical_this_only = ClosureCallHeader {
            function_id: 1,
            flags: CLOSURE_CALL_FLAG_BOUND_THIS,
            upvalue_base: 0,
            upvalue_count: 0,
            eval_env,
        };
        assert!(!lexical_this_only.requires_runtime_setup());
        assert_eq!(lexical_this_only.eval_env, eval_env);
    }

    #[test]
    fn closure_call_abi_layout_is_stable() {
        assert_eq!(std::mem::size_of::<ClosureCallHeader>(), 24);
        assert_eq!(std::mem::align_of::<ClosureCallHeader>(), 8);
        assert_eq!(CLOSURE_BODY_FUNCTION_ID_OFFSET, 0);
        assert_eq!(CLOSURE_BODY_CALL_FLAGS_OFFSET, 4);
        assert_eq!(CLOSURE_BODY_UPVALUE_BASE_OFFSET, 8);
        assert_eq!(CLOSURE_BODY_UPVALUE_COUNT_OFFSET, 16);
        assert_eq!(CLOSURE_BODY_EVAL_ENV_OFFSET, 20);
        assert_eq!(CLOSURE_BODY_BOUND_THIS_OFFSET, 24);
        assert_eq!(CLOSURE_BODY_BOUND_NEW_TARGET_OFFSET, 32);
    }

    #[test]
    fn type_tag_matches_traceable_const() {
        assert_eq!(
            <JsClosureBody as otter_gc::SafeTraceable>::TYPE_TAG,
            JS_CLOSURE_BODY_TYPE_TAG
        );
    }
}
