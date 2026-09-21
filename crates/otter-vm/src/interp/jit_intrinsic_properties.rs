//! Immutable CacheIR programs for canonical exotic prototype data loads.
//!
//! # Contents
//! - Collection receiver proofs composed with ordinary slot guards.
//!
//! # Invariants
//! - Only own data slots of pinned realm prototypes are captured.
//! - Existing rooted shape preparation precedes every published shape guard;
//!   an unprepared or allocation-failed holder never contributes a program.
//! - Collection instance overrides cannot bypass their canonical lookup semantics.
//! - The `size` property remains on its existing specialized path.
//! - The program reads the live slot; it never substitutes a builtin callable.
//! - Every miss precedes effects and uses the existing committed property call.
//!
//! # See also
//! - `crate::cache_ir` — ordinary shape/atom/field programs.
//! - `crate::method_ops::jit_snapshot` — the shared intrinsic receiver proof.

use crate::jit::{JitCacheIrOp, JitCacheIrProgram, JitIntrinsicPrototype};
use crate::{Interpreter, object, property_atom::AtomizedPropertyKey};

impl Interpreter {
    pub(crate) fn jit_intrinsic_property_programs(
        &mut self,
        key: AtomizedPropertyKey<'_>,
    ) -> Vec<JitCacheIrProgram> {
        if key.name() == "size" {
            return Vec::new();
        }
        let candidates = [
            (
                self.realm_intrinsics.map_prototype(),
                crate::collections::MAP_BODY_TYPE_TAG,
                Some(crate::method_ops::collection_guard()),
            ),
            (
                self.realm_intrinsics.set_prototype(),
                crate::collections::SET_BODY_TYPE_TAG,
                Some(crate::method_ops::collection_guard()),
            ),
        ];
        candidates
            .into_iter()
            .filter_map(|(prototype, type_tag, guard)| {
                let mut prototype = prototype?;
                if !object::supports_fast_property_ic(prototype, &self.gc_heap) {
                    return None;
                }
                if !matches!(
                    object::lookup_own_slot(prototype, &self.gc_heap, key.name()).1,
                    object::PropertyLookup::Data { .. }
                ) {
                    return None;
                }
                self.migrate_slow_to_fast(&mut prototype);
                let shape = object::shape(prototype, &self.gc_heap);
                if shape.is_null() {
                    return None;
                }
                let (hit, lookup) = object::lookup_own_slot(prototype, &self.gc_heap, key.name());
                let hit = hit?;
                if !matches!(lookup, object::PropertyLookup::Data { .. }) {
                    return None;
                }
                let value_byte = u32::from(hit.slot) * std::mem::size_of::<crate::Value>() as u32;
                Some(JitCacheIrProgram {
                    ops: Box::new([
                        JitCacheIrOp::LoadIntrinsicPrototype {
                            object: 0,
                            result: 1,
                            target: JitIntrinsicPrototype {
                                type_tag,
                                guard,
                                proto_offset: prototype.offset(),
                            },
                        },
                        JitCacheIrOp::GuardShape {
                            object: 1,
                            shape: shape.offset(),
                        },
                        JitCacheIrOp::GuardAtomSlot {
                            object: 1,
                            atom: key.atom().id().raw(),
                            value_byte,
                            writable: false,
                        },
                        JitCacheIrOp::LoadField {
                            object: 1,
                            value_byte,
                        },
                    ]),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::property_atom::{NameInterner, PropertyAtom};

    #[test]
    fn collection_size_has_no_intrinsic_property_program() {
        let mut interpreter = Interpreter::new();
        let names = NameInterner::default();
        let key = |name: &'static str| {
            AtomizedPropertyKey::new(PropertyAtom::new(names.intern(name)), name)
        };
        let programs = interpreter.jit_intrinsic_property_programs(key("get"));
        assert!(
            !programs.is_empty(),
            "the initialized Map prototype must admit ordinary method slots"
        );
        assert!(
            programs
                .iter()
                .flat_map(|program| program.ops.iter())
                .all(|op| { !matches!(op, JitCacheIrOp::GuardShape { shape: 0, .. }) })
        );
        for prototype in [
            interpreter.realm_intrinsics.map_prototype().unwrap(),
            interpreter.realm_intrinsics.set_prototype().unwrap(),
        ] {
            assert!(object::define_own_property(
                prototype,
                &mut interpreter.gc_heap,
                "size",
                object::PropertyDescriptor::data(crate::Value::number_i32(900), true, false, true),
            ));
            assert!(object::supports_fast_property_ic(
                prototype,
                &interpreter.gc_heap
            ));
            assert!(matches!(
                object::lookup_own_slot(prototype, &interpreter.gc_heap, "size"),
                (Some(_), object::PropertyLookup::Data { .. })
            ));
        }
        assert!(
            interpreter
                .jit_intrinsic_property_programs(key("size"))
                .is_empty()
        );
    }
}
