//! Typed JIT compile snapshots for guarded builtin method calls.
//!
//! These methods freeze guarded method metadata consumed directly by generated
//! code. Runtime method dispatch remains interpreter-owned.
//!
//! # Contents
//! - Collection reads and writes, dense-array mutations and primitive-string
//!   builtins, each described as one [`crate::jit::JitGuardedMethodCall`].
//! - Body-word guards and the RegExp `lastIndex` body slot shared with the
//!   intrinsic property programs.
//!
//! # Invariants
//! - A snapshot names a receiver, a latch, a pinned prototype and a declared
//!   entry; the call protocol follows from the family the entry id resolves in,
//!   never from the builtin's identity.
//! - Every field is re-validated by the emitted guard, so a stale snapshot can
//!   only side-exit, never miscompile.
//! - Collection prototypes are prepared before capturing their live own-name,
//!   shape and slot proof; dictionary observations never become installed guards.
//!
//! # See also
//! - [`crate::realm_intrinsics`] owns retained canonical prototype roots.
//! - [`super`] validates and records the method-cache observations.

use super::MethodCallIc;
use crate::interp::jit_bake::Baker;
use crate::jit::{
    JitBodyGuard, JitBodySlot, JitGuardWidth, JitGuardedMethodCall, JitGuardedReceiver,
    JitIntrinsicPrototype, JitMethodHolder,
};

/// The 32-bit no-expando/no-override word every `Map` and `Set` body carries.
pub(crate) fn collection_guard() -> JitBodyGuard {
    JitBodyGuard::clear(
        otter_gc::header::HEADER_SIZE as u32
            + crate::collections::MAP_BODY_JIT_GUARD_FLAGS_OFFSET as u32,
        JitGuardWidth::Word32,
    )
}

/// The RegExp lookup flags: zero while no expando bag can shadow a
/// `%RegExp.prototype%` name and the `[[Prototype]]` is not overridden.
pub(crate) fn regexp_guard() -> JitBodyGuard {
    JitBodyGuard::clear(
        otter_gc::header::HEADER_SIZE as u32
            + crate::regexp::REGEXP_BODY_JIT_GUARD_FLAGS_OFFSET as u32,
        JitGuardWidth::Word32,
    )
}

/// A RegExp's own `lastIndex` slot. A write also requires the property's
/// writable bit, which `Object.defineProperty` and `Object.freeze` clear.
pub(crate) fn regexp_last_index_slot(store: bool) -> JitBodySlot {
    let header = otter_gc::header::HEADER_SIZE as u32;
    JitBodySlot {
        type_tag: crate::regexp::REGEXP_BODY_TYPE_TAG,
        guard: store.then_some(JitBodyGuard {
            byte: header + crate::regexp::REGEXP_BODY_LAST_INDEX_WRITABLE_OFFSET as u32,
            width: JitGuardWidth::Byte,
            expect: 1,
            mask: JitBodyGuard::WHOLE,
        }),
        byte: header + crate::regexp::REGEXP_BODY_LAST_INDEX_OFFSET as u32,
    }
}

/// The aligned rare/expando pair. Both compressed handles must be null before
/// the default realm's retained `%Array.prototype%` can answer an inherited key.
pub(crate) fn array_named_lookup_guard() -> JitBodyGuard {
    JitBodyGuard::clear(
        otter_gc::header::HEADER_SIZE as u32 + crate::array::ARRAY_BODY_EXOTIC_OFFSET as u32,
        JitGuardWidth::Word64,
    )
}

impl Baker<'_> {
    /// Snapshot a monomorphic dense-array method site (`push` / `pop` /
    /// `shift` / `unshift`, or a read the static-native registry declares)
    /// into JIT-readable guard metadata.
    ///
    /// `pop` / `shift` / `push` only rewrite the dense buffer in place, so they
    /// run as mutating leaves with no safepoint (`push` misses on a full
    /// buffer); `unshift` may grow it and needs the allocating entry with a
    /// precise root map. Returns `None` for any other
    /// method, family, or when the prototype slot no longer holds the original
    /// native builtin.
    pub(crate) fn jit_array_method_call(
        &mut self,
        site: usize,
        alloc_safepoint_id: crate::native_abi::SafepointId,
    ) -> Option<JitGuardedMethodCall> {
        use crate::native_abi::{
            NO_SAFEPOINT, STUB_ARRAY_POP_LEAF, STUB_ARRAY_PUSH_ALLOC, STUB_ARRAY_PUSH_LEAF,
            STUB_ARRAY_SHIFT_LEAF, STUB_ARRAY_UNSHIFT_ALLOC,
        };

        let ic = match self.env.method_ic(site)? {
            MethodCallIc::Array(ic) => ic,
            MethodCallIc::Collection(_) | MethodCallIc::Ordinary(_) => return None,
        };
        use crate::array_prototype::ArrayMethodTag as Tag;
        // A push the leaf refuses for a full buffer grows it through the
        // allocating entry instead of leaving the site.
        let grow = (ic.tag == Tag::Push).then_some((STUB_ARRAY_PUSH_ALLOC.id, alloc_safepoint_id));
        let proto = self.env.default_array_prototype?;
        if crate::object::shape_id(proto, &self.env.heap) != ic.proto_shape {
            return None;
        }
        let method = crate::object::data_slot_value_at(proto, &self.env.heap, ic.proto_slot)?;
        if !ic.tag.matches_builtin(method, &self.env.heap) {
            return None;
        }
        let (stub_id, safepoint_id, argument_count) = match ic.tag {
            Tag::Pop => (STUB_ARRAY_POP_LEAF.id, NO_SAFEPOINT, 0),
            Tag::Shift => (STUB_ARRAY_SHIFT_LEAF.id, NO_SAFEPOINT, 0),
            Tag::Push => (STUB_ARRAY_PUSH_LEAF.id, NO_SAFEPOINT, 1),
            Tag::Unshift => (STUB_ARRAY_UNSHIFT_ALLOC.id, alloc_safepoint_id, 1),
            // Any other method the static-native registry declares with a
            // `this` operand runs its read-only leaf.
            _ => {
                let declaration = crate::jit_static_native::jit_static_call_target(
                    method.as_native_function()?,
                    &self.env.heap,
                )
                .filter(|declaration| declaration.this_operand)?;
                (
                    declaration.leaf_stub_id,
                    NO_SAFEPOINT,
                    declaration.argument_count,
                )
            }
        };
        let builtin_native_ref = method
            .as_native_function()
            .and_then(|native| native.native_ref(&self.env.heap))?;
        Some(JitGuardedMethodCall {
            receiver: JitGuardedReceiver::Exotic(JitIntrinsicPrototype {
                type_tag: crate::array::ARRAY_BODY_TYPE_TAG,
                guard: Some(array_named_lookup_guard()),
                proto_offset: proto.offset(),
                active_realm: None,
            }),
            holder: self.jit_method_holder(proto)?,
            method_field: crate::object::field_location_at(
                proto,
                &self.env.heap,
                u32::from(ic.proto_slot),
            ),
            builtin_native_ref,
            entry_stub_id: stub_id,
            safepoint_id,
            argument_count,
            grow,
            holder_validity: crate::object::prototype_validity::current_chain_validity(
                proto,
                &self.env.heap,
            )
            .and_then(|cell| self.bake_prototype_validity(&cell)),
        })
    }

    /// One primitive-receiver builtin call (`"…".charCodeAt(i)`) as a guarded
    /// method call: the method `name` names on `%String.prototype%`, when it
    /// is a builtin the static-native registry declares with a `this`
    /// operand.
    ///
    /// The receiver's cell type tag stands in for a collection's latch word: a
    /// primitive body carries no expando or override state, so proving the tag
    /// plus the realm prototype's shape and method-slot identity is the whole
    /// guard. `%String.prototype%` is a String wrapper and never adopts a
    /// hidden class, so its slot layout is pinned by its dictionary id.
    pub(crate) fn jit_primitive_method_call(&mut self, name: &str) -> Option<JitGuardedMethodCall> {
        let proto = self.env.string_prototype?;
        let (hit, lookup) = crate::object::lookup_own_slot(proto, &self.env.heap, name);
        let hit = hit?;
        let method = match lookup {
            crate::object::PropertyLookup::Data { value, .. } => value,
            crate::object::PropertyLookup::Accessor { .. }
            | crate::object::PropertyLookup::Absent => return None,
        };
        let native = method.as_native_function()?;
        let declaration = crate::jit_static_native::jit_static_call_target(native, &self.env.heap)
            .filter(|declaration| declaration.this_operand)?;
        let builtin_native_ref = native.native_ref(&self.env.heap)?;
        Some(JitGuardedMethodCall {
            receiver: JitGuardedReceiver::Exotic(JitIntrinsicPrototype {
                type_tag: crate::string::JS_STRING_BODY_TYPE_TAG,
                guard: None,
                proto_offset: proto.offset(),
                active_realm: None,
            }),
            holder: self.jit_method_holder(proto)?,
            method_field: crate::object::field_location_at(
                proto,
                &self.env.heap,
                u32::from(hit.slot),
            ),
            builtin_native_ref,
            entry_stub_id: declaration.leaf_stub_id,
            safepoint_id: crate::native_abi::NO_SAFEPOINT,
            argument_count: declaration.argument_count,
            grow: None,
            holder_validity: None,
        })
    }

    /// One collection builtin (`map.get` / `map.set` / `set.add` / …) as a
    /// guarded method call.
    ///
    /// A read resolves in the leaf family and runs with no safepoint; a write
    /// resolves in the allocating family and publishes `alloc_safepoint_id`.
    ///
    /// This is intentionally stricter than the runtime IC guard: explicit
    /// prototype overrides, even if they point back to the canonical prototype,
    /// are left to the normal fallback path because generated code only checks
    /// the collection body's no-override/no-expando guard flags.
    pub(crate) fn jit_collection_method_call(
        &mut self,
        site: usize,
        alloc_safepoint_id: crate::native_abi::SafepointId,
    ) -> Option<JitGuardedMethodCall> {
        let ic = match self.env.method_ic(site)? {
            MethodCallIc::Collection(ic) => ic,
            MethodCallIc::Array(_) | MethodCallIc::Ordinary(_) => return None,
        };
        // A read that has a leaf entry never needs the allocating one: the leaf
        // family cannot collect, so it carries no safepoint and no root map.
        // A read resolves in the leaf family; an in-place write in the
        // mutating family. Both cannot collect, so neither owes a safepoint,
        // and only a write that may grow the table reaches the allocating one.
        let (stub_id, safepoint_id) = match (ic.leaf_stub_id, ic.mutating_stub_id) {
            (Some(leaf), _) => (leaf, crate::native_abi::NO_SAFEPOINT),
            (None, Some(mutating)) => (mutating, crate::native_abi::NO_SAFEPOINT),
            (None, None) => (ic.alloc_stub_id?, alloc_safepoint_id),
        };
        let (proto, receiver_type_tag) = if ic.op.is_map() {
            (
                self.env.map_prototype?,
                crate::collections::MAP_BODY_TYPE_TAG,
            )
        } else {
            (
                self.env.set_prototype?,
                crate::collections::SET_BODY_TYPE_TAG,
            )
        };
        // Publish only stable hidden-class guards: the session's preparation
        // migrated the prototype, including for method-only bodies compiled
        // before any named collection property was observed.
        let holder_shape = crate::object::keyed_shape(proto, &self.env.heap);
        if holder_shape.is_null() {
            return None;
        }
        // Shape preparation can replace dictionary metadata after this runtime
        // IC was observed. Compile the live own-name proof, including its slot,
        // rather than freezing the earlier dictionary identity into code.
        let (hit, lookup) = crate::object::lookup_own_slot(proto, &self.env.heap, ic.op.name());
        let hit = hit?;
        let crate::object::PropertyLookup::Data { value: method, .. } = lookup else {
            return None;
        };
        if !ic.op.matches_builtin(method, &self.env.heap) {
            return None;
        }
        let builtin_native_ref = method
            .as_native_function()
            .and_then(|native| native.native_ref(&self.env.heap))?;
        Some(JitGuardedMethodCall {
            receiver: JitGuardedReceiver::Exotic(JitIntrinsicPrototype {
                type_tag: receiver_type_tag,
                guard: Some(collection_guard()),
                proto_offset: proto.offset(),
                active_realm: None,
            }),
            holder: JitMethodHolder::Shape(self.bake_shape(holder_shape)?),
            method_field: crate::object::field_location_at(
                proto,
                &self.env.heap,
                u32::from(hit.slot),
            ),
            builtin_native_ref,
            entry_stub_id: stub_id,
            safepoint_id,
            argument_count: ic.op.argument_count(),
            grow: None,
            holder_validity: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Interpreter;
    use crate::method_ops::ArrayMethodCallIc;
    use crate::{ExecutionContext, Value, object};
    use otter_bytecode::{
        BytecodeModule, Constant, Function, Instruction, Op, Operand, SourceKind,
    };

    #[test]
    fn array_method_snapshot_rejects_deleted_name_even_when_slot_reuses_builtin() {
        let context = ExecutionContext::from_module(
            BytecodeModule {
                module: "array-method-snapshot.js".into(),
                template_sites: Vec::new(),
                source_kind: SourceKind::JavaScript,
                functions: vec![Function {
                    id: 0,
                    name: "<main>".into(),
                    locals: 3,
                    code: vec![
                        Instruction {
                            pc: 0,
                            op: Op::CallMethodValue,
                            operands: vec![
                                Operand::Register(0),
                                Operand::Register(1),
                                Operand::ConstIndex(0),
                                Operand::ConstIndex(1),
                                Operand::Register(2),
                            ],
                        },
                        Instruction {
                            pc: 1,
                            op: Op::ReturnUndefined,
                            operands: Vec::new(),
                        },
                    ]
                    .into(),
                    ..Function::default()
                }],
                constants: vec![Constant::String {
                    utf16: "push".encode_utf16().collect(),
                }],
                module_resolutions: Vec::new(),
                module_inits: Vec::new(),
                lazy_environments: Vec::new(),
                function_source: None,
            },
            crate::source_registry::SourceRegistry::default(),
        )
        .expect("valid method-site fixture");
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        vm.gc_heap.set_gc_stress(0, false);
        vm.ensure_method_feedback_context(&context);
        let site = context
            .feedback_slot_addresses()
            .into_iter()
            .find(|(_, address)| address.is_method())
            .unwrap()
            .0;
        vm.with_handle_scope(|vm, scope| {
            let prototype =
                vm.scoped_value(scope, Value::object(vm.default_array_prototype().unwrap()));
            let proto = vm.escape_scoped(prototype).as_object().unwrap();
            let keys = object::with_properties(proto, &vm.gc_heap, |properties| {
                properties.keys().map(str::to_owned).collect::<Vec<_>>()
            });
            let position = keys.iter().position(|key| key == "push").unwrap();
            let next_name = &keys[position + 1];
            let method = vm.scoped_value(scope, object::get(proto, &vm.gc_heap, "push").unwrap());
            let mut proto = vm.escape_scoped(prototype).as_object().unwrap();
            let descriptor =
                object::PropertyDescriptor::data(vm.escape_scoped(method), true, false, true);
            assert!(
                object::define_own_property_in_place(
                    &mut proto,
                    &mut vm.gc_heap,
                    next_name,
                    descriptor,
                )
                .unwrap()
            );
            let proto = vm.escape_scoped(prototype).as_object().unwrap();
            let (hit, _) = object::lookup_own_slot(proto, &vm.gc_heap, "push");
            let hit = hit.unwrap();
            assert!(vm.method_feedback_mut().install_method_ic(
                site,
                MethodCallIc::Array(ArrayMethodCallIc {
                    proto_shape: hit.shape_id,
                    proto_slot: hit.slot,
                    tag: crate::array_prototype::ArrayMethodTag::Push,
                })
            ));
            assert!(
                Baker::new(&crate::interp::jit_bake_env::BakeEnv::capture(&vm), None)
                    .jit_array_method_call(site, 1)
                    .is_some()
            );
            let mut proto = vm.escape_scoped(prototype).as_object().unwrap();
            assert!(object::delete(&mut proto, &mut vm.gc_heap, "push").unwrap());
            let proto = vm.escape_scoped(prototype).as_object().unwrap();
            assert!(object::get(proto, &vm.gc_heap, "push").is_none());
            assert_eq!(
                object::data_slot_value_at(proto, &vm.gc_heap, hit.slot),
                Some(vm.escape_scoped(method))
            );
            assert_ne!(object::shape_id(proto, &vm.gc_heap), hit.shape_id);
            assert!(
                Baker::new(&crate::interp::jit_bake_env::BakeEnv::capture(&vm), None)
                    .jit_array_method_call(site, 1)
                    .is_none()
            );
        });
    }
}
