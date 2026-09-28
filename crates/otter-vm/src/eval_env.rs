//! Eval extensions: the bindings a sloppy direct eval creates at run time.
//!
//! §19.2.1.3 EvalDeclarationInstantiation — a sloppy direct eval declares its
//! `var` and function bindings in the caller's VariableEnvironment, and they
//! are deletable (`CreateMutableBinding(vn, true)`). Every other binding has a
//! fixed context slot; these cannot, because their names are known only when
//! the eval runs. They live in an extension hung off the var-scope context
//! (or the callee context of a parameter-expression function) whose scope
//! descriptor sets `has_extension`.
//!
//! # Contents
//! - [`EvalExtensionBody`] — GC body holding the names and values.
//! - [`EvalExtensionHandle`] — its compressed handle.
//! - Lookup, insert, set-or-create, and delete helpers used by the `Lookup*`,
//!   `ResolveLookupRef` / `StoreRef`, `DeclareEvalVar`, and `StoreVarScope`
//!   kernels.
//!
//! # Invariants
//! - `names[i]` labels `values[i]`; names are unique, and deletion compacts
//!   both vectors in lockstep. No compiled code holds a positional index:
//!   every access is by name.
//! - Every value store records the write barrier on the extension.
//! - An extension holds no parent, no cells, and no sequence numbers: scope
//!   order comes from the context chain that owns it.
//! - An extension owns Rust-heap vectors, so a heap image cannot carry one;
//!   isolate snapshot capture refuses while any is live.
//!
//! # See also
//! - [`crate::context`] — the contexts that own extensions and the probe.
//! - [`crate::eval_ops`] — direct eval and the caller chain it builds.

use otter_gc::GcHeap;
use otter_gc::OutOfMemory;
use otter_gc::heap::RootSlotVisitor;
use otter_macros::Pelt;

use crate::Value;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`EvalExtensionBody`].
pub const EVAL_EXTENSION_BODY_TYPE_TAG: u8 = 0x2E;

/// GC body of one scope's eval extension.
#[derive(Debug, Pelt)]
#[pelt(tag = EVAL_EXTENSION_BODY_TYPE_TAG)]
pub struct EvalExtensionBody {
    /// Binding names, parallel to `values`. Plain Rust strings, not GC slots.
    #[pelt(skip)]
    pub names: Vec<String>,
    /// Binding values, traced in place.
    pub values: Vec<Value>,
}

impl EvalExtensionBody {
    fn position(&self, name: &str) -> Option<usize> {
        self.names.iter().position(|candidate| candidate == name)
    }

    pub(crate) fn visit_function_ids(&self, visitor: &mut dyn FnMut(u32)) {
        for value in &self.values {
            crate::code_liveness::visit_value(value, visitor);
        }
    }
}

/// 4-byte compressed GC handle.
pub type EvalExtensionHandle = otter_gc::Gc<EvalExtensionBody>;

/// Allocate an empty extension.
///
/// # Errors
/// Surfaces [`OutOfMemory`] verbatim.
pub(crate) fn alloc_extension_with_roots(
    heap: &mut GcHeap,
    external_visit: &mut RootSlotVisitor<'_>,
) -> Result<EvalExtensionHandle, OutOfMemory> {
    heap.alloc_with_roots(
        EvalExtensionBody {
            names: Vec::new(),
            values: Vec::new(),
        },
        external_visit,
    )
}

/// Whether `extension` binds `name`.
#[must_use]
pub(crate) fn extension_has(heap: &GcHeap, extension: EvalExtensionHandle, name: &str) -> bool {
    heap.read_payload(extension, |body| body.position(name).is_some())
}

/// The value `extension` binds to `name`.
#[must_use]
pub(crate) fn extension_get(
    heap: &GcHeap,
    extension: EvalExtensionHandle,
    name: &str,
) -> Option<Value> {
    heap.read_payload(extension, |body| {
        body.position(name).map(|i| body.values[i])
    })
}

/// Overwrite an existing binding. Returns `false` when `name` is absent.
pub(crate) fn extension_set_existing(
    heap: &mut GcHeap,
    extension: EvalExtensionHandle,
    name: &str,
    value: Value,
) -> bool {
    let stored = heap.with_payload(extension, |body| match body.position(name) {
        Some(index) => {
            body.values[index] = value;
            true
        }
        None => false,
    });
    if stored {
        heap.record_write(extension, &value);
    }
    stored
}

/// Set `name` to `value`, creating the binding when absent.
pub(crate) fn extension_set_or_insert(
    heap: &mut GcHeap,
    extension: EvalExtensionHandle,
    name: &str,
    value: Value,
) {
    heap.with_payload(extension, |body| match body.position(name) {
        Some(index) => body.values[index] = value,
        None => {
            body.names.push(name.to_owned());
            body.values.push(value);
        }
    });
    heap.record_write(extension, &value);
}

/// Create `name = undefined` unless the extension already binds it. Returns
/// whether a binding was created.
pub(crate) fn extension_insert_absent(
    heap: &mut GcHeap,
    extension: EvalExtensionHandle,
    name: &str,
) -> bool {
    heap.with_payload(extension, |body| {
        if body.position(name).is_some() {
            return false;
        }
        body.names.push(name.to_owned());
        body.values.push(Value::undefined());
        true
    })
}

/// Remove `name`. Returns whether a binding was removed.
pub(crate) fn extension_remove(
    heap: &mut GcHeap,
    extension: EvalExtensionHandle,
    name: &str,
) -> bool {
    heap.with_payload(extension, |body| match body.position(name) {
        Some(index) => {
            body.names.remove(index);
            body.values.remove(index);
            true
        }
        None => false,
    })
}

/// Names currently bound, in insertion order.
#[must_use]
pub(crate) fn extension_names(heap: &GcHeap, extension: EvalExtensionHandle) -> Vec<String> {
    heap.read_payload(extension, |body| body.names.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bindings_insert_set_and_delete_by_name() {
        let mut heap = GcHeap::new().expect("heap");
        let extension = alloc_extension_with_roots(&mut heap, &mut |_| {}).expect("extension");
        assert!(!extension_has(&heap, extension, "x"));
        assert!(extension_insert_absent(&mut heap, extension, "x"));
        assert!(!extension_insert_absent(&mut heap, extension, "x"));
        assert_eq!(
            extension_get(&heap, extension, "x"),
            Some(Value::undefined())
        );
        assert!(extension_set_existing(
            &mut heap,
            extension,
            "x",
            Value::number_i32(7)
        ));
        assert!(!extension_set_existing(
            &mut heap,
            extension,
            "y",
            Value::number_i32(8)
        ));
        extension_set_or_insert(&mut heap, extension, "y", Value::number_i32(9));
        assert_eq!(extension_names(&heap, extension), vec!["x", "y"]);
        assert!(extension_remove(&mut heap, extension, "x"));
        assert!(!extension_remove(&mut heap, extension, "x"));
        assert_eq!(extension_get(&heap, extension, "x"), None);
        assert_eq!(
            extension_get(&heap, extension, "y"),
            Some(Value::number_i32(9))
        );
    }

    #[test]
    fn extension_values_survive_a_scavenge() {
        let mut heap = GcHeap::new().expect("heap");
        let extension = alloc_extension_with_roots(&mut heap, &mut |_| {}).expect("extension");
        let object =
            crate::object::alloc_object_with_roots(&mut heap, &mut |_| {}).expect("young object");
        extension_set_or_insert(&mut heap, extension, "o", Value::object(object));
        let mut root = Value::eval_extension(extension);
        let slot: *mut Value = &mut root;
        let mut roots = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
            // SAFETY: `root` outlives the collection.
            unsafe { (*slot).trace_value_slot_mut(visitor) };
        };
        heap.collect_minor_with_roots(&mut roots).expect("scavenge");
        let extension = root.as_eval_extension().expect("extension survives");
        let held = extension_get(&heap, extension, "o")
            .and_then(Value::as_object)
            .expect("value survives");
        assert_eq!(
            heap.debug_header_tag(held),
            Some(crate::object::OBJECT_BODY_TYPE_TAG)
        );
    }
}
