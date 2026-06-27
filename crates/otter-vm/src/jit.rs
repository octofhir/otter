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
//! - [`JitFunctionView`] and [`JitInstrView`] — owned snapshots of the frozen
//!   executable bytecode stream.
//! - [`JitCompilerHook`] — runtime-installed compile hook implemented outside
//!   `otter-vm`.
//! - [`JitFunctionCode`] and [`JitCompileStatus`] — type-erased compiled-code
//!   result handles that keep executable memory ownership outside this crate.
//!
//! # Invariants
//! - DTOs are owned and borrow-free. JIT compilation must not hold references
//!   into `ExecutionContext`, `ExecutableFunction`, or interpreter frames.
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

/// Owned compile request for one bytecode function.
#[derive(Debug, Clone)]
pub struct JitCompileRequest {
    /// Function snapshot to compile.
    pub function: JitFunctionView,
    /// Loop-header byte-PC for an OSR-target compile. `None` means normal
    /// function-entry compilation.
    pub osr_pc: Option<u32>,
}

/// Owned snapshot of one executable function body.
#[derive(Debug, Clone)]
pub struct JitFunctionView {
    /// Global VM function id.
    pub function_id: u32,
    /// Number of parameter registers at the start of the frame.
    pub param_count: u16,
    /// Total register window size: params + locals + scratch.
    pub register_count: u16,
    /// Total encoded byte length of the function.
    pub code_byte_len: u32,
    /// `true` when this function uses strict-mode call semantics.
    pub is_strict: bool,
    /// `true` when this function is async.
    pub is_async: bool,
    /// `true` when this function is a generator.
    pub is_generator: bool,
    /// `true` when this function is an async generator.
    pub is_async_generator: bool,
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
    /// Byte offset from a decompressed object pointer to its flat
    /// `[[Prototype]]` mirror (`HEADER_SIZE + OBJECT_BODY_JIT_PROTO_OFFSET`). A
    /// `#[repr(C)]` constant; the method-inline guard reads
    /// `[recv_ptr + jit_proto_byte]` to chase the receiver's prototype chain
    /// in machine code without a resolve bridge.
    pub jit_proto_byte: u32,
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
    /// Instruction stream in byte-PC order.
    pub instructions: Vec<JitInstrView>,
    /// Inline-candidate callees for baseline leaf-inlining, keyed by the
    /// caller's `Op::Call` byte-PC. Populated only for sites the interpreter
    /// observed resolving to a single plain synchronous bytecode callee; baked
    /// by `Interpreter::bake_inline_callees`. Empty in the raw `jit_view()`
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
    pub instructions: Vec<JitInstrView>,
}

/// A method the baseline may splice into a caller's `Op::CallMethodValue` site.
/// Carries the method's body plus the data to guard it: the receiver shape the
/// body's sealed property loads/stores are baked against, and, per body
/// `LoadProperty`/`StoreProperty` byte-PC, the value byte offset within the
/// decompressed receiver.
/// Method identity is verified inline every call: the emitter loads the
/// receiver's flat prototype handle, guards the prototype's shape against
/// [`proto_shape`](Self::proto_shape), reads the method slot at
/// [`method_value_byte`](Self::method_value_byte), and compares the resolved
/// closure's `function_id` to [`method_fid`](Self::method_fid). A
/// prototype-method reassignment or shape change falls back to the in-place
/// method call — no per-call resolve bridge.
#[derive(Debug, Clone)]
pub struct JitInlineMethod {
    /// Method function id the call-site identity check is keyed on.
    pub method_fid: u32,
    /// Receiver shape-handle compressed offset the sealed loads are baked for.
    pub recv_shape: u32,
    /// Receiver prototype shape-handle compressed offset the inline identity
    /// guard requires (the shape of the object holding the method slot).
    pub proto_shape: u32,
    /// Byte offset inside the prototype object's value slab for the method
    /// slot, baked from the prototype shape.
    pub method_value_byte: u32,
    /// `true` when the method slot is an own property on the receiver rather
    /// than a property on the receiver's prototype.
    pub method_on_receiver: bool,
    /// Method formal parameter count (excluding `this`); must equal argc.
    pub param_count: u16,
    /// Method register-window length; the body runs in a scratch block of this
    /// many slots plus one for `this`.
    pub register_count: u16,
    /// Method instruction stream, emitted inline.
    pub instructions: Vec<JitInstrView>,
    /// Body `LoadProperty`/`StoreProperty` byte-PC → value slab byte offset,
    /// baked from the receiver shape.
    pub prop_offsets: rustc_hash::FxHashMap<u32, u32>,
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
    /// Number of formal parameter registers.
    pub param_count: u16,
    /// Total callee register-window length.
    pub register_count: u16,
}

/// Prepared direct-call entry state returned by the VM to emitted code.
///
/// The frame has already been published onto the active [`JitFrameStack`], so
/// its value slots are visible to precise GC tracing. Emitted code uses this to
/// construct the callee `JitCtx` and branch to `entry_addr` without the generic
/// call bridge.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct JitPreparedDirectCall {
    /// Raw compiled entry address.
    pub entry_addr: usize,
    /// Callee register-window base.
    pub regs: *mut u64,
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

/// Owned snapshot of one executable instruction.
#[derive(Debug, Clone)]
pub struct JitInstrView {
    /// Opcode.
    pub op: Op,
    /// Byte-offset PC in the encoded function stream.
    pub byte_pc: u32,
    /// Encoded instruction length in bytes.
    pub byte_len: u32,
    /// Dense property-IC site id for named property ops.
    pub property_ic_site: Option<usize>,
    /// Operands in declaration order. Branch immediates are already rewritten
    /// to byte-offset deltas in VM dispatch coordinates.
    pub operands: Vec<Operand>,
    /// `true` for a `MakeFunction` / `MakeClosure` whose target is the function
    /// being compiled (the named-function SELF binding). The emitter
    /// materializes it as a direct read of the frame's own closure (carried in
    /// `JitCtx`) instead of a Rust round-trip through the closure builder.
    pub make_self: bool,
    /// `true` when this instruction is a named-property read of literal
    /// `"length"`. The emitter uses it to try the Array exotic length fast
    /// path before falling back to ordinary property semantics.
    pub load_array_length: bool,
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
    /// `Interpreter::bake_arith_feedback` at tier-up; the raw `jit_view()`
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
    /// For a `NewObject` that begins an object literal (`{ k: v, … }` with
    /// constant string keys), the plan to allocate it directly in its final
    /// hidden class instead of running per-property shape transitions. `None`
    /// for every other `NewObject` and every non-`NewObject` op. Baked by
    /// `Interpreter::bake_object_literals`.
    pub object_literal: Option<ObjectLiteralPlan>,
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
