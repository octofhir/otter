//! Exact live ownership of lexical DerivedThis contexts.
//!
//! # Contents
//! - Immutable descriptor selection from the actual SELF context chain.
//! - Canonical Frame identity-root matching for arrow and direct-eval super.
//! - Authoritative context-slot hole reads before terminal ticket transfer.
//!
//! # Invariants
//! These helpers cannot allocate, collect, mutate a context or execute JS. A
//! ContextHandle is retained only within this LeafNoAlloc operation. The own
//! CreateContext publisher proves provenance; a parameter, recursive closure or
//! unrelated tagged root with the same scope id cannot claim terminal ownership.
//! Escaped lexical callers with no live owner complete their child locally.
//!
//! # See also
//! - `crate::context_ops` owns the sole descriptor slot-kind selector.
//! - `crate::native_abi::Frame::derived_this_context` is the traced identity root.

use crate::{ExecutionContext, Interpreter, VmError, context::ContextHandle, native_abi::Frame};

fn derived_slot(
    vm: &Interpreter,
    context: &ExecutionContext,
    handle: ContextHandle,
) -> Result<Option<u16>, VmError> {
    let (fid, index) = crate::context::scope_identity(&vm.gc_heap, handle);
    crate::context_ops::with_scope(context, fid, index, crate::context_ops::derived_this_slot)?
}

/// An own constructor may keep this in its frame when no capture exists. When
/// it has a context, the descriptor slot owns the binding even if a lexical
/// arrow/eval performed the first bind without updating Frame.this_value.
pub(super) fn this_is_unbound(
    vm: &Interpreter,
    context: &ExecutionContext,
    frame: &Frame,
) -> Result<bool, VmError> {
    if frame.derived_this_context.is_null() {
        return Ok(frame.this_value.is_hole());
    }
    let handle = frame.derived_this_context;
    let (fid, _) = crate::context::scope_identity(&vm.gc_heap, handle);
    if !frame.is_derived_constructor() || fid != frame.header.function_id {
        return Err(VmError::InvalidOperand);
    }
    let slot = derived_slot(vm, context, handle)?.ok_or(VmError::InvalidOperand)?;
    Ok(crate::context::read_slot(&vm.gc_heap, handle, slot)
        .ok_or(VmError::InvalidOperand)?
        .is_hole())
}

fn lexical_binding(
    vm: &Interpreter,
    context: &ExecutionContext,
    caller: &Frame,
) -> Result<Option<ContextHandle>, VmError> {
    let Some(closure) = caller.self_value.as_closure(&vm.gc_heap) else {
        return Ok(None);
    };
    let mut current = crate::context_ops::context_operand(closure.context(&vm.gc_heap))?;
    while let Some(handle) = current {
        if let Some(slot) = derived_slot(vm, context, handle)? {
            crate::context::read_slot(&vm.gc_heap, handle, slot).ok_or(VmError::InvalidOperand)?;
            return Ok(Some(handle));
        }
        current = crate::context::parent(&vm.gc_heap, handle);
    }
    Ok(None)
}

/// Resolve only after the explicit source's exact semantic opcode proved a
/// SuperConstruct/SuperConstructSpread edge. Return a pointer to the canonical
/// live owner; caller chain storage stays published through this whole leaf.
pub(super) fn owner(
    vm: &Interpreter,
    context: &ExecutionContext,
    caller: *mut Frame,
) -> Result<Option<*mut Frame>, VmError> {
    // SAFETY: terminal dispatch retains this published frame chain. All borrows
    // here finish before the caller mutates/takes a ticket, without GC/reentry.
    let Some(caller_record) = (unsafe { caller.as_ref() }) else {
        return Ok(None);
    };
    if caller_record.is_derived_constructor() {
        return Ok(Some(caller));
    }
    let Some(binding) = lexical_binding(vm, context, caller_record)? else {
        return Ok(None);
    };
    let (fid, _) = crate::context::scope_identity(&vm.gc_heap, binding);
    let mut candidate = caller_record.caller_frame();
    while let Some(frame) = unsafe { candidate.as_ref() } {
        if frame.derived_this_context == binding {
            if !frame.is_derived_constructor() || frame.header.function_id != fid {
                return Err(VmError::InvalidOperand);
            }
            return Ok(Some(candidate));
        }
        candidate = frame.caller_frame();
    }
    Ok(None)
}
