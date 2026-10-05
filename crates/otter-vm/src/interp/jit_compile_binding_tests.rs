//! Global-object descriptor admission in the real compile-snapshot producer.
//!
//! # Contents
//! - Clean migrated globals and live data-value replacements retain proofs.
//! - Heap-only integrity/descriptor changes retire the old ordinary identity.
//! - Fresh dictionary proofs carry the current writable flag and live value.
//! - Dictionary watches preserve an unchanged slot through unrelated changes.
//! - Foreign source admission cannot bake ambient globals or an inline body.
//! - Disposed source realms cannot substitute the default global.
//!
//! # Invariants
//! - Every snapshot uses linked, verifier-valid bytecode and its encoded PCs.
//! - The receiver is the interpreter's actual global, with its normal storage.
//! - Native freeze enters through the production context and handle scope.
//! - These tests prove producer admission; executable guards are covered by
//!   `otter-runtime/tests/jit_global_access.rs`.
//!
//! # See also
//! - `super::Interpreter::bake_binding_hit_proofs` owns binding admission.
//! - `crate::object::descriptor_mutation` retires dictionary descriptor proofs.

use super::*;
use otter_bytecode::{Constant, FunctionCodeBuilder, Operand};

const NAME: &str = "producerBinding";
const UNRELATED: &str = "producerUnrelated";

fn fixture(dictionary: bool) -> (Interpreter, ExecutionContext) {
    let mut module = crate::test_support::minimal_bytecode_module("binding-admission.js");
    module.constants = vec![Constant::String {
        utf16: NAME.encode_utf16().collect(),
    }];
    module.functions[0].locals = 3;
    let mut code = FunctionCodeBuilder::new();
    for (op, destination) in [
        (Op::LoadGlobalOrThrow, 0),
        (Op::LoadGlobalOrUndefined, 1),
        (Op::GlobalBindingExists, 2),
    ] {
        code.push(
            op,
            &[Operand::Register(destination), Operand::ConstIndex(0)],
        );
    }
    code.push(
        Op::StoreGlobalBinding,
        &[
            Operand::Register(0),
            Operand::ConstIndex(0),
            Operand::Imm32(0),
        ],
    );
    code.push(
        Op::StoreGlobalChecked,
        &[
            Operand::Register(0),
            Operand::ConstIndex(0),
            Operand::Register(2),
        ],
    );
    code.push(Op::ReturnUndefined, &[]);
    module.functions[0].code = code.finish();

    let mut interpreter = Interpreter::new().expect("fixture interpreter bootstrap");
    interpreter
        .set_global(NAME, Value::number_i32(7))
        .expect("fixture global descriptor");
    interpreter
        .set_global(UNRELATED, Value::number_i32(11))
        .expect("fixture global descriptor");
    let context = interpreter
        .link_module(module, crate::source_registry::SourceRegistry::default())
        .expect("valid binding source");
    if !dictionary {
        interpreter.with_handle_scope(|interpreter, scope| {
            let global = interpreter.scoped_value(scope, Value::object(interpreter.global_this));
            let mut object = interpreter.escape_scoped(global).as_object().unwrap();
            interpreter.migrate_slow_to_fast(&mut object);
            assert_eq!(
                object,
                interpreter.escape_scoped(global).as_object().unwrap(),
                "migration preserves the rooted actual global"
            );
            interpreter.global_this = object;
        });
    }
    assert_eq!(
        object::is_dictionary(interpreter.global_this, &interpreter.gc_heap),
        dictionary,
        "fixture reaches its requested production storage"
    );
    let state = object::state(interpreter.global_this, &interpreter.gc_heap);
    assert_eq!(state.is_dictionary(), dictionary);
    assert!(!state.is_provisional() && !state.is_opaque() && !state.is_prototype());
    assert!(state.is_extensible());
    let descriptor =
        object::get_own_descriptor(interpreter.global_this, &interpreter.gc_heap, NAME)
            .expect("fixture current own descriptor");
    assert_eq!(
        (
            descriptor.writable(),
            descriptor.enumerable(),
            descriptor.configurable()
        ),
        (true, false, true),
        "set_global installs the actual non-enumerable host data descriptor"
    );
    assert!(
        matches!(descriptor.kind, object::DescriptorKind::Data { value } if value == Value::number_i32(7))
    );
    (interpreter, context)
}

fn bake(interpreter: &mut Interpreter, context: &ExecutionContext) -> jit::JitCompileSnapshot {
    let fid = context.function_base();
    let mut view = context.jit_compile_snapshot(fid).expect("linked snapshot");
    interpreter.bake_global_lexical_loads(&mut view, context, fid);
    interpreter.bake_binding_hit_proofs(&mut view, context, fid);
    view
}

fn assert_proofs(
    interpreter: &Interpreter,
    view: &jit::JitCompileSnapshot,
    dictionary: bool,
    writable: bool,
) {
    let (Some(hit), object::PropertyLookup::Data { .. }) =
        object::lookup_own_slot(interpreter.global_this, &interpreter.gc_heap, NAME)
    else {
        panic!("fixture owns its actual data slot");
    };
    let shape = if dictionary {
        u64::from(
            object::dictionary_layout(interpreter.global_this, &interpreter.gc_heap)
                .expect("provable dictionary epoch"),
        )
    } else {
        u64::from(object::keyed_shape(interpreter.global_this, &interpreter.gc_heap).offset())
    };
    let field = object::field_location_at(
        interpreter.global_this,
        &interpreter.gc_heap,
        u32::from(hit.slot),
    );
    let expected = jit::BindingHitProof::GlobalObject {
        shape,
        dictionary,
        field,
        global_lexical_epoch: interpreter.global_lexical_epoch,
        writable,
    };
    let mut binding_pcs = Vec::new();
    let mut load_pcs = Vec::new();
    for instruction in &view.instructions {
        let op = instruction.op(&view.code_block);
        if op == Op::ReturnUndefined {
            continue;
        }
        binding_pcs.push(instruction.byte_pc);
        assert_eq!(
            view.binding_hit_proofs.get(&instruction.byte_pc),
            Some(&expected)
        );
        if op == Op::LoadGlobalOrThrow {
            load_pcs.push(instruction.byte_pc);
            assert_eq!(
                view.global_object_loads.get(&instruction.byte_pc),
                Some(&jit::JitGlobalObjectLoad {
                    shape,
                    dictionary,
                    field,
                    global_lexical_epoch: interpreter.global_lexical_epoch,
                })
            );
        }
    }
    assert_eq!(binding_pcs.len(), 5, "all exact binding opcode families");
    assert_eq!(view.binding_hit_proofs.len(), binding_pcs.len());
    assert_eq!(load_pcs.len(), 1);
    assert_eq!(view.global_object_loads.len(), load_pcs.len());
    assert!(view.global_lexical_loads.is_empty());
}

fn replace_value(interpreter: &mut Interpreter, value: i32) {
    interpreter.with_handle_scope(|interpreter, scope| {
        let global = interpreter.scoped_value(scope, Value::object(interpreter.global_this));
        let object = interpreter.escape_scoped(global).as_object().unwrap();
        assert!(
            interpreter
                .ordinary_set_data_property(object, NAME, Value::number_i32(value))
                .expect("ordinary value replacement")
        );
    });
}

fn native_freeze(interpreter: &mut Interpreter, context: &ExecutionContext) {
    NativeCtx::with_host_context(
        interpreter,
        NativeCallInfo::default_call(),
        Some(context),
        |ctx| {
            ctx.scope(|mut scope| {
                let global = scope.global_this();
                scope.freeze(global)
            })
        },
    )
    .expect("production scoped heap-only freeze");
}

#[test]
fn ordinary_global_value_updates_keep_proofs_but_native_freeze_retires_the_identity() {
    let (mut interpreter, context) = fixture(false);
    let before = bake(&mut interpreter, &context);
    assert_proofs(&interpreter, &before, false, true);
    let shape = object::keyed_shape(interpreter.global_this, &interpreter.gc_heap);
    let epoch = interpreter.global_lexical_epoch;

    replace_value(&mut interpreter, 719);

    let updated = bake(&mut interpreter, &context);
    assert_proofs(&interpreter, &updated, false, true);
    assert_eq!(updated.binding_hit_proofs, before.binding_hit_proofs);
    assert_eq!(updated.global_object_loads, before.global_object_loads);
    assert_eq!(
        object::get_own(interpreter.global_this, &interpreter.gc_heap, NAME),
        Some(Value::number_i32(719)),
        "the unchanged field holds the current value"
    );

    native_freeze(&mut interpreter, &context);

    assert!(!shape.is_null(), "the old proof owns an ordinary shape");
    assert_ne!(
        object::shape(interpreter.global_this, &interpreter.gc_heap),
        shape,
        "heap-only freeze retires the exact installed ordinary identity"
    );
    assert!(object::keyed_shape(interpreter.global_this, &interpreter.gc_heap).is_null());
    let state = object::state(interpreter.global_this, &interpreter.gc_heap);
    assert!(state.is_dictionary() && !state.is_extensible());
    assert!(!state.is_provisional() && !state.is_opaque() && !state.is_prototype());
    assert_eq!(
        interpreter.global_lexical_epoch, epoch,
        "no lexical shadowing masks refusal"
    );
    assert!(object::is_frozen(
        interpreter.global_this,
        &interpreter.gc_heap
    ));
    let descriptor =
        object::get_own_descriptor(interpreter.global_this, &interpreter.gc_heap, NAME)
            .expect("frozen current own descriptor");
    assert!(!descriptor.writable() && !descriptor.configurable());
    assert!(
        !descriptor.enumerable(),
        "freeze preserves the actual host enumerable flag"
    );
    assert!(
        matches!(descriptor.kind, object::DescriptorKind::Data { value } if value == Value::number_i32(719))
    );
    let current = bake(&mut interpreter, &context);
    assert_proofs(&interpreter, &current, true, false);
    assert_ne!(current.binding_hit_proofs, before.binding_hit_proofs);
    assert_ne!(current.global_object_loads, before.global_object_loads);
    assert!(
        current.binding_hit_proofs.values().all(|proof| matches!(
            proof,
            jit::BindingHitProof::GlobalObject {
                dictionary: true,
                writable: false,
                ..
            }
        )),
        "readonly current metadata cannot authorize a generated store hit"
    );
}

#[test]
fn descriptor_changes_retire_ordinary_identity_and_bake_current_dictionary_flags() {
    for writable in [true, false] {
        let (mut interpreter, context) = fixture(false);
        let shape = object::keyed_shape(interpreter.global_this, &interpreter.gc_heap);
        let before = bake(&mut interpreter, &context);
        assert_proofs(&interpreter, &before, false, true);
        let epoch = interpreter.global_lexical_epoch;
        interpreter.with_handle_scope(|interpreter, scope| {
            let global = interpreter.scoped_value(scope, Value::object(interpreter.global_this));
            let object = interpreter.escape_scoped(global).as_object().unwrap();
            assert!(
                object::define_own_property(
                    object,
                    &mut interpreter.gc_heap,
                    NAME,
                    object::PropertyDescriptor::data(Value::number_i32(7), writable, false, false),
                )
                .expect("descriptor fixture allocation")
            );
        });
        assert!(!shape.is_null());
        assert_ne!(
            object::shape(interpreter.global_this, &interpreter.gc_heap),
            shape
        );
        assert!(object::keyed_shape(interpreter.global_this, &interpreter.gc_heap).is_null());
        let state = object::state(interpreter.global_this, &interpreter.gc_heap);
        assert!(state.is_dictionary() && state.is_extensible());
        assert!(!state.is_provisional() && !state.is_opaque() && !state.is_prototype());
        assert_eq!(interpreter.global_lexical_epoch, epoch);
        let descriptor =
            object::get_own_descriptor(interpreter.global_this, &interpreter.gc_heap, NAME)
                .unwrap();
        assert_eq!(descriptor.writable(), writable);
        assert!(!descriptor.enumerable() && !descriptor.configurable());
        assert!(
            matches!(descriptor.kind, object::DescriptorKind::Data { value } if value == Value::number_i32(7))
        );
        let current = bake(&mut interpreter, &context);
        assert_proofs(&interpreter, &current, true, writable);
        assert_ne!(current.binding_hit_proofs, before.binding_hit_proofs);
        assert_ne!(current.global_object_loads, before.global_object_loads);
        assert!(
            current.binding_hit_proofs.values().all(|proof| matches!(
                proof,
                jit::BindingHitProof::GlobalObject { dictionary: true, writable: actual, .. }
                    if *actual == writable
            )),
            "only current dictionary metadata supplies descriptor authority"
        );
    }
}

#[test]
fn dictionary_binding_watches_preserve_values_and_unrelated_descriptor_changes() {
    let (mut interpreter, context) = fixture(true);
    let before = bake(&mut interpreter, &context);
    assert_proofs(&interpreter, &before, true, true);
    let layout = object::dictionary_layout(interpreter.global_this, &interpreter.gc_heap).unwrap();

    replace_value(&mut interpreter, 719);
    let updated = bake(&mut interpreter, &context);
    assert_proofs(&interpreter, &updated, true, true);
    assert_eq!(updated.binding_hit_proofs, before.binding_hit_proofs);
    assert_eq!(updated.global_object_loads, before.global_object_loads);

    let structural = object::shape_id(interpreter.global_this, &interpreter.gc_heap);
    interpreter.with_handle_scope(|interpreter, scope| {
        let global = interpreter.scoped_value(scope, Value::object(interpreter.global_this));
        let object = interpreter.escape_scoped(global).as_object().unwrap();
        assert!(
            object::define_own_property(
                object,
                &mut interpreter.gc_heap,
                UNRELATED,
                object::PropertyDescriptor::data(Value::number_i32(11), false, false, false),
            )
            .expect("descriptor fixture allocation")
        );
    });
    assert_ne!(
        object::shape_id(interpreter.global_this, &interpreter.gc_heap),
        structural
    );
    assert_eq!(
        object::dictionary_layout(interpreter.global_this, &interpreter.gc_heap),
        Some(layout)
    );
    let unrelated = bake(&mut interpreter, &context);
    assert_proofs(&interpreter, &unrelated, true, true);
    assert_eq!(unrelated.binding_hit_proofs, before.binding_hit_proofs);
    assert_eq!(unrelated.global_object_loads, before.global_object_loads);

    native_freeze(&mut interpreter, &context);
    assert!(object::is_dictionary(
        interpreter.global_this,
        &interpreter.gc_heap
    ));
    assert_eq!(
        object::dictionary_layout(interpreter.global_this, &interpreter.gc_heap),
        Some(layout + 1)
    );
    let frozen = bake(&mut interpreter, &context);
    assert_proofs(&interpreter, &frozen, true, false);
    assert_ne!(frozen.binding_hit_proofs, before.binding_hit_proofs);
}

fn realm_binding_module() -> otter_bytecode::BytecodeModule {
    let mut module = crate::test_support::minimal_bytecode_module("foreign-binding-admission.js");
    module.constants = vec![Constant::String {
        utf16: NAME.encode_utf16().collect(),
    }];
    module.functions[0].locals = 3;
    let mut code = FunctionCodeBuilder::new();
    code.push(
        Op::LoadGlobalOrThrow,
        &[Operand::Register(0), Operand::ConstIndex(0)],
    );
    code.push(Op::LoadGlobalThis, &[Operand::Register(1)]);
    code.push(
        Op::GlobalBindingExists,
        &[Operand::Register(2), Operand::ConstIndex(0)],
    );
    code.push(Op::Return, &[Operand::Register(0)]);
    module.functions[0].code = code.finish();
    module
}

#[test]
fn foreign_linked_source_never_bakes_ambient_global_proofs_or_inline_body() {
    let (mut interpreter, default) = fixture(true);
    let realm = interpreter
        .create_host_realm()
        .expect("actual source realm");
    let foreign = interpreter
        .with_host_realm(realm, |vm| {
            vm.set_global(NAME, Value::number_i32(19))?;
            vm.link_module(
                realm_binding_module(),
                crate::source_registry::SourceRegistry::default(),
            )
            .map_err(|_| VmError::InvalidOperand)
        })
        .expect("foreign linked source");
    let fid = foreign.function_base();
    assert_eq!(interpreter.function_realm_ids[&fid], realm.0);
    assert_eq!(interpreter.foreign_function_realm(fid), Some(realm.0));
    let source_global = interpreter.global_this_for_function(fid).unwrap();
    assert_ne!(source_global, interpreter.global_this);
    assert_eq!(
        object::get(source_global, &interpreter.gc_heap, NAME),
        Some(Value::number_i32(19))
    );
    assert!(
        !bake(&mut interpreter, &default)
            .binding_hit_proofs
            .is_empty()
    );

    let foreign_view = bake(&mut interpreter, &foreign);
    assert!(foreign_view.binding_hit_proofs.is_empty());
    assert!(foreign_view.global_lexical_loads.is_empty());
    assert!(foreign_view.global_object_loads.is_empty());
    assert!(
        interpreter
            .bake_inline_body(
                &foreign,
                fid,
                jit_debug::JitDebugTier::Optimizing,
                &mut InlineSnapshotBudget::new(),
            )
            .is_none(),
        "source ownership is not same-realm inline admission"
    );

    interpreter
        .with_host_realm(realm, |vm| {
            let own = bake(vm, &foreign);
            let first = own.instructions[0].byte_pc;
            assert!(own.binding_hit_proofs.contains_key(&first));
            assert!(own.global_object_loads.contains_key(&first));
            assert_eq!(own.binding_hit_proofs.len(), 2);
            assert_eq!(
                object::get(vm.global_this, &vm.gc_heap, NAME),
                Some(Value::number_i32(19))
            );
            Ok(())
        })
        .expect("own source realm retains ordinary proof admission");
    assert_eq!(interpreter.active_host_realm_id(), 0);
}

#[test]
fn disposed_source_global_never_substitutes_the_active_global() {
    let mut interpreter = Interpreter::new().expect("actual realm bootstrap");
    let realm = interpreter.create_host_realm().expect("source realm");
    let foreign = interpreter
        .with_host_realm(realm, |vm| {
            vm.link_module(
                realm_binding_module(),
                crate::source_registry::SourceRegistry::default(),
            )
            .map_err(|_| VmError::InvalidOperand)
        })
        .expect("actual source link");
    let fid = foreign.function_base();
    assert!(interpreter.global_this_for_function(fid).is_ok());
    assert!(interpreter.dispose_host_realm(realm));
    assert!(interpreter.extra_realms.is_empty());
    assert_eq!(interpreter.foreign_function_realm(fid), Some(realm.0));
    assert!(matches!(
        interpreter.global_this_for_function(fid),
        Err(VmError::InvalidOperand)
    ));
    assert!(
        bake(&mut interpreter, &foreign)
            .binding_hit_proofs
            .is_empty()
    );
    assert!(
        interpreter
            .bake_inline_body(
                &foreign,
                fid,
                jit_debug::JitDebugTier::Optimizing,
                &mut InlineSnapshotBudget::new(),
            )
            .is_none()
    );
}
