//! Intrinsic error registry roots and same-process restore completeness.
//!
//! # Contents
//! - Complete eleven-class constructor/prototype/root contracts.
//! - Actual full collection and isolate snapshot restoration.
//!
//! # Invariants
//! - Every observation reloads canonical registry slots after collection.
//! - WebAssembly prototypes are excluded from ErrorData themselves and are
//!   not installed as global identifiers; subclasses retain the original Error.
//! - No raw handle survives a collection in Rust fixture state.
//!
//! # See also
//! - `super::ErrorClassRegistry` owns every traced class slot.

use super::*;
use crate::Interpreter;

fn assert_complete_registry(interp: &Interpreter) {
    let registry = &interp.error_classes;
    let heap = interp.gc_heap();
    let mut roots = 0;
    registry.trace_gc_roots(&mut |_| roots += 1);
    assert_eq!(roots, 22, "one constructor/prototype pair per intrinsic");
    assert_eq!(ErrorKind::all().len(), 11);
    let mut identities = std::collections::BTreeSet::new();
    for &kind in ErrorKind::all() {
        let constructor = registry.constructor(kind);
        let prototype = registry.prototype(kind);
        assert!(identities.insert(constructor.offset()));
        assert!(identities.insert(prototype.offset()));
        assert_eq!(
            object::get(constructor, heap, "prototype"),
            Some(Value::object(prototype)),
            "{kind:?} constructor owns its current prototype"
        );
        assert_eq!(
            object::get(prototype, heap, "constructor"),
            Some(Value::object(constructor)),
            "{kind:?} prototype owns its current constructor"
        );
        let name = object::get(prototype, heap, "name")
            .expect("intrinsic prototype name")
            .as_string(heap)
            .expect("intrinsic prototype name string")
            .to_lossy_string(heap);
        assert_eq!(name, kind.class_name());
        assert!(
            !crate::object_statics::object_has_error_data_value(prototype, interp),
            "{kind:?} prototype does not itself carry ErrorData"
        );
        if kind != ErrorKind::Error {
            assert_eq!(
                object::prototype(constructor, heap),
                Some(registry.constructor(ErrorKind::Error)),
                "{kind:?} intrinsic constructor inherits original Error"
            );
            assert_eq!(
                object::prototype(prototype, heap),
                Some(registry.prototype(ErrorKind::Error)),
                "{kind:?} intrinsic prototype inherits original Error.prototype"
            );
        }
    }
    for kind in [
        ErrorKind::WasmCompileError,
        ErrorKind::WasmLinkError,
        ErrorKind::WasmRuntimeError,
    ] {
        assert_eq!(ErrorKind::from_class_name(kind.class_name()), None);
        assert_eq!(
            object::get(*interp.global_this(), heap, kind.class_name()),
            None,
            "WebAssembly class is a namespace intrinsic, not a global binding"
        );
    }
}

#[test]
fn complete_intrinsic_registry_survives_collection_and_snapshot_restore() {
    let snapshot = {
        let mut donor = Interpreter::new().expect("intrinsic bootstrap");
        assert_complete_registry(&donor);
        donor.force_gc().expect("real collection before capture");
        assert_complete_registry(&donor);
        donor
            .capture_isolate_snapshot()
            .expect("actual isolate capture")
    };
    let mut restored =
        Interpreter::from_isolate_snapshot(&snapshot, &otter_resource::ResourceAccount::default())
            .expect("actual isolate restore");
    assert_complete_registry(&restored);
    restored.force_gc().expect("real restored collection");
    assert_complete_registry(&restored);
}
