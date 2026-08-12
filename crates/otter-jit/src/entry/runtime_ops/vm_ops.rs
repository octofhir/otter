//! Property, element, binding, and collection VM slow paths.
//!
//! # Contents
//! - Self-patching property IC cells and miss handlers.
//! - Element/global/upvalue/object runtime operations.
//! - Write-barrier entries.
//!
//! # Invariants
//! Register-index operands address the published JIT window. Computed element
//! and named-property entries instead receive fixed boxed-value operands and
//! return a status pair. Named operations derive their immutable property name
//! and feedback site from the published function/logical-PC identity.
//! Allocating or throwing operations keep precise roots live and park failures
//! in the shared error slot.
//!
//! # See also
//! - `otter_vm::jit_runtime_ops` — safe VM-side implementations.

use super::super::{JitCtx, JitRet, STATUS_RETURNED, STATUS_THREW};
use super::park_jit_error;
use otter_vm::{RuntimeStubResult, RuntimeStubResultPair, Value};

/// Number of shapes a WhiskerIC site caches inline before it is megamorphic and
/// always misses to the stub. Four matches the polymorphism most real sites
/// reach (V8 / JSC use the same width); a bimorphic site (e.g. two object
/// layouts alternating through one loop) then stays fully inline instead of
/// thrashing a single cell.
pub(crate) const IC_WAYS: usize = 4;

/// One lowered cache program in a [`WhiskerIcCell`].
///
/// This is [`otter_vm::JitPropertyIcWay`] as generated code sees it. The four
/// words keep ways 16-byte strided so the inline probe indexes them with a
/// shift.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct WhiskerIcWay {
    /// Guarded receiver shape-handle compressed offset; `0` == empty.
    shape: u32,
    /// Guarded holder shape when the program hops to the receiver's
    /// prototype; `0` when the receiver owns the slot.
    holder_shape: u32,
    /// Byte offset from the holder's value slab pointer to the value slot.
    value_byte: u32,
    /// Child hidden class for an add-property transition; `0` keeps the
    /// existing-slot program above.
    transition_shape: u32,
}

/// Byte stride between ways, shared by the cell and the emitted probes.
pub(crate) const WHISKER_IC_WAY_BYTES: u32 = 16;

const _: () = assert!(
    std::mem::size_of::<WhiskerIcWay>() == WHISKER_IC_WAY_BYTES as usize,
    "emitted probes index ways by a baked stride"
);

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

/// Self-patch one IC cell with a lowered cache program: fill the first empty
/// way, or evict way 0 when all are full (the site is more polymorphic than the
/// cache is wide). The guard token is invalidated before rewriting an occupied
/// way and published last, so a concurrent inline probe never reads a live
/// parent shape against stale program words.
///
/// # Safety
/// `cell` must be a valid, stable [`WhiskerIcCell`] pointer (a site's cell from
/// the owning code object's backing slice).
unsafe fn whisker_ic_fill(cell: *mut WhiskerIcCell, way: otter_vm::JitPropertyIcWay) {
    unsafe {
        let ways = &mut (*cell).ways;
        let slot = ways
            .iter()
            .position(|w| w.shape == 0 || w.shape == way.receiver_shape)
            .unwrap_or(0);
        ways[slot].shape = 0;
        ways[slot].value_byte = way.value_byte;
        ways[slot].holder_shape = way.holder_shape;
        ways[slot].transition_shape = way.transition_shape;
        ways[slot].shape = way.receiver_shape;
    }
}

/// Complete the exact published `LoadProperty` from one boxed receiver.
///
/// The native frame supplies function/logical-PC identity; the VM validates
/// the opcode and derives its property name and feedback site. Success returns
/// the loaded value and may patch `cell`. Failure parks the error and returns
/// `Throw`; this boundary never requests replay or exact deoptimization.
pub(crate) extern "C" fn jit_load_property_stub(
    ctx: *mut JitCtx,
    receiver_bits: u64,
    cell: *mut WhiskerIcCell,
) -> RuntimeStubResultPair {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let result = ctx
        .runtime_call()
        .and_then(|mut runtime| runtime.load_property_value(Value::from_bits(receiver_bits)));
    match result {
        Ok((value, fill)) => {
            if !cell.is_null()
                && let Some(way) = fill
            {
                // SAFETY: stable per-site cell address baked into this code.
                unsafe {
                    whisker_ic_fill(cell, way);
                }
            }
            RuntimeStubResultPair::from_result(RuntimeStubResult::ok_bits(value.to_bits()))
        }
        Err(err) => {
            park_jit_error(ctx, err);
            RuntimeStubResultPair::from_result(RuntimeStubResult::thrown())
        }
    }
}

/// Complete the exact published `StoreProperty` from boxed receiver/value
/// operands.
///
/// A successful return means the full store committed once and may patch
/// `cell`; a setter/proxy exception is parked and returned as `Throw` without
/// replay.
pub(crate) extern "C" fn jit_store_property_stub(
    ctx: *mut JitCtx,
    receiver_bits: u64,
    value_bits: u64,
    cell: *mut WhiskerIcCell,
) -> RuntimeStubResultPair {
    // SAFETY: as `jit_load_property_stub`.
    let ctx = unsafe { &mut *ctx };
    let result = ctx.runtime_call().and_then(|mut runtime| {
        runtime.store_property_value(
            Value::from_bits(receiver_bits),
            Value::from_bits(value_bits),
        )
    });
    match result {
        Ok(fill) => {
            if !cell.is_null()
                && let Some(way) = fill
            {
                // SAFETY: stable per-site cell address baked into this code.
                unsafe {
                    whisker_ic_fill(cell, way);
                }
            }
            RuntimeStubResultPair::from_result(RuntimeStubResult::ok_bits(
                Value::undefined().to_bits(),
            ))
        }
        Err(err) => {
            park_jit_error(ctx, err);
            RuntimeStubResultPair::from_result(RuntimeStubResult::thrown())
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

/// Complete computed `[[Get]]` from fixed boxed-value operands.
///
/// The active native frame supplies the exact function/PC feedback identity
/// and precise roots. Success returns the loaded value. Failure parks the
/// error and returns `Throw`; this entry never requests replay or deopt.
pub(crate) extern "C" fn jit_load_element_stub(
    ctx: *mut JitCtx,
    receiver_bits: u64,
    key_bits: u64,
) -> RuntimeStubResultPair {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let result = ctx.runtime_call().and_then(|mut runtime| {
        runtime.load_element_value(Value::from_bits(receiver_bits), Value::from_bits(key_bits))
    });
    match result {
        Ok(value) => {
            RuntimeStubResultPair::from_result(RuntimeStubResult::ok_bits(value.to_bits()))
        }
        Err(err) => {
            park_jit_error(ctx, err);
            RuntimeStubResultPair::from_result(RuntimeStubResult::thrown())
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

/// SSA value form of a captured-binding read.
pub(crate) extern "C" fn jit_load_upvalue_value_stub(
    ctx: *mut JitCtx,
    idx: u64,
    _reserved0: u64,
    _reserved1: u64,
    _reserved2: u64,
) -> JitRet {
    // SAFETY: the live `JitCtx` entry contract.
    let ctx = unsafe { &mut *ctx };
    match ctx
        .runtime_call()
        .and_then(|call| call.load_upvalue_value(idx as u32 as i32))
    {
        Ok(value) => JitRet {
            value: value.to_bits(),
            status: STATUS_RETURNED,
        },
        Err(error) => {
            park_jit_error(ctx, error);
            JitRet {
                value: 0,
                status: STATUS_THREW,
            }
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

/// Complete computed `[[Set]]` from fixed boxed-value operands.
///
/// Success returns `undefined`. Failure parks the error and returns `Throw`;
/// the committed operation is never replayed or converted into a deopt miss.
pub(crate) extern "C" fn jit_store_element_stub(
    ctx: *mut JitCtx,
    receiver_bits: u64,
    key_bits: u64,
    value_bits: u64,
) -> RuntimeStubResultPair {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let result = ctx.runtime_call().and_then(|mut runtime| {
        runtime.store_element_value(
            Value::from_bits(receiver_bits),
            Value::from_bits(key_bits),
            Value::from_bits(value_bits),
        )
    });
    match result {
        Ok(()) => RuntimeStubResultPair::from_result(RuntimeStubResult::ok_bits(
            Value::undefined().to_bits(),
        )),
        Err(err) => {
            park_jit_error(ctx, err);
            RuntimeStubResultPair::from_result(RuntimeStubResult::thrown())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm::{
        RuntimeStubStatus, VmError,
        native_abi::{NativeFrame, NativeFrameKind, VmFrameHeader, VmThread},
    };

    #[test]
    fn named_property_entries_use_fixed_pair_abi_and_park_boundary_errors() {
        let _load_abi: extern "C" fn(
            *mut JitCtx,
            u64,
            *mut WhiskerIcCell,
        ) -> RuntimeStubResultPair = jit_load_property_stub;
        let _store_abi: extern "C" fn(
            *mut JitCtx,
            u64,
            u64,
            *mut WhiskerIcCell,
        ) -> RuntimeStubResultPair = jit_store_property_stub;

        let mut registers = [Value::undefined()];
        let mut frame = NativeFrame::new(
            VmFrameHeader {
                function_id: 0,
                code_block_id: 0,
                pc: 0,
                register_count: 1,
                kind: NativeFrameKind::Optimizing,
                flags: Default::default(),
            },
            registers.as_mut_ptr() as u64,
            Value::function(0),
            Value::undefined(),
        );
        frame.set_stack_registers();
        let mut thread = VmThread::empty();
        thread.current_frame = std::ptr::addr_of_mut!(frame) as u64;
        let mut error = None;
        let mut ctx = JitCtx {
            thread: std::ptr::addr_of_mut!(thread),
            native_frame: std::ptr::addr_of_mut!(frame),
            error: std::ptr::addr_of_mut!(error),
            activation_base: std::ptr::null_mut(),
            activation_top_ptr: std::ptr::null_mut(),
            activation_limit: 0,
            global_this_offset: std::ptr::null(),
            native_stack_limit: 0,
            generated_feedback_clean: 1,
            machine_roots_ptr: std::ptr::null_mut(),
            receiver_alloc: otter_vm::jit::JitMachineAllocationWindow::disabled(),
            runtime_stats: std::ptr::null_mut(),
        };
        let mut cell = WhiskerIcCell::default();

        let loaded = jit_load_property_stub(&mut ctx, Value::undefined().to_bits(), &mut cell);
        assert_eq!(loaded.status(), RuntimeStubStatus::Throw);
        assert!(matches!(error, Some(VmError::InvalidOperand)));

        error = None;
        let stored = jit_store_property_stub(
            &mut ctx,
            Value::undefined().to_bits(),
            Value::number_i32(33).to_bits(),
            &mut cell,
        );
        assert_eq!(stored.status(), RuntimeStubStatus::Throw);
        assert!(matches!(error, Some(VmError::InvalidOperand)));
    }

    #[test]
    fn named_property_cell_fill_keeps_metadata_and_publishes_guard() {
        let mut cell = WhiskerIcCell::default();
        // SAFETY: `cell` is a live stable cell owned by this test.
        unsafe {
            whisker_ic_fill(
                &mut cell,
                otter_vm::JitPropertyIcWay {
                    receiver_shape: 17,
                    holder_shape: 23,
                    value_byte: 40,
                    transition_shape: 29,
                },
            );
        }
        assert_eq!(cell.ways[0].value_byte, 40);
        assert_eq!(cell.ways[0].holder_shape, 23);
        assert_eq!(cell.ways[0].transition_shape, 29);
        assert_eq!(cell.ways[0].shape, 17);
    }
}
