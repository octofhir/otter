//! Atomic preparation and publication of heap-only seal/freeze mutations.
//!
//! # Contents
//! - `apply` prepares the final dictionary state and descriptor tables.
//! - Existing dictionary metadata uses the common descriptor retirement owner.
//!
//! # Invariants
//! - Real receiver roots cover every allocation and typed OOM return.
//! - All required shapes/tables exist before observable state publication.
//! - One ordinary-to-dictionary mutation assigns one fresh structural identity.
//! - Watches retire before descriptor writes; state-only variants keep epochs.
//! - Repeated completed integrity operations allocate nothing and preserve IDs.
//!
//! # See also
//! - `super::descriptor_mutation` owns descriptor-change retirement and writes.
//! - `super::state_transition` owns immutable same-geometry state preparation.

use super::{
    JsObject,
    descriptor_mutation::{self, IntegrityLevel},
    shape_body,
};
use crate::rooting::RootScopeExt;
use otter_gc::{GcHeap, HandleScope, RootScope};

pub(super) fn apply(
    object: &mut JsObject,
    heap: &mut GcHeap,
    level: IntegrityLevel,
) -> Result<(), otter_gc::OutOfMemory> {
    let unchanged = match level {
        IntegrityLevel::Sealed => super::is_sealed(*object, heap),
        IntegrityLevel::Frozen => super::is_frozen(*object, heap),
    };
    if unchanged {
        return Ok(());
    }
    let mut roots = RootScope::new(heap);
    // SAFETY: the actual caller receiver slot is stationary and outlives both
    // preparation and publication, including any cap-triggered full GC.
    unsafe {
        roots.add_object(object);
    }
    // SAFETY: the collector handle arena outlives this cold operation; no Local
    // escapes, and every returned old handle is published without allocation.
    let scope = unsafe { HandleScope::from_ptr(heap.handle_stack_ptr()) };
    let source = scope.local(super::shape(*object, heap));
    let dictionary = shape_body::dictionary_of(source.get());
    let target = shape_body::state_of(dictionary).with_extensible(false);
    let shape =
        super::state_transition::prepare_state_shape(heap, dictionary, target, &mut |_| {})?;
    let shape = scope.local(shape);
    let tables = super::prepare_dictionary_tables(object, heap, &mut [])?;
    heap.with_payload(*object, |body| {
        if let Some((slots, keys)) = tables {
            // These metadata copies have the source descriptors. The ordinary
            // shape still owns lookup while they are installed; no safepoint or
            // user code observes this nonallocating publication transaction.
            body.exotic_mut().slots = slots;
            body.exotic_mut().dictionary_keys = keys;
        }
        descriptor_mutation::apply_integrity_level(body, level);
        if tables.is_some() {
            // Retirement above occurred while the old shape was ordinary, so
            // only this transition mints the final structural identity.
            body.enter_dictionary_mode(false);
        }
        if body.shape != shape.get() {
            body.invalidate_prototype_proofs();
            body.shape = shape.get();
        }
    });
    let sidecar = heap.read_payload(*object, |body| body.exotic.get());
    if let Some((slots, keys)) = tables {
        heap.record_write(sidecar, &slots);
        heap.record_write(sidecar, &keys);
    }
    heap.record_write(*object, &shape.get());
    Ok(())
}
