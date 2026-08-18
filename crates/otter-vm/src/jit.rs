//! Dependency-inverted native-tier hook surface.
//!
//! This module defines the safe VM-side contract used by an external JIT crate.
//! `otter-vm` owns bytecode metadata, call-frame layout, property-IC site ids,
//! and GC rooting rules; `otter-jit` owns executable memory and machine-code
//! emission. The VM therefore exposes owned compile-input DTOs and accepts a
//! trait object installed by the runtime layer, avoiding any dependency from
//! `otter-vm` back to `otter-jit`.
//!
//! # Contents
//! - [`JitCompileSnapshot`], [`JitClosureCallLayout`], and
//!   [`JitInstructionMetadata`] — immutable CodeBlock plus per-tier
//!   feedback/layout metadata.
//! - [`JitCompilerHook`] — runtime-installed compile hook implemented outside
//!   `otter-vm`.
//! - [`JitFunctionCode`] and [`JitCompileStatus`] — type-erased compiled-code
//!   result handles and validity dependencies that keep executable memory
//!   ownership outside this crate.
//! - [`JitDirectCallPlan`], [`JitDirectCallee`], guarded static-native call
//!   plans, and the isolate-local direct-call cache record used to reuse an
//!   already-validated native entry.
//! - [`JitCodeGenerationSnapshot`] — explicit cold introspection over live
//!   generations and retained entry-cell tombstones.
//! - Collector-owned receiver-allocation windows and immutable constructor
//!   allocation plans used by generated fixed, spread, and superclass linkage.
//! - [`VmRuntimeActivation`] owns the dynamic VM bridge, including the single
//!   post-entry feedback/result transaction after generated execution.
//!
//! # Invariants
//! - DTOs are owned and borrow-free. JIT compilation must not hold references
//!   into `ExecutionContext`, `CodeBlock`, or interpreter frames.
//! - A `JitCompileSnapshot` is stable across moving collection: it contains no
//!   untraced moving `Value` or GC handle. Heap identities are compressed
//!   offsets rooted elsewhere, permanent cells, or address-stable traced
//!   cells; process-local addresses name only allocations with isolate-long
//!   lifetime.
//! - No unsafe is required here. Native entry pointers, executable mappings, and
//!   call ABI details remain encapsulated by the JIT implementation crate.
//! - Baseline code uses the interpreter frame register array as its precise root
//!   provider. Values may be cached in machine registers only between
//!   safepoints; allocation and call slow paths must reload from frame slots.
//! - A prepared string literal exposes only its address-stable traced `Value`
//!   cell. Generated code never bakes the moving string handle; collection
//!   rewrites the cell in place and the cell outlives isolate code objects.
//! - Optimized entries use the same runtime activation and published native
//!   frame as baseline entries. Before bailing they reconstruct every
//!   interpreter register in the rooted frame window and publish the exact
//!   logical resume PC.
//! - A cached direct-call plan is reusable only at the registry invalidation
//!   epoch at which it was selected. Dynamic callable state is never cached.
//! - Generated receiver allocation may mutate only the published nursery
//!   window and its accounting words; every miss returns to the rooted VM
//!   allocator before any constructor effect begins.
//! - A compiled result crosses back as one `NativeResultPair`; only the VM may
//!   root and collector-rewrite a validated Return/Throw payload while cold
//!   feedback reconciliation runs.
//!
//! # See also
//! - [`crate::execution_context`] for snapshot creation from frozen bytecode.
//! - [`crate::Frame`] for the traced register array the baseline tier reuses.
//! - `JIT_DESIGN.md` §3.2, §3.5, and §4 for backend, GC, and phasing.

use std::sync::Arc;

use otter_bytecode::{Op, Operand};
use serde::Serialize;

/// Opaque collector-owned nursery window carried by the compiled-entry ABI.
pub type JitMachineAllocationWindow = otter_gc::MachineAllocationWindow;
/// Machine allocation page-layout constants, derived on the VM side so the
/// backend does not depend directly on the collector crate.
pub const JIT_PAGE_SPACE_OFFSET: u32 =
    std::mem::offset_of!(otter_gc::page::PageHeader, space) as u32;
/// Byte offset of the nursery page's committed bump cursor.
pub const JIT_PAGE_BUMP_CURSOR_OFFSET: u32 =
    std::mem::offset_of!(otter_gc::page::PageHeader, bump_cursor) as u32;
/// Byte offset of the nursery page's allocated-byte accounting word.
pub const JIT_PAGE_ALLOCATED_BYTES_OFFSET: u32 =
    std::mem::offset_of!(otter_gc::page::PageHeader, allocated_bytes) as u32;
/// Machine discriminant proving a page is the active young from-space.
pub const JIT_NEW_FROM_SPACE_KIND: u32 = otter_gc::page::SpaceKind::NewFrom as u32;
/// Fixed regular GC page size used by the bump-limit check.
pub const JIT_GC_PAGE_SIZE: u32 = otter_gc::page::PAGE_SIZE as u32;
/// Header flag installed on a generated young object.
pub const JIT_GC_YOUNG_FLAG: u8 = otter_gc::header::GENERATION_YOUNG_FLAG;

use crate::{
    CodeBlock, CodeBlockInstruction,
    feedback::ArithFeedback,
    native_abi::{
        CodeDependency, CodeLifetimeState, NativeFrameKind, NativeResultPair, SafepointId,
        SafepointRecord,
    },
};

/// Canonical handlers owned by one compiled `EnterTry` instruction.
///
/// This is an owned scalar view of the CodeBlock control-flow table. The JIT
/// consumes these already-resolved logical PCs and never decodes relative
/// exception targets independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitExceptionRegion {
    /// Catch entry, absent for a `try/finally` region.
    pub catch_pc: Option<u32>,
    /// Finally entry, absent for a catch-only region.
    pub finally_pc: Option<u32>,
    /// Register receiving the thrown value on catch entry.
    pub exception_register: u16,
}

/// Owned compile request for one bytecode function.
#[derive(Debug, Clone)]
pub struct JitCompileRequest {
    /// Code and feedback snapshot to compile.
    pub snapshot: JitCompileSnapshot,
    /// Default-off diagnostics requested by the owning isolate.
    pub debug: crate::jit_debug::JitDebugRequest,
    /// Owned source identity present only when artifact capture is requested.
    ///
    /// Keeping this optional avoids cloning names and module URLs on the
    /// ordinary default-off compilation path.
    pub artifact_identity: Option<crate::jit_artifact::JitArtifactIdentity>,
    /// Loop-header logical PC for an OSR-target compile. `None` means normal
    /// function-entry compilation.
    pub osr_pc: Option<u32>,
    /// Unique isolate-assigned identity for the produced code object. The
    /// emitter stamps it into the code metadata and every published frame, so
    /// the isolate code registry can resolve safepoints for any installed
    /// object — including a nested callee's — from `(id, safepoint_id)`.
    pub code_object_id: u64,
}

/// Typed closure-call layout shared by VM snapshotting and native backends.
///
/// Every byte offset is measured from the decompressed closure's
/// [`otter_gc::GcHeader`], not from [`crate::closure::JsClosureBody`]'s payload.
/// The flag masks travel with the offsets so a backend never duplicates Rust
/// enum/container layout or independently guesses closure-call semantics.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JitClosureCallLayout {
    /// Byte offset of [`crate::closure::ClosureCallHeader::function_id`].
    pub function_id_byte: u32,
    /// Byte offset of [`crate::closure::ClosureCallHeader::flags`].
    pub flags_byte: u32,
    /// Byte offset of [`crate::closure::ClosureCallHeader::upvalue_base`].
    pub upvalue_base_byte: u32,
    /// Byte offset of [`crate::closure::ClosureCallHeader::upvalue_count`].
    pub upvalue_count_byte: u32,
    /// Byte offset of the nullable compressed
    /// [`crate::closure::ClosureCallHeader::eval_env`] handle.
    pub eval_env_byte: u32,
    /// Byte offset of canonical [`crate::closure::JsClosureBody::bound_this`].
    pub bound_this_byte: u32,
    /// Byte offset of canonical
    /// [`crate::closure::JsClosureBody::bound_new_target`].
    pub bound_new_target_byte: u32,
    /// Presence bit for canonical `bound_this`.
    pub bound_this_flag: u32,
    /// Presence bit for canonical `bound_new_target`.
    pub bound_new_target_flag: u32,
    /// Flags requiring the call-setup runtime stub before compiled entry.
    pub runtime_setup_flags: u32,
}

/// Machine-readable class-constructor wrapper layout.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JitClassConstructorLayout {
    /// GC body tag proving the wrapper family.
    pub type_tag: u8,
    /// Byte offset from the decompressed body pointer to its underlying
    /// callable value.
    pub callable_byte: u32,
    /// Byte offset from the wrapper body to its live superclass identity.
    pub super_constructor_byte: u32,
    /// Byte offset from the wrapper body to its live instance prototype.
    pub prototype_byte: u32,
}

/// Maximum ordinary prototype depth admitted by generated receiver allocation.
pub const JIT_RECEIVER_PROTOTYPE_GUARD_CAP: usize = 8;

/// One GC-movement-stable class receiver allocation program.
///
/// The program uses the live class wrapper's prototype value, guards its full
/// ordinary shape chain, and initializes an in-body shaped object. Any mismatch
/// is pre-effect and returns to rooted receiver preparation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitReceiverAllocationPlan {
    /// Expected underlying constructor function of the live `new.target` class.
    pub new_target_function_id: u32,
    /// Initial receiver hidden class.
    pub receiver_shape: u32,
    /// Number of already-visible undefined slots described by that shape.
    pub initial_field_count: u8,
    /// Reserved in-body slot capacity for later constructor transitions.
    pub reserved_capacity: u8,
    /// Number of live entries in [`Self::prototype_shapes`].
    pub prototype_shape_count: u8,
    /// Complete nearest-first ordinary prototype chain.
    pub prototype_shapes: [u32; JIT_RECEIVER_PROTOTYPE_GUARD_CAP],
}

/// One constructor-owned add-property transition executable in generated code.
///
/// The receiver and every ordinary prototype shape are guarded immediately
/// before the store. A miss therefore leaves the canonical `StoreProperty`
/// operation completely unstarted; a hit appends exactly one pre-reserved
/// slot, publishes the child hidden class, and performs the ordinary barrier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitConstructorFieldTransition {
    /// Receiver hidden class before the field is added.
    pub from_shape: u32,
    /// Receiver hidden class after the field is added.
    pub to_shape: u32,
    /// Full ordinary prototype-chain shapes, nearest first.
    pub prototype_shapes: Vec<u32>,
    /// Appended own-slot index.
    pub slot: u16,
}

/// GC-movement-stable VM plan behind [`JitConstructorFieldTransition`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JitConstructorFieldTransitionPlan {
    pub(crate) from_shape: crate::object::ShapeId,
    pub(crate) to_shape: crate::object::ShapeId,
    pub(crate) prototype_shapes: Vec<crate::object::ShapeId>,
    pub(crate) slot: u16,
}

const _: [(); 40] = [(); std::mem::size_of::<JitClosureCallLayout>()];
const _: [(); 4] = [(); std::mem::align_of::<JitClosureCallLayout>()];
const _: [(); 0] = [(); std::mem::offset_of!(JitClosureCallLayout, function_id_byte)];
const _: [(); 4] = [(); std::mem::offset_of!(JitClosureCallLayout, flags_byte)];
const _: [(); 8] = [(); std::mem::offset_of!(JitClosureCallLayout, upvalue_base_byte)];
const _: [(); 12] = [(); std::mem::offset_of!(JitClosureCallLayout, upvalue_count_byte)];
const _: [(); 16] = [(); std::mem::offset_of!(JitClosureCallLayout, eval_env_byte)];
const _: [(); 20] = [(); std::mem::offset_of!(JitClosureCallLayout, bound_this_byte)];
const _: [(); 24] = [(); std::mem::offset_of!(JitClosureCallLayout, bound_new_target_byte)];
const _: [(); 28] = [(); std::mem::offset_of!(JitClosureCallLayout, bound_this_flag)];
const _: [(); 32] = [(); std::mem::offset_of!(JitClosureCallLayout, bound_new_target_flag)];
const _: [(); 36] = [(); std::mem::offset_of!(JitClosureCallLayout, runtime_setup_flags)];

/// Owned, moving-GC-stable snapshot of one executable function body.
///
/// The DTO may remain live while nested target preparation allocates and moves
/// the heap. It therefore carries only immutable code, scalar layout data,
/// compressed offsets rooted by the VM, and addresses of permanent or traced
/// stable cells. A raw moving `Value` or GC handle must never be added here.
#[derive(Debug, Clone)]
pub struct JitCompileSnapshot {
    /// Exact immutable executable body this feedback overlay decorates.
    ///
    /// Function identity, register-window shape, instruction stream, and
    /// function-mode flags are owned solely by this `CodeBlock`. The JIT must
    /// not keep a second scalar representation of executable state.
    pub code_block: Arc<CodeBlock>,
    /// Whether entry starts with the `this` binding in the derived-constructor
    /// TDZ until `super()` commits `BindThisValue`.
    ///
    /// Every other function receives an initialized `this` value, allowing a
    /// backend to omit the per-`LoadThis` hole guard.
    pub derived_constructor: bool,
    /// GC cage base address (`otter_gc::cage_base()`), baked at compile time.
    /// Stable for the isolate's life, so emitted inline property loads add it
    /// to a compressed `Gc` offset to decompress an object pointer without a
    /// runtime load. `0` when no inline access is baked.
    pub cage_base: usize,
    /// Static heap-layout offsets for inline typed-array element access. Baked
    /// once at compile time from `otter-vm`'s `#[repr(C)]` body layouts so the
    /// emitter stays layout-agnostic.
    pub array_layout: JitArrayLayout,
    /// Property-load sites whose receiver shape and own slot are settled,
    /// keyed by byte-PC. Generated code compares against the shape directly
    /// instead of loading the site's cache cell.
    pub property_loads: rustc_hash::FxHashMap<u32, Vec<JitInlinePropertyLoad>>,
    /// Property-store sites whose receiver shape and own slot are settled,
    /// keyed by byte-PC. A store may not write through a prototype, so a
    /// settled store needs no hop check either.
    pub property_stores: rustc_hash::FxHashMap<u32, Vec<JitInlinePropertyLoad>>,
    /// Direct physical hit proofs for schema-owned binding sites, keyed by
    /// byte-PC. See [`BindingHitProof`].
    pub binding_hit_proofs: rustc_hash::FxHashMap<u32, BindingHitProof>,
    /// Constructor `StoreProperty` sites with a pre-reserved, guarded hidden
    /// class transition, keyed by byte-PC.
    pub constructor_field_transitions: rustc_hash::FxHashMap<u32, JitConstructorFieldTransition>,
    /// Load sites whose every program reaches its slot through the receiver's
    /// prototype, keyed by byte-PC. Guarding the receiver shape fixes which
    /// prototype the hop reaches and guarding the holder shape fixes the slot,
    /// so generated code runs the hop without the site's cache cell.
    pub property_prototype_loads: rustc_hash::FxHashMap<u32, Vec<JitInlinePropertyHop>>,
    /// Logical PCs an earlier optimized generation of this function
    /// deoptimized at. Moving a settled access out of a loop makes its guard
    /// run on paths that would not have reached it, so a pass that would place
    /// one at a PC already known to deoptimize leaves the loop alone instead.
    pub optimized_bail_pcs: std::collections::BTreeSet<u32>,
    /// How the indexed-element program addresses each site's receiver, keyed by
    /// the site's byte-PC. Generated code reads only this; the family's body
    /// layout never reaches the emitter, so a second element-bearing family
    /// costs a declaration.
    pub element_accesses: rustc_hash::FxHashMap<u32, JitElementAccess>,
    /// Static heap-layout offsets for inline primitive string `.length`.
    pub string_layout: JitStringLayout,
    /// Byte offset from a decompressed upvalue-cell pointer to its captured
    /// `Value`, for the inline captured-binding read.
    pub upvalue_value_byte: u32,
    /// Byte offset from a decompressed object pointer to its shape handle
    /// (`HEADER_SIZE + OBJECT_BODY_SHAPE_OFFSET`). A `#[repr(C)]` constant; the
    /// emitter reads `[obj_ptr + object_shape_byte]` for the WhiskerIC
    /// `LoadProperty` cell guard, staying layout-agnostic.
    pub object_shape_byte: u32,
    /// Byte offset from a decompressed object pointer to its dictionary-mode
    /// structural identity. Read only after the ordinary shape handle is null.
    pub object_dictionary_shape_id_byte: u32,
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
    /// Byte offset from a decompressed object pointer to the out-of-line slab
    /// handle (`HEADER_SIZE + OBJECT_BODY_SLAB_HANDLE_OFFSET`). The emitter
    /// reads this 4-byte handle to pick the slab base: null means the slots
    /// live in the in-body inline array, non-null means they moved to the
    /// out-of-line slab (which `values_ptr` addresses). `slab_len` cannot
    /// decide this — the capacity model can spill a small object early.
    pub object_slab_handle_byte: u32,
    /// Byte offset from a decompressed object pointer to the `u16`
    /// [`slab_len`](crate::object) counter (`HEADER_SIZE +
    /// OBJECT_BODY_SLAB_LEN_OFFSET`). The emitter reads it to pick the inline vs
    /// out-of-line slab base.
    pub object_slab_len_byte: u32,
    /// Inline slab capacity (`INLINE_SLOT_CAP`): a body with this many
    /// string-keyed slots or fewer holds them inline; a larger one spills to the
    /// out-of-line `values` vector whose base is a stable heap allocation.
    pub object_inline_slot_cap: u32,
    /// Byte offset of the ordinary object's one-byte `[[Extensible]]` Boolean.
    pub object_extensible_byte: u32,
    /// Byte offset of the `u8` fast-shape eligibility discriminant. Generated
    /// property programs compare it with [`Self::object_shape_cache_fast`]
    /// before trusting an otherwise-equal hidden class.
    pub object_shape_cache_mode_byte: u32,
    /// Exact `u8` discriminant for append-only fast-shape semantics.
    pub object_shape_cache_fast: u8,
    /// Byte offset of the one-byte Boolean set by in-place descriptor mutations
    /// such as `freeze` and `defineProperty`. Generated property programs
    /// require zero.
    pub object_slot_attrs_overridden_byte: u32,
    /// Byte offset of the 4-byte rare-state GC handle. Generated property
    /// programs require a zero handle on every receiver/prototype they inspect;
    /// a sidecar may carry semantics the shape alone cannot prove.
    pub object_exotic_handle_byte: u32,
    /// Fixed aligned bytes in one ordinary object cell, header included.
    pub object_cell_bytes: u32,
    /// Static GC layout for the inline generational write barrier emitted on a
    /// pointer-valued `StoreProperty`. Isolate-independent `#[repr(C)]` / `const`
    /// values; the card-mark is gated on [`cage_base`](Self::cage_base) being
    /// baked (the emitter decompresses parent/child pointers against it).
    pub gc_barrier: JitGcBarrierLayout,
    /// Byte offset from a decompressed object pointer to its flat
    /// `[[Prototype]]` mirror (`HEADER_SIZE + OBJECT_BODY_JIT_PROTO_OFFSET`). A
    /// `#[repr(C)]` constant; the method-inline guard reads
    /// `[recv_ptr + jit_proto_byte]` to chase the receiver's prototype chain
    /// in machine code without runtime resolution.
    pub jit_proto_byte: u32,
    /// Complete VM-owned closure-call ABI contract. Method identity guards use
    /// its function-id offset; native call linkage additionally consumes its
    /// flags, immutable upvalue spine, and canonical bound-value metadata.
    pub closure_call_layout: JitClosureCallLayout,
    /// VM-baked class wrapper layout used by generated construct guards.
    pub class_constructor_layout: JitClassConstructorLayout,
    /// GC body tags whose cell values remain ECMAScript primitives.
    pub primitive_cell_type_tags: [u8; 3],
    /// Ready-to-use byte offsets and type tags for baseline collection method
    /// IC guards.
    pub collection_layout: JitCollectionLayout,
    /// Byte offset from a decompressed native-function pointer to its
    /// machine-readable static builtin identity: a `u32` index into the
    /// isolate's external-reference table, not an entry address.
    pub native_ref_byte: u32,
    /// Instruction overlays in canonical logical-PC order.
    pub instructions: Vec<JitInstructionMetadata>,
    /// Direct reads of permanent global-lexical cells keyed by the
    /// `Op::LoadGlobalOrThrow` byte-PC. The global declarative record roots
    /// each non-moving cell; generated code reads its current value and takes
    /// the semantic stub only for a TDZ hole.
    pub global_lexical_loads: rustc_hash::FxHashMap<u32, JitGlobalLexicalLoad>,
    /// Prepared address-stable, GC-traced primitive-string constant cells
    /// keyed by `Op::LoadString` byte PC. Every site is a leaf load and remains
    /// valid for the lifetime of every code object carrying its relocation.
    pub string_constant_cells: rustc_hash::FxHashMap<u32, JitStringConstantCell>,
    /// Guarded own-data reads from the global object record keyed by the
    /// `Op::LoadGlobalOrThrow` byte-PC. Generated code validates both the live
    /// global-declarative epoch and the global object's hidden class before
    /// reading the current `Value` slot word.
    pub global_object_loads: rustc_hash::FxHashMap<u32, JitGlobalObjectLoad>,
    /// Guarded static-native leaf calls keyed by the caller's `Op::Call`
    /// byte-PC. Each plan names one exact bootstrap function identity and one
    /// machine-code operation; misses side-exit before effects.
    pub static_native_calls: rustc_hash::FxHashMap<u32, JitStaticNativeCall>,
    /// Compiler-native direct callees keyed by the caller's `Op::Call` or
    /// `Op::CallSpread` byte-PC. Each entry names one monomorphic synchronous callee
    /// with an exact fresh/inherited upvalue spine and a current non-OSR native entry.
    /// The emitter guards the dynamic callable against
    /// [`JitDirectCallPlan::function_id`] and side-exits if the identity or
    /// entry-cell lease no longer matches; no runtime resolver is part of the
    /// compiled hit path.
    pub direct_callees: rustc_hash::FxHashMap<u32, JitDirectCallee>,
    /// Compiler-native constructors keyed by fixed or spread `Op::New` /
    /// `Op::SuperConstruct` byte-PCs. The dynamic callee must be the exact
    /// function or closure identity; receiver creation, derived-`this`, and
    /// `new.target` publication/inheritance are part of the generated boundary.
    pub direct_constructs: rustc_hash::FxHashMap<u32, JitDirectCallee>,
    /// Compiler-native direct-method chains keyed by the caller's
    /// `Op::CallMethodValue` byte-PC. Each target carries one exact receiver /
    /// prototype / method-slot guard and one current entry-capable callee
    /// generation. Targets retain feedback order, so generated code emits one
    /// bounded most-frequent-first guard chain and constructs a callee frame
    /// only after one exact guard succeeds.
    pub direct_methods: rustc_hash::FxHashMap<u32, Vec<JitDirectMethod>>,
    /// Inline-candidate callees for baseline leaf-inlining, keyed by the
    /// caller's `Op::Call` byte-PC. Populated only for sites the interpreter
    /// observed resolving to a single plain synchronous bytecode callee; baked
    /// by `Interpreter::bake_inline_callees`. Empty in the raw compile
    /// snapshot. The emitter applies the final pure-leaf / size / arity test
    /// and splices accepted bodies under an identity guard; a failed guard
    /// side-exits at the exact caller PC.
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
    /// to the next entry; a receiver matching none of them side-exits before
    /// method lookup. Baked by `Interpreter::bake_inline_callees`. The optimizing
    /// tier ignores polymorphic inline bodies.
    pub inline_poly_methods: rustc_hash::FxHashMap<u32, Vec<JitInlineMethod>>,
    /// Guarded method calls into a declared native entry, keyed by call byte PC.
    pub guarded_method_calls: rustc_hash::FxHashMap<u32, JitGuardedMethodCall>,
    /// Safepoint records baked for allocating runtime-stub call sites, keyed by
    /// `SafepointId`. Baseline uses frame-slot roots for the full register
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
    /// Compact `Map` table layout for generated Int32 probes.
    pub map_table: JitMapTableLayout,
}

/// Stable words needed to probe one compact `Map` table from generated code.
#[derive(Debug, Clone, Copy, Default)]
pub struct JitMapTableLayout {
    /// Byte offset from the Map header to its compressed table handle.
    pub map_table_byte: u32,
    /// `GcHeader::type_tag` for a Map's ordered table.
    pub table_type_tag: u8,
    /// Byte offset from the table header to its appended-entry length.
    pub table_len_byte: u32,
    /// Byte offset from the table header to `bucket_count - 1`.
    pub table_bucket_mask_byte: u32,
    /// Byte offset from the table header to the first bucket word.
    pub table_buckets_byte: u32,
    /// Width of one compact Map entry.
    pub entry_size: u32,
    /// Byte offset from an entry to its original key value.
    pub entry_key_byte: u32,
    /// Byte offset from an entry to its mapped value.
    pub entry_value_byte: u32,
    /// Byte offset from an entry to its collision-chain successor.
    pub entry_next_byte: u32,
    /// Byte offset from an entry to its state flags.
    pub entry_flags_byte: u32,
    /// State bit proving that an entry is live rather than tombstoned.
    pub entry_live_flag: u32,
    /// Word mixed first for a Number key.
    pub number_hash_tag: u64,
    /// Per-word Fx hash multiplier.
    pub fx_hash_multiplier: u64,
    /// First low-bit avalanche multiplier.
    pub hash_avalanche_1: u64,
    /// Second low-bit avalanche multiplier.
    pub hash_avalanche_2: u64,
}

/// Width of one body word a declared layout guards.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum JitGuardWidth {
    /// One byte, as a `bool` flag occupies.
    Byte,
    /// A 32-bit word, as a flags word or a `#[repr(u32)]` discriminant.
    #[default]
    Word32,
    /// A pointer-width word, as an optional sidecar handle.
    Word64,
}

/// One body word whose value a declared layout depends on.
///
/// Body state that can make a layout the wrong answer is all the same shape —
/// read a word at a fixed offset, require an exact value, otherwise leave the
/// fast path. A collection's no-expando flags word, an array's exotic sidecar,
/// a typed array's element kind and its length-tracking flag are that one guard
/// with different offsets, widths and expected values, so generated code has
/// one lowering per *width* and never one per family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitBodyGuard {
    /// Byte offset of the word from the body's `GcHeader`.
    pub byte: u32,
    /// How much of it to read.
    pub width: JitGuardWidth,
    /// Value the word must hold for the layout to apply.
    pub expect: u32,
}

impl JitBodyGuard {
    /// A word that must read zero: an absent expando, a null sidecar, a clear
    /// flag.
    #[must_use]
    pub const fn clear(byte: u32, width: JitGuardWidth) -> Self {
        Self {
            byte,
            width,
            expect: 0,
        }
    }
}

/// How a guarded method site proves the receiver it recorded.
///
/// Both forms end the same way — a value slab holding the method slot — so one
/// emitter lowers them; they differ only in what identifies the receiver and
/// where the method lives.
#[derive(Debug, Clone, Copy)]
pub enum JitGuardedReceiver {
    /// An ordinary object named by its hidden class. The method is in the
    /// receiver's own slab, or in a prototype resolved at run time when
    /// [`JitGuardedMethodCall::holder_shape`] is set, because `setPrototypeOf`
    /// moves the holder while the shape stays put. The entry reads only the
    /// call's arguments.
    Shape {
        /// Guarded receiver shape handle offset.
        shape: u32,
    },
    /// An exotic body named by its cell type tag, whose method always lives on
    /// a pinned realm prototype. The entry reads the receiver as its first
    /// argument, since the operation is *on* that body.
    Exotic {
        /// Expected receiver `GcHeader::type_tag`.
        type_tag: u8,
        /// Instance state that must hold before the prototype's method may be
        /// trusted. `None` for a body that carries no such state.
        guard: Option<JitBodyGuard>,
        /// Compressed offset of the pinned realm prototype holding the builtin.
        proto_offset: u32,
    },
}

/// One `Op::CallMethodValue` site whose callee is a declared native entry.
///
/// The layout fields are the same lowered cache program a property site
/// caches — guarded receiver shape, an optional guarded prototype holder, and
/// the slot byte — so generated code reuses the way walk and the prototype hop
/// rather than describing this access a second time. The entry id then selects
/// the call, exactly as it does at an ordinary call site: the family the id
/// resolves in is what decides the call protocol, so a read, an in-place
/// mutation and an allocating write are one description.
#[derive(Debug, Clone, Copy)]
pub struct JitGuardedMethodCall {
    /// How the receiver is proven before the method slot is read.
    pub receiver: JitGuardedReceiver,
    /// Guarded holder shape handle offset: the hopped prototype's shape for a
    /// [`JitGuardedReceiver::Shape`] receiver (`0` when it owns the slot), or
    /// the pinned prototype's shape for an exotic one.
    pub holder_shape: u32,
    /// Byte offset of the method slot inside the holder's value slab.
    pub method_value_byte: u32,
    /// External-reference index of the exact bootstrap function guarded before
    /// the entry runs. Isolate-local and build-stable, so unlike a raw entry
    /// address it can be compared by generated code without a relocation.
    pub builtin_native_ref: u32,
    /// Declared entry invoked once every guard passes.
    pub entry_stub_id: crate::native_abi::RuntimeStubId,
    /// Safepoint published for an entry that may collect.
    /// [`crate::native_abi::NO_SAFEPOINT`] for one that cannot.
    pub safepoint_id: crate::native_abi::SafepointId,
    /// Exact JavaScript argument count the entry implements.
    pub argument_count: u8,
}

/// A callee the compiler may splice into a caller's `Op::Call` site.
///
/// The callee is compiled from its *own* inputs, exactly as it would be as an
/// outermost function: its constant pool, its instruction overlays, its global
/// cells, its property and element facts, its call plans. A spliced body whose
/// facts came from the caller would have nothing to fold against and could only
/// lower its loads as exits, so the whole baked body travels with the
/// candidate. A runtime callable whose function id does not match
/// [`JitInlineCallee::function_id`] side-exits at the caller's canonical call
/// PC.
#[derive(Debug, Clone)]
pub struct JitInlineCallee {
    /// Fully baked compile inputs for the callee body.
    pub body: Arc<JitCompileSnapshot>,
}

impl JitInlineCallee {
    /// Callee function id the call-site identity guard is keyed on.
    #[must_use]
    pub fn function_id(&self) -> u32 {
        self.body.code_block.id
    }

    /// Callee formal parameter count; must equal the call's argument count for
    /// the site to inline.
    #[must_use]
    pub fn param_count(&self) -> u16 {
        self.body.code_block.param_count
    }

    /// Callee register-window length; the spliced body runs in a window of this
    /// many slots.
    #[must_use]
    pub fn register_count(&self) -> u16 {
        self.body.code_block.register_count
    }
}

/// Exact receiver/prototype/method-slot identity for one monomorphic method
/// target.
///
/// This guard is shared by leaf inlining and compiler-generated method calls:
/// both must re-read the current slot and prove the same callable before any
/// effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitMethodGuard {
    /// Method function id the slot identity check is keyed on.
    pub method_fid: u32,
    /// Receiver shape-handle compressed offset.
    pub recv_shape: u32,
    /// Shape-handle compressed offsets of each prototype hopped from the
    /// receiver to the method holder, in hop order.
    pub proto_chain: Vec<u32>,
    /// Byte offset inside the holder object's value slab for the method slot.
    pub method_value_byte: u32,
}

/// A method the baseline may splice into a caller's `Op::CallMethodValue` site.
/// Carries the method's body plus the shared identity guard. Per body
/// `LoadProperty`/`StoreProperty` byte-PC, the value byte offset within the
/// decompressed receiver.
/// Method identity is verified inline before body entry: the emitter chases
/// the flat prototype handle once per
/// [`JitMethodGuard::proto_chain`] entry,
/// guards each hopped object's shape, reads the method slot at
/// [`JitMethodGuard::method_value_byte`] from the final holder, and
/// compares the resolved closure's `function_id` to
/// [`JitMethodGuard::method_fid`]. A prototype-method reassignment or any
/// shape change along the chain side-exits at the original method call before
/// effects. An optimizing backend may reuse that proof across iterations only
/// after proving the receiver loop-invariant and the whole natural loop free
/// of mutation and reentrant execution.
#[derive(Debug, Clone)]
pub struct JitInlineMethod {
    /// Fully baked compile inputs for the method body, resolved against the
    /// method's own constant pool and feedback rather than the caller's.
    pub body: Arc<JitCompileSnapshot>,
    /// Exact receiver/prototype/method-slot guard shared with generated calls.
    pub guard: JitMethodGuard,
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
    /// with bounded recursion.
    pub nested_methods: rustc_hash::FxHashMap<u32, JitInlineMethod>,
}

impl JitInlineMethod {
    /// Method formal parameter count (excluding `this`); must equal argc.
    #[must_use]
    pub fn param_count(&self) -> u16 {
        self.body.code_block.param_count
    }

    /// Method virtual-register-window length.
    #[must_use]
    pub fn register_count(&self) -> u16 {
        self.body.code_block.register_count
    }
}

/// Source opcode represented by compiler-generated call linkage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum JitDirectCallKind {
    /// Ordinary `Op::Call`.
    Plain,
    /// Guarded `Op::CallMethodValue`.
    Method,
    /// Base `Op::New` construction.
    Construct,
    /// `Op::New` targeting a derived constructor.
    DerivedConstruct,
    /// `Op::SuperConstruct` targeting a base constructor.
    SuperConstruct,
    /// `Op::SuperConstruct` targeting another derived constructor.
    DerivedSuperConstruct,
}

/// `this` binding performed by compiler-generated call linkage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum JitDirectCallThisMode {
    /// Strict functions receive `undefined`; arrows retain their lexical
    /// binding from the guarded closure metadata.
    StrictOrLexical,
    /// An ordinary sloppy `Op::Call` receives the active realm's rooted global
    /// object. Closures carrying an explicit bound receiver side-exit before
    /// entry because primitive `ToObject` remains an interpreter operation.
    SloppyGlobal,
    /// A guarded method call passes its exact object receiver.
    MethodReceiver,
    /// Construction linkage owns the receiver state and `new.target`; derived
    /// entry uses the hole sentinel until `super()` commits `this`.
    ConstructReceiver,
    /// A derived constructor enters with an uninitialized `this` binding and
    /// applies the derived-return contract after `super()` or object return.
    DerivedConstructor,
}

/// A settled prototype-hop load: the two shapes it guards and the slot it
/// reaches.
///
/// Both shapes are compressed shape-handle offsets — stable tokens, since
/// shapes are interned, immortal, and pinned in non-moving old space, so a
/// descriptor holds no GC pointer and needs no tracing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct JitInlinePropertyHop {
    /// Hidden class the receiver must still carry for the hop to reach this
    /// prototype.
    pub receiver_shape: u32,
    /// Hidden class the prototype must still carry for the slot to be there.
    pub holder_shape: u32,
    /// Byte offset of the slot inside the holder's value slab.
    pub value_byte: u32,
}

/// One cache program lowered to the form generated code executes inline.
///
/// This is the single description of a property fast path: the interpreter's
/// [`crate::cache_ir::CacheStub`] lowers its op sequence to this, and every
/// tier's emitted probe consumes exactly this. Nothing re-derives a slot or a
/// guard on its own, so the tiers cannot disagree about what a site caches.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct JitPropertyIcWay {
    /// Guarded receiver shape. `0` marks an empty way and never matches a live
    /// receiver, so empty ways are skipped without a branch of their own.
    pub receiver_shape: u32,
    /// Guarded shape of the direct prototype. For an existing-slot program it
    /// owns the loaded slot; `0` means the receiver owns the slot. For an add
    /// transition it proves the property absent on that direct prototype;
    /// `0` means the receiver instead has a null prototype.
    pub holder_shape: u32,
    /// Byte offset of the slot inside the holder's value slab.
    pub value_byte: u32,
    /// Canonical child shape installed by an add-property transition. `0`
    /// keeps the existing-slot load/store meaning above. A non-zero token
    /// changes the program into an append: `receiver_shape` is then the parent
    /// shape, `value_byte` is the new own slot, and `holder_shape` is either
    /// zero for a null receiver prototype or the guarded direct-prototype
    /// shape for a completely missing property.
    pub transition_shape: u32,
}

impl JitPropertyIcWay {
    /// Whether the program reaches the slot through the receiver's prototype.
    #[must_use]
    pub const fn hops_to_prototype(&self) -> bool {
        self.transition_shape == 0 && self.holder_shape != 0
    }

    /// Whether the program appends one own slot and publishes a child shape.
    #[must_use]
    pub const fn is_add_transition(&self) -> bool {
        self.transition_shape != 0
    }
}

const _: [(); 16] = [(); std::mem::size_of::<JitPropertyIcWay>()];
const _: [(); 4] = [(); std::mem::align_of::<JitPropertyIcWay>()];
const _: [(); 0] = [(); std::mem::offset_of!(JitPropertyIcWay, receiver_shape)];
const _: [(); 4] = [(); std::mem::offset_of!(JitPropertyIcWay, holder_shape)];
const _: [(); 8] = [(); std::mem::offset_of!(JitPropertyIcWay, value_byte)];
const _: [(); 12] = [(); std::mem::offset_of!(JitPropertyIcWay, transition_shape)];

/// One monomorphic native leaf call selected from ordinary-call feedback.
///
/// The declared entry id is the target's whole identity: it selects the machine
/// code, keys the relocation, and names the operation in diagnostics through
/// [`crate::native_abi::runtime_stub_name`], which is process-independent. No
/// second taxonomy of builtins exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitStaticNativeCall {
    /// External-reference index of the exact bootstrap function checked before
    /// the leaf executes. Isolate-local and build-stable, so unlike a raw entry
    /// address it can be compared by generated code without a relocation.
    pub builtin_native_ref: u32,
    /// Declared leaf entry the call site invokes once its guards pass. The
    /// operation lives in that entry, so a new builtin needs no generated code.
    pub leaf_stub_id: crate::native_abi::RuntimeStubId,
    /// Exact JavaScript argument count the declared entry implements. A site
    /// whose count differs is not lowered, so variadic and defaulted forms keep
    /// the ordinary path instead of being truncated to this entry's arity.
    pub argument_count: u8,
}

/// VM-resolved direct-call target for one eligible compiled callee.
///
/// This is metadata only: frame reservation/rooting stays VM-owned, while the
/// backend consumes `entry_cell` once it can emit the matching frame build and
/// call/return sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitDirectCallPlan {
    /// Callee function id in the executable module.
    pub function_id: u32,
    /// Generation current when this cold compile plan was captured. Generated
    /// code uses it only for diagnostics; dispatch follows [`Self::entry_cell`].
    pub code_object_id: u64,
    /// Stable address of the function's
    /// [`crate::native_abi::FunctionEntryCell`]. The cell is registry-owned,
    /// never reused, and atomically selects the current generation.
    pub entry_cell: u64,
    /// Native tier current when this cold compile plan was captured.
    pub tier: NativeFrameKind,
    /// Exact plain-call `this` binding the generated linkage must perform.
    pub this_mode: JitDirectCallThisMode,
    /// Whether the target enters with an uninitialized `this` binding and
    /// applies derived-constructor return semantics.
    pub is_derived_constructor: bool,
    /// Persistent machine-stack bytes reserved by the target after native
    /// entry, when that tier can safely cold-deopt from a stack-owned caller.
    ///
    /// `None` rejects compiler-generated stack linkage for this generation.
    /// Existing VM-owned call paths may still enter it through their own
    /// materialized-frame contract.
    pub generated_stack_frame_bytes: Option<u32>,
    /// Number of formal parameter registers.
    pub param_count: u16,
    /// Total callee register-window length.
    pub register_count: u16,
    /// Fresh capture cells allocated by each invocation.
    pub own_upvalue_count: u16,
    /// Closure-owned capture cells copied after the fresh prefix.
    pub inherited_upvalue_count: u16,
}

/// One baked compiler-native target for a monomorphic plain-call site.
///
/// Presence in [`JitCompileSnapshot::direct_callees`] proves the callee is an
/// ordinary synchronous body and has a stable entry cell with one current
/// entry-capable generation. Dynamic callable
/// identity, supported `this` binding, and target liveness remain machine-code
/// guards; tier promotion patches the cell without invalidating this caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitDirectCallee {
    /// Function identity, stable entry cell, and callee register-window shape.
    pub plan: JitDirectCallPlan,
    /// Optional class-only safepoint-free receiver allocation program.
    pub receiver_allocation: Option<JitReceiverAllocationPlan>,
}

/// Physical proof for one generated binding hit.
///
/// This enum deliberately contains no read/write/delete semantic variant. The
/// authoritative operation and operand roles live in
/// `otter_bytecode::opcode_schema::BindingSemantics`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingHitProof {
    /// Permanent global-declarative cell rooted by the environment record.
    GlobalLexical {
        /// Compressed GC-cage offset of the non-moving upvalue cell.
        cell_offset: u32,
        /// Whether assignment may update the live cell.
        writable: bool,
    },
    /// Guarded own-data slot in the global object record.
    GlobalObject {
        /// Expected ordinary shape handle or dictionary structural id.
        shape: u64,
        /// Whether `shape` names a dictionary structural id.
        dictionary: bool,
        /// Byte offset of the property inside the object's value slab.
        value_byte: u32,
        /// Global declarative epoch that keeps later lexicals from shadowing it.
        global_lexical_epoch: u64,
        /// Whether the proven own data descriptor is writable.
        writable: bool,
    },
}

/// One permanent global-declarative binding available to generated code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitGlobalLexicalLoad {
    /// Compressed GC-cage offset of the rooted, non-moving upvalue cell.
    pub cell_offset: u32,
}

/// One primitive-string constant cell available to generated code.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct JitStringConstantCell {
    /// Process-local address of the isolate-owned, GC-traced `Value` cell.
    /// The cell allocation is stable and outlives every code object in the
    /// isolate; artifacts retain only function/byte-PC identity.
    pub cell_addr: usize,
}

impl std::fmt::Debug for JitStringConstantCell {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "JitStringConstantCell {{ cell_addr: <redacted> }}"
        )
    }
}

/// One guarded own-data load from the global object record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitGlobalObjectLoad {
    /// Expected ordinary shape-handle offset or dictionary structural id.
    pub shape: u64,
    /// Whether [`Self::shape`] names the dictionary structural id rather than
    /// an ordinary compressed shape handle.
    pub dictionary: bool,
    /// Byte offset of the property inside the object's value slab.
    pub value_byte: u32,
    /// Global declarative-record epoch captured with the slot lookup. A
    /// mismatch means a later lexical declaration may shadow this property.
    pub global_lexical_epoch: u64,
}

/// One guarded method target with an exact generated callee plan.
#[derive(Debug, Clone)]
pub struct JitDirectMethod {
    /// Zero-based position in the observed bounded method-target chain.
    pub target_index: u32,
    /// Total observed targets at this call site when the plan was baked.
    pub target_count: u32,
    /// Receiver/prototype/method-slot identity checked immediately before call.
    pub guard: JitMethodGuard,
    /// Exact entry generation and callee frame shape.
    pub callee: JitDirectCallee,
}

/// VM-owned root descriptor for one native JIT activation.
///
/// The pointer names the canonical [`crate::native_abi::NativeFrame`]. Machine
/// IR values that live across reentrant calls are published separately through
/// the interpreter-owned [`JitMachineRootRecord`] chain.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct JitNativeActivation {
    /// Published canonical native activation.
    pub frame: *mut crate::native_abi::NativeFrame,
}

impl JitNativeActivation {
    /// Empty inactive descriptor.
    pub const EMPTY: Self = Self {
        frame: std::ptr::null_mut(),
    };
}

/// One stack-owned Machine IR root publication linked into the interpreter's
/// active native-root chain.
#[repr(C, align(8))]
#[derive(Clone, Copy, Debug)]
pub struct JitMachineRootRecord {
    /// Previous record address, or zero at the outermost Machine safepoint.
    pub previous: u64,
    /// Base of compact initialized tagged root homes.
    pub root_base: *mut u64,
    /// Code generation whose safepoint table owns the homes.
    pub code_object_id: u64,
    /// Number of initialized tagged homes.
    pub root_count: u16,
    /// Reserved padding kept zero by generated publishers.
    pub reserved: u16,
    /// Dense code-object-local safepoint identity.
    pub safepoint_id: crate::native_abi::SafepointId,
}

const _: [(); 32] = [(); std::mem::size_of::<JitMachineRootRecord>()];
const _: [(); 0] = [(); std::mem::offset_of!(JitMachineRootRecord, previous)];
const _: [(); 8] = [(); std::mem::offset_of!(JitMachineRootRecord, root_base)];
const _: [(); 16] = [(); std::mem::offset_of!(JitMachineRootRecord, code_object_id)];
const _: [(); 24] = [(); std::mem::offset_of!(JitMachineRootRecord, root_count)];
const _: [(); 28] = [(); std::mem::offset_of!(JitMachineRootRecord, safepoint_id)];

const _: [(); 8] = [(); std::mem::size_of::<JitNativeActivation>()];
const _: [(); 8] = [(); std::mem::align_of::<JitNativeActivation>()];
const _: [(); 0] = [(); std::mem::offset_of!(JitNativeActivation, frame)];

/// Current static offsets needed by native Array guards.
///
/// Dense element storage remains behind runtime stubs because Rust container
/// layout is not part of the native ABI.
#[derive(Debug, Clone, Copy, Default)]
pub struct JitArrayLayout {
    /// `GcHeader::type_tag` of an ordinary `ArrayBody`.
    pub type_tag: u8,
    /// Offset to `ArrayBody.length`, the logical `length` property.
    pub length_byte: u32,
}

/// Where a receiver family keeps the base of its element storage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum JitElementBase {
    /// No family is described and the site keeps the runtime path.
    #[default]
    None,
    /// A word in the receiver body itself, as a dense array keeps.
    InBody {
        /// Byte offset of the base pointer from the body's `GcHeader`.
        byte: u32,
    },
    /// A word in a separate buffer cell the receiver names by a compressed
    /// handle, at the receiver's own byte offset into it, as a typed view
    /// keeps. The buffer's own liveness and live byte length are guarded on the
    /// way through, because detach or resizable-buffer shrinkage leaves a fixed
    /// view's cached length untouched and so is invisible to that bounds check.
    ThroughLocalBuffer {
        /// Byte offset, in the receiver body, of the buffer's storage
        /// discriminant.
        storage_tag_byte: u32,
        /// Discriminant value naming an in-heap local buffer. Any other
        /// storage leaves the fast path.
        local_tag: u32,
        /// Byte offset, in the receiver body, of the compressed buffer handle.
        handle_byte: u32,
        /// Byte offset, in the buffer body, of the detached flag.
        detached_byte: u32,
        /// Byte offset, in the buffer body, of the element base pointer.
        data_ptr_byte: u32,
        /// Byte offset, in the buffer body, of the live backing-store byte
        /// length. A fixed view is wholly out of bounds when its complete
        /// construction-time extent no longer fits after a resize.
        byte_len_byte: u32,
        /// Byte offset, in the receiver body, of the view's own byte offset
        /// into the buffer.
        view_offset_byte: u32,
    },
}

/// How one element is stored, which fixes both the address stride and the load.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum JitElementRepr {
    /// A boxed `Value`. A hole is an absent property, so it leaves the fast
    /// path.
    #[default]
    Boxed,
    /// A raw signed 32-bit scalar, boxed on the way out.
    Int32,
    /// A raw IEEE-754 double, canonicalized and boxed on the way out.
    Float64,
}

impl JitElementRepr {
    /// Log2 of the element stride in bytes.
    #[must_use]
    pub const fn stride_shift(self) -> u32 {
        match self {
            Self::Boxed | Self::Float64 => 3,
            Self::Int32 => 2,
        }
    }
}

/// One receiver family's indexed-element storage, as the guard program reads it.
///
/// Every element-bearing body answers the same questions — which cell tag it
/// carries, what instance state invalidates the layout, where its live element
/// count lives, where its element base lives, and how one element is stored —
/// so the address program is written once and the family is data.
#[derive(Debug, Clone, Copy, Default)]
pub struct JitElementAccess {
    /// Expected receiver `GcHeader::type_tag`.
    pub type_tag: u8,
    /// Instance state that must hold before the offsets below are trusted.
    /// Read in order; an absent entry ends the list.
    pub guards: [Option<JitBodyGuard>; 2],
    /// Byte offset of the VM-maintained live element count.
    pub length_byte: u32,
    /// Width of that count.
    pub length_width: JitGuardWidth,
    /// Where the element base pointer lives. The storage is a plain host
    /// allocation, so it survives a moving collection of the body; every
    /// mutation refreshes it.
    pub base: JitElementBase,
    /// How one element is stored.
    pub element: JitElementRepr,
}

impl JitElementAccess {
    /// Build the VM's complete ordinary packed-double Array access program.
    #[must_use]
    pub fn packed_double_array() -> Self {
        let header = std::mem::size_of::<otter_gc::GcHeader>() as u32;
        Self {
            type_tag: crate::array::ARRAY_BODY_TYPE_TAG,
            guards: [
                Some(JitBodyGuard::clear(
                    header + std::mem::offset_of!(crate::array::ArrayBody, exotic) as u32,
                    JitGuardWidth::Word32,
                )),
                Some(Self::packed_double_kind_guard(
                    header + crate::array::ARRAY_BODY_DENSE_KIND_OFFSET as u32,
                )),
            ],
            length_byte: header + crate::array::ARRAY_BODY_DENSE_LEN_OFFSET as u32,
            length_width: JitGuardWidth::Word32,
            base: JitElementBase::InBody {
                byte: header + crate::array::ARRAY_BODY_ELEMENTS_PTR_OFFSET as u32,
            },
            element: JitElementRepr::Float64,
        }
    }

    /// Build the exact body guard for the VM's packed-double array storage.
    ///
    /// The byte offset is snapshot data; the physical discriminant remains
    /// owned here instead of leaking the private storage enum to a backend.
    #[must_use]
    pub const fn packed_double_kind_guard(byte: u32) -> JitBodyGuard {
        JitBodyGuard {
            byte,
            width: JitGuardWidth::Byte,
            expect: crate::array::DENSE_ELEMENT_KIND_PACKED_DOUBLE,
        }
    }

    /// Whether this immutable guard program proves an ordinary Array whose
    /// complete live dense prefix is stored as raw hole-free doubles.
    ///
    /// The physical discriminant remains VM-private; JIT backends consume the
    /// semantic layout through this snapshot-owned predicate.
    #[must_use]
    pub fn is_packed_double_array(&self) -> bool {
        let [Some(exotic), Some(kind)] = self.guards else {
            return false;
        };
        let header = std::mem::size_of::<otter_gc::GcHeader>() as u32;
        self.type_tag == crate::array::ARRAY_BODY_TYPE_TAG
            && self.element == JitElementRepr::Float64
            && matches!(
                self.base,
                JitElementBase::InBody { byte }
                    if byte == header + crate::array::ARRAY_BODY_ELEMENTS_PTR_OFFSET as u32
            )
            && self.length_byte == header + crate::array::ARRAY_BODY_DENSE_LEN_OFFSET as u32
            && self.length_width == JitGuardWidth::Word32
            && exotic.byte == header + std::mem::offset_of!(crate::array::ArrayBody, exotic) as u32
            && exotic.width == JitGuardWidth::Word32
            && exotic.expect == 0
            && kind.byte == header + crate::array::ARRAY_BODY_DENSE_KIND_OFFSET as u32
            && kind.width == JitGuardWidth::Byte
            && kind.expect == crate::array::DENSE_ELEMENT_KIND_PACKED_DOUBLE
    }
}

/// One receiver shape a property site resolves, with the own slot it reaches.
///
/// A site contributes one of these per installed cache program, so a
/// monomorphic site is the one-element case of a polymorphic chain rather than
/// a separate mechanism.
///
/// The runtime cache cell exists because a site's shape can change after the
/// code is generated. A site the profile has settled on does not need it: the
/// shape is a compile-time constant, so the probe compares against an
/// immediate and reads a fixed slab offset instead of loading the cell and
/// walking its ways. A receiver that stops matching misses to the same window
/// transition the cell walk would have, which re-patches the cell and lets the
/// next compile re-bake.
#[derive(Debug, Clone, Copy)]
pub struct JitInlinePropertyLoad {
    /// Guarded receiver shape handle offset.
    pub receiver_shape: u32,
    /// Byte offset of the slot inside the receiver's own value slab.
    pub value_byte: u32,
}

/// Ready-to-use byte offsets and tags for inline primitive string fast paths.
#[derive(Debug, Clone, Copy, Default)]
pub struct JitStringLayout {
    /// `GcHeader::type_tag` of a `JsStringBody` (guarded at byte 0).
    pub string_type_tag: u8,
    /// Offset to `JsStringBody.len`, the UTF-16 code-unit length.
    pub string_len_byte: u32,
    /// Offset to the stable [`crate::string::JsStringBodyRepr`] tag.
    pub string_repr_byte: u32,
    /// Offset to the contiguous representation payload.
    pub string_repr_payload_byte: u32,
    /// Offset immediately after the body, where sequential units begin.
    pub string_body_size: u32,
    /// Stable tag for inline UTF-16 units.
    pub inline_flat_tag: u8,
    /// Stable tag for sequential UTF-16 units.
    pub seq_flat_tag: u8,
    /// Stable tag for inline Latin-1 units.
    pub inline_latin1_tag: u8,
    /// Stable tag for sequential Latin-1 units.
    pub seq_latin1_tag: u8,
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
    /// Already-in-the-remembered-set flag bit within the flag byte
    /// (`REMEMBERED_FLAG`). The generational barrier records a parent at most
    /// once per scavenge interval, so a set bit is the barrier's fast out.
    pub remembered_flag: u32,
}

/// Mutable JIT feedback overlay for one authoritative CodeBlock instruction.
#[derive(Debug, Clone)]
pub struct JitInstructionMetadata {
    /// Dense instruction index into the owning compile snapshot's CodeBlock.
    pub(crate) instruction_index: u32,
    /// Cold serialized byte PC used by profiling and diagnostics.
    pub byte_pc: u32,
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
    /// Whether this call-family instruction reached semantic dispatch.
    ///
    /// Recording precedes callable and method-property resolution. `false`
    /// therefore proves a genuinely unattempted cold branch; a throwing,
    /// non-callable, static-native, or otherwise unsupported hot site reports
    /// `true` even when it has no compiler-native direct target.
    pub call_attempted: bool,
    /// Arithmetic representation observations frozen for this canonical PC.
    pub(crate) arith_feedback: ArithFeedback,
}

impl JitInstructionMetadata {
    fn without_feedback(instruction_index: u32, byte_pc: u32) -> Self {
        Self {
            instruction_index,
            byte_pc,
            make_self: false,
            load_array_length: false,
            method_hint: JitMethodHint::None,
            load_number: None,
            call_attempted: false,
            arith_feedback: ArithFeedback::default(),
        }
    }
}

/// Transient backend-test instruction input.
///
/// This is consumed while building one authoritative [`CodeBlock`]; it is not
/// retained as an executable or frozen compatibility representation.
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct JitTestInstruction {
    pub(crate) op: Op,
    pub(crate) instruction_pc: u32,
    pub(crate) byte_pc: u32,
    pub(crate) operands: Vec<Operand>,
}

impl JitTestInstruction {
    /// Build transient input for a backend unit-test CodeBlock.
    #[must_use]
    pub fn new(op: Op, instruction_pc: u32, byte_pc: u32, operands: Vec<Operand>) -> Self {
        Self {
            op,
            instruction_pc,
            byte_pc,
            operands,
        }
    }
}

impl JitCompileSnapshot {
    /// Build a feedback-free snapshot for backend lowering tests.
    ///
    /// Production compilation always starts at
    /// [`CodeBlock::jit_compile_snapshot`]. This fixture still creates one
    /// authoritative `CodeBlock`; its dynamic overlay addresses the same dense
    /// instruction slice by index.
    #[must_use]
    pub fn without_feedback(
        function_id: u32,
        param_count: u16,
        register_count: u16,
        instructions: Vec<JitTestInstruction>,
    ) -> Self {
        let code_block =
            CodeBlock::jit_test_stub(function_id, param_count, register_count, &instructions);
        let instructions = code_block
            .code
            .iter()
            .enumerate()
            .map(|(index, _)| {
                JitInstructionMetadata::without_feedback(
                    index as u32,
                    code_block
                        .instruction_byte_pc(index)
                        .expect("test CodeBlock metadata matches instructions"),
                )
            })
            .collect();
        Self {
            code_block,
            derived_constructor: false,
            cage_base: 0,
            array_layout: JitArrayLayout::default(),
            element_accesses: rustc_hash::FxHashMap::default(),
            string_layout: JitStringLayout::default(),
            object_shape_byte: 0,
            object_dictionary_shape_id_byte: 0,
            object_values_ptr_byte: 0,
            object_inline_values_byte: 0,
            object_slab_handle_byte: 0,
            object_slab_len_byte: 0,
            object_inline_slot_cap: 0,
            object_extensible_byte: 0,
            object_shape_cache_mode_byte: 0,
            object_shape_cache_fast: 0,
            object_slot_attrs_overridden_byte: 0,
            object_exotic_handle_byte: 0,
            object_cell_bytes: 0,
            gc_barrier: JitGcBarrierLayout::default(),
            jit_proto_byte: 0,
            closure_call_layout: JitClosureCallLayout::default(),
            class_constructor_layout: JitClassConstructorLayout::default(),
            primitive_cell_type_tags: [0; 3],
            upvalue_value_byte: 0,
            collection_layout: JitCollectionLayout::default(),
            native_ref_byte: 0,
            instructions,
            global_lexical_loads: rustc_hash::FxHashMap::default(),
            string_constant_cells: rustc_hash::FxHashMap::default(),
            global_object_loads: rustc_hash::FxHashMap::default(),
            static_native_calls: rustc_hash::FxHashMap::default(),
            direct_callees: rustc_hash::FxHashMap::default(),
            direct_constructs: rustc_hash::FxHashMap::default(),
            direct_methods: rustc_hash::FxHashMap::default(),
            inline_callees: rustc_hash::FxHashMap::default(),
            inline_methods: rustc_hash::FxHashMap::default(),
            inline_poly_methods: rustc_hash::FxHashMap::default(),
            guarded_method_calls: rustc_hash::FxHashMap::default(),
            property_loads: rustc_hash::FxHashMap::default(),
            property_stores: rustc_hash::FxHashMap::default(),
            binding_hit_proofs: rustc_hash::FxHashMap::default(),
            constructor_field_transitions: rustc_hash::FxHashMap::default(),
            property_prototype_loads: rustc_hash::FxHashMap::default(),
            optimized_bail_pcs: std::collections::BTreeSet::new(),
            safepoints: rustc_hash::FxHashMap::default(),
        }
    }

    /// Arithmetic feedback frozen for one canonical instruction PC.
    #[must_use]
    pub fn feedback_at(&self, instruction_pc: u32) -> ArithFeedback {
        self.instructions
            .get(instruction_pc as usize)
            .map_or_else(ArithFeedback::default, |instruction| {
                instruction.arith_feedback
            })
    }

    /// Seed arithmetic feedback in a backend test snapshot.
    ///
    /// This only changes the immutable test overlay; production snapshots are
    /// populated from their owning `CodeBlock` feedback cells.
    #[doc(hidden)]
    pub fn seed_arith_feedback_for_test(&mut self, instruction_pc: u32, feedback: ArithFeedback) {
        self.instructions
            .get_mut(instruction_pc as usize)
            .expect("test feedback PC belongs to the snapshot")
            .arith_feedback = feedback;
    }

    /// Mark one backend-test call site as previously attempted.
    ///
    /// Production snapshots obtain this fact from their owning CodeBlock's
    /// dense feedback cell.
    #[doc(hidden)]
    pub fn seed_call_attempted_for_test(&mut self, instruction_pc: u32) {
        self.instructions
            .get_mut(instruction_pc as usize)
            .expect("test feedback PC belongs to the snapshot")
            .call_attempted = true;
    }
}

impl JitInstructionMetadata {
    /// Resolve this overlay entry against its authoritative CodeBlock.
    #[must_use]
    pub fn resolve<'a>(&self, code_block: &'a CodeBlock) -> &'a CodeBlockInstruction {
        code_block
            .instr_at_index(self.instruction_index as usize)
            .expect("JIT metadata instruction index belongs to its CodeBlock")
    }

    /// Arithmetic feedback recorded for this instruction.
    ///
    /// Read per instruction rather than per compile snapshot: a spliced callee
    /// carries its own overlay, and a snapshot-wide lookup would read the root
    /// body's cell for it.
    #[must_use]
    pub fn arith_feedback(&self) -> ArithFeedback {
        self.arith_feedback
    }

    /// Opcode from the authoritative CodeBlock instruction.
    #[must_use]
    pub fn op(&self, code_block: &CodeBlock) -> Op {
        code_block.op(self.resolve(code_block))
    }

    /// Canonical instruction PC from the authoritative CodeBlock instruction.
    #[must_use]
    pub fn instruction_pc(&self, code_block: &CodeBlock) -> u32 {
        self.resolve(code_block).instruction_pc
    }

    /// Cold serialized byte PC retained by the immutable compile snapshot.
    #[must_use]
    pub const fn byte_pc(&self) -> u32 {
        self.byte_pc
    }

    /// Dense property IC site from the authoritative CodeBlock instruction.
    #[must_use]
    pub fn property_ic_site(&self, code_block: &CodeBlock) -> Option<usize> {
        self.resolve(code_block).property_ic_site()
    }

    /// Decode one schema-typed operand from the authoritative CodeBlock.
    #[must_use]
    pub fn operand(&self, code_block: &CodeBlock, index: usize) -> Option<Operand> {
        code_block.operand(self.resolve(code_block), index)
    }

    /// Decode one constant-pool index operand.
    #[must_use]
    pub fn const_index(&self, code_block: &CodeBlock, index: usize) -> Option<u32> {
        code_block.const_index(self.resolve(code_block), index)
    }

    /// Decode one signed immediate operand.
    #[must_use]
    pub fn imm32(&self, code_block: &CodeBlock, index: usize) -> Option<i32> {
        code_block.imm32(self.resolve(code_block), index)
    }

    /// Borrow all schema-typed operands from the authoritative CodeBlock.
    #[must_use]
    pub fn operand_view<'a>(&self, code_block: &'a CodeBlock) -> crate::OperandView<'a> {
        code_block.operand_view(self.resolve(code_block))
    }

    /// Pre-resolved exception handlers for this `EnterTry` instruction.
    #[must_use]
    pub fn exception_region(&self, code_block: &CodeBlock) -> Option<JitExceptionRegion> {
        let region = code_block.exception_region(self.instruction_pc(code_block))?;
        Some(JitExceptionRegion {
            catch_pc: region.catch_pc,
            finally_pc: region.finally_pc,
            exception_register: region.exception_register,
        })
    }
}

/// Receiver family observed at one indexed-element site.
///
/// Ordinary arrays are split by their live dense-storage representation. A
/// representation transition is therefore the same material feedback change
/// as switching between an Array and a TypedArray: generated code either
/// proves the exact current layout or leaves through its pre-effect deopt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum JitElementFamily {
    /// Nothing observed yet, or a receiver no instance describes.
    #[default]
    Unseen,
    /// Ordinary dense arrays whose slots contain boxed [`Value`] words.
    DenseTagged,
    /// Ordinary dense arrays whose complete live prefix contains raw `f64`s.
    DenseFloat64,
    /// `Int32Array` receivers only.
    TypedInt32,
    /// `Float64Array` receivers only.
    TypedFloat64,
    /// More than one family, or one no instance describes.
    Generic,
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
    /// `String.prototype.codePointAt`.
    StringCodePointAt,
    /// `String.prototype.indexOf`.
    StringIndexOf,
    /// `String.prototype.includes`.
    StringIncludes,
    /// `String.prototype.startsWith`.
    StringStartsWith,
    /// `String.prototype.endsWith`.
    StringEndsWith,
    /// `Number.prototype.toString`.
    NumberToString,
}

/// Cold reconstruction input for one frame in a nested inline deopt.
///
/// Decoded from the emitted side-exit buffer and consumed only by
/// [`crate::Interpreter::jit_deopt_materialize_inline_frames`]. Normal tier
/// transitions retain the canonical native activation and never build this
/// owned record.
pub struct JitDeoptFrame {
    /// Function id this frame executes.
    pub callee_fid: u32,
    /// Logical PC to resume this frame at.
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

/// VM-owned runtime state retained behind [`crate::native_abi::VmThread`]
/// while compiled code can re-enter the VM for typed runtime operations such
/// as closure allocation.
///
/// # Invariants
/// - Pointers are valid only for the duration of one
///   [`JitFunctionCode::run_entry`] call; the JIT must not retain them.
/// - The VM guarantees no live `&mut` aliases these pointers for
///   the call's duration (it forms them from its own borrows and does not touch
///   those borrows until the call returns).
#[repr(C, align(8))]
#[derive(Clone, Copy)]
pub struct VmRuntimeActivation {
    /// Owning interpreter.
    pub(crate) vm: *mut crate::Interpreter,
    /// Active stable-address frame stack.
    pub(crate) stack: *mut crate::ActivationStack,
    /// Linked execution context.
    pub(crate) context: *const crate::ExecutionContext,
    /// Index of the executing (compiled) frame within `stack`.
    frame_index: usize,
}

impl VmRuntimeActivation {
    /// Publish one synchronous compiled activation from live VM borrows.
    pub(crate) fn new(
        vm: &mut crate::Interpreter,
        stack: &mut crate::ActivationStack,
        context: &crate::ExecutionContext,
        frame_index: usize,
    ) -> Self {
        Self {
            vm,
            stack,
            context,
            frame_index,
        }
    }

    /// Owning interpreter address. Dereferencing requires the activation's
    /// dynamic non-aliasing contract.
    #[must_use]
    pub const fn vm_ptr(self) -> *mut crate::Interpreter {
        self.vm
    }

    /// Active stable-address frame-stack address.
    #[must_use]
    pub const fn stack_ptr(self) -> *mut crate::ActivationStack {
        self.stack
    }

    /// Linked execution-context address.
    #[must_use]
    pub const fn context_ptr(self) -> *const crate::ExecutionContext {
        self.context
    }

    /// Executing frame index.
    #[must_use]
    pub const fn frame_index(&self) -> usize {
        self.frame_index
    }

    /// Complete generated execution through the VM-owned post-entry boundary.
    ///
    /// This notes exact generated-call feedback, defers all cold work while a
    /// parent native activation remains published, and at the outer boundary
    /// returns the sole result carrier with any validated boxed Return/Throw
    /// payload rewritten by the collector. The JIT never observes a temporary
    /// root index or token.
    ///
    /// # Errors
    ///
    /// Returns [`crate::VmError::InvalidOperand`] when the activation no longer
    /// names its live interpreter or execution context.
    #[doc(hidden)]
    pub fn finish_compiled_entry(
        &self,
        result: NativeResultPair,
        feedback_dirty: bool,
    ) -> Result<NativeResultPair, crate::VmError> {
        // SAFETY: `VmRuntimeActivation::new` stores frozen pointers for exactly
        // this compiled-entry transaction; the JIT calls this only after its
        // own native frame has been unpublished and before returning control.
        let vm = unsafe { self.vm.as_mut() }.ok_or(crate::VmError::InvalidOperand)?;
        // SAFETY: same dynamic activation contract; the immutable context
        // outlives generated execution and the post-entry transaction.
        let context = unsafe { self.context.as_ref() }.ok_or(crate::VmError::InvalidOperand)?;
        vm.finish_compiled_entry_transaction(context, result, feedback_dirty)
    }

    /// Mirror non-lexical materialized call state into a fresh native frame.
    ///
    /// This is the sole materialized-entry bridge. The activation stores no
    /// tagged or compressed GC handles: it re-reads the materialized frame's
    /// cold call state immediately before the caller publishes `native_frame`.
    /// Direct-eval environment ownership is transferred separately by
    /// [`Self::with_native_eval_env_owner`].
    ///
    /// # Errors
    ///
    /// Returns [`crate::VmError::InvalidOperand`] when the frozen activation
    /// pointers or function identity do not match the destination frame.
    #[doc(hidden)]
    pub fn initialize_native_frame_state(
        &self,
        native_frame: &mut crate::native_abi::NativeFrame,
    ) -> Result<(), crate::VmError> {
        // SAFETY: `VmRuntimeActivation::new` stores live, frozen VM and stack
        // pointers for this entry transaction.
        let vm = unsafe { self.vm.as_ref() }.ok_or(crate::VmError::InvalidOperand)?;
        let stack = unsafe { self.stack.as_ref() }.ok_or(crate::VmError::InvalidOperand)?;
        let frame = stack
            .get(self.frame_index)
            .ok_or(crate::VmError::InvalidOperand)?;
        if frame.header.function_id != native_frame.header.function_id {
            return Err(crate::VmError::InvalidOperand);
        }
        if let Some(cold) = vm.frame_cold(frame) {
            native_frame.set_new_target(cold.new_target.unwrap_or_else(crate::Value::undefined));
            if cold.is_derived_constructor {
                native_frame.set_derived_constructor();
            }
        }
        Ok(())
    }

    /// Transfer the materialized frame's direct-eval environment to its
    /// published native frame for exactly one compiled dynamic extent.
    ///
    /// The materialized slot is null while generated code can allocate. A
    /// normal return, side exit, throw, or fatal status restores the surviving
    /// native handle before the activation is unpublished. Cold deopt paths
    /// may instead move that handle into a temporary materialized continuation;
    /// in that case both outer slots remain null when `operation` returns.
    ///
    /// # Safety
    ///
    /// `native_frame` must be the live frame just published for this exact
    /// activation. `operation` may access it only through the generated-code
    /// ABI and must return before the stack frame or native record is reclaimed.
    #[doc(hidden)]
    pub unsafe fn with_native_eval_env_owner<T>(
        &self,
        native_frame: *mut crate::native_abi::NativeFrame,
        operation: impl FnOnce() -> T,
    ) -> Result<T, crate::VmError> {
        let stack = unsafe { self.stack.as_mut() }.ok_or(crate::VmError::InvalidOperand)?;
        let frame = stack
            .get_mut(self.frame_index)
            .ok_or(crate::VmError::InvalidOperand)?;
        let native = unsafe { native_frame.as_mut() }.ok_or(crate::VmError::InvalidOperand)?;
        if frame.header.function_id != native.header.function_id || !native.eval_env.is_null() {
            return Err(crate::VmError::InvalidOperand);
        }
        std::mem::swap(&mut frame.eval_env, &mut native.eval_env);

        let result = operation();

        // Re-resolve both owners after generated execution: nested calls and
        // deopt continuations may grow the ActivationStack backing vector.
        let stack = unsafe { self.stack.as_mut() }.ok_or(crate::VmError::InvalidOperand)?;
        let frame = stack
            .get_mut(self.frame_index)
            .ok_or(crate::VmError::InvalidOperand)?;
        let native = unsafe { native_frame.as_mut() }.ok_or(crate::VmError::InvalidOperand)?;
        if frame.header.function_id != native.header.function_id || !frame.eval_env.is_null() {
            return Err(crate::VmError::InvalidOperand);
        }
        std::mem::swap(&mut frame.eval_env, &mut native.eval_env);
        Ok(result)
    }

    #[cfg(test)]
    pub(crate) const fn for_test(vm: *mut crate::Interpreter) -> Self {
        Self {
            vm,
            stack: std::ptr::null_mut(),
            context: std::ptr::null(),
            frame_index: 0,
        }
    }
}

const _: [(); 32] = [(); std::mem::size_of::<VmRuntimeActivation>()];
const _: [(); 8] = [(); std::mem::align_of::<VmRuntimeActivation>()];
const _: [(); 0] = [(); std::mem::offset_of!(VmRuntimeActivation, vm)];
const _: [(); 8] = [(); std::mem::offset_of!(VmRuntimeActivation, stack)];
const _: [(); 16] = [(); std::mem::offset_of!(VmRuntimeActivation, context)];
const _: [(); 24] = [(); std::mem::offset_of!(VmRuntimeActivation, frame_index)];

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
    /// A generated callee propagated one pure JavaScript exception value.
    Throw(crate::Value),
    /// A structural engine failure escaped generated code. The exception
    /// payload channel is never used for this outcome.
    Fatal(crate::run_control::VmError),
}

/// Type-erased compiled-code handle owned by the JIT implementation.
///
/// The JIT implementation owns executable memory and the unsafe ABI calls. The
/// VM still needs raw entry metadata for compiled-to-compiled direct branches:
/// emitted callers branch to an already-installed callee through its guarded
/// entry cell.
pub trait JitFunctionCode: std::fmt::Debug + Send + Sync {
    /// Immutable metadata for this installed code object.
    fn metadata(&self) -> crate::native_abi::CodeObjectMetadata;

    /// Native activation kind published for this tier.
    fn native_frame_kind(&self) -> NativeFrameKind {
        NativeFrameKind::Baseline
    }

    /// Exact persistent native-frame reservation for stack-owned generated
    /// calls, or `None` when this code generation cannot safely cold-deopt
    /// through that entry contract.
    ///
    /// The value excludes the caller-owned [`crate::native_abi::NativeFrame`]
    /// and tagged register window: the shared emitter accounts those
    /// separately. A tier must return `None` unless all of its bailout paths
    /// can resume from a stack-owned activation.
    fn generated_stack_frame_bytes(&self) -> Option<u32> {
        None
    }

    /// Whether stack-owned generated calls may initially publish only the
    /// initialized formal-parameter prefix of the tagged register window.
    ///
    /// A tier may opt in only when no GC, throw, safepoint, or VM transition
    /// can observe the abbreviated window. Every cold exit must materialize
    /// the remaining slots and publish the full register count before handing
    /// the frame back to shared VM machinery.
    fn generated_entry_uses_parameter_prefix(&self) -> bool {
        false
    }

    /// Immutable isolate-state dependencies declared by this code object.
    ///
    /// Implementations that record dependencies own the backing slice for the
    /// code object's lifetime and set `metadata().dependency_count` to the
    /// exact slice length. Baseline/template code uses the empty default.
    fn dependencies(&self) -> &[crate::native_abi::CodeDependency] {
        &[]
    }

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

    /// Number of safepoints owned by this installed code object.
    fn safepoint_count(&self) -> u32 {
        0
    }

    /// Resolve one code-object-owned safepoint record by dense id.
    ///
    /// The returned reference is owned by this code object; the isolate code
    /// registry keeps the object alive while any native frame can name it.
    fn safepoint_record(
        &self,
        _safepoint_id: crate::native_abi::SafepointId,
    ) -> Option<&crate::native_abi::SafepointRecord> {
        None
    }

    /// Execute the compiled function for the frame at
    /// `activation.frame_index`.
    ///
    /// Compiled code reads/writes that frame's register window in place and,
    /// for typed runtime operations, re-enters the VM through entries reached
    /// through the activation published by [`crate::native_abi::VmThread`].
    /// The window stays rooted on the VM frame
    /// stack throughout, so allocation/calls in the body are GC-safe.
    fn run_entry(&self, activation: VmRuntimeActivation) -> JitExecOutcome;

    /// Execute this object through the optimizing entry ABI.
    ///
    /// The default identifies baseline/template objects. Optimized code owns
    /// the unsafe machine entry and uses the activation to publish the same
    /// runtime-capable context and native frame as [`Self::run_entry`].
    fn run_optimized_entry(&self, _activation: VmRuntimeActivation) -> Option<JitExecOutcome> {
        None
    }

    /// Enter optimizing code at one loop header from an interpreter frame.
    ///
    /// The default identifies baseline/template objects. Optimized code uses
    /// the current interpreter register window to materialize its allocated
    /// machine state before branching to the requested header.
    fn run_optimized_osr_entry(
        &self,
        _activation: VmRuntimeActivation,
        _logical_pc: u32,
    ) -> Option<JitExecOutcome> {
        None
    }

    /// Enter compiled code mid-function at the loop header whose logical PC is
    /// `logical_pc` (on-stack replacement). Returns `None` when this code has no
    /// OSR entry for that PC (the VM keeps interpreting).
    ///
    /// The baseline keeps every live value in the frame register array at each
    /// instruction boundary, so a loop header is a valid resume point: the
    /// interpreter's live registers are exactly what the compiled code reads.
    /// The default returns `None` for codes that do not support OSR.
    fn osr_entry(
        &self,
        _activation: VmRuntimeActivation,
        _logical_pc: u32,
    ) -> Option<JitExecOutcome> {
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
    /// Installed optimizing-tier entry bodies.
    pub installed_optimized_bodies: u64,
    /// Installed non-OSR function bodies.
    pub installed_entry_bodies: u64,
    /// Installed OSR-target bodies.
    pub installed_osr_bodies: u64,
    /// Unique executable code objects reachable from all runtime caches.
    pub unique_code_objects: u64,
    /// Sum of finalized executable buffer lengths.
    pub code_bytes: u64,
}

/// Owned cold snapshot of one isolate-local JIT code generation.
///
/// A retired generation remains visible as long as its permanent entry-cell
/// tombstone exists. `dependencies` is `None` only after executable metadata
/// has retired; a registered generation with no dependencies reports
/// `Some(Vec::new())`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitCodeGenerationSnapshot {
    /// Immutable isolate-local code-object identity.
    pub code_object_id: u64,
    /// Bytecode function implemented by this generation.
    pub function_id: u32,
    /// Native tier that emitted the generation.
    pub tier: NativeFrameKind,
    /// Current executable lifecycle, including retained retired tombstones.
    pub lifecycle: CodeLifetimeState,
    /// Whether the stable entry cell still accepts new leases.
    pub linked: bool,
    /// Native activations currently holding an entry lease.
    pub active_count: u32,
    /// Formal parameter count frozen into the entry cell.
    pub param_count: u16,
    /// Initialized tagged register-window length.
    pub register_count: u16,
    /// Generated native entries observed for this exact generation.
    pub generated_entries: u64,
    /// Generated entries that returned normally.
    pub generated_returns: u64,
    /// Generated entries that cold-deoptimized.
    pub generated_deopts: u64,
    /// Generated entries that propagated a throw.
    pub generated_throws: u64,
    /// Consecutive generated deopts since the last return or throw.
    pub generated_bail_streak: u32,
    /// Exact validity dependencies while executable metadata remains
    /// registered; `None` denotes a retired tombstone.
    pub dependencies: Option<Vec<CodeDependency>>,
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
        /// Optional owned debug sidecar returned independently of installed
        /// executable code.
        artifact: Option<Box<crate::jit_artifact::JitArtifactBundle>>,
        /// Opt-in compiler-emitted events describing actual backend choices.
        diagnostics: Box<[crate::jit_debug::JitCompilerDiagnostic]>,
    },
}

/// Compile-time error from the JIT implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitCompileError {
    /// Human-readable internal diagnostic.
    pub message: String,
}

/// One typed machine entry supplied during explicit JIT installation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitRuntimeStubBinding {
    /// VM-owned dense descriptor id.
    pub id: crate::native_abi::RuntimeStubId,
    /// Descriptor signature family compiled at the call site.
    pub signature: crate::native_abi::RuntimeStubSignature,
    /// Nonzero machine entry address.
    pub entry_addr: usize,
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
    /// Whether this hook exposes an optimizing tier in addition to its baseline
    /// compiler. The VM checks this before collecting feedback snapshots or
    /// running promotion policy. Hooks overriding
    /// [`Self::compile_optimized_function`] must also return `true` here.
    fn optimizing_tier_enabled(&self) -> bool {
        false
    }

    /// JIT-owned runtime transitions installed once into the target VM.
    fn runtime_stub_bindings(&self) -> Vec<JitRuntimeStubBinding> {
        Vec::new()
    }

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

    /// Attempt the narrow optimizing tier before template compilation.
    ///
    /// Hooks that provide only a baseline compiler keep the default and leave
    /// execution on their existing tier without changing semantics.
    fn compile_optimized_function(
        &self,
        _request: JitCompileRequest,
    ) -> Result<JitCompileStatus, JitCompileError> {
        Ok(JitCompileStatus::Unavailable)
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    #[test]
    fn closure_call_layout_has_stable_c_field_offsets() {
        assert_eq!(std::mem::size_of::<JitClosureCallLayout>(), 40);
        assert_eq!(std::mem::align_of::<JitClosureCallLayout>(), 4);
        assert_eq!(
            std::mem::offset_of!(JitClosureCallLayout, function_id_byte),
            0
        );
        assert_eq!(std::mem::offset_of!(JitClosureCallLayout, flags_byte), 4);
        assert_eq!(
            std::mem::offset_of!(JitClosureCallLayout, upvalue_base_byte),
            8
        );
        assert_eq!(
            std::mem::offset_of!(JitClosureCallLayout, upvalue_count_byte),
            12
        );
        assert_eq!(
            std::mem::offset_of!(JitClosureCallLayout, eval_env_byte),
            16
        );
        assert_eq!(
            std::mem::offset_of!(JitClosureCallLayout, bound_this_byte),
            20
        );
        assert_eq!(
            std::mem::offset_of!(JitClosureCallLayout, bound_new_target_byte),
            24
        );
        assert_eq!(
            std::mem::offset_of!(JitClosureCallLayout, bound_this_flag),
            28
        );
        assert_eq!(
            std::mem::offset_of!(JitClosureCallLayout, bound_new_target_flag),
            32
        );
        assert_eq!(
            std::mem::offset_of!(JitClosureCallLayout, runtime_setup_flags),
            36
        );
    }

    #[test]
    fn runtime_activation_is_only_the_four_word_owner_link() {
        assert_eq!(std::mem::size_of::<VmRuntimeActivation>(), 32);
        assert_eq!(std::mem::align_of::<VmRuntimeActivation>(), 8);
        assert_eq!(std::mem::offset_of!(VmRuntimeActivation, frame_index), 24);
    }
}
