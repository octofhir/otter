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
    /// Instruction stream in byte-PC order.
    pub instructions: Vec<JitInstrView>,
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
}

/// Baked monomorphic inline property-load plan for one `LoadProperty` site.
///
/// Computed VM-side from a warm own-data IC at tier-up: the emitter reads the
/// receiver's shape handle at `[obj_ptr + shape_byte]`, compares it to
/// `cached_shape_offset`, and on a hit loads the value at
/// `[obj_ptr + value_byte]` — no interpreter round-trip. `obj_ptr` is the
/// decompressed `Gc` pointer (`cage_base + offset`); `*_byte` already include
/// the GC header. A guard miss falls through to the shared runtime stub.
#[derive(Debug, Clone, Copy)]
pub struct JitInlineLoad {
    /// Cached shape handle compressed offset to compare against the receiver's.
    pub cached_shape_offset: u32,
    /// Byte offset from the decompressed object pointer to the shape handle.
    pub shape_byte: u32,
    /// Byte offset from the decompressed object pointer to the cached value.
    pub value_byte: u32,
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
    /// Baked monomorphic inline-load plan for a `LoadProperty` whose IC was a
    /// warm own-data hit on an in-object slot at tier-up. `None` → emit the
    /// shared runtime stub (cold, polymorphic, accessor, dict, or overflow).
    pub inline_load: Option<JitInlineLoad>,
    /// Operands in declaration order. Branch immediates are already rewritten
    /// to byte-offset deltas in VM dispatch coordinates.
    pub operands: Vec<Operand>,
    /// `true` for a `MakeFunction` whose target is the function being compiled
    /// (the named-function SELF binding). The emitter materializes it as a
    /// direct read of the frame's own closure (carried in `JitCtx`) instead of
    /// a Rust round-trip through `jit_runtime_make_function`.
    pub make_self: bool,
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
/// The VM never transmutes or calls raw entry pointers directly: it hands the
/// JIT the reentry pointers and receives a structured outcome, keeping
/// executable memory ownership and the unsafe ABI call inside `otter-jit`.
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
