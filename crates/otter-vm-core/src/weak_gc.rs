//! Thread-local tracking for WeakRef targets and FinalizationRegistry instances.
//!
//! WeakRef targets must NOT be stored as GC-traced properties (that would keep them
//! alive, defeating the purpose of weak references). Instead we use an untraced
//! thread-local side table. During the GC pre-sweep phase, dead targets are cleared.
//!
//! FinalizationRegistry instances are also tracked so we can call `sweep_dead_targets()`
//! on each one during pre-sweep, moving dead target entries to the pending queue.

use std::cell::RefCell;

use rustc_hash::FxHashMap;

use crate::gc::GcRef;
use crate::object::{JsObject, PropertyKey};
use crate::value::Value;
use otter_vm_gc::object::MarkColor;
use otter_vm_gc::{FinalizationRegistryData, WeakRefCell};

thread_local! {
    /// Untraced side table: WeakRefCell pointer (as usize) → target Value.
    ///
    /// The Value stored here is NOT traced by the GC. This is intentional:
    /// when the target object has no strong references left, GC marks it White
    /// and we clear the entry in pre-sweep (before the sweep frees the object).
    static WEAK_REF_TARGETS: RefCell<FxHashMap<usize, (GcRef<WeakRefCell>, Value)>> =
        RefCell::new(FxHashMap::default());

    /// All live FinalizationRegistry instances: (data, wrapper JsObject).
    ///
    /// We need the wrapper to read the callback and held-values array.
    static FINALIZATION_REGISTRIES: RefCell<Vec<(GcRef<FinalizationRegistryData>, GcRef<JsObject>)>> =
        const { RefCell::new(Vec::new()) };

    /// Pending cleanup pairs: (callback, held_value).
    ///
    /// Populated during pre-sweep when a FinalizationRegistry target dies.
    /// Drained and executed by the runtime after GC completes.
    static PENDING_CLEANUPS: RefCell<Vec<(Value, Value)>> =
        const { RefCell::new(Vec::new()) };
}

// ============================================================================
// WeakRef side table API
// ============================================================================

/// Register a WeakRef target in the untraced side table.
pub fn register_weak_ref_target(cell: GcRef<WeakRefCell>, target: Value) {
    let key = cell.as_ptr() as usize;
    WEAK_REF_TARGETS.with(|m| m.borrow_mut().insert(key, (cell, target)));
}

/// Look up a WeakRef target from the side table.
///
/// Returns `Some(target)` if the cell is registered and alive, `None` otherwise.
pub fn get_weak_ref_target(cell: &GcRef<WeakRefCell>) -> Option<Value> {
    let key = cell.as_ptr() as usize;
    WEAK_REF_TARGETS.with(|m| m.borrow().get(&key).map(|(_, v)| v.clone()))
}

// ============================================================================
// FinalizationRegistry tracking API
// ============================================================================

/// Register a FinalizationRegistry for sweep tracking.
pub fn register_finalization_registry(
    data: GcRef<FinalizationRegistryData>,
    wrapper: GcRef<JsObject>,
) {
    FINALIZATION_REGISTRIES.with(|v| v.borrow_mut().push((data, wrapper)));
}

/// Drain pending cleanup pairs (callback, held_value).
///
/// Call this after GC completes to get callbacks that need to be invoked.
pub fn drain_pending_cleanups() -> Vec<(Value, Value)> {
    PENDING_CLEANUPS.with(|v| std::mem::take(&mut *v.borrow_mut()))
}

/// Check if there are pending FinalizationRegistry cleanups.
pub fn has_pending_cleanups() -> bool {
    PENDING_CLEANUPS.with(|v| !v.borrow().is_empty())
}

// ============================================================================
// GC pre-sweep hook
// ============================================================================

/// Pre-sweep hook for WeakRef and FinalizationRegistry.
///
/// Called between GC mark and sweep phases, when mark colors are valid but
/// dead objects are still in memory.
///
/// # Safety
/// Must be called during the GC pre-sweep phase when mark bits are valid.
unsafe fn pre_sweep_weak_refs() {
    // 1. Clear dead WeakRef targets from the side table
    WEAK_REF_TARGETS.with(|m| {
        m.borrow_mut().retain(|_key, (cell, _value)| {
            // If the WeakRefCell itself is dead, remove entry
            if cell.header().mark() == MarkColor::White {
                return false;
            }

            // WeakRefCell is alive — check if its target is alive
            if let Some(target_header_ptr) = cell.target() {
                let target_header = unsafe { &*target_header_ptr };
                if target_header.mark() == MarkColor::White {
                    // Target is dead — clear the weak reference
                    cell.clear();
                    return false;
                }
            } else {
                // Cell already cleared (shouldn't happen, but clean up)
                return false;
            }

            true
        });
    });

    // 2. Sweep FinalizationRegistries and collect pending cleanups
    FINALIZATION_REGISTRIES.with(|v| {
        let mut registries = v.borrow_mut();

        registries.retain(|(data, wrapper)| {
            // If the registry wrapper itself is dead, remove
            if wrapper.header().mark() == MarkColor::White {
                return false;
            }

            // If the data object is dead, remove
            if data.header().mark() == MarkColor::White {
                return false;
            }

            // Sweep dead targets — moves their indices to pending queue
            unsafe { data.sweep_dead_targets() };

            // Drain pending indices and collect cleanup pairs
            if data.has_pending() {
                let pending_indices = data.drain_pending();

                // Get callback from wrapper
                let callback = wrapper
                    .get(&PropertyKey::string("__finreg_callback__"))
                    .unwrap_or_else(Value::undefined);

                // Get held values array from wrapper
                let held_arr = wrapper
                    .get(&PropertyKey::string("__finreg_held__"))
                    .and_then(|v| v.as_array());

                for idx in pending_indices {
                    let held_value = held_arr
                        .as_ref()
                        .and_then(|arr| arr.get(&PropertyKey::Index(idx)))
                        .unwrap_or_else(Value::undefined);

                    PENDING_CLEANUPS.with(|c| c.borrow_mut().push((callback.clone(), held_value)));
                }
            }

            true
        });
    });
}

/// Combined pre-sweep hook: prune dead string table entries + weak ref/finreg sweep.
///
/// This replaces the standalone `prune_dead_string_table_entries` in all GC call sites.
pub fn combined_pre_sweep_hook() {
    // 1. Prune dead strings from the intern table (existing behavior)
    crate::string::prune_dead_string_table_entries();

    // 2. Clear dead WeakRefs and sweep FinalizationRegistries
    // SAFETY: called from GC pre-sweep phase where mark bits are valid
    unsafe { pre_sweep_weak_refs() };
}

/// Clear all thread-local weak GC state.
///
/// Called during VM shutdown to prevent dangling pointers.
pub fn clear_weak_gc_state() {
    WEAK_REF_TARGETS.with(|m| m.borrow_mut().clear());
    FINALIZATION_REGISTRIES.with(|v| v.borrow_mut().clear());
    PENDING_CLEANUPS.with(|v| v.borrow_mut().clear());
}
