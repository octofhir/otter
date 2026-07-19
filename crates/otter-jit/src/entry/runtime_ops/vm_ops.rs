//! Property, element, binding, and collection VM slow paths.
//!
//! # Contents
//! - Self-patching property IC cells and miss handlers.
//! - Element/global/upvalue/object runtime operations.
//! - Write-barrier entries.
//!
//! # Invariants
//! Register operands address the published JIT window. Allocating or throwing
//! operations keep that window live and park failures in the shared error slot.
//!
//! # See also
//! - `otter_vm::jit_runtime_ops` — safe VM-side implementations.

use otter_vm::Value;

use super::super::JitCtx;
use super::park_jit_error;

/// Number of shapes a WhiskerIC site caches inline before it is megamorphic and
/// always misses to the stub. Four matches the polymorphism most real sites
/// reach (V8 / JSC use the same width); a bimorphic site (e.g. two object
/// layouts alternating through one loop) then stays fully inline instead of
/// thrashing a single cell.
pub(crate) const IC_WAYS: usize = 4;

/// One cached `(shape → slot)` mapping in a [`WhiskerIcCell`].
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct WhiskerIcWay {
    /// Cached receiver shape-handle compressed offset; `0` == empty.
    shape: u32,
    /// Byte offset from the value slab pointer to the value slot.
    value_byte: u32,
}

/// WhiskerIC self-patching cell for one named-property site (one per
/// `LoadProperty` / `StoreProperty` op in the compiled function). Emitted code
/// walks the [`IC_WAYS`] ways comparing each `shape` (a `0` shape never matches
/// a live receiver, so empty ways are skipped for free); on a hit it reads the
/// matched way's `value_byte`. On a monomorphic own-data inline-slot miss the
/// stub fills the next empty way, so a poly site caches every shape it sees up
/// to the width. The cell holds only compressed offsets (no GC pointers), so it
/// needs no tracing, and a shape offset is a stable token (shapes are immortal
/// and pinned in old space).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct WhiskerIcCell {
    ways: [WhiskerIcWay; IC_WAYS],
}

/// Self-patch one IC cell with a resolved `(shape, value_byte)` mapping: fill
/// the first empty way, or evict way 0 when all are full (the site is more
/// polymorphic than the cache is wide). Writes `value_byte` before `shape` so a
/// concurrent inline guard never reads a live shape against a stale offset.
///
/// # Safety
/// `cell` must be a valid, stable [`WhiskerIcCell`] pointer (a site's cell from
/// the owning code object's backing slice).
unsafe fn whisker_ic_fill(cell: *mut WhiskerIcCell, shape: u32, value_byte: u32) {
    unsafe {
        let ways = &mut (*cell).ways;
        let slot = ways
            .iter()
            .position(|w| w.shape == 0 || w.shape == shape)
            .unwrap_or(0);
        ways[slot].value_byte = value_byte;
        ways[slot].shape = shape;
    }
}

/// `LoadProperty` miss handler over the canonical active register window.
/// Resolves the own-data IC directly and completes every remaining `[[Get]]`
/// case through the VM.
/// Returns `0` when handled and `1` on throw; it never requests an exact side
/// exit. `function_id` is baked by the emitter.
pub(crate) extern "C" fn jit_load_property_stub(
    ctx: *mut JitCtx,
    dst: u64,
    obj: u64,
    name_idx: u64,
    site: u64,
    cell: u64,
    function_id: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let mut runtime = match ctx.runtime_call() {
        Ok(runtime) => runtime,
        Err(err) => {
            park_jit_error(ctx, err);
            return 1;
        }
    };
    let result = runtime.load_property(
        function_id as u32,
        dst as u16,
        obj as u16,
        name_idx as u32,
        site as usize,
    );
    match result {
        Ok(fill) => {
            if cell != 0 && fill != 0 {
                let cell = cell as *mut WhiskerIcCell;
                // SAFETY: stable per-site cell address baked into this code.
                unsafe {
                    whisker_ic_fill(cell, fill as u32, (fill >> 32) as u32);
                }
            }
            0
        }
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// `StoreProperty` miss handler — the [`jit_load_property_stub`]
/// counterpart. Resolves existing-own-data stores and shape transitions against
/// the canonical active window, then completes all remaining `[[Set]]`
/// semantics through the VM's shared value-level funnel.
pub(crate) extern "C" fn jit_store_property_stub(
    ctx: *mut JitCtx,
    obj: u64,
    name_idx: u64,
    src: u64,
    site: u64,
    cell: u64,
    function_id: u64,
) -> u64 {
    // SAFETY: as `jit_load_property_stub`.
    let ctx = unsafe { &mut *ctx };
    let mut runtime = match ctx.runtime_call() {
        Ok(runtime) => runtime,
        Err(err) => {
            park_jit_error(ctx, err);
            return 1;
        }
    };
    let result = runtime.store_property(
        function_id as u32,
        obj as u16,
        name_idx as u32,
        src as u16,
        site as usize,
    );
    match result {
        Ok(fill) => {
            if cell != 0 && fill != 0 {
                let cell = cell as *mut WhiskerIcCell;
                // SAFETY: stable per-site cell address baked into this code.
                unsafe {
                    whisker_ic_fill(cell, fill as u32, (fill >> 32) as u32);
                }
            }
            0
        }
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Runtime stub: run the GC write barrier for an inline `StoreProperty` whose
/// stored value is a heap pointer. The emitted fast path skips this for
/// primitive values (the common case); a pointer store calls here so an
/// old→young edge marks the parent object's card. Always returns `0`.
pub(crate) extern "C" fn jit_write_barrier_stub(ctx: *mut JitCtx, obj: u64, src: u64) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let mut runtime = match ctx.runtime_call() {
        Ok(runtime) => runtime,
        Err(err) => {
            park_jit_error(ctx, err);
            return 1;
        }
    };
    let result = runtime.write_barrier(obj as u16, src as u16);
    match result {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Runtime stub: perform a computed `LoadElement` (`recv[idx]`) from compiled
/// code, delegating to the safe [`Interpreter::jit_runtime_load_element`].
/// Returns `0` on success, `1` when the read threw (error parked in `ctx`).
pub(crate) extern "C" fn jit_load_element_stub(
    ctx: *mut JitCtx,
    dst: u64,
    recv: u64,
    idx: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let mut runtime = match ctx.runtime_call() {
        Ok(runtime) => runtime,
        Err(err) => {
            park_jit_error(ctx, err);
            return 1;
        }
    };
    let result = runtime.load_element(dst as u16, recv as u16, idx as u16);
    match result {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Runtime stub: perform a `LoadGlobalOrThrow` from compiled code through
/// the safe [`Interpreter::jit_runtime_load_global`]. Returns `0` on success,
/// `1` when the read threw (unbound identifier / throwing accessor; error
/// parked in `ctx`).
pub(crate) extern "C" fn jit_load_global_stub(
    ctx: *mut JitCtx,
    dst: u64,
    name_idx: u64,
    function_id: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let mut runtime = match ctx.runtime_call() {
        Ok(runtime) => runtime,
        Err(err) => {
            park_jit_error(ctx, err);
            return 1;
        }
    };
    let result = runtime.load_global(function_id as u32, dst as u16, name_idx as u32);
    match result {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Runtime stub: perform a `LoadUpvalue` (captured-binding read) from compiled
/// code, delegating to [`Interpreter::jit_runtime_load_upvalue`]. `idx` carries
/// the bytecode's signed upvalue index. Returns `0` on success, `1` on throw
/// (TDZ `ReferenceError`, error parked in `ctx`).
pub(crate) extern "C" fn jit_load_upvalue_stub(ctx: *mut JitCtx, dst: u64, idx: u64) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let mut runtime = match ctx.runtime_call() {
        Ok(runtime) => runtime,
        Err(err) => {
            park_jit_error(ctx, err);
            return 1;
        }
    };
    let result = runtime.load_upvalue(dst as u16, idx as u32 as i32);
    match result {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Runtime stub: perform a `StoreUpvalue` (captured-binding write) from compiled
/// code, delegating to [`Interpreter::jit_runtime_store_upvalue`]. Returns `0`
/// on success, `1` on throw (error parked in `ctx`).
pub(crate) extern "C" fn jit_store_upvalue_stub(ctx: *mut JitCtx, src: u64, idx: u64) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let mut runtime = match ctx.runtime_call() {
        Ok(runtime) => runtime,
        Err(err) => {
            park_jit_error(ctx, err);
            return 1;
        }
    };
    let result = runtime.store_upvalue(src as u16, idx as u32 as i32);
    match result {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Runtime stub: allocate an ordinary object for `NewObject` from compiled code.
/// Uses the VM's stack-rooted allocator so moving young-GC semantics match the
/// interpreter path.
pub(crate) extern "C" fn jit_new_object_stub(ctx: *mut JitCtx, dst: u64) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let mut runtime = match ctx.runtime_call() {
        Ok(runtime) => runtime,
        Err(err) => {
            park_jit_error(ctx, err);
            return 1;
        }
    };
    let result = runtime.new_object(dst as u16);
    match result {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

/// Materialize a regex literal (`Op::LoadRegExp`) into the frame's
/// destination register. Allocating; a bad pattern reports status 1.
pub(crate) extern "C" fn jit_load_regexp_stub(ctx: *mut JitCtx, dst: u64, idx: u64) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let mut runtime = match ctx.runtime_call() {
        Ok(runtime) => runtime,
        Err(err) => {
            park_jit_error(ctx, err);
            return 1;
        }
    };
    let result = runtime.load_regexp(dst as u16, idx as u32);
    match result {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}

pub(crate) extern "C" fn otter_jit_math_random() -> u64 {
    Value::number(otter_vm::math::random_number()).to_bits()
}

/// Runtime stub: perform a computed `StoreElement` (`recv[idx] = src`) from
/// compiled code, delegating to the safe
/// [`Interpreter::jit_runtime_store_element`]. Returns `0` on success, `1` when
/// the write threw (error parked in `ctx`).
pub(crate) extern "C" fn jit_store_element_stub(
    ctx: *mut JitCtx,
    recv: u64,
    idx: u64,
    src: u64,
) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let mut runtime = match ctx.runtime_call() {
        Ok(runtime) => runtime,
        Err(err) => {
            park_jit_error(ctx, err);
            return 1;
        }
    };
    let result = runtime.store_element(recv as u16, idx as u16, src as u16);
    match result {
        Ok(()) => 0,
        Err(err) => {
            park_jit_error(ctx, err);
            1
        }
    }
}
