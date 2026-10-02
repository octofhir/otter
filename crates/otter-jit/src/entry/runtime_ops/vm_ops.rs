//! Property, element, binding, and collection VM slow paths.
//!
//! # Contents
//! - Immutable property source cells and committed cold handlers.
//! - Effect-once boxed-span method-call completion.
//! - Element/global/object runtime operations.
//!
//! # Invariants
//! Register-index operands address the published JIT window. Computed element
//! and named-property entries instead receive fixed boxed-value operands and
//! return a committed value/exception pair. Method calls copy their complete receiver/argument
//! packet before reentry. Named operations derive their immutable property
//! name and feedback site from the immutable source identity in their source cell;
//! the published activation supplies execution ownership, not property lookup.
//! Property source cells contain identity only; semantic CacheIR is transpiled
//! before allocation and cannot be patched into generated code. Inline frame recipes belong to the code-owned
//! safepoint record selected by the current precise root publication. They
//! publish descendants only on cold reentry and normalize exceptions before those frames are removed.
//! Allocating or throwing operations keep precise
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

/// Immutable source identity for one compiled named-property site.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct PropertySourceCell {
    source: Option<(u32, u32)>,
}

impl PropertySourceCell {
    /// Set once during emission, before the code object is published.
    pub(crate) fn set_source(&mut self, function_id: u32, instruction_pc: u32) {
        self.source = Some((function_id, instruction_pc));
    }
}

/// Complete the source-owned `LoadProperty` from one boxed receiver.
///
/// The code-owned cell supplies function/logical-PC identity; the VM validates
/// the opcode and derives its property name and feedback site. Success returns
/// the loaded value without mutating compiled proof state. Failure returns either a pure
/// JavaScript exception or a structural Fatal; this boundary never requests
/// replay or exact deoptimization.
pub(crate) extern "C" fn jit_load_property_stub(
    ctx: *mut JitCtx,
    receiver_bits: u64,
    cell: *mut PropertySourceCell,
) -> NativeResultPair {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    // SAFETY: generated code supplies its live code-owned cell, or null for
    // an invalid boundary invocation. Copy the source before possible reentry.
    let source = unsafe { cell.as_ref() }.and_then(|cell| cell.source);
    let result = (|| {
        let (function_id, pc) = source.ok_or(VmError::InvalidOperand)?;
        // The site's own function and PC name the operation; inline parents
        // stay in the call site's recipe, which stack walks read.
        let mut call = ctx.runtime_call()?;
        match call.load_property_value(function_id, pc, Value::from_bits(receiver_bits)) {
            Ok(value) => Ok(NativeResultPair::success(value)),
            Err(error) => call.take_js_throw(error).map(NativeResultPair::throw_value),
        }
    })();
    match result {
        Ok(pair) => pair,
        Err(err) => committed_vm_result(ctx, Err(err)),
    }
}

/// Complete the exact published `StoreProperty` from boxed receiver/value
/// operands.
///
/// A successful return means the full store committed once; a setter/proxy
/// exception is returned as a pure exception value without replay.
pub(crate) extern "C" fn jit_store_property_stub(
    ctx: *mut JitCtx,
    receiver_bits: u64,
    value_bits: u64,
    cell: *mut PropertySourceCell,
) -> NativeResultPair {
    // SAFETY: as `jit_load_property_stub`.
    let ctx = unsafe { &mut *ctx };
    // SAFETY: the same stable code-owned cell contract as the load boundary.
    let source = unsafe { cell.as_ref() }.and_then(|cell| cell.source);
    let result = (|| {
        let (function_id, pc) = source.ok_or(VmError::InvalidOperand)?;
        let mut call = ctx.runtime_call()?;
        match call.store_property_value(
            function_id,
            pc,
            Value::from_bits(receiver_bits),
            Value::from_bits(value_bits),
        ) {
            Ok(()) => Ok(NativeResultPair::success(Value::undefined())),
            Err(error) => call.take_js_throw(error).map(NativeResultPair::throw_value),
        }
    })();
    match result {
        Ok(pair) => pair,
        Err(err) => committed_vm_result(ctx, Err(err)),
    }
}

/// Resolve the callable of the published `CallMethodValue` from `[receiver]`.
///
/// The generated caller enters the call trampoline with the returned method;
/// resolution may run getters but never calls the method.
pub(crate) extern "C" fn jit_resolve_method_stub(
    ctx: *mut JitCtx,
    packet: *const Value,
    count: u32,
) -> NativeResultPair {
    let receiver = if count != 1
        || packet.is_null()
        || !(packet as usize).is_multiple_of(std::mem::align_of::<Value>())
    {
        Err(VmError::InvalidOperand)
    } else {
        // SAFETY: generated code passes one live, aligned receiver word, read
        // before any runtime operation begins.
        Ok(unsafe { *packet })
    };
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let result = receiver.and_then(|receiver| {
        ctx.runtime_call()
            .and_then(|mut runtime| runtime.resolve_method_values(&[receiver]))
    });
    committed_vm_result(ctx, result)
}

/// Stage a dense spread array's elements as the pending request's actuals.
pub(crate) extern "C" fn jit_stage_spread_stub(ctx: *mut JitCtx, array: u64) -> NativeResultPair {
    // SAFETY: the live `JitCtx` reentry contract.
    let ctx = unsafe { &mut *ctx };
    let result = ctx
        .stage_spread_arguments(Value::from_bits(array))
        .map(|()| Value::undefined());
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
            Frame, NativeFrameKind, NativeResultDomain, NativeResultStatus, VmFrameHeader, VmThread,
        },
    };

    #[test]
    fn named_property_entries_use_fixed_committed_pair_abi() {
        let _load_abi: extern "C" fn(
            *mut JitCtx,
            u64,
            *mut PropertySourceCell,
        ) -> NativeResultPair = jit_load_property_stub;
        let _store_abi: extern "C" fn(
            *mut JitCtx,
            u64,
            u64,
            *mut PropertySourceCell,
        ) -> NativeResultPair = jit_store_property_stub;

        let mut registers = [Value::undefined()];
        let mut frame = Frame::new(
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

        let mut thread = VmThread::empty();
        let mut error = None;
        let mut ctx = JitCtx {
            thread: std::ptr::addr_of_mut!(thread),
            native_frame: std::ptr::addr_of_mut!(frame),
            error: std::ptr::addr_of_mut!(error),
            generated_depth_limit: u64::MAX,
            global_this_offset: std::ptr::null(),
            native_stack_limit: 0,
            generated_feedback_clean: 1,
            completion_destination: u32::MAX,
            completion_generation: 0,
            pending_call: otter_vm::native_abi::CallRequest::EMPTY,
            completion: otter_vm::native_abi::NativeResultPair::success(otter_vm::Value::UNDEFINED),
            alloc_window: otter_vm::jit::JitMachineAllocationWindow::disabled(),
            runtime_stats: std::ptr::null_mut(),
        };
        unsafe { (*ctx.thread).frame_cell = std::ptr::addr_of_mut!(ctx.native_frame) as u64 };
        let mut cell = PropertySourceCell::default();

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

}
