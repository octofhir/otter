//! Property, element, binding, and collection VM slow paths.
//!
//! # Contents
//! - Self-patching property IC cells and miss handlers.
//! - Effect-once boxed-span method-call completion.
//! - Element/global/upvalue/object runtime operations.
//!
//! # Invariants
//! Register-index operands address the published JIT window. Computed element
//! and named-property entries instead receive fixed boxed-value operands and
//! return a committed value/exception pair. Method calls copy their complete receiver/argument
//! packet before reentry. Named operations derive their immutable property
//! name and feedback site from the immutable source identity in their IC cell;
//! the published activation supplies execution ownership, not property lookup.
//! IC cells store the VM-owned `JitPropertyIcWay` directly; the generated stride
//! derives from that same type. Allocating or throwing operations keep precise
//! roots live. Committed
//! JavaScript throws travel in the pair payload; only structural failures use
//! the shared error slot.
//!
//! # See also
//! - `otter_vm::jit_runtime_ops` — safe VM-side implementations.

use super::super::JitCtx;
use super::{committed_vm_result, park_jit_error};
use otter_vm::{
    Value, VmError,
    native_abi::{NativeResultPair, NativeResultStatus},
};

/// Number of shapes a WhiskerIC site caches inline before it is megamorphic and
/// always misses to the stub. Four matches the polymorphism most real sites
/// reach (V8 / JSC use the same width); a bimorphic site (e.g. two object
/// layouts alternating through one loop) then stays fully inline instead of
/// thrashing a single cell.
pub(crate) const IC_WAYS: usize = 4;

/// Byte stride of the VM-owned program, shared by the cell and emitted probes.
pub(crate) const WHISKER_IC_WAY_BYTES: u32 =
    std::mem::size_of::<otter_vm::JitPropertyIcWay>() as u32;

/// WhiskerIC self-patching cell for one named-property site (one per
/// `LoadProperty` / `StoreProperty` op in the compiled function). Emitted code
/// walks the [`IC_WAYS`] ways comparing each `shape` (a `0` shape never matches
/// a live receiver, so empty ways are skipped for free); on a hit it reads the
/// matched way's `value_byte`. On a monomorphic own-data inline-slot miss the
/// stub fills the next empty way, so a poly site caches every shape it sees up
/// to the width. The cell holds stable shape offsets and immutable source ids
/// (no GC pointers), so it needs no tracing. Shape offsets are stable tokens:
/// shapes are immortal and pinned in old space.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct WhiskerIcCell {
    ways: [otter_vm::JitPropertyIcWay; IC_WAYS],
    // Only the cold stub reads this immutable identity. Keep ways first so
    // generated probes retain the shared VM-owned program layout.
    source: Option<(u32, u32)>,
}

impl WhiskerIcCell {
    /// Set once during emission, before the code object is published.
    pub(crate) fn set_source(&mut self, function_id: u32, instruction_pc: u32) {
        self.source = Some((function_id, instruction_pc));
    }
}

/// Self-patch one IC cell with a lowered cache program: fill the first empty
/// way, or evict way 0 when all are full (the site is more polymorphic than the
/// cache is wide). The guard token is invalidated before rewriting an occupied
/// way and published last, so the complete program is installed before its
/// receiver guard becomes live. Patching and probing belong to the same
/// isolate thread; this cell is not a cross-thread synchronization primitive.
///
/// # Safety
/// `cell` must be a valid, stable [`WhiskerIcCell`] pointer (a site's cell from
/// the owning code object's backing slice).
unsafe fn whisker_ic_fill(cell: *mut WhiskerIcCell, way: otter_vm::JitPropertyIcWay) {
    unsafe {
        let ways = &mut (*cell).ways;
        let slot = ways
            .iter()
            .position(|w| w.receiver_shape == 0 || w.receiver_shape == way.receiver_shape)
            .unwrap_or(0);
        ways[slot].receiver_shape = 0;
        ways[slot].value_byte = way.value_byte;
        ways[slot].holder_shape = way.holder_shape;
        ways[slot].transition_shape = way.transition_shape;
        ways[slot].chain_shape = way.chain_shape;
        ways[slot].prototype_guard = way.prototype_guard;
        ways[slot].receiver_shape = way.receiver_shape;
    }
}

/// Complete the source-owned `LoadProperty` from one boxed receiver.
///
/// The code-owned cell supplies function/logical-PC identity; the VM validates
/// the opcode and derives its property name and feedback site. Success returns
/// the loaded value and may patch `cell`. Failure returns either a pure
/// JavaScript exception or a structural Fatal; this boundary never requests
/// replay or exact deoptimization.
pub(crate) extern "C" fn jit_load_property_stub(
    ctx: *mut JitCtx,
    receiver_bits: u64,
    cell: *mut WhiskerIcCell,
) -> NativeResultPair {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    // SAFETY: generated code supplies its live code-owned cell, or null for
    // an invalid boundary invocation. Copy the source before possible reentry.
    let source = unsafe { cell.as_ref() }.and_then(|cell| cell.source);
    let result = source
        .ok_or(VmError::InvalidOperand)
        .and_then(|(function_id, pc)| {
            ctx.runtime_call()?.load_property_value(
                function_id,
                pc,
                Value::from_bits(receiver_bits),
            )
        });
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
            NativeResultPair::success(value)
        }
        Err(err) => committed_vm_result(ctx, Err(err)),
    }
}

/// Complete the exact published `StoreProperty` from boxed receiver/value
/// operands.
///
/// A successful return means the full store committed once and may patch
/// `cell`; a setter/proxy exception is returned as a pure exception value
/// without replay.
pub(crate) extern "C" fn jit_store_property_stub(
    ctx: *mut JitCtx,
    receiver_bits: u64,
    value_bits: u64,
    cell: *mut WhiskerIcCell,
) -> NativeResultPair {
    // SAFETY: as `jit_load_property_stub`.
    let ctx = unsafe { &mut *ctx };
    // SAFETY: the same stable code-owned cell contract as the load boundary.
    let source = unsafe { cell.as_ref() }.and_then(|cell| cell.source);
    let result = source
        .ok_or(VmError::InvalidOperand)
        .and_then(|(function_id, pc)| {
            ctx.runtime_call()?.store_property_value(
                function_id,
                pc,
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
            NativeResultPair::success(Value::undefined())
        }
        Err(err) => committed_vm_result(ctx, Err(err)),
    }
}

/// Complete the exact published `CallMethodValue` from boxed SSA values.
///
/// `packet[0]` is the receiver and the remaining `count - 1` values are every
/// actual argument. The packet is copied before binding the VM runtime call so
/// no pointer into generated stack storage survives allocation or JavaScript
/// reentry. The published frame supplies exact function/PC identity, precise
/// roots, and the immutable bytecode declaration of method name and argc.
pub(crate) extern "C" fn jit_call_method_value_stub(
    ctx: *mut JitCtx,
    packet: *const Value,
    count: u32,
) -> NativeResultPair {
    let copied = if count == 0
        || packet.is_null()
        || !(packet as usize).is_multiple_of(std::mem::align_of::<Value>())
    {
        Err(VmError::InvalidOperand)
    } else {
        let count = count as usize;
        // SAFETY: generated code passes one live, naturally aligned span of
        // `count` initialized Value words. `u32 * size_of::<Value>()` fits the
        // addressable-object bound on every supported 64-bit target. Copying
        // ends the machine-memory borrow before any runtime operation begins.
        let values = unsafe { std::slice::from_raw_parts(packet, count) };
        Ok(smallvec::SmallVec::<[Value; 8]>::from_slice(values))
    };

    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let result = copied.and_then(|values| {
        ctx.runtime_call()
            .and_then(|mut runtime| runtime.call_method_values(values.as_slice()))
    });
    committed_vm_result(ctx, result)
}

/// Complete the exact published explicit-receiver call from boxed SSA values.
///
/// `packet[0]` is the callee, `packet[1]` the receiver, and the remaining
/// `count - 2` values are every actual argument; the packet is copied before
/// the VM runtime call is bound, exactly like the method-value entry.
pub(crate) extern "C" fn jit_call_with_this_value_stub(
    ctx: *mut JitCtx,
    packet: *const Value,
    count: u32,
) -> NativeResultPair {
    let copied = if count < 2
        || packet.is_null()
        || !(packet as usize).is_multiple_of(std::mem::align_of::<Value>())
    {
        Err(VmError::InvalidOperand)
    } else {
        let count = count as usize;
        // SAFETY: generated code passes one live, naturally aligned span of
        // `count` initialized Value words; copying ends the machine-memory
        // borrow before any runtime operation begins.
        let values = unsafe { std::slice::from_raw_parts(packet, count) };
        Ok(smallvec::SmallVec::<[Value; 8]>::from_slice(values))
    };

    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let result = copied.and_then(|values| {
        ctx.runtime_call()
            .and_then(|mut runtime| runtime.call_with_this_values(values.as_slice()))
    });
    committed_vm_result(ctx, result)
}

/// Complete the exact published `New` from boxed SSA values.
///
/// `packet[0]` is the constructor and the remaining `count - 1` values are
/// every actual argument; the packet is copied before the VM runtime call is
/// bound, exactly like the method-value entry.
pub(crate) extern "C" fn jit_construct_value_stub(
    ctx: *mut JitCtx,
    packet: *const Value,
    count: u32,
) -> NativeResultPair {
    let copied = if count == 0
        || packet.is_null()
        || !(packet as usize).is_multiple_of(std::mem::align_of::<Value>())
    {
        Err(VmError::InvalidOperand)
    } else {
        let count = count as usize;
        // SAFETY: generated code passes one live, naturally aligned span of
        // `count` initialized Value words; copying ends the machine-memory
        // borrow before any runtime operation begins.
        let values = unsafe { std::slice::from_raw_parts(packet, count) };
        Ok(smallvec::SmallVec::<[Value; 8]>::from_slice(values))
    };

    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let result = copied.and_then(|values| {
        ctx.runtime_call()
            .and_then(|mut runtime| runtime.construct_values(values.as_slice()))
    });
    committed_vm_result(ctx, result)
}

/// Complete computed `[[Get]]` from fixed boxed-value operands.
///
/// The active native frame supplies the exact function/PC feedback identity
/// and precise roots. Success returns the loaded value. Failure returns either
/// a pure JavaScript exception or a structural Fatal; this entry never requests
/// replay or deopt.
pub(crate) extern "C" fn jit_load_element_stub(
    ctx: *mut JitCtx,
    receiver_bits: u64,
    key_bits: u64,
) -> NativeResultPair {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let result = ctx.runtime_call().and_then(|mut runtime| {
        runtime.load_element_value(Value::from_bits(receiver_bits), Value::from_bits(key_bits))
    });
    committed_vm_result(ctx, result)
}

/// Runtime stub: build the activation's `arguments` object for
/// `CollectArguments` from compiled code. A stack-owned frame reads the actual
/// arguments its generated caller published; a materialized activation reads
/// its cold record. Allocating; never re-enters JavaScript.
pub(crate) extern "C" fn jit_collect_arguments_stub(ctx: *mut JitCtx, dst: u64) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let result = ctx
        .runtime_call()
        .and_then(|mut runtime| runtime.collect_arguments(dst as u16));
    match result {
        Ok(()) => NativeResultStatus::Success as u64,
        Err(err) => {
            park_jit_error(ctx, err);
            NativeResultStatus::Throw as u64
        }
    }
}

/// Materialize a regex literal (`Op::LoadRegExp`) into the frame's
/// destination register. Allocating; a bad pattern reports `Throw`.
pub(crate) extern "C" fn jit_load_regexp_stub(ctx: *mut JitCtx, dst: u64, idx: u64) -> u64 {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let mut runtime = match ctx.runtime_call() {
        Ok(runtime) => runtime,
        Err(err) => {
            park_jit_error(ctx, err);
            return NativeResultStatus::Throw as u64;
        }
    };
    let result = runtime.load_regexp(dst as u16, idx as u32);
    match result {
        Ok(()) => NativeResultStatus::Success as u64,
        Err(err) => {
            park_jit_error(ctx, err);
            NativeResultStatus::Throw as u64
        }
    }
}

/// Complete computed `[[Set]]` from fixed boxed-value operands.
///
/// Success returns `undefined`. Failure returns either a pure JavaScript
/// exception or a structural Fatal; the committed operation is never replayed
/// or converted into a deopt miss.
pub(crate) extern "C" fn jit_store_element_stub(
    ctx: *mut JitCtx,
    receiver_bits: u64,
    key_bits: u64,
    value_bits: u64,
) -> NativeResultPair {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let result = ctx.runtime_call().and_then(|mut runtime| {
        runtime.store_element_value(
            Value::from_bits(receiver_bits),
            Value::from_bits(key_bits),
            Value::from_bits(value_bits),
        )
    });
    committed_vm_result(ctx, result.map(|()| Value::undefined()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm::{
        VmError,
        native_abi::{
            NativeFrame, NativeFrameKind, NativeResultDomain, NativeResultStatus, VmFrameHeader,
            VmThread,
        },
    };

    #[test]
    fn named_property_entries_use_fixed_committed_pair_abi() {
        let _load_abi: extern "C" fn(*mut JitCtx, u64, *mut WhiskerIcCell) -> NativeResultPair =
            jit_load_property_stub;
        let _store_abi: extern "C" fn(
            *mut JitCtx,
            u64,
            u64,
            *mut WhiskerIcCell,
        ) -> NativeResultPair = jit_store_property_stub;

        let mut registers = [Value::undefined()];
        let mut frame = NativeFrame::new(
            VmFrameHeader {
                function_id: 0,
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
        assert_eq!(
            loaded.validate(NativeResultDomain::Committed),
            Some(NativeResultStatus::Fatal)
        );
        assert!(matches!(error, Some(VmError::InvalidOperand)));

        error = None;
        let stored = jit_store_property_stub(
            &mut ctx,
            Value::undefined().to_bits(),
            Value::number_i32(33).to_bits(),
            &mut cell,
        );
        assert_eq!(
            stored.validate(NativeResultDomain::Committed),
            Some(NativeResultStatus::Fatal)
        );
        assert!(matches!(error, Some(VmError::InvalidOperand)));
    }

    #[test]
    fn method_call_entry_uses_owned_value_span_pair_abi() {
        let _method_abi: extern "C" fn(*mut JitCtx, *const Value, u32) -> NativeResultPair =
            jit_call_method_value_stub;

        let mut registers = [Value::undefined()];
        let mut frame = NativeFrame::new(
            VmFrameHeader {
                function_id: 0,
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
        let packet = [Value::number_i32(7), Value::number_i32(11)];

        let result = jit_call_method_value_stub(&mut ctx, packet.as_ptr(), packet.len() as u32);
        assert_eq!(
            result.validate(NativeResultDomain::Committed),
            Some(NativeResultStatus::Fatal)
        );
        assert!(matches!(error, Some(VmError::InvalidOperand)));

        error = None;
        let result = jit_call_method_value_stub(&mut ctx, std::ptr::null(), 0);
        assert_eq!(
            result.validate(NativeResultDomain::Committed),
            Some(NativeResultStatus::Fatal)
        );
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
                    chain_shape: 31,
                    prototype_guard: otter_vm::jit::JitPropertyIcPrototypeGuard::WritableData,
                },
            );
        }
        assert_eq!(cell.ways[0].value_byte, 40);
        assert_eq!(cell.ways[0].holder_shape, 23);
        assert_eq!(cell.ways[0].transition_shape, 29);
        assert_eq!(cell.ways[0].chain_shape, 31);
        assert_eq!(cell.ways[0].receiver_shape, 17);
        assert_eq!(
            cell.ways[0].prototype_guard,
            otter_vm::jit::JitPropertyIcPrototypeGuard::WritableData
        );
    }
}
