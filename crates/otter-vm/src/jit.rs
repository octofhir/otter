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

pub use crate::property_cache::jit::JitPropertyActionCache;
pub use crate::property_cache::{PropertyLoadAction, PropertyStoreAction};

pub use crate::property_ic::{
    EMPTY_SHAPE as PROPERTY_IC_EMPTY_SHAPE, IcHandlerKind as PropertyIcHandlerKind,
    LOOKUP_START_KEY_BIT as PROPERTY_IC_LOOKUP_START_KEY_BIT, PropertyIcLayout,
};

/// Native layout of every named-property IC slot generated code reads.
pub const PROPERTY_IC_LAYOUT: PropertyIcLayout = crate::property_ic::PropertyIcSlot::LAYOUT;

/// The `(function id, instruction pc)` a generated miss passes back to the VM
/// for the IC slot at `slot` (a [`JitPropertyAccess::ic_slot`] address).
///
/// # Safety
/// `slot` must be the address of a live slot of a CodeBlock the caller's code
/// object retains.
#[must_use]
pub unsafe fn property_ic_site(slot: u64) -> (u32, u32) {
    // SAFETY: the caller guarantees a live slot address.
    unsafe { &*(slot as *const crate::property_ic::PropertyIcSlot) }.site()
}

/// Exact named-site atom and current Graph shared-feedback eligibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitPropertyAccess {
    /// Isolate-global atom captured from this immutable source name.
    pub atom: u32,
    /// The site currently has terminal megamorphic property feedback.
    pub shared: bool,
    /// Address of the site's native IC slot
    /// ([`crate::property_ic::PropertyIcSlot`]); zero when the site owns none.
    /// The slot lives in the CodeBlock the generated code retains.
    pub ic_slot: u64,
}

mod literal_allocations;
pub use literal_allocations::{
    JitArrayLiteralAllocationPlan, JitDenseArrayAllocationPlan, JitEmptyArrayAllocationPlan,
    JitEmptyObjectAllocationPlan, JitLiteralAllocationPlans, JitObjectLiteralAllocationPlan,
    ordinary_object_header_word,
};

mod string_layout;
pub use string_layout::JitStringLayout;

mod native_callable;
pub use native_callable::JitNativeCallLayout;

/// Opaque collector-owned nursery window carried by the compiled-entry ABI.
pub type JitMachineAllocationWindow = otter_gc::MachineAllocationWindow;
/// Largest context (slots plus extension word) generated code carves inline;
/// a bigger scope takes the allocating runtime entry.
pub const JIT_INLINE_CONTEXT_MAX_WORDS: usize = 32;
/// GC header word of a young context cell with a zero size; generated code
/// ORs the cell size in at [`JIT_GC_HEADER_SIZE_BYTES_OFFSET`].
pub const JIT_YOUNG_CONTEXT_HEADER_WORD: u64 = crate::context::CONTEXT_BODY_TYPE_TAG as u64
    | ((JIT_GC_YOUNG_FLAG as u64) << (8 * otter_gc::header::HEADER_FLAGS_BYTE_OFFSET));
/// Bit of a context's `u16` scope index marking an eval-extension word.
pub const JIT_CONTEXT_HAS_EXTENSION_BIT: u32 =
    crate::context::CONTEXT_HAS_EXTENSION.trailing_zeros();

// The plan's body word packs the function id, the scope index word and the
// slot count in payload order, directly before the parent word.
const _: () = {
    assert!(crate::context::CONTEXT_BODY_SCOPE_FUNCTION_ID_OFFSET == 0);
    assert!(crate::context::CONTEXT_BODY_SCOPE_INDEX_OFFSET == 4);
    assert!(crate::context::CONTEXT_BODY_SLOT_COUNT_OFFSET == 6);
    assert!(crate::context::CONTEXT_BODY_PARENT_OFFSET == 8);
};

/// Everything generated code needs to carve one `CreateContext` from the
/// linear allocation buffer: the cell size, its complete GC header word, the
/// fixed body word before the parent, and every trailing word's initial
/// value (each slot's `hole` or `undefined`, then the eval-extension word
/// when the scope has one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitContextAllocationPlan {
    /// Cell bytes, header included.
    pub cell_bytes: u32,
    /// GC header word: context tag, young flag, cell size.
    pub header_word: u64,
    /// Scope function id, scope index word and slot count, as one word.
    pub body_word: u64,
    /// Initial value of every trailing word.
    pub initial_words: Box<[u64]>,
    /// Own DerivedThis slot, if this scope binds a derived constructor's this.
    /// Generated clients publish the resulting cell on the matching physical
    /// frame; the value of the slot is never copied into this plan.
    pub derived_this_slot: Option<u16>,
}

/// The inline allocation plan for a context of `code_block`'s scope
/// `scope`, or `None` when the scope is unknown or too large to carve inline.
#[must_use]
pub fn context_allocation_plan(
    code_block: &crate::executable::CodeBlock,
    scope: u32,
) -> Option<JitContextAllocationPlan> {
    let descriptor = code_block.scopes.get(scope as usize)?;
    let scope_index = u16::try_from(scope).ok()?;
    if scope_index & crate::context::CONTEXT_HAS_EXTENSION != 0 {
        return None;
    }
    let slot_count = u16::try_from(descriptor.slots.len()).ok()?;
    let has_extension = descriptor.flags.has_extension;
    let words = usize::from(slot_count) + usize::from(has_extension);
    if words > JIT_INLINE_CONTEXT_MAX_WORDS {
        return None;
    }
    let cell_bytes = u32::try_from(
        otter_gc::header::HEADER_SIZE
            + std::mem::size_of::<crate::context::ContextBody>()
            + words * std::mem::size_of::<crate::Value>(),
    )
    .ok()?;
    let scope_index_word = if has_extension {
        scope_index | crate::context::CONTEXT_HAS_EXTENSION
    } else {
        scope_index
    };
    let mut initial_words: Vec<u64> = descriptor
        .slots
        .iter()
        .map(|slot| {
            if slot.kind.initial_hole() {
                crate::Value::hole().to_bits()
            } else {
                crate::Value::undefined().to_bits()
            }
        })
        .collect();
    if has_extension {
        initial_words.push(crate::Value::undefined().to_bits());
    }
    Some(JitContextAllocationPlan {
        cell_bytes,
        header_word: JIT_YOUNG_CONTEXT_HEADER_WORD
            | (u64::from(cell_bytes) << (8 * otter_gc::header::HEADER_SIZE_BYTES_OFFSET)),
        body_word: u64::from(code_block.id)
            | (u64::from(scope_index_word) << 32)
            | (u64::from(slot_count) << 48),
        initial_words: initial_words.into_boxed_slice(),
        derived_this_slot: crate::context_ops::derived_this_slot(descriptor).ok()?,
    })
}

/// GC header word of a young closure cell with a zero size; generated code
/// ORs the cell size in at [`JIT_GC_HEADER_SIZE_BYTES_OFFSET`].
pub const JIT_YOUNG_CLOSURE_HEADER_WORD: u64 = crate::closure::JS_CLOSURE_BODY_TYPE_TAG as u64
    | ((JIT_GC_YOUNG_FLAG as u64) << (8 * otter_gc::header::HEADER_FLAGS_BYTE_OFFSET));
/// Bytes of a closure cell without bound words.
pub const JIT_CLOSURE_CELL_BYTES: u32 =
    (otter_gc::header::HEADER_SIZE + std::mem::size_of::<crate::closure::JsClosureBody>()) as u32;

// A carve writes the call header as one word (function id, then flags),
// then clears the rare handle and its required alignment padding with one
// store. No instance observation occupies the fixed closure cell.
const _: () = {
    assert!(crate::closure::CLOSURE_CALL_HEADER_FUNCTION_ID_OFFSET == 0);
    assert!(crate::closure::CLOSURE_CALL_HEADER_FLAGS_OFFSET == 4);
    assert!(
        crate::closure::CLOSURE_BODY_RARE_OFFSET + 8
            == std::mem::size_of::<crate::closure::JsClosureBody>()
    );
};

/// Inline nesting of one optimizing compilation: any body below this depth,
/// and a small one down to [`JIT_SMALL_INLINE_DEPTH`] (Maglev's
/// `max_maglev_inline_depth` and `max_maglev_hard_inline_depth`).
pub const JIT_INLINE_DEPTH: u8 = 3;
/// The deepest nesting a small body is inlined at.
pub const JIT_SMALL_INLINE_DEPTH: u8 = 8;
/// The longest body inlined at one call, in encoded bytecode bytes.
pub const JIT_MAX_INLINED_BYTECODE_BYTES: u32 = 460;
/// The longest small body in encoded bytecode bytes: a body of a few
/// instructions, which costs less inlined than its call does.
pub const JIT_SMALL_INLINE_BYTECODE_BYTES: u32 = 80;

/// Bytes of the BigInt cell generated code carves: header, body and one
/// digit, rounded to the cell size.
pub const JIT_BIGINT64_CELL_BYTES: u32 = ((otter_gc::header::HEADER_SIZE
    + std::mem::size_of::<crate::bigint::BigIntBody>()
    + std::mem::size_of::<u64>())
.next_multiple_of(otter_gc::page::CELL_SIZE)) as u32;
/// GC header word of a young [`JIT_BIGINT64_CELL_BYTES`] BigInt cell.
pub const JIT_YOUNG_BIGINT64_HEADER_WORD: u64 = crate::bigint::BIG_INT_BODY_TYPE_TAG as u64
    | ((JIT_GC_YOUNG_FLAG as u64) << (8 * otter_gc::header::HEADER_FLAGS_BYTE_OFFSET))
    | ((JIT_BIGINT64_CELL_BYTES as u64) << (8 * otter_gc::header::HEADER_SIZE_BYTES_OFFSET));
/// Cell byte of a BigInt's shape word: the digit count in the low half and
/// the sign byte at bit 32.
pub const JIT_BIGINT_SHAPE_BYTE: u32 =
    (otter_gc::header::HEADER_SIZE + crate::bigint::gc_body::BIG_INT_LEN_OFFSET) as u32;
/// Cell byte of a BigInt's sign byte.
pub const JIT_BIGINT_NEGATIVE_BYTE: u32 =
    (otter_gc::header::HEADER_SIZE + crate::bigint::gc_body::BIG_INT_NEGATIVE_OFFSET) as u32;
/// Cell byte of a BigInt's least significant digit.
pub const JIT_BIGINT_DIGIT_BYTE: u32 =
    (otter_gc::header::HEADER_SIZE + std::mem::size_of::<crate::bigint::BigIntBody>()) as u32;

const _: () = {
    assert!(crate::bigint::gc_body::BIG_INT_LEN_OFFSET == 0);
    assert!(crate::bigint::gc_body::BIG_INT_NEGATIVE_OFFSET == 4);
    assert!(std::mem::size_of::<crate::bigint::BigIntBody>() == 8);
    // The carve's digit is the cell's last word.
    assert!(JIT_BIGINT_DIGIT_BYTE + 8 == JIT_BIGINT64_CELL_BYTES);
};

/// Everything generated code needs to carve the closure one `MakeClosure`
/// or `MakeFunction` site creates from the linear allocation buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitClosureAllocationPlan {
    /// The call-header word: function id, then the flags word every closure
    /// of the site starts with (the named-lookup summary and, for an arrow,
    /// the bound-`this` bit).
    pub call_word: u64,
    /// The site binds the creating activation's `this`, and its `new.target`
    /// when defined (a `MakeClosure` of an arrow). A defined `new.target`
    /// adds [`Self::bound_new_target_word`] to the call word.
    pub arrow: bool,
}

impl JitClosureAllocationPlan {
    /// Call-word bit of a bound `new.target`.
    #[must_use]
    pub const fn bound_new_target_word() -> u64 {
        (crate::closure::CLOSURE_CALL_FLAG_BOUND_NEW_TARGET as u64) << 32
    }
}

/// Bytes of one per-type-tag allocation statistics row; generated
/// allocators update the row at `tag * JIT_TYPE_STATS_ROW_BYTES`.
pub const JIT_TYPE_STATS_ROW_BYTES: u32 = std::mem::size_of::<otter_gc::TypeStats>() as u32;
/// Offset of a statistics row's live-byte counter.
pub const JIT_TYPE_STATS_LIVE_BYTES_OFFSET: u32 =
    std::mem::offset_of!(otter_gc::TypeStats, live_bytes) as u32;
/// Offset of a statistics row's allocation counter.
pub const JIT_TYPE_STATS_ALLOC_COUNT_OFFSET: u32 =
    std::mem::offset_of!(otter_gc::TypeStats, alloc_count_total) as u32;
/// Offset of a statistics row's allocated-byte counter.
pub const JIT_TYPE_STATS_ALLOC_BYTES_OFFSET: u32 =
    std::mem::offset_of!(otter_gc::TypeStats, alloc_bytes_total) as u32;

/// Byte offset of the linear allocation buffer's `top` address, derived on
/// the VM side so the backend does not depend directly on the collector.
pub const JIT_LAB_TOP_OFFSET: u32 = otter_gc::LAB_TOP_OFFSET;
/// Byte offset of the linear allocation buffer's `limit` address.
pub const JIT_LAB_LIMIT_OFFSET: u32 = otter_gc::LAB_LIMIT_OFFSET;
/// Header flag installed on a generated young object.
pub const JIT_GC_YOUNG_FLAG: u8 = otter_gc::header::GENERATION_YOUNG_FLAG;
/// Byte offset of the `u32` cell size inside a GC header.
pub const JIT_GC_HEADER_SIZE_BYTES_OFFSET: u32 = otter_gc::header::HEADER_SIZE_BYTES_OFFSET as u32;

use crate::{
    CodeBlock, CodeBlockInstruction,
    feedback::ArithFeedback,
    native_abi::{
        CodeDependency, CodeLifetimeState, NativeFrameKind, NativeResultStatus, SafepointId,
        SafepointRecord,
    },
};

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
    /// Loop-header logical PC that triggered an OSR compile. `None` means
    /// normal function-entry compilation. Template compilation uses this for
    /// target diagnostics only: one returned whole-function body owns the
    /// trampolines for every eligible loop header.
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
    /// Byte offset of the 8-byte [`crate::closure::ClosureCallHeader::context`]
    /// word: a context value (full heap address) or `undefined`.
    pub context_byte: u32,
    /// Byte offset of the trailing bound `this` word, valid only while
    /// [`Self::bound_this_flag`] is set.
    pub bound_this_byte: u32,
    /// Byte offset of the trailing bound `new.target` word, valid only while
    /// [`Self::bound_new_target_flag`] is set.
    pub bound_new_target_byte: u32,
    /// Presence bit for canonical `bound_this`.
    pub bound_this_flag: u32,
    /// Presence bit for canonical `bound_new_target`.
    pub bound_new_target_flag: u32,
    /// Flags requiring the call-setup runtime stub before compiled entry.
    pub runtime_setup_flags: u32,
    /// Byte offset of the closure's 4-byte handle to its rare record
    /// ([`crate::closure_construct::ClosureRareBody`]); zero means none.
    pub rare_byte: u32,
    /// Byte offset of the own-property bag handle, from the decompressed
    /// rare-record pointer.
    pub own_props_byte: u32,
    /// Byte offset of the function's `prototype` slot value (the hole until
    /// the default object is allocated), from the rare record.
    pub prototype_byte: u32,
    /// Traced compressed family-list head, from the decompressed rare record.
    pub constructor_layouts_byte: u32,
    /// Byte flag, from the rare record: nonzero exactly while the
    /// `prototype` slot holds an ordinary object.
    pub prototype_ordinary_byte: u32,
    /// Byte flag, from the rare record, generated `instanceof` sets when it
    /// caches the closure in a site's cell.
    pub instanceof_cached_byte: u32,
}

/// Byte offset of the cached target in a generated `instanceof` site's cell.
pub const JIT_INSTANCEOF_CELL_TARGET_OFFSET: u32 = 0;
/// Byte offset of the cached prototype in a generated `instanceof` site's cell.
pub const JIT_INSTANCEOF_CELL_PROTOTYPE_OFFSET: u32 = 8;

/// Machine-readable class-constructor wrapper layout.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JitClassConstructorLayout {
    /// GC body tag proving the wrapper family.
    pub type_tag: u8,
    /// Byte offset from the wrapper body to its traced exact-family head.
    pub constructor_layouts_byte: u32,
}

/// One VM-owned layout of the traced constructor family cell. Every offset
/// includes the GC header and is consumed only after a live owner-head load.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JitConstructorLayout {
    /// Header-inclusive byte offset of the exact family's monotonic identity.
    pub family_id_byte: u32,
    /// Header-inclusive byte offset of the traced receiver root shape.
    pub root_byte: u32,
}

/// Address-stable validity word for a complete ordinary prototype chain.
/// The VM retains its owner until the compiled generation retires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JitPrototypeValidity {
    /// Process-local address of an atomic `u32`: one is valid, zero invalid.
    pub address: usize,
    /// Instance-root shape identity, used only for address-free artifacts.
    pub identity: u64,
}

/// The cells generated code tests to open and close fast Array iterator
/// records (see `crate::iterator_record`) in place of the realm's proofs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitArrayIteration {
    /// Valid while `%Array.prototype%[@@iterator]` is the built-in `values`.
    pub iterable: JitPrototypeValidity,
    /// Valid while `%ArrayIteratorPrototype%`'s chain answers as proven.
    pub iterator: JitPrototypeValidity,
    /// Whether that chain proves `return` absent or built-in, so a close of a
    /// live record runs nothing.
    pub close: bool,
    /// Realm whose proofs these are; generated code runs them only while it
    /// is the active realm.
    pub realm: u32,
    /// Array cell `GcHeader::type_tag`.
    pub array_type_tag: u8,
    /// Byte offset of the array's exotic sidecar handle: zero for an ordinary
    /// array with no own symbol property and no prototype override.
    pub array_exotic_byte: u32,
}

/// One GC-movement-stable receiver allocation program.
///
/// The program reads the live class or guarded closure prototype, proves its
/// required ordinary shape chain, and initializes an in-body shaped object. Any mismatch
/// is pre-effect and returns to rooted receiver preparation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitReceiverAllocationPlan {
    /// Expected underlying function of the live `new.target` class or closure.
    pub new_target_function_id: u32,
    /// Actual new.target family belongs to a class wrapper, not a closure.
    pub new_target_is_class: bool,
    /// Nonzero monotonic identity of the exact actual constructor family.
    /// This scalar never retains a callable or a receiver by itself.
    pub family_id: u64,
    /// Initial receiver hidden class naming its initial fields; generated
    /// code proves the prototype it fixes is the live prototype. `0` for a
    /// receiver with no initial fields: it uses the exact `prototype_root`.
    pub receiver_shape: u32,
    /// Number of already-visible undefined slots described by that shape.
    pub initial_field_count: u8,
    /// In-object slots of the allocated receiver: room for its initial
    /// fields and every field its constructor is known to add.
    pub inline_capacity: u8,
    /// Optional chain validity used by preinitialized own-field proofs.
    pub prototype_validity: Option<JitPrototypeValidity>,
    /// Nonzero exact capacity root; its live prototype is checked before allocation.
    pub prototype_root: u32,
}

impl JitReceiverAllocationPlan {
    /// Ordinary-object GC header: type tag, young flag and cell size only.
    /// Immutable shape state owns extensibility/lookup facts, and the shape
    /// fixes inline capacity; neither is duplicated in the cell header.
    #[must_use]
    pub fn cell_header_word(&self, cell_bytes: u32) -> u64 {
        literal_allocations::ordinary_object_header_word(cell_bytes)
    }
}

/// Where generated code reads `typeof` from a cell's GC type tag. A tag
/// listed as `slow_*` needs the heap-aware leaf (a callable plain object, a
/// native function that may be `[[IsHTMLDDA]]`, a proxy, an internal body);
/// every other cell tag not listed is `"object"`.
#[derive(Debug, Clone, Copy)]
pub struct JitTypeOfTags {
    /// `"string"` cells.
    pub string: u8,
    /// `"symbol"` cells.
    pub symbol: u8,
    /// `"bigint"` cells.
    pub bigint: u8,
    /// Always-callable cells: closures, bound functions, class constructors.
    pub functions: [u8; 3],
    /// Plain objects: `"object"` unless their sidecar carries a native call.
    pub plain_object: u8,
    /// Cells the leaf decides.
    pub slow: [u8; 4],
}

/// The GC type tags behind `typeof`, for inline type tests.
pub const JIT_TYPEOF_TAGS: JitTypeOfTags = JitTypeOfTags {
    string: crate::string::JS_STRING_BODY_TYPE_TAG,
    symbol: crate::symbol::SYMBOL_BODY_TYPE_TAG,
    bigint: crate::bigint::BIG_INT_BODY_TYPE_TAG,
    functions: [
        crate::closure::JS_CLOSURE_BODY_TYPE_TAG,
        crate::bound_function::BOUND_FUNCTION_BODY_TYPE_TAG,
        crate::class_constructor::CLASS_CONSTRUCTOR_BODY_TYPE_TAG,
    ],
    plain_object: crate::object::OBJECT_BODY_TYPE_TAG,
    slow: [
        crate::native_function::NATIVE_FUNCTION_BODY_TYPE_TAG,
        crate::proxy::PROXY_BODY_TYPE_TAG,
        crate::context::CONTEXT_BODY_TYPE_TAG,
        crate::eval_env::EVAL_EXTENSION_BODY_TYPE_TAG,
    ],
};

const _: [(); 56] = [(); std::mem::size_of::<JitClosureCallLayout>()];
const _: [(); 4] = [(); std::mem::align_of::<JitClosureCallLayout>()];
const _: [(); 0] = [(); std::mem::offset_of!(JitClosureCallLayout, function_id_byte)];
const _: [(); 4] = [(); std::mem::offset_of!(JitClosureCallLayout, flags_byte)];
const _: [(); 8] = [(); std::mem::offset_of!(JitClosureCallLayout, context_byte)];
const _: [(); 12] = [(); std::mem::offset_of!(JitClosureCallLayout, bound_this_byte)];
const _: [(); 16] = [(); std::mem::offset_of!(JitClosureCallLayout, bound_new_target_byte)];
const _: [(); 20] = [(); std::mem::offset_of!(JitClosureCallLayout, bound_this_flag)];
const _: [(); 24] = [(); std::mem::offset_of!(JitClosureCallLayout, bound_new_target_flag)];
const _: [(); 28] = [(); std::mem::offset_of!(JitClosureCallLayout, runtime_setup_flags)];
const _: [(); 32] = [(); std::mem::offset_of!(JitClosureCallLayout, rare_byte)];
const _: [(); 36] = [(); std::mem::offset_of!(JitClosureCallLayout, own_props_byte)];
const _: [(); 40] = [(); std::mem::offset_of!(JitClosureCallLayout, prototype_byte)];
const _: [(); 44] = [(); std::mem::offset_of!(JitClosureCallLayout, constructor_layouts_byte)];
const _: [(); 48] = [(); std::mem::offset_of!(JitClosureCallLayout, prototype_ordinary_byte)];
const _: [(); 52] = [(); std::mem::offset_of!(JitClosureCallLayout, instanceof_cached_byte)];

/// Machine-readable [`crate::context::ContextBody`] layout.
///
/// Every byte offset is measured from the cell's [`otter_gc::GcHeader`] — a
/// heap `Value` is the header's full address, so `parent` and a closure's
/// context word are each one 8-byte load away from the next hop.
/// Slot `i` of a context is at `slots_byte + 8 * i`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JitContextLayout {
    /// GC body tag of a context cell.
    pub type_tag: u8,
    /// Byte offset of the `u32` descriptor-owning function id.
    pub scope_function_id_byte: u32,
    /// Byte offset of the `u16` scope index.
    pub scope_index_byte: u32,
    /// Byte offset of the `u16` slot count.
    pub slot_count_byte: u32,
    /// Byte offset of the 8-byte parent word (context value or `undefined`).
    pub parent_byte: u32,
    /// Byte offset of slot 0.
    pub slots_byte: u32,
}

impl JitContextLayout {
    /// The layout of the current [`crate::context::ContextBody`].
    #[must_use]
    pub const fn current() -> Self {
        let header = otter_gc::header::HEADER_SIZE as u32;
        Self {
            type_tag: crate::context::CONTEXT_BODY_TYPE_TAG,
            scope_function_id_byte: header
                + crate::context::CONTEXT_BODY_SCOPE_FUNCTION_ID_OFFSET as u32,
            scope_index_byte: header + crate::context::CONTEXT_BODY_SCOPE_INDEX_OFFSET as u32,
            slot_count_byte: header + crate::context::CONTEXT_BODY_SLOT_COUNT_OFFSET as u32,
            parent_byte: header + crate::context::CONTEXT_BODY_PARENT_OFFSET as u32,
            slots_byte: header + crate::context::CONTEXT_BODY_SLOTS_OFFSET as u32,
        }
    }
}

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
    /// Complete CodeBlock-owned CacheIR programs, copied at the compilation
    /// boundary and keyed by source byte PC. Every installed program must be
    /// representable or the site is absent and uses its committed cold edge.
    /// A `CallMethodValue` site's programs describe its method lookup.
    pub property_programs: rustc_hash::FxHashMap<u32, Vec<JitCacheIrProgram>>,
    /// Stable layout of this isolate's one shared key/action table.
    /// Generated loads and stores consume independent facts under one key.
    pub property_action_cache: Option<JitPropertyActionCache>,
    /// Every immutable named access, including method lookup, keyed by byte PC.
    /// The atom is available to baseline shared probes; `shared` preserves the
    /// optimizing tier's existing terminal-feedback admission.
    pub property_accesses: rustc_hash::FxHashMap<u32, JitPropertyAccess>,
    /// Direct physical hit proofs for schema-owned binding sites, keyed by
    /// byte-PC. See [`BindingHitProof`].
    pub binding_hit_proofs: rustc_hash::FxHashMap<u32, BindingHitProof>,
    /// Inline `CreateContext` plans keyed by `(function id, scope index)` —
    /// for this function and every body inlined into it.
    pub context_allocations: rustc_hash::FxHashMap<(u32, u32), JitContextAllocationPlan>,
    /// Inline `MakeClosure` / `MakeFunction` plans of this function, keyed by
    /// byte-PC. A site without a plan (a generator or async function) takes
    /// the allocating runtime entry.
    pub closure_allocations: rustc_hash::FxHashMap<u32, JitClosureAllocationPlan>,
    /// Empty literal geometry and intrinsic shape prepared for this exact source body.
    pub literal_allocations: JitLiteralAllocationPlans,
    /// Typed reasons observed at each logical PC in earlier optimized
    /// generations. The reason identity remains available to later policy and
    /// lowering instead of collapsing distinct failures into a PC-only bit.
    /// The baked per-instruction feedback of every listed PC is already widened
    /// past the exited speculation
    /// ([`JitInstructionMetadata::note_optimized_exit`]).
    pub optimized_exit_reasons:
        std::collections::BTreeMap<u32, std::collections::BTreeSet<crate::native_abi::ExitReason>>,
    /// Logical PCs at which an earlier optimized generation left to collect
    /// feedback. A site listed here has executed since; if its feedback is
    /// still empty, nothing the site meets is something a speculation could
    /// describe, and leaving again would only repeat the exit.
    pub feedback_exits: std::collections::BTreeSet<u32>,
    /// Widest value each parameter was seen to hold when an optimized entry's
    /// parameter guards failed, indexed by parameter; empty when none did.
    /// Entry representations are capped by it, as JSC's argument value
    /// profiles widen from the exit's own values.
    pub parameter_widening: Box<[JitParameterWidening]>,
    /// How the indexed-element program addresses each site's receiver, keyed by
    /// the site's byte-PC. Generated code reads only this; the family's body
    /// layout never reaches the emitter, so a second element-bearing family
    /// costs a declaration.
    pub element_accesses: rustc_hash::FxHashMap<u32, JitElementAccess>,
    /// The active realm's Array iteration proof, when the function opens or
    /// closes a synchronous iterator record and the proof holds at compile
    /// time (see [`JitArrayIteration`]).
    pub array_iteration: Option<JitArrayIteration>,
    /// Byte-PCs of indexed sites that have never executed. An optimizing
    /// compile deoptimizes when it reaches one ("insufficient feedback"), so
    /// the next generation specializes it instead of calling cold forever.
    pub unseen_element_sites: rustc_hash::FxHashSet<u32>,
    /// Static heap-layout offsets for inline primitive string `.length`.
    pub string_layout: JitStringLayout,
    /// Byte offset from a decompressed global-lexical cell pointer to its
    /// `Value`, for the inline global declarative-record read.
    pub global_lexical_value_byte: u32,
    /// Byte offsets of [`crate::context::ContextBody`] for inline
    /// context-slot access, parent hops, and the extension probe.
    pub context_layout: JitContextLayout,
    /// Byte offset from a decompressed object pointer to its shape handle
    /// (`HEADER_SIZE + OBJECT_BODY_SHAPE_OFFSET`). A `#[repr(C)]` constant; the
    /// emitter reads `[obj_ptr + object_shape_byte]` for CacheIR shape guards
    /// `LoadProperty` cell guard, staying layout-agnostic.
    pub object_shape_byte: u32,
    /// Byte offset from a decompressed sidecar (`ExoticSlots`) cell pointer
    /// to a dictionary-mode object's `u32` slot-layout epoch. Read only after
    /// the object's shape is a dictionary shape and its sidecar handle
    /// non-null; a
    /// match keeps every existing key at its captured slot.
    pub exotic_dictionary_layout_byte: u32,
    /// Byte offset from a decompressed sidecar cell pointer to the compressed
    /// root shape a prototype caches for its instances; null until the
    /// runtime first creates an instance of it.
    pub exotic_instance_root_byte: u32,
    /// One VM-owned description of dynamic inline-or-slab field storage.
    pub field_layout: crate::object::FieldLayout,
    /// Header-inclusive offset of a shaped object's u32 live property count.
    pub shape_property_count_byte: u32,
    /// Byte offset of the 4-byte rare-state GC handle. Conservative generated
    /// property programs require a zero handle; programs that prove ordinary
    /// named lookup separately can admit benign sidecars such as symbol keys.
    pub object_exotic_handle_byte: u32,
    /// Static GC layout for the inline generational write barrier emitted on a
    /// pointer-valued `StoreProperty`. Isolate-independent `#[repr(C)]` / `const`
    /// values; the card-mark is gated on [`cage_base`](Self::cage_base) being
    /// baked (the emitter decompresses parent/child pointers against it).
    pub gc_barrier: JitGcBarrierLayout,
    /// Byte offset from a decompressed shape cell pointer to the compressed
    /// `[[Prototype]]` the shape fixes (`HEADER_SIZE +
    /// SHAPE_BODY_PROTOTYPE_OFFSET`): an ordinary object, or null for a `null`
    /// or non-ordinary prototype. Prototype-chain guards read the receiver's
    /// shape, then this word, without runtime resolution.
    pub shape_prototype_byte: u32,
    /// Header-inclusive offset of the sole immutable [`crate::object::ShapeState`]
    /// byte on a decompressed shape. Exact eligible baked shape identity fixes
    /// these facts; only dynamic/shared and dictionary probes read them live.
    pub shape_state_byte: u32,
    /// Immutable inline-prefix capacity on the shape cell.
    pub shape_inline_capacity_byte: u32,
    /// Complete VM-owned closure-call ABI contract. Method identity guards use
    /// its function-id offset; native call linkage additionally consumes its
    /// flags, context word, and canonical bound-value metadata.
    pub closure_call_layout: JitClosureCallLayout,
    /// VM-baked class wrapper layout used by generated construct guards.
    pub class_constructor_layout: JitClassConstructorLayout,
    /// Exact actual-constructor family guard offsets.
    pub constructor_layout: JitConstructorLayout,
    /// GC body tags whose cell values remain ECMAScript primitives.
    pub primitive_cell_type_tags: [u8; 3],
    /// Ready-to-use byte offsets and type tags for baseline collection method
    /// IC guards.
    pub collection_layout: JitCollectionLayout,
    /// Complete native callable header and traced capture-slab offsets.
    pub native_call_layout: JitNativeCallLayout,
    /// Instruction overlays in canonical logical-PC order.
    pub instructions: Vec<JitInstructionMetadata>,
    /// Direct reads of permanent global-lexical cells keyed by the
    /// `Op::LoadGlobalOrThrow` byte-PC. The global declarative record roots
    /// each non-moving cell; generated code reads its current value and takes
    /// the semantic stub only for a TDZ hole.
    pub global_lexical_loads: rustc_hash::FxHashMap<u32, JitGlobalLexicalLoad>,
    /// Prepared address-stable, GC-traced literal cells keyed by the byte PC
    /// of a `LoadString` or `LoadBigInt`. Every site is a leaf load and remains
    /// valid for the lifetime of every code object carrying its relocation.
    pub literal_cells: rustc_hash::FxHashMap<u32, JitLiteralCell>,
    /// Guarded own-data reads from the global object record keyed by the
    /// `Op::LoadGlobalOrThrow` byte-PC. Generated code validates both the live
    /// global-declarative epoch and the global object's hidden class before
    /// reading the current `Value` slot word.
    pub global_object_loads: rustc_hash::FxHashMap<u32, JitGlobalObjectLoad>,
    /// Native source call plans keyed by byte PC. Exact audited leaves retain
    /// their bootstrap identity; general native plans prove only the live kind.
    /// Every guard miss enters the committed canonical call before any effect.
    pub native_calls: rustc_hash::FxHashMap<u32, JitNativeCall>,
    /// Compiler-native ordinary-call candidates keyed by byte PC. Fixed and
    /// spread calls have one target; forwarded arguments admit the bounded
    /// feedback population. Each synchronous target has a stable non-OSR entry
    /// cell and receives its context through its exact SELF closure. Generated code guards
    /// callable identity and binds the current generation. A miss precedes call
    /// effects; forwarding retains its committed apply value for canonical
    /// completion. Body-inline candidates remain separate and monomorphic.
    pub direct_callees: rustc_hash::FxHashMap<u32, Vec<JitDirectCallee>>,
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
    /// `f.call(...)` sites keyed by byte PC: an `Op::CallMethodValue` named
    /// `call`, or an `Op::CallWithThis` whose loaded callee was
    /// `%Function.prototype.call%`. Generated code proves the intrinsic and
    /// calls the function it ran directly, with the first argument as `this`.
    pub function_prototype_calls: rustc_hash::FxHashMap<u32, JitFunctionPrototypeCallSite>,
    /// `new` sites whose callee was the realm's original `%Array%`, keyed by
    /// byte PC: the address of an identity cell holding that constructor.
    pub array_constructor_sites: rustc_hash::FxHashMap<u32, u64>,
    /// Address of each `instanceof` site's cell, keyed by byte PC: the last
    /// target the site proved and the prototype it searches for.
    pub instanceof_cells: rustc_hash::FxHashMap<u32, u64>,
    /// External-reference index of `%Function.prototype.apply%` for a body
    /// that forwards its arguments (`Op::CallForwardArguments`): generated
    /// code proves a forwarding site's method is the intrinsic before it
    /// passes the activation's actual arguments on.
    pub forward_apply_native_ref: Option<u32>,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitGuardedReceiver {
    /// An ordinary object named by its hidden class. The method is in the
    /// receiver's own slab, or in a prototype resolved at run time when
    /// [`JitGuardedMethodCall::holder`] names one, because `setPrototypeOf`
    /// moves the holder while the shape stays put. The entry reads only the
    /// call's arguments.
    Shape {
        /// Guarded receiver shape handle offset.
        shape: u32,
    },
    /// An exotic body named by its cell type tag, whose method always lives on
    /// a pinned realm prototype. The entry reads the receiver as its first
    /// argument, since the operation is *on* that body.
    Exotic(JitIntrinsicPrototype),
}

/// Shared receiver proof for lookup through a pinned realm intrinsic prototype.
///
/// Primitive strings carry no own named fields except length and indices.
/// Collection receivers additionally require the existing clean-instance latch.
/// The prototype is pinned; its shape, descriptor state and fields stay live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitIntrinsicPrototype {
    /// Exact receiver `GcHeader::type_tag`.
    pub type_tag: u8,
    /// Instance state required before the canonical prototype may be used.
    pub guard: Option<JitBodyGuard>,
    /// Compressed offset of the pinned realm prototype.
    pub proto_offset: u32,
    /// Realm whose intrinsic `proto_offset` names, when the receiver's
    /// prototype is resolved in the *active* realm rather than recorded on
    /// the body: generated code then also compares the isolate's active
    /// realm id. `None` when the body proof alone pins the prototype.
    pub active_realm: Option<u32>,
}

impl JitIntrinsicPrototype {
    /// Whether generated code may prove this receiver and continue at the
    /// pinned prototype.
    ///
    /// A latched `Map`/`Set` body has no instance override. A primitive string
    /// body owns only `length` and its indices, so every other name resolves on
    /// `%String.prototype%`. An array body without an exotic sidecar likewise
    /// owns only `length` and its elements, and its `[[Prototype]]` is still the
    /// realm `%Array.prototype%`. A closure whose named-lookup byte reads
    /// exactly ordinary has no own bag and no prototype override, and its
    /// kind resolves every non-virtual name on the active realm's
    /// `%Function.prototype%`, so its proof also pins that realm. The proof establishes only the receiver; the
    /// following holder guards decide which names that prototype answers. A
    /// String wrapper prototype stays opaque to ordinary lookup guards, so no
    /// `length`/index load can be answered from its shape.
    #[must_use]
    pub fn is_generated_receiver(self) -> bool {
        let receiver = match self.type_tag {
            crate::collections::MAP_BODY_TYPE_TAG | crate::collections::SET_BODY_TYPE_TAG => {
                self.guard == Some(crate::method_ops::collection_guard())
            }
            crate::string::JS_STRING_BODY_TYPE_TAG => self.guard.is_none(),
            crate::array::ARRAY_BODY_TYPE_TAG => {
                self.guard == Some(crate::method_ops::dense_array_guard())
            }
            crate::closure::JS_CLOSURE_BODY_TYPE_TAG => {
                self.guard == Some(crate::closure::ordinary_named_lookup_guard())
                    && self.active_realm.is_some()
            }
            _ => false,
        };
        receiver && self.proto_offset != 0 && self.proto_offset.is_multiple_of(8)
    }
}

/// Layout proof for the object owning a guarded method slot.
///
/// Either form pins which key the slot at
/// [`JitGuardedMethodCall::method_field`] belongs to; the builtin identity
/// guard then proves the slot's live value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitMethodHolder {
    /// The shaped receiver owns the slot.
    Receiver,
    /// An inherited ordinary method proved by one chain cell.
    Prototype {
        /// Retained chain proof.
        validity: JitPrototypeValidity,
        /// Root shape whose traced prototype is the holder.
        root: u32,
    },
    /// A fast-mode intrinsic holder with this nonzero hidden-class handle offset.
    Shape(u32),
    /// A dictionary-mode holder with this slot-layout epoch. Every delete,
    /// descriptor change or re-entry into dictionary mode advances it, so an
    /// unchanged epoch keeps each existing key at its captured slot; appending
    /// an unrelated key does not disturb it. This is the only proof available
    /// for a String wrapper such as `%String.prototype%`, which never adopts a
    /// hidden class.
    Dictionary(u64),
}

/// One `Op::CallMethodValue` site whose callee is a declared native entry.
///
/// The layout fields are the same lowered cache program a property site
/// caches — guarded receiver shape, an optional guarded prototype holder, and
/// the logical field — so generated code reuses the way walk and the prototype hop
/// rather than describing this access a second time. The entry id then selects
/// the call, exactly as it does at an ordinary call site: the family the id
/// resolves in is what decides the call protocol, so a read, an in-place
/// mutation and an allocating write are one description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitGuardedMethodCall {
    /// How the receiver is proven before the method slot is read.
    pub receiver: JitGuardedReceiver,
    /// Live layout proof for the object that owns the method slot: the hopped
    /// prototype of a [`JitGuardedReceiver::Shape`] receiver, or the pinned
    /// prototype of an exotic one.
    pub holder: JitMethodHolder,
    /// Shape-owned method bank and relative index in the holder.
    pub method_field: crate::object::FieldLocation,
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

/// Widest representation a parameter held at a failed optimized entry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum JitParameterWidening {
    /// No exit showed anything wider than Int32.
    #[default]
    Int32,
    /// An exit showed a non-Int32 Number.
    Number,
    /// An exit showed a non-Number.
    Tagged,
}

impl JitParameterWidening {
    /// The representation `value` needs.
    #[must_use]
    pub fn of(value: crate::Value) -> Self {
        if value.is_int32() {
            Self::Int32
        } else if value.is_number() {
            Self::Number
        } else {
            Self::Tagged
        }
    }
}

/// Lookup half of a method-call `f.call` proof: the closure property
/// program's receiver proof (an exact ordinary closure of the active realm,
/// whose named lookups resolve on the pinned `%Function.prototype%`) and the
/// holder shape that pins the `call` slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitFunctionCallLookup {
    /// Closure receiver proof naming the pinned `%Function.prototype%`.
    pub receiver: JitIntrinsicPrototype,
    /// Hidden class of `%Function.prototype%` when the site was compiled.
    pub holder_shape: u32,
    /// Shape-owned storage bank and index of the holder's `call` slot.
    pub call_field: crate::object::FieldLocation,
}

/// Proof that a site's callee is `%Function.prototype.call%`: the method a
/// lookup reads (for a method call) or the loaded callee (for an explicit
/// receiver call) must carry the intrinsic's external-reference identity, so
/// a replaced or redefined `call` fails it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitFunctionPrototypeCall {
    /// How a method call reads `call` from its receiver; `None` when the site
    /// already holds the callee.
    pub lookup: Option<JitFunctionCallLookup>,
    /// External-reference index of `%Function.prototype.call%`.
    pub call_native_ref: u32,
}

/// One `f.call(...)` site: its proof and the function `f` the call ran.
#[derive(Debug, Clone, Copy)]
pub struct JitFunctionPrototypeCallSite {
    /// Proof that the callee is the intrinsic.
    pub proof: JitFunctionPrototypeCall,
    /// Current entry generation of the function the intrinsic runs.
    pub callee: JitDirectCallee,
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
    /// Shared ordinary prototype-chain proof, absent for an own method.
    pub prototype_validity: Option<JitPrototypeValidity>,
    /// Root shape whose traced prototype is the method holder; zero for own.
    pub holder_root: u32,
    /// Shape-owned method bank and relative index in the holder.
    pub method_field: crate::object::FieldLocation,
}

/// A method the baseline may splice into a caller's `Op::CallMethodValue` site.
/// Carries the method's body plus the shared identity guard. Per body
/// `LoadProperty`/`StoreProperty` byte-PC, the logical own field within the
/// receiver's shape-owned storage bank.
/// Method identity is verified before body entry by the receiver shape and
/// one prototype validity cell. The holder is read from a traced root shape;
/// the current slot must still identify [`JitMethodGuard::method_fid`].
/// Prototype mutation invalidates the proof before any dependent call has
/// effects. An optimizing backend may reuse that proof across iterations only
/// after proving the receiver loop-invariant and the whole natural loop free
/// of mutation and reentrant execution.
#[derive(Debug, Clone)]
pub struct JitInlineMethod {
    /// Fully baked compile inputs for the method body, resolved against the
    /// method's own constant pool and feedback rather than the caller's.
    /// Nested plain and method candidates live in this snapshot's own tables.
    pub body: Arc<JitCompileSnapshot>,
    /// Exact receiver/prototype/method-slot guard shared with generated calls.
    pub guard: JitMethodGuard,
    /// Body `LoadProperty`/`StoreProperty` byte-PC → logical own field. A
    /// receiver-shape property is baked from the identity-guarded receiver shape;
    /// a non-receiver property is baked from its own monomorphic site feedback,
    /// with the required shape recorded in [`Self::prop_shapes`].
    pub prop_fields: rustc_hash::FxHashMap<u32, crate::object::FieldLocation>,
    /// Body byte-PC → the compressed shape-handle offset a **non-receiver**
    /// property access must match, for the guard the inliner emits before the
    /// slot load/store. A receiver property is absent here — the entry
    /// `CheckMethodIdentity` already proves its shape.
    pub prop_shapes: rustc_hash::FxHashMap<u32, u32>,
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
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum JitDirectCallThisMode {
    /// Strict functions, and sloppy functions that never observe `this`,
    /// receive `undefined`; arrows retain their lexical binding from the
    /// guarded closure metadata.
    StrictOrLexical,
    /// A sloppy callee binds `this` by OrdinaryCallBindThis in generated code:
    /// a plain `Op::Call` and a nullish explicit receiver bind the active
    /// realm's rooted global object, an Object receiver binds itself, and a
    /// primitive receiver side-exits before entry because `ToObject` remains
    /// an interpreter operation, as does a closure carrying a bound receiver.
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

/// One target-neutral operation in an immutable CacheIR compilation program.
///
/// Object operands are tiny CacheIR register ids: operand zero is the receiver
/// and operand one is its direct or pinned intrinsic prototype. Ordinary
/// programs are the optimizing tier's compile-time reading of a site's IC
/// handlers ([`crate::property_ic`]); intrinsic programs compose the same
/// slot proof with a realm-owned receiver declaration. Shape tokens are stable
/// compressed offsets validated by the VM while the snapshot is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitCacheIrOp {
    /// Read a moving holder through its pinned, traced instance-root shape.
    LoadPrototypeHolder {
        /// Root shape fixed by the preceding chain proof.
        root: u32,
        /// CacheIR operand receiving the holder.
        result: u8,
    },
    /// Prove every prototype dependency with one immutable cell identity.
    GuardPrototypeValidity {
        /// Retained chain proof, checked before any property effect.
        validity: JitPrototypeValidity,
    },
    /// Continue only while the object has the expected fast hidden class.
    GuardShape {
        /// CacheIR object operand to inspect.
        object: u8,
        /// Stable compressed hidden-class token.
        shape: u32,
    },
    /// Continue only while a dictionary-mode object keeps one key/slot
    /// layout: a null shape and an unchanged dictionary slot-layout epoch, which
    /// every add, delete or descriptor change replaces. Emitted only for a
    /// pinned intrinsic prototype whose captured key it cannot shadow.
    GuardDictionaryLayout {
        /// CacheIR object operand to inspect.
        object: u8,
        /// Captured dictionary slot-layout epoch; never zero or saturated.
        layout: u64,
    },
    /// Prove that the immutable shape mapping still authorizes the atom's data
    /// slot and that no object-local descriptor or exotic state overrides it.
    GuardAtomSlot {
        /// CacheIR object operand whose slot metadata is guarded.
        object: u8,
        /// Isolate-global atom identity captured by the CacheIR program.
        atom: u32,
        /// Shape-owned storage bank and relative field index for this atom.
        field: crate::object::FieldLocation,
        /// Whether the terminal operation requires a writable data slot.
        writable: bool,
    },
    /// Prove an exotic receiver and read its pinned canonical prototype.
    /// Subsequent ordinary guards validate the prototype's live data slot.
    LoadIntrinsicPrototype {
        /// CacheIR receiver operand to guard.
        object: u8,
        /// CacheIR operand receiving the prototype.
        result: u8,
        /// Existing receiver/type/latch proof shared with native methods.
        target: JitIntrinsicPrototype,
    },
    /// Prove that an object's direct prototype is null.
    GuardPrototypeNull {
        /// CacheIR object operand whose prototype link is guarded.
        object: u8,
    },
    /// Read one already-guarded own data field.
    LoadField {
        /// Already-guarded object operand owning the slot.
        object: u8,
        /// Shape-owned storage bank and relative field index.
        field: crate::object::FieldLocation,
    },
    /// Write one already-guarded existing own data field.
    StoreField {
        /// Already-guarded receiver operand owning the slot.
        object: u8,
        /// Shape-owned storage bank and relative field index.
        field: crate::object::FieldLocation,
    },
    /// Prove that an ordinary receiver can append the named slot without
    /// allocating or changing storage representation.
    GuardExtensible {
        /// CacheIR object operand receiving the new own slot.
        object: u8,
        /// Logical field that must be the exact next append.
        field: crate::object::FieldLocation,
    },
    /// Publish the child hidden class and new logical slot length after every
    /// miss-capable guard has completed.
    PublishShape {
        /// CacheIR object operand whose structure changes.
        object: u8,
        /// Stable compressed child hidden-class token.
        shape: u32,
    },
}

/// Complete immutable CacheIR program consumed by a native tier.
///
/// A site is publishable only when every installed handler can be represented.
/// Unsupported CacheIR operations therefore keep the entire site on its
/// committed canonical cold edge; a tier never executes a partial program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JitCacheIrProgram {
    /// Complete operation sequence proving one effect-once property hit.
    pub ops: Box<[JitCacheIrOp]>,
}

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

/// Selected native entry for one source call site.
///
/// A leaf is an exact bootstrap identity and its declared NoAlloc operation.
/// `Native` proves only the live cell kind: its current callback, policy,
/// captures and realm are selected by the canonical Host native kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitNativeCall {
    /// An exact audited leaf declaration.
    Leaf(JitStaticNativeCall),
    /// Any NativeFunction, entered through the private Native kind convention.
    Native,
}

impl JitNativeCall {
    /// Exact leaf payload, when this plan permits the declared leaf ABI.
    #[must_use]
    pub const fn leaf(&self) -> Option<&JitStaticNativeCall> {
        match self {
            Self::Leaf(leaf) => Some(leaf),
            Self::Native => None,
        }
    }
}

/// VM-resolved direct-call target for one eligible compiled callee.
///
/// The compiler consumes identity and call semantics, then emits a branch
/// through the permanent `entry_cell`. Every caller passes only its actual
/// span; register-window geometry and missing-formal initialization belong to
/// the callee. Generated code never decodes this Rust DTO or its enum layout.
/// It contains no moving roots and is private VM/JIT plumbing, not an
/// embedding or external ABI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitDirectCallPlan {
    /// Callee function id in the executable module.
    pub function_id: u32,
    /// Generation current when this plan was captured. Generated
    /// code uses it only for diagnostics; dispatch follows [`Self::entry_cell`].
    pub code_object_id: u64,
    /// Stable address of the function's
    /// [`crate::native_abi::FunctionEntryCell`]. The cell is registry-owned,
    /// never reused, and atomically selects the current generation.
    pub entry_cell: u64,
    /// Native tier current when this plan was captured.
    pub tier: NativeFrameKind,
    /// Exact plain-call `this` binding the generated linkage must perform.
    pub this_mode: JitDirectCallThisMode,
    /// Whether the target enters with an uninitialized `this` binding and
    /// applies derived-constructor return semantics.
    pub is_derived_constructor: bool,
    /// The target's `FUNCTION_CALL_*` call semantics.
    pub call_flags: u32,
    /// Address of the site's callee identity cell: the last callee value
    /// proved to be this target. Zero when the site has none.
    pub callee_cell: u64,
}

/// One baked compiler-native target in an ordinary-call candidate population.
///
/// Presence in [`JitCompileSnapshot::direct_callees`] proves the callee is an
/// ordinary synchronous body and has a stable entry cell with one current
/// entry-capable generation. Dynamic callable
/// identity, supported `this` binding, and target liveness remain machine-code
/// guards; tier promotion patches the cell without invalidating this caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitDirectCallee {
    /// Function identity, stable entry cell, and call semantics.
    pub plan: JitDirectCallPlan,
    /// Optional guarded safepoint-free receiver allocation program.
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
        /// Compressed GC-cage offset of the non-moving global-lexical cell.
        cell_offset: u32,
        /// Whether assignment may update the live cell.
        writable: bool,
    },
    /// Guarded own-data slot in the global object record.
    GlobalObject {
        /// Expected ordinary shape handle, or the dictionary slot-layout
        /// epoch that keeps this key at `field` while unrelated globals
        /// are added.
        shape: u64,
        /// Whether `shape` names a dictionary slot-layout epoch.
        dictionary: bool,
        /// Logical property field in the object's selected storage.
        field: crate::object::FieldLocation,
        /// Global declarative epoch that keeps later lexicals from shadowing it.
        global_lexical_epoch: u64,
        /// Whether the proven own data descriptor is writable.
        writable: bool,
    },
}

/// One permanent global-declarative binding available to generated code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitGlobalLexicalLoad {
    /// Compressed GC-cage offset of the rooted, non-moving global-lexical cell.
    pub cell_offset: u32,
}

/// One string or BigInt literal cell available to generated code.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct JitLiteralCell {
    /// Process-local address of the isolate-owned, GC-traced `Value` cell.
    /// The cell allocation is stable and outlives every code object in the
    /// isolate; artifacts retain only function/byte-PC identity.
    pub cell_addr: usize,
}

impl std::fmt::Debug for JitLiteralCell {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "JitLiteralCell {{ cell_addr: <redacted> }}")
    }
}

/// One guarded own-data load from the global object record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitGlobalObjectLoad {
    /// Expected ordinary shape-handle offset or dictionary slot-layout epoch.
    pub shape: u64,
    /// Whether [`Self::shape`] names the dictionary slot-layout epoch rather
    /// than an ordinary compressed shape handle.
    pub dictionary: bool,
    /// Logical property field in the object's selected storage.
    pub field: crate::object::FieldLocation,
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
    /// The method's fully baked body, for an optimizing caller that builds
    /// it in place of the call; absent when the method needs an activation
    /// of its own or the inline budget is spent.
    pub body: Option<Arc<JitCompileSnapshot>>,
}

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
        /// Byte offset, in the receiver body, of the view's cached element
        /// base. Non-null only for a view over a fixed-length local buffer;
        /// while the isolate's detach protector is intact it replaces every
        /// buffer proof above.
        cached_data_byte: u32,
    },
}

/// How one element is stored, which fixes both the address stride and the load.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum JitElementRepr {
    /// A boxed `Value`. A hole is an absent property, so it leaves the fast
    /// path.
    #[default]
    Boxed,
    /// A raw signed 8-bit scalar, sign-extended and boxed as an int32.
    Int8,
    /// A raw unsigned 8-bit scalar, boxed as an int32.
    Uint8,
    /// A raw unsigned 8-bit scalar whose stores clamp to `0..=255`
    /// (§7.1.12 `ToUint8Clamp` over an int32).
    Uint8Clamped,
    /// A raw signed 16-bit scalar, sign-extended and boxed as an int32.
    Int16,
    /// A raw unsigned 16-bit scalar, boxed as an int32.
    Uint16,
    /// A raw signed 32-bit scalar, boxed on the way out.
    Int32,
    /// A raw unsigned 32-bit scalar. Values above `i32::MAX` box as doubles.
    Uint32,
    /// A raw IEEE-754 single, widened to a double and boxed on the way out.
    Float32,
    /// A raw IEEE-754 double, canonicalized and boxed on the way out.
    Float64,
}

impl JitElementRepr {
    /// Log2 of the element stride in bytes.
    #[must_use]
    pub const fn stride_shift(self) -> u32 {
        match self {
            Self::Int8 | Self::Uint8 | Self::Uint8Clamped => 0,
            Self::Int16 | Self::Uint16 => 1,
            Self::Int32 | Self::Uint32 | Self::Float32 => 2,
            Self::Boxed | Self::Float64 => 3,
        }
    }

    /// Whether a store takes an int32-boxed value and writes its low bits
    /// (modular `ToInt8`/`ToUint16`/… of an int32 is its truncation).
    #[must_use]
    pub const fn stores_int32(self) -> bool {
        matches!(
            self,
            Self::Int8
                | Self::Uint8
                | Self::Uint8Clamped
                | Self::Int16
                | Self::Uint16
                | Self::Int32
                | Self::Uint32
        )
    }

    /// The raw representation of one typed-array element kind, or `None` for
    /// the BigInt and Float16 kinds generated code does not convert.
    #[must_use]
    pub const fn for_typed_kind(kind: crate::binary::TypedArrayKind) -> Option<Self> {
        use crate::binary::TypedArrayKind as Kind;
        Some(match kind {
            Kind::Int8 => Self::Int8,
            Kind::Uint8 => Self::Uint8,
            Kind::Uint8Clamped => Self::Uint8Clamped,
            Kind::Int16 => Self::Int16,
            Kind::Uint16 => Self::Uint16,
            Kind::Int32 => Self::Int32,
            Kind::Uint32 => Self::Uint32,
            Kind::Float32 => Self::Float32,
            Kind::Float64 => Self::Float64,
            Kind::BigInt64 | Kind::BigUint64 | Kind::Float16 => return None,
        })
    }
}

/// An ordinary Array's dense storage as a generic keyed store reads it,
/// whatever storage kind the array currently has (V8's
/// `KeyedStoreGenericAssembler` over fast elements kinds). Offsets count
/// from the Array's `GcHeader`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitArrayStorage {
    /// The Array cell tag.
    pub type_tag: u8,
    /// Byte that reads zero while present dense slots have default own data
    /// semantics.
    pub own_guard_byte: u32,
    /// Compressed exotic-sidecar handle: zero while the array has neither a
    /// custom prototype nor exotic own state, so that, while the realm's
    /// array-index accessor protector also holds, a store into a hole is an
    /// ordinary own addition.
    pub exotic_byte: u32,
    /// `u32` live dense length.
    pub length_byte: u32,
    /// Element base pointer.
    pub base_byte: u32,
    /// Kind byte of tagged storage, whose holes are the hole sentinel.
    pub tagged_kind: u8,
    /// Numeric storage: the cached kind byte, its kinds and hole bitmap.
    pub numeric: JitHoleBitmap,
}

impl JitArrayStorage {
    /// The VM's current Array layout.
    #[must_use]
    pub const fn current() -> Self {
        let header = std::mem::size_of::<otter_gc::GcHeader>() as u32;
        Self {
            type_tag: crate::array::ARRAY_BODY_TYPE_TAG,
            own_guard_byte: header + crate::array::ARRAY_BODY_DENSE_OWN_GUARD_OFFSET as u32,
            exotic_byte: header + crate::array::ARRAY_BODY_EXOTIC_OFFSET as u32,
            length_byte: header + crate::array::ARRAY_BODY_DENSE_LEN_OFFSET as u32,
            base_byte: header + crate::array::ARRAY_BODY_ELEMENTS_PTR_OFFSET as u32,
            tagged_kind: crate::array::DENSE_ELEMENT_KIND_TAGGED as u8,
            numeric: JitHoleBitmap {
                capacity_byte: crate::array::elements::CAPACITY_FROM_DATA_BYTE,
                hole_count_byte: crate::array::elements::HOLE_COUNT_FROM_DATA_BYTE,
                storage_kind_byte: crate::array::elements::KIND_FROM_DATA_BYTE,
                kind_byte: header + crate::array::ARRAY_BODY_DENSE_KIND_OFFSET as u32,
                packed_kind: crate::array::DENSE_ELEMENT_KIND_PACKED_DOUBLE as u8,
                holey_kind: crate::array::DENSE_ELEMENT_KIND_HOLEY_DOUBLE as u8,
            },
        }
    }
}

/// One receiver family's indexed-element storage, as the guard program reads it.
///
/// Every element-bearing body answers the same questions — which cell tag it
/// carries, what instance state invalidates the layout, where its live element
/// count lives, where its element base lives, and how one element is stored —
/// so the address program is written once and the family is data.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
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
    /// Where the VM-maintained element base pointer lives. Ordinary arrays
    /// may own a moving young slab; tracing and mutation refresh this cache.
    /// Generated addresses must be reloaded from the rooted receiver after
    /// any collecting or reentrant operation.
    pub base: JitElementBase,
    /// How one element is stored.
    pub element: JitElementRepr,
    /// The side bitmap that marks holes in numeric storage, when the layout
    /// has one. A set bit is an absent element whatever the payload word
    /// holds.
    pub holes: Option<JitHoleBitmap>,
}

/// Numeric dense storage that may have holes: either numeric storage kind
/// applies, and a bitmap marks the holes, one bit per element, in `u64`
/// words that start right after the storage's `capacity` element words. A
/// packed prefix keeps every bit clear, so the one access serves both kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitHoleBitmap {
    /// Signed byte offset, from the element base, of the storage's `u32`
    /// element capacity.
    pub capacity_byte: i32,
    /// Signed byte offset, from the element base, of the storage's `u32`
    /// count of set bits. Clearing the last one makes the storage packed.
    pub hole_count_byte: i32,
    /// Signed byte offset, from the element base, of the storage's own kind
    /// byte, which a packed transition rewrites with the receiver's.
    pub storage_kind_byte: i32,
    /// Byte offset, from the receiver's `GcHeader`, of its one-byte storage
    /// kind.
    pub kind_byte: u32,
    /// The kind byte of hole-free numeric storage.
    pub packed_kind: u8,
    /// The kind byte of numeric storage with holes.
    pub holey_kind: u8,
}

impl JitElementAccess {
    /// Build the VM's complete packed-double own Array access program.
    /// A prototype-only sidecar is legal because every accessed slot is present.
    #[must_use]
    pub fn packed_double_array() -> Self {
        let header = std::mem::size_of::<otter_gc::GcHeader>() as u32;
        Self {
            type_tag: crate::array::ARRAY_BODY_TYPE_TAG,
            guards: [
                Some(JitBodyGuard::clear(
                    header + crate::array::ARRAY_BODY_DENSE_OWN_GUARD_OFFSET as u32,
                    JitGuardWidth::Byte,
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
            holes: None,
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

    /// Whether this immutable guard program proves an Array whose complete
    /// live dense prefix has default own semantics and raw hole-free doubles.
    ///
    /// The physical discriminant remains VM-private; JIT backends consume the
    /// semantic layout through this snapshot-owned predicate.
    #[must_use]
    pub fn is_packed_double_array(&self) -> bool {
        let [Some(own), Some(kind)] = self.guards else {
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
            && own.byte == header + crate::array::ARRAY_BODY_DENSE_OWN_GUARD_OFFSET as u32
            && own.width == JitGuardWidth::Byte
            && own.expect == 0
            && kind.byte == header + crate::array::ARRAY_BODY_DENSE_KIND_OFFSET as u32
            && kind.width == JitGuardWidth::Byte
            && kind.expect == crate::array::DENSE_ELEMENT_KIND_PACKED_DOUBLE
    }
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
    /// Whether this named-property site reached semantic dispatch, independently
    /// of whether its receivers admit an attached CacheIR program.
    pub property_attempted: bool,
    /// Arithmetic representation observations frozen for this canonical PC.
    pub(crate) arith_feedback: ArithFeedback,
    /// Address of the live arithmetic observation byte baseline code
    /// records into, or zero when the instruction has no feedback cell.
    pub arith_cell: u64,
}

impl JitInstructionMetadata {
    fn without_feedback(instruction_index: u32, byte_pc: u32) -> Self {
        Self {
            instruction_index,
            byte_pc,
            load_array_length: false,
            method_hint: JitMethodHint::None,
            load_number: None,
            call_attempted: false,
            property_attempted: false,
            arith_feedback: ArithFeedback::default(),
            arith_cell: 0,
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
        Self::without_feedback_with_handlers(
            function_id,
            param_count,
            register_count,
            instructions,
            &[],
        )
    }

    /// [`Self::without_feedback`] for a body with an exception handler
    /// table.
    #[must_use]
    pub fn without_feedback_with_handlers(
        function_id: u32,
        param_count: u16,
        register_count: u16,
        instructions: Vec<JitTestInstruction>,
        handlers: &[otter_bytecode::ExceptionHandler],
    ) -> Self {
        let code_block = CodeBlock::jit_test_stub(
            function_id,
            param_count,
            register_count,
            &instructions,
            handlers,
        );
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
            literal_allocations: JitLiteralAllocationPlans::default(),
            cage_base: 0,
            array_layout: JitArrayLayout::default(),
            element_accesses: rustc_hash::FxHashMap::default(),
            array_iteration: None,
            unseen_element_sites: rustc_hash::FxHashSet::default(),
            string_layout: JitStringLayout::default(),
            object_shape_byte: 0,
            exotic_dictionary_layout_byte: 0,
            exotic_instance_root_byte: 0,
            field_layout: crate::object::FieldLayout::current(),
            shape_property_count_byte: 0,
            object_exotic_handle_byte: 0,
            gc_barrier: JitGcBarrierLayout::default(),
            shape_prototype_byte: 0,
            shape_state_byte: 0,
            shape_inline_capacity_byte: 0,
            closure_call_layout: JitClosureCallLayout::default(),
            class_constructor_layout: JitClassConstructorLayout::default(),
            constructor_layout: JitConstructorLayout::default(),
            primitive_cell_type_tags: [0; 3],
            global_lexical_value_byte: 0,
            context_layout: JitContextLayout::current(),
            collection_layout: JitCollectionLayout::default(),
            native_call_layout: JitNativeCallLayout::default(),
            instructions,
            global_lexical_loads: rustc_hash::FxHashMap::default(),
            literal_cells: rustc_hash::FxHashMap::default(),
            global_object_loads: rustc_hash::FxHashMap::default(),
            native_calls: rustc_hash::FxHashMap::default(),
            direct_callees: rustc_hash::FxHashMap::default(),
            direct_constructs: rustc_hash::FxHashMap::default(),
            direct_methods: rustc_hash::FxHashMap::default(),
            inline_callees: rustc_hash::FxHashMap::default(),
            inline_methods: rustc_hash::FxHashMap::default(),
            inline_poly_methods: rustc_hash::FxHashMap::default(),
            guarded_method_calls: rustc_hash::FxHashMap::default(),
            function_prototype_calls: rustc_hash::FxHashMap::default(),
            instanceof_cells: rustc_hash::FxHashMap::default(),
            array_constructor_sites: rustc_hash::FxHashMap::default(),
            forward_apply_native_ref: None,
            property_programs: rustc_hash::FxHashMap::default(),
            property_action_cache: None,
            property_accesses: rustc_hash::FxHashMap::default(),
            binding_hit_proofs: rustc_hash::FxHashMap::default(),
            context_allocations: rustc_hash::FxHashMap::default(),
            closure_allocations: rustc_hash::FxHashMap::default(),
            optimized_exit_reasons: std::collections::BTreeMap::new(),
            feedback_exits: std::collections::BTreeSet::new(),
            parameter_widening: Box::default(),
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

    /// Set compiler-owned argument mapping in a backend-test snapshot.
    #[doc(hidden)]
    pub fn seed_argument_bindings_for_test(
        &mut self,
        kind: otter_bytecode::ArgumentsObjectKind,
        bindings: &[(u16, otter_bytecode::ArgumentBindingStorage)],
    ) {
        let code = std::sync::Arc::get_mut(&mut self.code_block)
            .expect("backend test snapshot uniquely owns its CodeBlock");
        code.arguments_object_kind = kind;
        code.mapped_argument_bindings = bindings
            .iter()
            .map(
                |&(argument_index, storage)| crate::executable::ExecMappedArgumentBinding {
                    argument_index,
                    storage,
                },
            )
            .collect();
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

    /// Mark one backend-test property site as previously attempted.
    /// Production snapshots read the CodeBlock-owned property's lifecycle state.
    #[doc(hidden)]
    pub fn seed_property_attempted_for_test(&mut self, instruction_pc: u32) {
        self.instructions
            .get_mut(instruction_pc as usize)
            .expect("test feedback PC belongs to the snapshot")
            .property_attempted = true;
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

    /// Fold an earlier optimized generation's exit at this instruction into
    /// its baked feedback, so lowering speculates no narrower than the exit
    /// already refuted.
    pub fn note_optimized_exit(&mut self) {
        self.arith_feedback = self.arith_feedback.after_optimized_exit();
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
    /// Ordinary dense arrays of raw `f64`s, some of them with holes selected
    /// by a side bitmap. Covers [`Self::DenseFloat64`] receivers as well.
    DenseHoleyFloat64,
    /// Fixed-length typed views of exactly this element kind. BigInt and
    /// Float16 views are recorded as [`Self::Generic`].
    Typed(crate::binary::TypedArrayKind),
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
    /// Optional admitted source context; native-only turns have no bytecode chunk.
    pub(crate) context: *const crate::ExecutionContext,
}

impl VmRuntimeActivation {
    /// Publish one synchronous compiled activation from live VM borrows.
    pub(crate) fn new(
        vm: &mut crate::Interpreter,
        stack: &mut crate::ActivationStack,
        context: Option<&crate::ExecutionContext>,
    ) -> Self {
        Self {
            vm,
            stack,
            context: context.map_or(std::ptr::null(), std::ptr::from_ref),
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

    /// Execution context that owns `function_id`. Chunk-local constants,
    /// code and resolutions of a published frame resolve through its owner,
    /// never through the entry's ambient context, which may name another chunk.
    ///
    /// # Safety
    /// The activation's context must remain live for this call.
    #[must_use]
    pub unsafe fn owner_context<'a>(
        self,
        function_id: u32,
    ) -> Option<crate::code_space::ResolvedCtx<'a>> {
        // SAFETY: forwarded from the caller's liveness contract; the admitted
        // context outlives every transition of this activation.
        match unsafe { self.context.as_ref() } {
            Some(context) => context.for_function(function_id).ok(),
            None => {
                let vm = unsafe { self.vm.as_ref() }?;
                vm.function_context(None, function_id)
                    .ok()
                    .map(crate::code_space::ResolvedCtx::Owned)
            }
        }
    }

    /// Current VM-owned execution context for this dynamic entry.
    /// Dereferencing requires the activation's exclusive-mutator contract.
    #[doc(hidden)]
    pub fn execution_context_ptr(&self) -> *mut crate::native_abi::JitCtx {
        unsafe { self.stack.as_ref() }
            .map_or(std::ptr::null_mut(), |stack| stack.execution_context())
    }

    #[cfg(test)]
    pub(crate) const fn for_test(vm: *mut crate::Interpreter) -> Self {
        Self {
            vm,
            stack: std::ptr::null_mut(),
            context: std::ptr::null(),
        }
    }
}

const _: [(); 24] = [(); std::mem::size_of::<VmRuntimeActivation>()];
const _: [(); 8] = [(); std::mem::align_of::<VmRuntimeActivation>()];
const _: [(); 0] = [(); std::mem::offset_of!(VmRuntimeActivation, vm)];
const _: [(); 8] = [(); std::mem::offset_of!(VmRuntimeActivation, stack)];
const _: [(); 16] = [(); std::mem::offset_of!(VmRuntimeActivation, context)];

/// Lifetime optimizing exit evidence for one source site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JitExitProfile {
    /// Cold policy carried by the generated exit.
    pub action: crate::native_abi::ExitAction,
    /// Exits observed across generations until the owning chunk is evicted.
    pub count: u32,
    /// For a shape-guard exit at a named property site, the receiver
    /// programs its inline cache held when the exit was taken
    /// ([`crate::executable::CodeBlock::property_site_population`]). A cache
    /// that has grown since describes the receiver that left, so the site
    /// speculates again; an unchanged one cannot, so the site stays generic.
    pub feedback_population: Option<u32>,
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

    /// Entry of this generation in the JavaScript call ABI, which builds,
    /// publishes and retires the function's own frame; see
    /// [`crate::native_abi::CodeEntryCell::entry_addr`]. `None` for a body
    /// entered only over an interpreter frame.
    fn call_entry_addr(&self) -> Option<usize> {
        None
    }

    /// Immutable isolate-state dependencies declared by this code object.
    ///
    /// Implementations that record dependencies own the backing slice for the
    /// code object's lifetime and set `metadata().dependency_count` to the
    /// exact slice length. Baseline/template code uses the empty default.
    fn dependencies(&self) -> &[crate::native_abi::CodeDependency] {
        &[]
    }

    /// Source functions whose bodies were actually spliced into this code.
    /// The immutable list is sorted, unique, and excludes this code's own
    /// function. It covers bodies with no exits or safepoints. Ordinary
    /// function-cell call targets do not belong here: their tier is selected
    /// independently on each entry.
    fn spliced_functions(&self) -> &[u32] {
        &[]
    }

    /// Base address of the complete executable mapping, retained by this owner.
    /// It is distinct from a tier or call entry address inside that mapping.
    fn native_code_address(&self) -> Option<u64> {
        None
    }

    /// Exact sorted associations of real machine returns to source/root records.
    fn return_sites(&self) -> &[crate::native_abi::SafepointEntry] {
        &[]
    }

    /// Size in bytes of the complete finalized native code mapping.
    fn code_len(&self) -> usize;

    /// Total bytes this installed code object retains: the executable
    /// mapping plus owned metadata, safepoint records, IC cells, operand
    /// tables, and deopt maps. The isolate registry charges this amount to
    /// `GeneratedCodeBytes` at installation and releases it at retirement.
    /// The default covers the executable mapping only; tiers with owned
    /// side tables override it with their exact retained sum.
    fn retained_bytes(&self) -> u64 {
        self.code_len() as u64
    }

    /// `true` when this code was compiled with unsupported opcodes emitted as
    /// bail-to-interpreter, making it sound to enter only at a supported loop
    /// header via OSR (not at function entry). The function-entry tier-up path
    /// skips such code; loop OSR uses it. Default `false`.
    fn osr_only(&self) -> bool {
        false
    }

    /// Entry continuing an already published interpreter frame at PC zero
    /// (`JitEntry`): the function-entry tier transfer.
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

    /// Address of the generated OSR entry for this logical loop header.
    ///
    /// The common trampoline enters it over the published frame after the
    /// interpreter has returned. The code object owns no execution stack.
    fn osr_entry_addr(&self, _logical_pc: u32) -> Option<usize> {
        None
    }

    /// Whether an optimizing generation can enter this loop header.
    fn enters_optimized_osr_header(&self, logical_pc: u32) -> bool {
        self.native_frame_kind() == NativeFrameKind::Optimizing
            && self.osr_entry_addr(logical_pc).is_some()
    }

    /// Observe a validated compiled completion without VM allocation or reentry.
    ///
    /// Cold compiler measurement uses this scalar notification; production
    /// code objects require no completion bookkeeping here.
    fn note_completion(&self, _status: NativeResultStatus) {}
}

/// On-demand snapshot of executable code retained by one interpreter.
///
/// Code objects are deduplicated by allocation identity across the canonical
/// shared Template cache, the separate optimizing cache, and auxiliary
/// direct-call caches. `code_bytes` sums finalized native buffer lengths, not Rust metadata
/// or page-rounding overhead.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JitCodeResidency {
    /// Installed optimizing-tier entry bodies.
    pub installed_optimized_bodies: u64,
    /// Canonical Template bodies eligible for ordinary function entry.
    pub installed_entry_bodies: u64,
    /// Canonical Template bodies selected by at least one OSR header. A shared
    /// entry-capable body contributes to both category counts but only once to
    /// `unique_code_objects` and `code_bytes`.
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
    /// Whether the permanent function entry cell currently selects this exact
    /// generation. A linked fallback tier can be Installed without being the
    /// current function-cell destination.
    pub current_entry: bool,
    /// Actual private-JavaScript call entry relative to this retained native
    /// mapping, checked against its physical byte length. `None` means there
    /// is no in-mapping call entry or the mapping has retired. This describes
    /// emitted capability independently of the compile trigger and linkage.
    pub call_entry_offset: Option<u32>,
    /// Published native frames executing this generation plus explicit
    /// entry leases.
    pub active_count: u32,
    /// Formal parameter count frozen into the entry cell.
    pub param_count: u16,
    /// Initialized tagged register-window length.
    pub register_count: u16,
    /// Template generated native entries observed for this exact generation.
    /// The Graph backend omits entry accounting, so zero does not prove that
    /// an optimizing generation never ran. Ordinary VM/OSR entries are also
    /// outside this generated-call counter.
    pub generated_entries: u64,
    /// Recorded Template generated entries minus their cold deopts.
    /// Graph generations report zero because their entries are uncounted.
    pub generated_returns: u64,
    /// Generated entries that cold-deoptimized.
    pub generated_deopts: u64,
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
        /// Number of lowered IR operations presented to native emission.
        ir_node_count: u64,
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
        assert_eq!(std::mem::size_of::<JitClosureCallLayout>(), 56);
        assert_eq!(std::mem::align_of::<JitClosureCallLayout>(), 4);
        let fields = [
            std::mem::offset_of!(JitClosureCallLayout, function_id_byte),
            std::mem::offset_of!(JitClosureCallLayout, flags_byte),
            std::mem::offset_of!(JitClosureCallLayout, context_byte),
            std::mem::offset_of!(JitClosureCallLayout, bound_this_byte),
            std::mem::offset_of!(JitClosureCallLayout, bound_new_target_byte),
            std::mem::offset_of!(JitClosureCallLayout, bound_this_flag),
            std::mem::offset_of!(JitClosureCallLayout, bound_new_target_flag),
            std::mem::offset_of!(JitClosureCallLayout, runtime_setup_flags),
            std::mem::offset_of!(JitClosureCallLayout, rare_byte),
            std::mem::offset_of!(JitClosureCallLayout, own_props_byte),
            std::mem::offset_of!(JitClosureCallLayout, prototype_byte),
            std::mem::offset_of!(JitClosureCallLayout, constructor_layouts_byte),
            std::mem::offset_of!(JitClosureCallLayout, prototype_ordinary_byte),
            std::mem::offset_of!(JitClosureCallLayout, instanceof_cached_byte),
        ];
        assert_eq!(
            fields,
            [0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 48, 52]
        );
    }

    #[test]
    fn context_layout_names_header_relative_words() {
        let layout = JitContextLayout::current();
        let header = otter_gc::header::HEADER_SIZE as u32;
        assert_eq!(layout.type_tag, crate::context::CONTEXT_BODY_TYPE_TAG);
        assert_eq!(layout.scope_function_id_byte, header);
        assert_eq!(layout.scope_index_byte, header + 4);
        assert_eq!(layout.slot_count_byte, header + 6);
        assert_eq!(layout.parent_byte, header + 8);
        assert_eq!(layout.slots_byte, header + 16);
    }

    #[test]
    fn runtime_activation_contains_only_execution_services() {
        assert_eq!(std::mem::size_of::<VmRuntimeActivation>(), 24);
        assert_eq!(std::mem::align_of::<VmRuntimeActivation>(), 8);
        assert_eq!(std::mem::offset_of!(VmRuntimeActivation, context), 16);
    }
}
