//! Pure actual-owner family admission and committed source bookkeeping.
//!
//! # Contents
//! - A boxed LeafValue2 Probe over the exact live closure/class owner.
//! - A ContextWords leaf that validates the already published callee ticket.
//!
//! # Invariants
//! Neither entry allocates, collects, calls JavaScript, clones a chain proof or
//! retains an input. The Probe returns a boxed int32 containing the full u32
//! layout offset, borrowed under the actual new.target frame root until the
//! no-GC allocation extent publishes its ticket. Only complete finalized family
//! preparation admits a hit. The family's base FID and current direct prototype
//! prove receiver allocation independently of the new.target static constructor
//! chain. Post-publication failures are Fatal, never replay.
//!
//! # See also
//! - `preparation` owns the sole nonreviving prototype proof.
//! - `crate::call_ops::constructor` owns ticket validation and source feedback.
//! - `crate::native_abi` owns the physical pair and descriptor domains.

use super::ConstructorLayout;
use crate::{
    Value, VmError,
    native_abi::{JitCtx, NativeResultPair},
    object,
};

/// Read the exact actual new.target owner and its current direct prototype.
/// The prepared family proves the entered base identity independently of the
/// new.target callable or static superclass chain. Canonical construction has
/// already admitted the operand; this read observes no property protocol.
fn current_family(heap: &otter_gc::GcHeap, target: Value) -> Option<(ConstructorLayout, Value)> {
    if let Some(closure) = target.as_closure(heap) {
        let (prototype, _) = closure.prototype_slot(heap)?;
        if prototype.as_object().is_none() {
            return None;
        }
        return Some((closure.constructor_layouts(heap), prototype));
    }
    let class = target.as_class_constructor()?;
    Some((
        class.constructor_layouts(heap),
        Value::object(class.prototype(heap)),
    ))
}

/// Private engine Probe: `(heap, actual_new_target, boxed_base_fid)`.
/// The heap and value cells must be rooted by the published caller throughout
/// this synchronous noalloc extent. Null heap and unsupported operands miss.
#[doc(hidden)]
pub extern "C" fn constructor_receiver_probe(
    heap: *const otter_gc::GcHeap,
    target: u64,
    base: u64,
) -> NativeResultPair {
    // SAFETY: generated entry supplies its current heap, retained until return.
    let Some(heap) = (unsafe { heap.as_ref() }) else {
        return NativeResultPair::miss();
    };
    let Some(base) = Value::from_abi_bits(base).as_function() else {
        return NativeResultPair::miss();
    };
    let Some((layout, prototype)) = current_family(heap, Value::from_abi_bits(target)) else {
        return NativeResultPair::miss();
    };
    if layout.is_null() {
        return NativeResultPair::miss();
    }
    let admitted = heap.read_payload(layout, |body| {
        if body.base_function_id() != base { return false; }
        let root = body.root();
        let Some(reserved) = body.prepared_capacity(root, prototype) else { return false; };
        let capacity = object::shape_body::inline_capacity_of(root);
        capacity <= object::MAX_INLINE_CAPACITY
            && reserved <= capacity
            && object::shape_body::property_count_of(root) == 0
            && matches!(object::shape_body::prototype_of(root), object::shape_body::ShapePrototype::Object(owner) if Value::object(owner) == prototype)
    });
    if !admitted {
        return NativeResultPair::miss();
    }
    NativeResultPair::success(Value::number_i32(layout.offset() as i32))
}

/// Private engine commit: the cell, this and exact ticket are already visible
/// through the one published callee frame. Failure parks only the structural
/// scalar in the existing JitCtx error owner; no JavaScript error is synthesized.
///
/// # Safety
/// The context/frame/activation are live and exclusive until this leaf returns.
#[doc(hidden)]
pub unsafe extern "C" fn constructor_receiver_commit(
    ctx: *mut JitCtx,
    receiver: u64,
) -> NativeResultPair {
    let Some(ctx) = (unsafe { ctx.as_mut() }) else {
        return NativeResultPair::fatal_internal();
    };
    let result = (|| {
        let frame = unsafe { ctx.native_frame.as_mut() }.ok_or(VmError::InvalidOperand)?;
        let activation = ctx
            .checked_activation()
            .copied()
            .ok_or(VmError::InvalidOperand)?;
        // The callee's current family/ticket proves its body identity. Source
        // feedback belongs to the suspended caller, so preserve the admitted
        // context as a borrow rather than resolving/cloning a callee owner.
        let context = unsafe { activation.context.as_ref() };
        let vm = unsafe { activation.vm.as_mut() }.ok_or(VmError::InvalidOperand)?;
        vm.note_published_constructor_receiver(context, frame, Value::from_abi_bits(receiver))?;
        vm.release_sampled_construct_ticket(frame);
        Ok(())
    })();
    match result {
        Ok(()) => NativeResultPair::success(Value::from_abi_bits(receiver)),
        Err(error) => {
            if let Some(slot) = unsafe { ctx.error.as_mut() } {
                *slot = Some(error);
            }
            NativeResultPair::fatal_internal()
        }
    }
}

#[cfg(test)]
mod tests;
