//! Immutable CacheIR programs for canonical exotic prototype data loads.
//!
//! # Contents
//! - Collection receiver proofs composed with ordinary slot guards.
//! - RegExp receiver proofs (no expando bag, no `[[Prototype]]` override)
//!   composed with `%RegExp.prototype%` slot guards, for every name but the
//!   own `lastIndex`.
//! - Primitive string receiver proofs composed with the `%String.prototype%`
//!   dictionary-layout guard, for every name a primitive string cannot own.
//! - Array receiver proofs (no rare state, no named bag) composed with the
//!   default realm's `%Array.prototype%` slot guards, for every name but
//!   `length` and the indices.
//! - Ordinary closure proofs (no bag, no prototype override, plain kind, the
//!   compiling realm active) composed with `%Function.prototype%` slot guards.
//! - A closure's own `prototype`, read from its rare record's slot.
//! - A RegExp's own `lastIndex`, read and written in its body slot.
//!
//! # Invariants
//! - Only own data slots of pinned realm prototypes are captured.
//! - Existing rooted shape preparation precedes every published shape guard;
//!   an unprepared or allocation-failed holder never contributes a program.
//!   A prototype holder takes its prototype role before its shape is read, so
//!   a later instance root cannot retire the guarded shape.
//! - Collection instance overrides cannot bypass their canonical lookup semantics.
//! - The `size` property remains on its existing specialized path; string
//!   `length` and canonical numeric names never produce a prototype program.
//! - The program reads the live slot; it never substitutes a builtin callable.
//! - Every miss precedes effects and uses the existing committed property call.
//!
//! # See also
//! - `crate::cache_ir` — ordinary shape/atom/field programs.
//! - `crate::method_ops::jit_snapshot` — the shared intrinsic receiver proof.

use crate::jit::{JitCacheIrOp, JitCacheIrProgram, JitIntrinsicPrototype};
use crate::object::ShapeState;
use crate::{Interpreter, Value, object, property_atom::AtomizedPropertyKey};

/// A RegExp's own `lastIndex`, read or written in its body slot. Every RegExp
/// owns it as a non-configurable data property, so the cell type proves a
/// read; a write also checks the writable bit.
pub(crate) fn body_slot_program(
    key: AtomizedPropertyKey<'_>,
    store: bool,
) -> Option<JitCacheIrProgram> {
    if key.name() != "lastIndex" {
        return None;
    }
    let slot = crate::method_ops::regexp_last_index_slot(store);
    let op = if store {
        JitCacheIrOp::StoreBodySlot { object: 0, slot }
    } else {
        JitCacheIrOp::LoadBodySlot { object: 0, slot }
    };
    Some(JitCacheIrProgram {
        ops: Box::new([op]),
    })
}

impl Interpreter {
    pub(crate) fn jit_intrinsic_property_programs(
        &mut self,
        key: AtomizedPropertyKey<'_>,
    ) -> Vec<JitCacheIrProgram> {
        if key.name() == "size" {
            return Vec::new();
        }
        let mut programs = [
            (
                self.realm_intrinsics.map_prototype(),
                crate::collections::MAP_BODY_TYPE_TAG,
            ),
            (
                self.realm_intrinsics.set_prototype(),
                crate::collections::SET_BODY_TYPE_TAG,
            ),
        ]
        .into_iter()
        .filter_map(|(prototype, type_tag)| {
            self.shaped_prototype_program(prototype?, key, |proto_offset| JitIntrinsicPrototype {
                type_tag,
                guard: Some(crate::method_ops::collection_guard()),
                proto_offset,
                active_realm: None,
            })
        })
        .collect::<Vec<_>>();
        // A RegExp owns `lastIndex`; every other name resolves on the active
        // realm's `%RegExp.prototype%` while no expando bag or `[[Prototype]]`
        // override exists — the reads behind `re.exec(…)` and `re.test(…)`.
        // A RegExp made while another realm was active carries an override,
        // so the realm id pins which prototype the program read.
        if key.name() != "lastIndex"
            && let Some(prototype) = self.realm_intrinsics.regexp_prototype()
        {
            let active_realm = self.active_realm_id;
            programs.extend(
                self.shaped_prototype_program(prototype, key, |proto_offset| {
                    JitIntrinsicPrototype {
                        type_tag: crate::regexp::REGEXP_BODY_TYPE_TAG,
                        guard: Some(crate::method_ops::regexp_guard()),
                        proto_offset,
                        active_realm: Some(active_realm),
                    }
                }),
            );
        }
        // An array without rare state or a named bag owns only `length` and
        // its elements; every other name resolves on the default realm's
        // retained `%Array.prototype%` — the reads behind `a.includes(…)`,
        // `a.slice(…)`, `a.join(…)`.
        if key.name() != "length"
            && object::array_index_property_name(key.name()).is_none()
            && let Some(prototype) = self.default_array_prototype()
        {
            programs.extend(
                self.shaped_prototype_program(prototype, key, |proto_offset| {
                    JitIntrinsicPrototype {
                        type_tag: crate::array::ARRAY_BODY_TYPE_TAG,
                        guard: Some(crate::method_ops::array_named_lookup_guard()),
                        proto_offset,
                        active_realm: None,
                    }
                }),
            );
        }
        programs.extend(self.string_property_program(key));
        programs.extend(self.closure_property_program(key));
        if key.name() == "prototype" {
            // `F.prototype` — read directly from the closure's slot.
            programs.push(JitCacheIrProgram {
                ops: Box::new([JitCacheIrOp::LoadClosurePrototype { object: 0 }]),
            });
        }
        programs
    }

    /// Ordinary closures own only virtual `name` / `length` / `prototype`
    /// (and the sloppy `caller` / `arguments` metadata) until a bag
    /// materializes, so every other name resolves on the active realm's
    /// `%Function.prototype%` — the load behind `f.apply(...)` and
    /// `f.call(...)`. The named-lookup byte excludes bags, prototype
    /// overrides and generator/async kinds; the realm id pins which
    /// `%Function.prototype%` the program read.
    pub(crate) fn closure_property_program(
        &mut self,
        key: AtomizedPropertyKey<'_>,
    ) -> Option<JitCacheIrProgram> {
        if matches!(
            key.name(),
            "name" | "length" | "prototype" | "caller" | "arguments"
        ) {
            return None;
        }
        let prototype = self.realm_intrinsics.function_prototype()?;
        let active_realm = self.active_realm_id;
        self.shaped_prototype_program(prototype, key, |proto_offset| JitIntrinsicPrototype {
            type_tag: crate::closure::JS_CLOSURE_BODY_TYPE_TAG,
            guard: Some(crate::closure::ordinary_named_lookup_guard()),
            proto_offset,
            active_realm: Some(active_realm),
        })
    }

    /// An exotic receiver proof, then the prepared prototype's fast shape and
    /// immutable atom slot.
    fn shaped_prototype_program(
        &mut self,
        mut prototype: object::JsObject,
        key: AtomizedPropertyKey<'_>,
        proof: impl FnOnce(u32) -> JitIntrinsicPrototype,
    ) -> Option<JitCacheIrProgram> {
        // An ordinary dictionary prototype is a valid migration input.
        // Reject semantic lookup facts before preparation, then require the
        // actual finalized keyed holder after the canonical migration.
        if object::state(prototype, &self.gc_heap).is_opaque() {
            return None;
        }
        if !matches!(
            object::lookup_own_slot(prototype, &self.gc_heap, key.name()).1,
            object::PropertyLookup::Data { .. }
        ) {
            return None;
        }
        self.migrate_slow_to_fast(&mut prototype);
        // The first instance root over a prototype gives it the prototype
        // role, which is a new hidden class. Take the role before reading the
        // shape the program guards, so a later root (a builtin fast-path
        // proof, `Object.create`) leaves the guarded shape in place.
        let prototype = self.with_handle_scope(|this, scope| {
            let rooted = this.scoped_value(scope, Value::object(prototype));
            this.object_root(
                Some(prototype),
                object::DEFAULT_INLINE_CAPACITY,
                ShapeState::ORDINARY,
            )
            .ok()?;
            this.escape_scoped(rooted).as_object()
        })?;
        if !object::supports_fast_property_ic(prototype, &self.gc_heap) {
            return None;
        }
        let shape = object::keyed_shape(prototype, &self.gc_heap);
        if shape.is_null() {
            return None;
        }
        let (hit, lookup) = object::lookup_own_slot(prototype, &self.gc_heap, key.name());
        let hit = hit?;
        if !matches!(lookup, object::PropertyLookup::Data { .. }) {
            return None;
        }
        let field = crate::object::field_location_at(prototype, &self.gc_heap, u32::from(hit.slot));
        Some(JitCacheIrProgram {
            ops: Box::new([
                JitCacheIrOp::LoadIntrinsicPrototype {
                    object: 0,
                    result: 1,
                    target: proof(prototype.offset()),
                },
                JitCacheIrOp::GuardShape {
                    object: 1,
                    shape: self.bake_shape(shape)?,
                },
                JitCacheIrOp::GuardAtomSlot {
                    object: 1,
                    atom: key.atom().id().raw(),
                    field,
                    writable: false,
                },
                JitCacheIrOp::LoadField { object: 1, field },
            ]),
        })
    }

    /// Primitive string receivers own only `length` and their indices, so
    /// every other name resolves on `%String.prototype%`. That String wrapper
    /// never adopts a hidden class; its dictionary layout id pins the slot and
    /// the load reads the slot's live data value.
    fn string_property_program(
        &mut self,
        key: AtomizedPropertyKey<'_>,
    ) -> Option<JitCacheIrProgram> {
        let name = key.name();
        if name == "length"
            || crate::property_dispatch::canonical_numeric_index_string(name).is_some()
        {
            return None;
        }
        let prototype = self.realm_intrinsics.string_prototype()?;
        let crate::jit::JitMethodHolder::Dictionary(layout) = self.jit_method_holder(prototype)?
        else {
            return None;
        };
        let (hit, lookup) = object::lookup_own_slot(prototype, &self.gc_heap, name);
        let hit = hit?;
        if !matches!(lookup, object::PropertyLookup::Data { .. }) {
            return None;
        }
        // The program loads this slot's value directly; watching it makes a
        // redefinition advance the layout the guard below compares.
        if !object::watch_dictionary_slot(prototype, &mut self.gc_heap, hit.slot) {
            return None;
        }
        let field = crate::object::field_location_at(prototype, &self.gc_heap, u32::from(hit.slot));
        Some(JitCacheIrProgram {
            ops: Box::new([
                JitCacheIrOp::LoadIntrinsicPrototype {
                    object: 0,
                    result: 1,
                    target: JitIntrinsicPrototype {
                        type_tag: crate::string::JS_STRING_BODY_TYPE_TAG,
                        guard: None,
                        proto_offset: prototype.offset(),
                        active_realm: None,
                    },
                },
                JitCacheIrOp::GuardDictionaryLayout { object: 1, layout },
                JitCacheIrOp::LoadField { object: 1, field },
            ]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::property_atom::PropertyAtom;

    #[test]
    fn ordinary_eligibility_preserves_intrinsic_collection_and_string_proofs() {
        let mut interpreter = Interpreter::new().expect("fixture interpreter bootstrap");
        let key = |interpreter: &mut Interpreter, name: &'static str| {
            AtomizedPropertyKey::new(
                PropertyAtom::new(
                    crate::isolate_names::names_mut(&mut interpreter.gc_heap).intern(name),
                ),
                name,
            )
        };

        for (name, type_tag, prototype) in [
            (
                "get",
                crate::collections::MAP_BODY_TYPE_TAG,
                interpreter.realm_intrinsics.map_prototype().unwrap(),
            ),
            (
                "has",
                crate::collections::SET_BODY_TYPE_TAG,
                interpreter.realm_intrinsics.set_prototype().unwrap(),
            ),
        ] {
            // Bootstrap gives every intrinsic prototype its hidden class.
            assert!(!object::keyed_shape(prototype, &interpreter.gc_heap).is_null());
            assert!(object::supports_fast_property_ic(
                prototype,
                &interpreter.gc_heap
            ));
            let key = key(&mut interpreter, name);
            let programs = interpreter.jit_intrinsic_property_programs(key);
            // Baking takes the prototype role; the program guards the live
            // hidden class, which a second bake keeps.
            let shape = object::keyed_shape(prototype, &interpreter.gc_heap);
            assert!(object::state(prototype, &interpreter.gc_heap).is_prototype());
            assert!(!object::state(prototype, &interpreter.gc_heap).is_provisional());
            let _ = interpreter.jit_intrinsic_property_programs(key);
            assert_eq!(object::keyed_shape(prototype, &interpreter.gc_heap), shape);
            assert!(
                programs.iter().any(|program| matches!(
                    program.ops.first(),
                    Some(JitCacheIrOp::LoadIntrinsicPrototype { target, .. })
                        if target.type_tag == type_tag
                ) && program.ops.iter().any(|op| matches!(
                    op, JitCacheIrOp::GuardShape { object: 1, shape: baked }
                        if *baked == shape.offset()
                ))),
                "the program guards the prototype's bootstrap hidden class"
            );
        }

        let string_prototype = interpreter.realm_intrinsics.string_prototype().unwrap();
        assert!(!object::supports_fast_property_ic(
            string_prototype,
            &interpreter.gc_heap
        ));
        let char_code_at = key(&mut interpreter, "charCodeAt");
        let string_program = interpreter.string_property_program(char_code_at).unwrap();
        assert!(matches!(
            string_program.ops.first(),
            Some(JitCacheIrOp::LoadIntrinsicPrototype { target, .. })
                if target.type_tag == crate::string::JS_STRING_BODY_TYPE_TAG
        ));
        assert!(matches!(
            string_program.ops.get(1),
            Some(JitCacheIrOp::GuardDictionaryLayout { .. })
        ));
    }

    #[test]
    fn regexp_programs_prove_generated_receivers_in_the_active_realm() {
        let mut interpreter = Interpreter::new().expect("fixture interpreter bootstrap");
        let key = |interpreter: &mut Interpreter, name: &'static str| {
            AtomizedPropertyKey::new(
                PropertyAtom::new(
                    crate::isolate_names::names_mut(&mut interpreter.gc_heap).intern(name),
                ),
                name,
            )
        };
        let exec = key(&mut interpreter, "exec");
        let programs = interpreter.jit_intrinsic_property_programs(exec);
        let target = programs
            .iter()
            .find_map(|program| match program.ops.first() {
                Some(JitCacheIrOp::LoadIntrinsicPrototype { target, .. })
                    if target.type_tag == crate::regexp::REGEXP_BODY_TYPE_TAG =>
                {
                    Some(*target)
                }
                _ => None,
            })
            .expect("%RegExp.prototype%.exec admits a receiver program");
        assert!(target.is_generated_receiver());
        assert_eq!(target.active_realm, Some(interpreter.active_realm_id));
        assert!(
            !crate::jit::JitIntrinsicPrototype {
                active_realm: None,
                ..target
            }
            .is_generated_receiver(),
            "an unpinned realm would read another realm's prototype"
        );

        // A later instance root over %RegExp.prototype% (the builtin
        // protocol proof takes one) keeps the hidden class the program guards.
        let guarded = programs
            .iter()
            .find(|program| {
                matches!(
                    program.ops.first(),
                    Some(JitCacheIrOp::LoadIntrinsicPrototype { target, .. })
                        if target.type_tag == crate::regexp::REGEXP_BODY_TYPE_TAG
                )
            })
            .and_then(|program| {
                program.ops.iter().find_map(|op| match op {
                    JitCacheIrOp::GuardShape { object: 1, shape } => Some(*shape),
                    _ => None,
                })
            })
            .expect("the RegExp program guards its holder's shape");
        let prototype = interpreter.realm_intrinsics.regexp_prototype().unwrap();
        interpreter
            .object_root(
                Some(prototype),
                object::DEFAULT_INLINE_CAPACITY,
                ShapeState::ORDINARY,
            )
            .expect("instance root");
        assert_eq!(
            object::keyed_shape(prototype, &interpreter.gc_heap).offset(),
            guarded,
            "the prototype took its role before the program read its shape"
        );

        let last_index = key(&mut interpreter, "lastIndex");
        let load = body_slot_program(last_index, false).expect("lastIndex load program");
        let store = body_slot_program(last_index, true).expect("lastIndex store program");
        assert!(matches!(
            *load.ops,
            [JitCacheIrOp::LoadBodySlot { object: 0, slot }] if slot.is_generated(false) && slot.guard.is_none()
        ));
        assert!(matches!(
            *store.ops,
            [JitCacheIrOp::StoreBodySlot { object: 0, slot }] if slot.is_generated(true) && slot.guard.is_some()
        ));
        assert!(body_slot_program(exec, false).is_none());
        assert!(
            interpreter
                .jit_intrinsic_property_programs(last_index)
                .iter()
                .all(|program| !matches!(
                    program.ops.first(),
                    Some(JitCacheIrOp::LoadIntrinsicPrototype { target, .. })
                        if target.type_tag == crate::regexp::REGEXP_BODY_TYPE_TAG
                )),
            "a RegExp owns `lastIndex`"
        );
    }

    #[test]
    fn collection_size_has_no_intrinsic_property_program() {
        let mut interpreter = Interpreter::new().expect("fixture interpreter bootstrap");
        let key = |interpreter: &mut Interpreter, name: &'static str| {
            AtomizedPropertyKey::new(
                PropertyAtom::new(
                    crate::isolate_names::names_mut(&mut interpreter.gc_heap).intern(name),
                ),
                name,
            )
        };
        let get = key(&mut interpreter, "get");
        let programs = interpreter.jit_intrinsic_property_programs(get);
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
            assert!(
                object::define_own_property(
                    prototype,
                    &mut interpreter.gc_heap,
                    "size",
                    object::PropertyDescriptor::data(
                        crate::Value::number_i32(900),
                        true,
                        false,
                        true
                    ),
                )
                .expect("descriptor fixture allocation")
            );
            assert!(object::is_dictionary(prototype, &interpreter.gc_heap));
            let mut prepared = prototype;
            interpreter.migrate_slow_to_fast(&mut prepared);
            assert_eq!(prepared, prototype, "pinned realm holder remains current");
            assert!(object::supports_fast_property_ic(
                prepared,
                &interpreter.gc_heap
            ));
            assert!(matches!(
                object::lookup_own_slot(prepared, &interpreter.gc_heap, "size"),
                (Some(_), object::PropertyLookup::Data { .. })
            ));
        }
        let size = key(&mut interpreter, "size");
        assert!(interpreter.jit_intrinsic_property_programs(size).is_empty());
    }
}
