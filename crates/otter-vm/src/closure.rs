//! GC body for closure values.
//!
//! A closure carries:
//!
//! - the bytecode function id it executes,
//! - the captured upvalue spine (one [`crate::UpvalueCell`] per
//!   binding, in declaration order),
//! - an optional bound `this` (arrow closures capture their receiver
//!   lexically; non-arrow closures take `this` from the call site),
//! - an optional bound `new.target` for arrow closures.
//! - an optional derived-constructor `this` cell for arrow
//!   `super()` calls that run after the original frame is off-stack.
//!
//! # Contents
//!
//! - [`ClosureCallHeader`] — stable machine-facing call ABI prefix.
//! - [`ClosureCallState`] — allocation-neutral VM call metadata.
//! - [`JsClosureBody`] — GC body holding the ABI prefix, canonical
//!   bound values, and the Rust-owned traced tail.
//! - [`JsClosure`] — 8-byte handle plus cached function id.
//! - [`alloc_closure`] / [`alloc_closure_with_roots`] — allocators.
//! - [`JS_CLOSURE_BODY_TYPE_TAG`] — reserved
//!   [`otter_gc::Traceable::TYPE_TAG`].
//!
//! # Invariants
//!
//! - The machine-facing prefix is `#[repr(C)]`: native linkage may read
//!   [`ClosureCallHeader`], `bound_this`, and `bound_new_target` only. It
//!   must never interpret the following Rust `Vec` / `Option` layout.
//! - The upvalue slice is built once at closure creation
//!   ([`Op::MakeClosure`](otter_bytecode::Op::MakeClosure)) and never
//!   resized. Its backing allocation therefore matches the immutable
//!   `upvalue_base` / `upvalue_count` pair for the closure's lifetime;
//!   native code must not retain that base beyond the live call.
//!   Per-cell mutation flows through
//!   [`crate::store_upvalue`] / [`crate::read_upvalue`].
//! - Canonical `Value` fields are always traced. Presence flags distinguish
//!   `None` from `Some(undefined)` while [`JsClosure`] keeps the ergonomic
//!   `Option<Value>` API.
//! - Bound `new.target`, derived-constructor `this`, and direct-eval
//!   environments require the call-setup runtime stub. The stub establishes
//!   that state and returns to the compiled callee; it does not force the
//!   activation out of the native tier. Their flags are stable even though the
//!   Rust-only tail uses `Option` for the high-level implementation.
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

use otter_gc::GcHeap;
use otter_gc::OutOfMemory;
use otter_gc::heap::RootSlotVisitor;
use otter_gc::raw::{RawGc, SlotVisitor};
use otter_macros::Pelt;

use crate::object::JsObject;
use crate::{UpvalueCell, Value, upvalue_source::UpvalueSource};

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`JsClosureBody`].
pub const JS_CLOSURE_BODY_TYPE_TAG: u8 = 0x23;

/// [`ClosureCallHeader::flags`] bit: `bound_this` is semantically present.
pub const CLOSURE_CALL_FLAG_BOUND_THIS: u32 = 1 << 0;
/// [`ClosureCallHeader::flags`] bit: `bound_new_target` is semantically present.
pub const CLOSURE_CALL_FLAG_BOUND_NEW_TARGET: u32 = 1 << 1;
/// [`ClosureCallHeader::flags`] bit: the Rust tail carries a derived-`this` cell.
pub const CLOSURE_CALL_FLAG_BOUND_DERIVED_THIS: u32 = 1 << 2;
/// [`ClosureCallHeader::flags`] bit: the Rust tail carries a direct-eval environment.
pub const CLOSURE_CALL_FLAG_EVAL_ENV: u32 = 1 << 3;

/// Flags whose semantics require the call-setup runtime stub.
///
/// Native linkage handles lexical `this` inline. Lexical `new.target`, shared
/// derived-constructor state, and eval environments route through setup before
/// control returns to the compiled callee in the same native activation.
pub const CLOSURE_CALL_RUNTIME_SETUP_FLAGS: u32 = CLOSURE_CALL_FLAG_BOUND_NEW_TARGET
    | CLOSURE_CALL_FLAG_BOUND_DERIVED_THIS
    | CLOSURE_CALL_FLAG_EVAL_ENV;

/// Stable machine-facing closure call metadata.
///
/// All addresses use fixed-width integers instead of Rust references. The
/// `upvalue_base` points at the immutable backing allocation of the body's
/// `Vec<UpvalueCell>` and is valid only while the closure remains live; it is
/// not a movable GC-object pointer and must not be cached across calls.
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
}

/// Allocation-neutral closure state consumed by call preparation.
///
/// `upvalues` borrows the closure's stable external vector allocation without
/// constructing a `Vec`/`Box`. The exact closure value must remain rooted for
/// every use of this record that can cross a collection.
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
        upvalues: &[UpvalueCell],
        bound_this: bool,
        bound_new_target: bool,
        bound_derived_this: bool,
        eval_env: bool,
    ) -> Self {
        let upvalue_count =
            u32::try_from(upvalues.len()).expect("closure upvalue spine exceeds the u32 call ABI");
        let upvalue_base = if upvalues.is_empty() {
            0
        } else {
            upvalues.as_ptr() as usize as u64
        };
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
        if eval_env {
            flags |= CLOSURE_CALL_FLAG_EVAL_ENV;
        }
        Self {
            function_id,
            flags,
            upvalue_base,
            upvalue_count,
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
    /// still remains in a [`crate::jit::VmRuntimeActivation`]: the setup stub
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
/// Everything after it is a traced Rust implementation detail.
#[repr(C, align(8))]
#[derive(Debug, Pelt)]
#[pelt(tag = JS_CLOSURE_BODY_TYPE_TAG)]
pub struct JsClosureBody {
    /// Fixed-layout metadata read by native linkage.
    #[pelt(skip)]
    pub call_header: ClosureCallHeader,
    /// Canonical traced lexical `this`; consult the header flag for presence.
    pub bound_this: Value,
    /// Canonical traced lexical `new.target`; consult the header flag for presence.
    pub bound_new_target: Value,
    /// Captured upvalue spine in declaration order. Per-cell mutation
    /// flows through [`crate::store_upvalue`] /
    /// [`crate::read_upvalue`]; the slice itself never resizes.
    pub upvalues: Vec<UpvalueCell>,
    /// Arrow closures created inside derived constructors capture the
    /// constructor's shared `this` cell so `super()` can bind it even
    /// when the arrow is invoked through a nested sync dispatch.
    pub bound_derived_this: Option<UpvalueCell>,
    /// §9.1 — the creating frame's direct-eval variable environment
    /// (when any enclosing function contains a direct eval call
    /// site). Calls re-expose it so eval-introduced `var` bindings
    /// stay visible through this closure's scope chain.
    pub eval_env: Option<crate::eval_env::EvalEnvHandle>,
    /// §10.2 — this closure instance's own-property bag. Each function
    /// object created by evaluating a function expression/declaration
    /// owns a DISTINCT property store (`f.foo = 1`, the materialized
    /// `f.prototype`, etc.), so it lives per-instance here rather than
    /// in a side table keyed by the bytecode template id (which every
    /// sibling closure of the same source would share). `None` until
    /// the first own property or `prototype` materialization.
    pub own_props: Option<JsObject>,
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
const _: [(); 0] = [(); CLOSURE_BODY_CALL_HEADER_OFFSET];
const _: [(); 24] = [(); CLOSURE_BODY_BOUND_THIS_OFFSET];
const _: [(); 32] = [(); CLOSURE_BODY_BOUND_NEW_TARGET_OFFSET];

impl JsClosureBody {
    fn new(
        function_id: u32,
        upvalues: Vec<UpvalueCell>,
        bound_this: Option<Value>,
        bound_new_target: Option<Value>,
        bound_derived_this: Option<UpvalueCell>,
        eval_env: Option<crate::eval_env::EvalEnvHandle>,
    ) -> Self {
        let call_header = ClosureCallHeader::new(
            function_id,
            &upvalues,
            bound_this.is_some(),
            bound_new_target.is_some(),
            bound_derived_this.is_some(),
            eval_env.is_some(),
        );
        Self {
            call_header,
            bound_this: bound_this.unwrap_or_else(Value::undefined),
            bound_new_target: bound_new_target.unwrap_or_else(Value::undefined),
            upvalues,
            bound_derived_this,
            eval_env,
            own_props: None,
        }
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
        debug_assert_eq!(
            self.call_header.has_flag(CLOSURE_CALL_FLAG_EVAL_ENV),
            self.eval_env.is_some()
        );
        self.eval_env
    }

    /// Copy call metadata while borrowing the immutable upvalue allocation.
    fn call_state(&self) -> ClosureCallState {
        // SAFETY: closure upvalue vectors are built once and never resized. A
        // consumer of ClosureCallState must root the exact closure value for
        // the record's live extent, as documented on the record itself.
        let upvalues = unsafe { UpvalueSource::from_stable_slice(&self.upvalues) }
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
        heap.read_payload(self.handle, |body| f(&body.upvalues))
    }

    /// Snapshot the captured upvalue spine into a fresh `Vec`.
    /// Use when the caller needs to return cells across a borrow
    /// boundary; otherwise prefer [`Self::with_upvalues`].
    #[must_use]
    pub fn upvalues_snapshot(self, heap: &GcHeap) -> Vec<UpvalueCell> {
        heap.read_payload(self.handle, |body| body.upvalues.to_vec())
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

/// Allocate a closure body in old-space (consistent with
/// [`crate::alloc_upvalue`]; the scavenger does not yet rewrite
/// embedded `UpvalueCell` slots).
///
/// # Errors
///
/// Surfaces [`OutOfMemory`] verbatim.
pub fn alloc_closure(
    heap: &mut GcHeap,
    function_id: u32,
    upvalues: Vec<UpvalueCell>,
    bound_this: Option<Value>,
    bound_new_target: Option<Value>,
    bound_derived_this: Option<UpvalueCell>,
    eval_env: Option<crate::eval_env::EvalEnvHandle>,
) -> Result<JsClosure, OutOfMemory> {
    let body = JsClosureBody::new(
        function_id,
        upvalues,
        bound_this,
        bound_new_target,
        bound_derived_this,
        eval_env,
    );
    let handle = heap.alloc_old(body)?;
    Ok(JsClosure::from_parts(handle, function_id))
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
    bound_this: Option<Value>,
    bound_new_target: Option<Value>,
    bound_derived_this: Option<UpvalueCell>,
    eval_env: Option<crate::eval_env::EvalEnvHandle>,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<JsClosure, OutOfMemory> {
    let body = JsClosureBody::new(
        function_id,
        upvalues,
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
    use crate::alloc_upvalue;

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
            assert!(body.upvalues.is_empty());
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
        heap.read_payload(closure.handle(), |body| {
            assert_eq!(body.call_header.function_id, 42);
            assert_eq!(body.call_header.upvalue_count, 2);
            assert_eq!(
                body.call_header.upvalue_base,
                body.upvalues.as_ptr() as usize as u64
            );
            assert_eq!(
                call_state.upvalues.base_ptr_or_null(),
                body.upvalues.as_ptr().cast_mut()
            );
            assert!(body.call_header.has_flag(CLOSURE_CALL_FLAG_BOUND_THIS));
            assert!(
                !body
                    .call_header
                    .has_flag(CLOSURE_CALL_FLAG_BOUND_NEW_TARGET)
            );
            assert_eq!(body.upvalues.len(), 2);
            assert_eq!(body.upvalues[0], cell_a);
            assert_eq!(body.upvalues[1], cell_b);
            assert!(body.bound_this.is_null());
            assert!(body.bound_new_target.is_undefined());
        });
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
    fn semantic_tail_flags_require_runtime_setup() {
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
        assert!(header.has_flag(CLOSURE_CALL_FLAG_EVAL_ENV));
        assert!(header.requires_runtime_setup());
        assert_eq!(closure.bound_new_target(&heap), Some(Value::null()));
        assert_eq!(closure.bound_derived_this(&heap), Some(derived_this));
        assert_eq!(closure.eval_env(&heap), Some(eval_env));

        let lexical_this_only = ClosureCallHeader {
            function_id: 1,
            flags: CLOSURE_CALL_FLAG_BOUND_THIS,
            upvalue_base: 0,
            upvalue_count: 0,
        };
        assert!(!lexical_this_only.requires_runtime_setup());
    }

    #[test]
    fn closure_call_abi_layout_is_stable() {
        assert_eq!(std::mem::size_of::<ClosureCallHeader>(), 24);
        assert_eq!(std::mem::align_of::<ClosureCallHeader>(), 8);
        assert_eq!(CLOSURE_BODY_FUNCTION_ID_OFFSET, 0);
        assert_eq!(CLOSURE_BODY_CALL_FLAGS_OFFSET, 4);
        assert_eq!(CLOSURE_BODY_UPVALUE_BASE_OFFSET, 8);
        assert_eq!(CLOSURE_BODY_UPVALUE_COUNT_OFFSET, 16);
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
