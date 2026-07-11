//! Dependency-inverted baseline-JIT hook surface.
//!
//! This module defines the safe VM-side contract used by an external JIT crate.
//! `otter-vm` owns bytecode metadata, call-frame layout, property-IC site ids,
//! and GC rooting rules; `otter-jit` owns executable memory and machine-code
//! emission. The VM therefore exposes owned compile-input DTOs and accepts a
//! trait object installed by the runtime layer, avoiding any dependency from
//! `otter-vm` back to `otter-jit`.
//!
//! # Contents
//! - [`JitCompileSnapshot`] and [`JitInstructionMetadata`] — immutable
//!   CodeBlock plus per-tier feedback/layout metadata.
//! - [`JitCompilerHook`] — runtime-installed compile hook implemented outside
//!   `otter-vm`.
//! - [`JitFunctionCode`] and [`JitCompileStatus`] — type-erased compiled-code
//!   result handles that keep executable memory ownership outside this crate.
//!
//! # Invariants
//! - DTOs are owned and borrow-free. JIT compilation must not hold references
//!   into `ExecutionContext`, `CodeBlock`, or interpreter frames.
//! - No unsafe is required here. Native entry pointers, executable mappings, and
//!   call ABI details remain encapsulated by the JIT implementation crate.
//! - Baseline v1 uses the interpreter frame register array as its precise root
//!   provider. Values may be cached in machine registers only between
//!   safepoints; allocation and call slow paths must reload from frame slots.
//!
//! # See also
//! - [`crate::execution_context`] for snapshot creation from frozen bytecode.
//! - [`crate::Frame`] for the traced register array the baseline tier reuses.
//! - `JIT_DESIGN.md` §3.2, §3.5, and §4 for backend, GC, and phasing.

use std::sync::Arc;

use otter_bytecode::{Op, Operand};

use crate::{
    CodeBlock, CodeBlockInstruction,
    native_abi::{SafepointId, SafepointRecord},
};

/// Owned compile request for one bytecode function.
#[derive(Debug, Clone)]
pub struct JitCompileRequest {
    /// Code and feedback snapshot to compile.
    pub snapshot: JitCompileSnapshot,
    /// Loop-header byte-PC for an OSR-target compile. `None` means normal
    /// function-entry compilation.
    pub osr_pc: Option<u32>,
}

/// Owned snapshot of one executable function body.
#[derive(Debug, Clone)]
pub struct JitCompileSnapshot {
    /// Exact immutable executable body this feedback overlay decorates.
    ///
    /// Function identity, register-window shape, instruction stream, and
    /// function-mode flags are owned solely by this `CodeBlock`. The JIT must
    /// not keep a second scalar representation of executable state.
    pub code_block: Arc<CodeBlock>,
    /// GC cage base address (`otter_gc::cage_base()`), baked at compile time.
    /// Stable for the isolate's life, so emitted inline property loads add it
    /// to a compressed `Gc` offset to decompress an object pointer without a
    /// runtime load. `0` when no inline access is baked.
    pub cage_base: usize,
    /// Static heap-layout offsets for inline typed-array element access. Baked
    /// once at compile time from `otter-vm`'s `#[repr(C)]` body layouts so the
    /// emitter stays layout-agnostic. The emitter inlines `LoadElement` /
    /// `StoreElement` for monomorphic `Float64Array` / `Int32Array` receivers
    /// only when [`cage_base`](Self::cage_base) is non-zero (baked).
    pub ta_layout: JitTypedArrayLayout,
    /// Static heap-layout offsets for inline primitive string `.length`.
    pub string_layout: JitStringLayout,
    /// Byte offset from a decompressed object pointer to its shape handle
    /// (`HEADER_SIZE + OBJECT_BODY_SHAPE_OFFSET`). A `#[repr(C)]` constant; the
    /// emitter reads `[obj_ptr + object_shape_byte]` for the WhiskerIC
    /// `LoadProperty` cell guard, staying layout-agnostic.
    pub object_shape_byte: u32,
    /// Byte offset from a decompressed object pointer to its cached
    /// string-keyed value slab pointer (`HEADER_SIZE +
    /// OBJECT_BODY_VALUES_PTR_OFFSET`). The emitter reads this pointer after a
    /// shape guard and applies the cached slot-byte offset inside the slab.
    pub object_values_ptr_byte: u32,
    /// Byte offset from a decompressed object pointer to its in-body inline slab
    /// (`HEADER_SIZE + OBJECT_BODY_INLINE_VALUES_OFFSET`). A small object
    /// (`slab_len <= `[`object_inline_slot_cap`](Self::object_inline_slot_cap))
    /// keeps its slots here, in the body itself. The emitter addresses the slab
    /// as `header + object_inline_values_byte` for such an object instead of
    /// loading the cached `values_ptr`: the cached pointer aims into the body and
    /// so is only valid until the moving collector relocates the object, whereas
    /// the header is recomputed from the (rooted) receiver every access and never
    /// dangles.
    pub object_inline_values_byte: u32,
    /// Byte offset from a decompressed object pointer to the `u16`
    /// [`slab_len`](crate::object) counter (`HEADER_SIZE +
    /// OBJECT_BODY_SLAB_LEN_OFFSET`). The emitter reads it to pick the inline vs
    /// out-of-line slab base.
    pub object_slab_len_byte: u32,
    /// Inline slab capacity (`INLINE_SLOT_CAP`): a body with this many
    /// string-keyed slots or fewer holds them inline; a larger one spills to the
    /// out-of-line `values` vector whose base is a stable heap allocation.
    pub object_inline_slot_cap: u32,
    /// Static GC layout for the inline generational write barrier emitted on a
    /// pointer-valued `StoreProperty`. Isolate-independent `#[repr(C)]` / `const`
    /// values; the card-mark is gated on [`cage_base`](Self::cage_base) being
    /// baked (the emitter decompresses parent/child pointers against it).
    pub gc_barrier: JitGcBarrierLayout,
    /// Byte offset from a decompressed object pointer to its flat
    /// `[[Prototype]]` mirror (`HEADER_SIZE + OBJECT_BODY_JIT_PROTO_OFFSET`). A
    /// `#[repr(C)]` constant; the method-inline guard reads
    /// `[recv_ptr + jit_proto_byte]` to chase the receiver's prototype chain
    /// in machine code without a resolve bridge.
    pub jit_proto_byte: u32,
    /// `GcHeader::type_tag` for heap-number boxes referenced by compressed
    /// object slots.
    pub heap_number_type_tag: u8,
    /// Byte offset from a decompressed heap-number pointer to its raw boxed
    /// `Value` bits (`HEADER_SIZE + offset_of!(HeapNumberBody, bits)`).
    pub heap_number_bits_byte: u32,
    /// Byte offset from a decompressed closure pointer to its `function_id`
    /// (`HEADER_SIZE + offset_of!(JsClosureBody, function_id)`). The
    /// method-inline guard reads `[closure_ptr + closure_fid_byte]` to compare
    /// a resolved prototype method against the baked target id.
    pub closure_fid_byte: u32,
    /// Byte offset from a decompressed closure pointer to the data pointer of
    /// its captured upvalue spine (`HEADER_SIZE + offset_of!(JsClosureBody,
    /// upvalues)`; the `Vec<UpvalueCell>` stores its backing pointer in its
    /// first word). An inlined closure body reads `[closure_ptr +
    /// closure_upvalues_ptr_byte]` to reach the spine, then the per-index
    /// compressed cell handle, mirroring the context-spine [`LoadUpvalue`] path.
    pub closure_upvalues_ptr_byte: u32,
    /// Ready-to-use byte offsets and type tags for baseline collection method
    /// IC guards.
    pub collection_layout: JitCollectionLayout,
    /// Byte offset from a decompressed native-function pointer to its
    /// machine-readable static builtin identity.
    pub native_static_fn_byte: u32,
    /// Instruction stream in byte-PC order.
    pub instructions: Vec<JitInstructionMetadata>,
    /// Inline-candidate callees for baseline leaf-inlining, keyed by the
    /// caller's `Op::Call` byte-PC. Populated only for sites the interpreter
    /// observed resolving to a single plain synchronous bytecode callee; baked
    /// by `Interpreter::bake_inline_callees`. Empty in the raw compile snapshot
    /// snapshot. The emitter applies the final pure-leaf / size / arity test and
    /// either splices the body under an identity guard or — for a site absent
    /// here, or one whose candidate fails the test — emits the normal
    /// direct-call bridge.
    pub inline_callees: rustc_hash::FxHashMap<u32, JitInlineCallee>,
    /// Inline-candidate methods for `Op::CallMethodValue` sites, keyed by the
    /// caller's call byte-PC. Populated for monomorphic method sites whose method
    /// is a tiny body of sealed property loads/stores and pure ops; baked by
    /// `Interpreter::bake_inline_callees`.
    pub inline_methods: rustc_hash::FxHashMap<u32, JitInlineMethod>,
    /// Inline-candidate method chains for *polymorphic* `Op::CallMethodValue`
    /// sites, keyed by the caller's call byte-PC. Each value is the
    /// most-frequent-first list (length ≥ 2) of per-receiver-shape inline
    /// methods the baseline emits as a guard chain: each entry guards its own
    /// receiver shape + prototype-method identity and, on a miss, falls through
    /// to the next entry; a receiver matching none of them takes the in-place
    /// method bridge. Baked by `Interpreter::bake_inline_callees`. The optimizing
    /// tier ignores this map and bridges polymorphic method sites.
    pub inline_poly_methods: rustc_hash::FxHashMap<u32, Vec<JitInlineMethod>>,
    /// Leaf collection method-call feedback keyed by the caller's
    /// `Op::CallMethodValue` byte-PC. These entries are fully JIT-readable:
    /// generated code can validate the receiver/prototype/builtin guards and
    /// call the VM-native leaf stub without a Rust resolver bridge.
    pub collection_leaf_methods: rustc_hash::FxHashMap<u32, JitCollectionLeafMethod>,
    /// Allocating collection method-call feedback keyed by the caller's
    /// `Op::CallMethodValue` byte-PC. These entries carry the same
    /// receiver/prototype/builtin guards as leaf feedback plus the target
    /// allocating stub id. Generated code must still attach an exact safepoint
    /// for the call site before it may invoke the stub.
    pub collection_alloc_methods: rustc_hash::FxHashMap<u32, JitCollectionAllocMethod>,
    /// Dense-array `push` / `pop` method-call feedback keyed by the caller's
    /// `Op::CallMethodValue` byte-PC. Each entry carries the receiver guard's
    /// prototype/shape/builtin metadata so the baseline can splice an inline
    /// fast path (length bump + element move) under a guard, with the runtime
    /// method bridge as the miss fallback.
    pub array_methods: rustc_hash::FxHashMap<u32, JitArrayMethod>,
    /// Primitive builtin method guard metadata keyed by the caller's
    /// `Op::CallMethodValue` byte-PC. Each entry validates the realm prototype
    /// shape and method slot before a primitive-specific leaf stub runs.
    pub primitive_method_guards: rustc_hash::FxHashMap<u32, JitPrimitiveMethodGuard>,
    /// Safepoint records baked for allocating runtime-stub call sites, keyed by
    /// `SafepointId`. Baseline v1 uses frame-slot roots for the full register
    /// window, so allocating stubs can trigger moving GC without keeping raw
    /// untracked `Value` bits live only in machine registers.
    pub safepoints: rustc_hash::FxHashMap<SafepointId, SafepointRecord>,
}

/// Static collection body layout used by JIT-readable method IC guards.
#[derive(Debug, Clone, Copy, Default)]
pub struct JitCollectionLayout {
    /// `GcHeader::type_tag` for `Map` bodies.
    pub map_type_tag: u8,
    /// `GcHeader::type_tag` for `Set` bodies.
    pub set_type_tag: u8,
    /// Byte offset from a decompressed Map/Set pointer to the guard flags word.
    pub guard_flags_byte: u32,
    /// `GcHeader::type_tag` for native-function bodies.
    pub native_function_type_tag: u8,
}

/// JIT-readable leaf collection method IC entry.
#[derive(Debug, Clone, Copy)]
pub struct JitCollectionLeafMethod {
    /// Expected receiver body type tag (`Map` or `Set`).
    pub receiver_type_tag: u8,
    /// Compressed offset of the realm prototype object holding the builtin.
    pub proto_offset: u32,
    /// Expected prototype shape handle compressed offset.
    pub proto_shape: u32,
    /// Byte offset inside the prototype object's value slab for the method.
    pub method_value_byte: u32,
    /// Raw static native builtin function address expected in the method slot.
    pub builtin_fn_addr: usize,
    /// VM-native leaf stub descriptor id to call after guards pass.
    pub leaf_stub_id: crate::native_abi::RuntimeStubId,
}

/// JIT-readable allocating collection method IC entry.
#[derive(Debug, Clone, Copy)]
pub struct JitCollectionAllocMethod {
    /// Expected receiver body type tag (`Map` or `Set`).
    pub receiver_type_tag: u8,
    /// Compressed offset of the realm prototype object holding the builtin.
    pub proto_offset: u32,
    /// Expected prototype shape handle compressed offset.
    pub proto_shape: u32,
    /// Byte offset inside the prototype object's value slab for the method.
    pub method_value_byte: u32,
    /// Raw static native builtin function address expected in the method slot.
    pub builtin_fn_addr: usize,
    /// VM-native allocating stub descriptor id to call after guards pass and a
    /// precise safepoint is published for the current frame.
    pub alloc_stub_id: crate::native_abi::RuntimeStubId,
    /// Safepoint record to publish when calling the allocating stub.
    pub safepoint_id: crate::native_abi::SafepointId,
    /// Number of raw boxed `Value` arguments in the uniform mutation ABI. The
    /// current collection mutation shape is `(receiver, arg0, arg1_or_undefined)`.
    pub value_arg_count: u8,
}

/// Which dense-array builtin a [`JitArrayMethod`] guards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitArrayMethodKind {
    /// `Array.prototype.pop` — leaf, no allocation.
    Pop,
    /// `Array.prototype.push` — may grow the backing store.
    Push,
}

/// JIT-readable dense-array `push` / `pop` method IC entry.
///
/// Holds no GC pointer: `proto_offset` is a stable compressed offset of the
/// realm `%Array.prototype%`, `proto_shape` is a plain shape-handle offset, and
/// the builtin is checked against a stable native `fn` address. The inline fast
/// path validates the receiver is an ordinary dense array (no exotic sidecar)
/// and the prototype still carries the original builtin at the cached slot;
/// any miss falls through to the rooted runtime method bridge.
#[derive(Debug, Clone, Copy)]
pub struct JitArrayMethod {
    /// Compressed offset of the realm `%Array.prototype%` object.
    pub proto_offset: u32,
    /// Expected prototype shape handle compressed offset.
    pub proto_shape: u32,
    /// Byte offset inside the prototype object's value slab for the method.
    pub method_value_byte: u32,
    /// Raw static native builtin function address expected in the method slot.
    pub builtin_fn_addr: usize,
    /// Which builtin this site resolved to.
    pub kind: JitArrayMethodKind,
}

/// JIT-readable guard for primitive prototype builtin calls.
///
/// Holds only stable compressed offsets, shape handles, and a native entry
/// address. Generated code still reloads the prototype slot from the heap and
/// validates the native function identity before using any primitive leaf stub.
#[derive(Debug, Clone, Copy)]
pub struct JitPrimitiveMethodGuard {
    /// Compressed offset of the realm primitive prototype object.
    pub proto_offset: u32,
    /// Expected prototype shape handle compressed offset.
    pub proto_shape: u32,
    /// Byte offset inside the prototype object's value slab for the method.
    pub method_value_byte: u32,
    /// Raw static native builtin function address expected in the method slot.
    pub builtin_fn_addr: usize,
}

/// Empty [`JitCollectionMethodIcSlot::state`].
pub const JIT_COLLECTION_METHOD_IC_EMPTY: u8 = 0;
/// Live collection method IC slot.
pub const JIT_COLLECTION_METHOD_IC_COLLECTION: u8 = 1;
/// Sentinel for absent leaf/alloc runtime stub ids in
/// [`JitCollectionMethodIcSlot`].
pub const JIT_COLLECTION_METHOD_IC_NO_STUB: crate::native_abi::RuntimeStubId =
    crate::native_abi::RuntimeStubId::MAX;

/// C-layout live method IC slot for Map/Set builtin calls.
///
/// This is the JIT-readable mirror of `method_ops::CollectionMethodCallIc`.
/// It intentionally stores only non-GC metadata and raw codegen constants;
/// generated code must still revalidate receiver family, prototype shape,
/// builtin identity, and no receiver override/expando before using a stub id.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JitCollectionMethodIcSlot {
    /// Slot state; `0` means empty.
    pub state: u8,
    /// Expected Map/Set body type tag.
    pub receiver_type_tag: u8,
    /// Reserved for stable C layout.
    pub reserved0: u16,
    /// Canonical realm prototype compressed offset.
    pub proto_offset: u32,
    /// Canonical realm prototype shape compressed offset.
    pub proto_shape: u32,
    /// Byte offset of the method value inside the prototype value slab.
    pub method_value_byte: u32,
    /// Leaf/no-allocation stub id, or
    /// [`JIT_COLLECTION_METHOD_IC_NO_STUB`].
    pub leaf_stub_id: crate::native_abi::RuntimeStubId,
    /// Allocating stub id, or [`JIT_COLLECTION_METHOD_IC_NO_STUB`].
    pub alloc_stub_id: crate::native_abi::RuntimeStubId,
    /// Canonical builtin native entry identity.
    pub builtin_fn_addr: usize,
}

impl JitCollectionMethodIcSlot {
    /// Empty slot.
    pub const EMPTY: Self = Self {
        state: JIT_COLLECTION_METHOD_IC_EMPTY,
        receiver_type_tag: 0,
        reserved0: 0,
        proto_offset: 0,
        proto_shape: 0,
        method_value_byte: 0,
        leaf_stub_id: JIT_COLLECTION_METHOD_IC_NO_STUB,
        alloc_stub_id: JIT_COLLECTION_METHOD_IC_NO_STUB,
        builtin_fn_addr: 0,
    };

    /// `true` when this slot contains a collection IC payload.
    #[must_use]
    pub const fn is_collection(self) -> bool {
        self.state == JIT_COLLECTION_METHOD_IC_COLLECTION
    }
}

/// A callee the baseline may splice into a caller's `Op::Call` site instead of
/// emitting the per-call bridge. Carries the callee's own bytecode (the body to
/// inline) plus the identity it is guarded against: a runtime closure whose bits
/// do not match this `function_id` makes the guard bail to the interpreter.
#[derive(Debug, Clone)]
pub struct JitInlineCallee {
    /// Callee function id the call-site identity guard is keyed on.
    pub function_id: u32,
    /// Callee formal parameter count; must equal the call's argument count for
    /// the site to inline.
    pub param_count: u16,
    /// Callee register-window length; the spliced body runs in a scratch block
    /// of this many slots.
    pub register_count: u16,
    /// Callee instruction stream in byte-PC order, emitted inline.
    pub instructions: Vec<JitInstructionMetadata>,
}

/// A method the baseline may splice into a caller's `Op::CallMethodValue` site.
/// Carries the method's body plus the data to guard it: the receiver shape the
/// body's sealed property loads/stores are baked against, and, per body
/// `LoadProperty`/`StoreProperty` byte-PC, the value byte offset within the
/// decompressed receiver.
/// Method identity is verified inline every call: the emitter chases the
/// flat prototype handle once per [`proto_chain`](Self::proto_chain) entry,
/// guards each hopped object's shape, reads the method slot at
/// [`method_value_byte`](Self::method_value_byte) from the final holder, and
/// compares the resolved closure's `function_id` to
/// [`method_fid`](Self::method_fid). A prototype-method reassignment or any
/// shape change along the chain falls back to the in-place method call — no
/// per-call resolve bridge.
#[derive(Debug, Clone)]
pub struct JitInlineMethod {
    /// Method function id the call-site identity check is keyed on.
    pub method_fid: u32,
    /// Receiver shape-handle compressed offset the sealed loads are baked for.
    pub recv_shape: u32,
    /// Shape-handle compressed offsets of each prototype hopped from the
    /// receiver to the object holding the method slot, in hop order (the last
    /// entry is the holder). Empty when the method slot is an own property on
    /// the receiver.
    pub proto_chain: Vec<u32>,
    /// Byte offset inside the holder object's value slab for the method
    /// slot, baked from the holder's shape.
    pub method_value_byte: u32,
    /// Method formal parameter count (excluding `this`); must equal argc.
    pub param_count: u16,
    /// Method register-window length; the body runs in a scratch block of this
    /// many slots plus one for `this`.
    pub register_count: u16,
    /// Method instruction stream, emitted inline.
    pub instructions: Vec<JitInstructionMetadata>,
    /// Body `LoadProperty`/`StoreProperty` byte-PC → value slab byte offset. A
    /// receiver-shape property is baked from the identity-guarded receiver shape;
    /// a non-receiver property is baked from its own monomorphic site feedback,
    /// with the required shape recorded in [`Self::prop_shapes`].
    pub prop_offsets: rustc_hash::FxHashMap<u32, u32>,
    /// Body byte-PC → the compressed shape-handle offset a **non-receiver**
    /// property access must match, for the guard the inliner emits before the
    /// slot load/store. A receiver property is absent here — the entry
    /// `CheckMethodIdentity` already proves its shape.
    pub prop_shapes: rustc_hash::FxHashMap<u32, u32>,
    /// Body `CallMethodValue` byte-PC → the monomorphic method it resolves to,
    /// baked recursively. Lets the inliner splice a nested call's body inline
    /// (bounded recursion) instead of leaving it a bridged call.
    pub nested_methods: rustc_hash::FxHashMap<u32, JitInlineMethod>,
}

/// VM-resolved direct-call target for one eligible compiled callee.
///
/// This is metadata only: frame reservation/rooting stays VM-owned, while the
/// backend consumes `entry_addr` once it can emit the matching frame build and
/// call/return sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitDirectCallPlan {
    /// Callee function id in the executable module.
    pub function_id: u32,
    /// Raw compiled entry address.
    pub entry_addr: usize,
    /// Base of the callee function's safepoint records.
    pub safepoint_records: *const crate::native_abi::SafepointRecord,
    /// Number of records starting at [`Self::safepoint_records`].
    pub safepoint_count: u32,
    /// Number of formal parameter registers.
    pub param_count: u16,
    /// Total callee register-window length.
    pub register_count: u16,
}

/// VM-owned root descriptor for one native JIT activation.
///
/// The slots point into the activation's live native [`JitCtx`](crate-local ABI)
/// record. The descriptor owns no executable state and copies no `Value`: the
/// collector rewrites the exact scalar fields machine code will reload after a
/// safepoint.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct JitNativeActivation {
    /// Address of the boxed SELF binding bits.
    pub self_slot: *mut u64,
    /// Address of the boxed `this` binding bits.
    pub this_slot: *mut u64,
}

impl JitNativeActivation {
    /// Empty inactive descriptor.
    pub const EMPTY: Self = Self {
        self_slot: std::ptr::null_mut(),
        this_slot: std::ptr::null_mut(),
    };
}

// The descriptor is an opaque pointer carrier. Dereferencing occurs only under
// `Interpreter`'s single-threaded execution contract; it does not make the VM
// itself transferable (the heap and value handles remain `!Send`/`!Sync`).
unsafe impl Send for JitNativeActivation {}
unsafe impl Sync for JitNativeActivation {}

/// Prepared direct-call entry state returned by the VM to emitted code.
///
/// The frame has already been published onto the active [`JitFrameStack`], so
/// its value slots are visible to precise GC tracing. Emitted code uses this to
/// Receiver shapes cached per direct-method call site, and the number of flat
/// inline-link ways the optimizing tier walks. Shared with the VM so the flat
/// table stride and the emitted walk agree.
pub const JIT_DIRECT_METHOD_WAYS: usize = 4;

/// construct the callee `JitCtx` and branch to `entry_addr` without the generic
/// call bridge.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct JitPreparedDirectCall {
    /// Raw compiled entry address.
    pub entry_addr: usize,
    /// Callee register-window base.
    pub regs: *mut u64,
    /// Base of the callee function's safepoint records.
    pub safepoint_records: *const crate::native_abi::SafepointRecord,
    /// Number of records starting at [`Self::safepoint_records`].
    pub safepoint_count: u32,
    /// Boxed SELF closure bits for the callee context.
    pub self_closure: u64,
    /// Boxed `this` bits for the callee context.
    pub this_value: u64,
    /// Callee frame index in the active stack.
    pub frame_index: usize,
    /// Base of the callee frame's upvalue spine (`Box<[UpvalueCell]>` data), or
    /// `0` when it captures nothing — lets emitted upvalue ops in the direct
    /// callee read its cells inline instead of routing through the stub.
    pub upvalues_ptr: usize,
}

/// Ready-to-use byte offsets and tags for the JIT's inline typed-array
/// element fast path, baked from `otter-vm`'s `#[repr(C)]` body layouts.
///
/// All `*_byte` fields are offsets **from the decompressed GC pointer**
/// (i.e. they already include the GC header), so the emitter adds them straight
/// to `cage_base + compressed_offset`. The chain a `LoadElement`/`StoreElement`
/// walks: receiver `Value` → typed-array body (`ta_*`) → embedded buffer handle
/// (`buffer_*`) → local array-buffer body (`buf_*`) → `Vec<u8>` data pointer.
#[derive(Debug, Clone, Copy, Default)]
pub struct JitTypedArrayLayout {
    /// `GcHeader::type_tag` of a `TypedArrayBodyGc` (guarded at byte 0).
    pub ta_type_tag: u8,
    /// `GcHeader::type_tag` of a `LocalArrayBufferBodyGc` (guarded at byte 0).
    pub local_buffer_type_tag: u8,
    /// `TypedArrayKind` discriminant for `Float64Array` (inlined kind).
    pub kind_float64: u32,
    /// `TypedArrayKind` discriminant for `Int32Array` (inlined kind).
    pub kind_int32: u32,
    /// `BufferStorage` discriminant value selecting the `Local` variant.
    pub buffer_local_tag: u32,
    /// Offset to the `TypedArrayBodyGc.kind` `u32`.
    pub ta_kind_byte: u32,
    /// Offset to the `TypedArrayBodyGc.byte_offset` `usize`.
    pub ta_byte_offset_byte: u32,
    /// Offset to the `TypedArrayBodyGc.length` `usize` (element count).
    pub ta_length_byte: u32,
    /// Offset to the `TypedArrayBodyGc.length_tracking` `bool`.
    pub ta_length_tracking_byte: u32,
    /// Offset to the `BufferStorage` discriminant inside the embedded buffer.
    pub buffer_disc_byte: u32,
    /// Offset to the `BufferStorage` 4-byte compressed handle payload.
    pub buffer_handle_byte: u32,
    /// Offset to the `LocalArrayBufferBodyGc.bytes` `Vec<u8>` itself (its first
    /// word). The emitter adds the probed `Vec<u8>` data-pointer and length
    /// sub-offsets to this — the std `Vec` field order is not guaranteed, so
    /// `otter-jit` discovers it by value-identity rather than hardcoding it.
    pub buf_bytes_byte: u32,
    /// `GcHeader::type_tag` of an ordinary `ArrayBody` (guarded at byte 0 for
    /// the inline dense-array element fast path).
    pub array_type_tag: u8,
    /// Offset to the `ArrayBody.elements` `Vec<Value>` itself (its first word).
    /// The emitter adds the probed `Vec` data-pointer / length sub-offsets;
    /// each element is a raw 8-byte `Value` (no box/unbox). A hole-sentinel
    /// element or an out-of-bounds index falls through to the runtime stub,
    /// which owns the spec-correct prototype / sparse / accessor handling.
    pub array_elements_byte: u32,
    /// Offset to `ArrayBody.length`, the logical `length` property.
    pub array_length_byte: u32,
    /// Offset to `ArrayBody.exotic`; a non-null sidecar means custom
    /// prototype/accessor/descriptor/source-text state may make dense stores
    /// observable, so inline stores must miss to the runtime path.
    pub array_exotic_byte: u32,
}

/// Ready-to-use byte offsets and tags for inline primitive string fast paths.
#[derive(Debug, Clone, Copy, Default)]
pub struct JitStringLayout {
    /// `GcHeader::type_tag` of a `JsStringBody` (guarded at byte 0).
    pub string_type_tag: u8,
    /// Offset to `JsStringBody.len`, the UTF-16 code-unit length.
    pub string_len_byte: u32,
}

/// Static GC layout the optimizing tier needs to emit an inline generational
/// write barrier for a pointer-valued `StoreProperty`. All `#[repr(C)]` /
/// `const` values, isolate-independent, so baked once from the executable
/// snapshot rather than per-compile.
///
/// The barrier marks the parent object's card dirty when an old parent gains a
/// young child (the generational remembered set the scavenger reads). The
/// insertion (marking) barrier is dormant under the Phase-1 STW collector, so
/// only the card-mark is emitted; it allocates nothing and never moves GC.
#[derive(Debug, Clone, Copy, Default)]
pub struct JitGcBarrierLayout {
    /// `GcHeader` flag-byte offset from the header base
    /// (`HEADER_FLAGS_BYTE_OFFSET`).
    pub header_flags_byte: u32,
    /// Young-generation flag bit within the flag byte (`GENERATION_YOUNG_FLAG`).
    pub young_flag: u32,
    /// Byte offset of the card-table bitmap inside a `PageHeader`
    /// (`offset_of!(PageHeader, card_bitmap)`); the page header sits at the
    /// page base (`page_addr & page_mask`).
    pub card_bitmap_byte: u32,
    /// `!(PAGE_SIZE - 1)` — masks a header address down to its page base.
    pub page_mask: u64,
    /// `log2(CARD_SIZE)` — right-shift a within-page byte offset to its card
    /// index.
    pub card_shift: u32,
}

/// Owned snapshot of one executable instruction.
#[derive(Debug, Clone)]
pub struct JitInstructionMetadata {
    /// Shared instruction owned by [`JitCompileSnapshot::code_block`].
    pub instruction: Arc<CodeBlockInstruction>,
    /// `true` for a `MakeFunction` / `MakeClosure` whose target is the function
    /// being compiled (the named-function SELF binding). The emitter
    /// materializes it as a direct read of the frame's own closure (carried in
    /// `JitCtx`) instead of a Rust round-trip through the closure builder.
    pub make_self: bool,
    /// `true` when this instruction is a named-property read of literal
    /// `"length"`. The emitter uses it to try the Array exotic length fast
    /// path before falling back to ordinary property semantics.
    pub load_array_length: bool,
    /// Compact VM-baked identity for common primitive method names.
    pub method_hint: JitMethodHint,
    /// Resolved `f64` value of a `LoadNumber` instruction, whose operand is a
    /// number-constant-pool index rather than an inline immediate. Baked at
    /// view build so the optimizing tier can materialize the constant as a
    /// `ConstF64` node without reaching back into the constant pool. `None` for
    /// every other opcode.
    pub load_number: Option<f64>,
    /// Baked operand-representation feedback bits for an arithmetic / relational
    /// site (see [`crate::jit_feedback`]). `0` for non-arithmetic instructions
    /// and for sites the interpreter never observed; the optimizing tier reads
    /// it to choose an unboxed `Int32` / `Float64` lowering and emit the
    /// matching speculation guard. Populated by
    /// `Interpreter::bake_arith_feedback` at tier-up; the raw compile snapshot
    /// snapshot leaves it `0`.
    pub arith_feedback: u8,
    /// Monomorphic own-data property feedback for a `LoadProperty` /
    /// `StoreProperty` site: `Some((shape_offset, slot_byte))` when the
    /// interpreter observed exactly one receiver shape that owns the named slot
    /// (`shape_offset` is the receiver shape's compressed `Gc` offset for the
    /// guard, `slot_byte` the value's byte offset within the object's value
    /// slab). The optimizing tier lowers such a site to a `CheckShape` guard plus
    /// an inline slot load/store. `None` for non-property ops and for
    /// polymorphic / megamorphic / prototype / dictionary sites. Baked by
    /// `Interpreter::bake_property_feedback`.
    pub property_feedback: Option<(u32, u32)>,
    /// Polymorphic own-data property feedback for a `LoadProperty` /
    /// `StoreProperty` site the interpreter saw with several receiver shapes that
    /// each own the named slot: one `(shape_offset, slot_byte)` case per observed
    /// shape (2..=[`MAX_POLY_PROPERTY_CASES`]). The optimizing tier lowers this to
    /// an inline structure-guard chain — try each case's shape, take its slot on a
    /// match, deopt on the final miss — mirroring a JSC `MultiGetByOffset` /
    /// `MultiPutByOffset`. Empty for monomorphic sites (which use
    /// [`property_feedback`] instead) and for megamorphic / prototype / dictionary
    /// sites. Baked by `Interpreter::bake_property_feedback`.
    pub property_feedback_poly: Vec<(u32, u32)>,
    /// Monomorphic direct-prototype data-property feedback for a `LoadProperty`
    /// site: `Some((receiver_shape_offset, prototype_shape_offset, slot_byte))`
    /// when the receiver shape and its direct prototype were observed resolving
    /// the named data property from the prototype's slot. The optimizing tier
    /// lowers this to receiver/prototype shape guards plus an inline prototype
    /// slot load. `None` for non-load ops and for own-data, polymorphic,
    /// dictionary, accessor, and deeper-prototype sites.
    pub property_proto_feedback: Option<(u32, u32, u32)>,
    /// For a `NewObject` that begins an object literal (`{ k: v, … }` with
    /// constant string keys), the plan to allocate it directly in its final
    /// hidden class instead of running per-property shape transitions. `None`
    /// for every other `NewObject` and every non-`NewObject` op. Baked by
    /// `Interpreter::bake_object_literals`.
    pub object_literal: Option<ObjectLiteralPlan>,
    /// Baked element-load specialization for a `LoadElement` site the interpreter
    /// observed loading exclusively from one unboxable typed-array kind
    /// (`Float64Array` / `Int32Array`). The optimizing tier lowers such a site to
    /// a kind-guarded load that leaves the element in a register in its native
    /// representation (an FP register for `Float64`, a raw int for `Int32`),
    /// skipping the box on load and the matching unbox in the numeric consumer.
    /// [`JitElementLoadKind::Any`] (the default) keeps the generic boxed load.
    /// Baked by `Interpreter::bake_element_load_kind`.
    pub element_load_kind: JitElementLoadKind,
    /// For a `LoadGlobalOrThrow` site whose free identifier resolves to a
    /// global declarative-record (lexical) cell, the cell's compressed `Gc`
    /// offset. The optimizing tier bakes `cage_base + offset` and reads the
    /// cell's value inline (one load + a TDZ-hole guard) instead of calling the
    /// per-access global-load bridge, which re-decodes the operand, hashes the
    /// name, and spills/reloads the live set around a full VM reentry. `None`
    /// when the name is not a lexical binding (a `var` / global-object property,
    /// left to the bridge) or the binding was unbound at compile time. The cell
    /// is a permanent, non-moving old-space allocation rooted by
    /// `global_lexicals`, so the baked offset stays valid for the code's life.
    /// Baked by `Interpreter::bake_global_lex_cells`.
    pub global_lex_cell: Option<u32>,
}

impl JitInstructionMetadata {
    /// Build a feedback-free metadata record around one verified instruction.
    ///
    /// Production snapshots reuse the exact instruction `Arc` owned by the
    /// interpreter's `CodeBlock`; this constructor exists for backend unit
    /// tests that exercise lowering without constructing a whole VM isolate.
    #[must_use]
    pub fn without_feedback(
        op: Op,
        instruction_pc: u32,
        byte_pc: u32,
        byte_len: u32,
        operands: Vec<Operand>,
    ) -> Self {
        let byte_len = u16::try_from(byte_len).expect("instruction byte length exceeds u16");
        Self {
            instruction: Arc::new(CodeBlockInstruction::from_operands(
                op,
                operands,
                instruction_pc,
                crate::executable::NO_PROPERTY_IC_SITE,
                byte_pc,
                byte_len,
            )),
            make_self: false,
            load_array_length: false,
            method_hint: JitMethodHint::None,
            load_number: None,
            arith_feedback: 0,
            property_feedback: None,
            property_feedback_poly: Vec::new(),
            property_proto_feedback: None,
            object_literal: None,
            element_load_kind: JitElementLoadKind::Any,
            global_lex_cell: None,
        }
    }
}

impl JitCompileSnapshot {
    /// Build a feedback-free snapshot for backend lowering tests.
    ///
    /// Production compilation always starts at
    /// [`CodeBlock::jit_compile_snapshot`]. This fixture still creates one
    /// authoritative `CodeBlock` and reuses the exact instruction `Arc`s in
    /// the dynamic overlay, so tests exercise the same ownership boundary.
    #[must_use]
    pub fn without_feedback(
        function_id: u32,
        param_count: u16,
        register_count: u16,
        instructions: Vec<JitInstructionMetadata>,
    ) -> Self {
        let code_block = CodeBlock::jit_test_stub(
            function_id,
            param_count,
            register_count,
            instructions
                .iter()
                .map(|metadata| Arc::clone(&metadata.instruction))
                .collect(),
        );
        Self {
            code_block,
            cage_base: 0,
            ta_layout: JitTypedArrayLayout::default(),
            string_layout: JitStringLayout::default(),
            object_shape_byte: 0,
            object_values_ptr_byte: 0,
            object_inline_values_byte: 0,
            object_slab_len_byte: 0,
            object_inline_slot_cap: 0,
            gc_barrier: JitGcBarrierLayout::default(),
            jit_proto_byte: 0,
            heap_number_type_tag: 0,
            heap_number_bits_byte: 0,
            closure_fid_byte: 0,
            closure_upvalues_ptr_byte: 0,
            collection_layout: JitCollectionLayout::default(),
            native_static_fn_byte: 0,
            instructions,
            inline_callees: rustc_hash::FxHashMap::default(),
            inline_methods: rustc_hash::FxHashMap::default(),
            inline_poly_methods: rustc_hash::FxHashMap::default(),
            collection_leaf_methods: rustc_hash::FxHashMap::default(),
            collection_alloc_methods: rustc_hash::FxHashMap::default(),
            array_methods: rustc_hash::FxHashMap::default(),
            primitive_method_guards: rustc_hash::FxHashMap::default(),
            safepoints: rustc_hash::FxHashMap::default(),
        }
    }
}

impl std::ops::Deref for JitInstructionMetadata {
    type Target = CodeBlockInstruction;

    fn deref(&self) -> &Self::Target {
        &self.instruction
    }
}

/// Native representation an observed `LoadElement` site can produce unboxed.
///
/// Recorded during warmup: a site that only ever reads from a `Float64Array`
/// bakes `Float64`, one that only reads from an `Int32Array` bakes `Int32`, and
/// a site that sees any other receiver (dense array, mixed typed-array kinds,
/// object) stays `Any` and keeps the boxed load.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum JitElementLoadKind {
    /// Generic boxed element load (no specialization).
    #[default]
    Any,
    /// The site only observed `Float64Array` receivers.
    Float64,
    /// The site only observed `Int32Array` receivers.
    Int32,
}

/// Common method names the external JIT can specialize without reading VM
/// constant pools.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum JitMethodHint {
    /// No recognized method name.
    #[default]
    None,
    /// `String.prototype.charCodeAt`.
    StringCharCodeAt,
    /// `Number.prototype.toString`.
    NumberToString,
}

/// Plan for lowering an object literal (`NewObject` + a source-order run of
/// `DefineDataProperty` with constant string keys) to a single shaped
/// allocation in the optimizing tier.
///
/// Computed at compile time by replaying the literal's shape transitions from
/// the empty root, so the final hidden class is known before any code runs and
/// the per-property `DefineDataProperty` shape walks are elided.
#[derive(Debug, Clone)]
pub struct ObjectLiteralPlan {
    /// Destination register the `NewObject` writes (the literal's object).
    pub obj_reg: u16,
    /// Final hidden-class shape the object ends up in, as a compressed
    /// `Gc<ShapeBody>` offset.
    pub shape_offset: u32,
    /// One entry per data property, in slot (source-definition) order: the
    /// `DefineDataProperty` byte-PC (where the value SSA is captured) and the
    /// value source register the define reads.
    pub defines: Vec<ObjectLiteralProp>,
    /// Byte-PCs of the `LoadString` key-load instructions the builder skips
    /// (the key is implied by the baked shape).
    pub key_pcs: Vec<u32>,
}

/// One data property of an object literal in [`ObjectLiteralPlan`].
#[derive(Debug, Clone, Copy)]
pub struct ObjectLiteralProp {
    /// Byte-PC of the `DefineDataProperty` instruction.
    pub define_pc: u32,
    /// Value source register the define reads, in the value slab's slot order.
    pub value_reg: u16,
}

/// Frame stack the interpreter dispatches over. Exposed so the JIT crate can
/// hold a `*mut JitFrameStack` in its reentry context and hand it back to the
/// VM-side bridge methods without naming the concrete stack shape itself. This
/// is the segmented, stable-address [`crate::holt_stack::HoltStack`] — the
/// stability is exactly what lets compiled code keep a frame/register pointer
/// across a re-entrant call.
pub type JitFrameStack = crate::holt_stack::HoltStack;

/// One reconstructed interpreter frame in a nested inline-resume, decoded from
/// the emitted deopt exit's stack buffer by the resume stub and handed to
/// [`crate::Interpreter::jit_resume_inline_callee_stack`]. Frames are ordered
/// outermost inlined method first; the top frame resumes at the failing guard.
pub struct JitResumeFrame {
    /// Function id this frame executes.
    pub callee_fid: u32,
    /// Byte-PC to resume this frame at.
    pub callee_pc: u32,
    /// Register in the parent frame that receives this frame's return value.
    /// Ignored for the outermost frame (its result bubbles out to emitted code).
    pub return_register: u16,
    /// Value bound as this frame's `this`.
    pub this: crate::Value,
    /// The method's closure, or `undefined` when the body reads no upvalue. The
    /// resumed frame draws its upvalue spine from this closure's captured cells.
    pub closure: crate::Value,
    /// Full register window (unwritten slots `undefined`, live slots boxed).
    pub registers: Vec<crate::Value>,
}

/// Raw, type-erased pointers the VM hands the JIT so compiled code can re-enter
/// the VM (recursive calls, closure allocation) through the safe bridge methods
/// ([`crate::Interpreter::jit_runtime_call`],
/// [`crate::Interpreter::jit_runtime_make_function`]).
///
/// # Invariants
/// - Pointers are valid only for the duration of one
///   [`JitFunctionCode::run_entry`] call; the JIT must not retain them.
/// - `vm`/`stack`/`context` are `*mut Interpreter` / `*mut JitFrameStack` /
///   `*const ExecutionContext` erased to avoid a naming dependency in the trait.
///   The JIT casts them back. The VM guarantees no live `&mut` aliases them for
///   the call's duration (it forms them from its own borrows and does not touch
///   those borrows until the call returns).
#[derive(Clone, Copy)]
pub struct JitReentryPtrs {
    /// Erased `*mut Interpreter`.
    pub vm: *mut std::ffi::c_void,
    /// Erased `*mut JitFrameStack`.
    pub stack: *mut std::ffi::c_void,
    /// Erased `*const ExecutionContext`.
    pub context: *const std::ffi::c_void,
    /// Index of the executing (compiled) frame within `stack`.
    pub frame_index: usize,
}

/// Outcome of executing compiled code for one function entry.
///
/// The compiled body runs over the entry frame's register window — which the
/// VM keeps rooted on its frame stack, so closure allocation and recursive
/// calls inside the body are GC-safe. It either runs to a `Return` (carrying
/// the completion Value), hits a typed guard it cannot honor and bails (the VM
/// re-runs on the interpreter), or a re-entered VM call threw.
#[derive(Debug)]
pub enum JitExecOutcome {
    /// `Return`/`ReturnValue` reached; carries the completion Value.
    Returned(crate::Value),
    /// A typed guard (or an unsupported opcode emitted as a bail) was hit; the
    /// VM resumes the interpreter at the carried byte-PC — the exact
    /// instruction, so committed side effects are preserved.
    Bailed(u32),
    /// A re-entered VM operation (recursive call) raised; propagate the error.
    Threw(crate::run_control::VmError),
}

/// Type-erased compiled-code handle owned by the JIT implementation.
///
/// The JIT implementation owns executable memory and the unsafe ABI calls. The
/// VM still needs raw entry metadata for compiled-to-compiled direct branches:
/// emitted callers can branch to an already-installed callee without routing
/// through the generic runtime call bridge.
pub trait JitFunctionCode: std::fmt::Debug + Send + Sync {
    /// Size in bytes of the finalized native code mapping.
    fn code_len(&self) -> usize;

    /// `true` when this code was compiled with unsupported opcodes emitted as
    /// bail-to-interpreter, making it sound to enter only at a supported loop
    /// header via OSR (not at function entry). The function-entry tier-up path
    /// skips such code; loop OSR uses it. Default `false`.
    fn osr_only(&self) -> bool {
        false
    }

    /// Raw function-entry address for emitted direct calls.
    ///
    /// The pointer is owned by this code object and remains valid while the
    /// object is installed in the VM JIT code table.
    fn entry_addr(&self) -> Option<usize> {
        None
    }

    /// `true` when this body is sound to run *frameless* — entered directly
    /// from another compiled function's machine code with only a raw register
    /// window and no published VM frame. That requires every op in the body to
    /// address registers through the window (`JitCtx.regs`); any stub that
    /// resolves registers through `JitCtx.frame_index` (interpreter delegates,
    /// call/closure bridges) would read and write the *caller's* frame instead.
    /// Gates the bridge-free direct-method inline link. Default `false`.
    fn frameless_entry_safe(&self) -> bool {
        false
    }

    /// Safepoint table for compiled code that can call allocating runtime
    /// stubs. Tiers without a published VM-native safepoint contract return an
    /// empty table and must not emit allocating stub calls.
    fn safepoint_table(&self) -> (*const crate::native_abi::SafepointRecord, u32) {
        (std::ptr::null(), 0)
    }

    /// Execute the compiled function for the frame at `ptrs.frame_index`.
    ///
    /// Compiled code reads/writes that frame's register window in place and,
    /// for `Call`/`MakeFunction`, re-enters the VM through the safe bridge
    /// methods reached via `ptrs`. The window stays rooted on the VM frame
    /// stack throughout, so allocation/calls in the body are GC-safe.
    fn run_entry(&self, ptrs: JitReentryPtrs) -> JitExecOutcome;

    /// Enter compiled code mid-function at the loop header whose bytecode PC is
    /// `byte_pc` (on-stack replacement). Returns `None` when this code has no
    /// OSR entry for that PC (the VM keeps interpreting).
    ///
    /// The baseline keeps every live value in the frame register array at each
    /// instruction boundary, so a loop header is a valid resume point: the
    /// interpreter's live registers are exactly what the compiled code reads.
    /// The default returns `None` for codes that do not support OSR.
    fn osr_entry(&self, _ptrs: JitReentryPtrs, _byte_pc: u32) -> Option<JitExecOutcome> {
        None
    }
}

/// On-demand snapshot of executable code retained by one interpreter.
///
/// Code objects are deduplicated by allocation identity across the canonical
/// entry/OSR maps and auxiliary direct-call caches. `code_bytes` sums finalized
/// native buffer lengths, not Rust metadata or page-rounding overhead.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JitCodeResidency {
    /// Installed non-OSR function bodies.
    pub installed_entry_bodies: u64,
    /// Installed OSR-target bodies.
    pub installed_osr_bodies: u64,
    /// Unique executable code objects reachable from all runtime caches.
    pub unique_code_objects: u64,
    /// Sum of finalized executable buffer lengths.
    pub code_bytes: u64,
}

/// Result of a JIT compile attempt.
#[derive(Debug, Clone)]
pub enum JitCompileStatus {
    /// Executable memory or the current target backend is unavailable; the VM
    /// should silently continue in the interpreter.
    Unavailable,
    /// Function is not yet in the baseline-supported opcode subset.
    Unsupported {
        /// Short diagnostic for internal tracing and tests.
        reason: String,
    },
    /// Function compiled successfully.
    Compiled {
        /// Type-erased native-code handle.
        code: Arc<dyn JitFunctionCode>,
    },
}

/// Compile-time error from the JIT implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitCompileError {
    /// Human-readable internal diagnostic.
    pub message: String,
}

impl JitCompileError {
    /// Construct an internal compile error.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for JitCompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for JitCompileError {}

/// Runtime-installed JIT compiler hook.
///
/// `otter-runtime` wires an implementation from `otter-jit`; `otter-vm` only
/// owns this trait object and supplies owned compile-input DTOs.
pub trait JitCompilerHook: Send + Sync {
    /// Attempt to compile one function snapshot.
    ///
    /// Returning [`JitCompileStatus::Unavailable`] or
    /// [`JitCompileStatus::Unsupported`] must leave execution semantics
    /// unchanged: the VM falls back to the interpreter without surfacing a JS
    /// error.
    fn compile_function(
        &self,
        request: JitCompileRequest,
    ) -> Result<JitCompileStatus, JitCompileError>;
}
