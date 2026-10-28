//! Baking tests: what a compilation reads, roots and asks the isolate to
//! install.

use otter_bytecode::{BytecodeModule, Constant, Function, Instruction, Op, Operand, SourceKind};

use crate::{ActivationStack, Interpreter, Value};

#[test]
fn method_feedback_declines_a_wide_dictionary_holder_without_losing_its_data() {
    let mut module = crate::test_support::minimal_bytecode_module("wide-method-holder.js");
    module.constants = vec![Constant::String {
        utf16: "method".encode_utf16().collect(),
    }];
    let mut interpreter = Interpreter::new().expect("fixture interpreter bootstrap");
    let context = interpreter
        .link_module(module, crate::source_registry::SourceRegistry::default())
        .expect("caller context");
    interpreter.with_handle_scope(|interpreter, scope| {
        let prototype = interpreter.scoped_object_bare(scope).expect("prototype");
        for index in 0..crate::object::MAX_FAST_PROPERTIES {
            let mut object = interpreter.escape_scoped(prototype).as_object().unwrap();
            interpreter
                .create_data_property(
                    &mut object,
                    &format!("p{index}"),
                    Value::number_i32(index as i32),
                )
                .expect("prototype field");
        }
        let mut object = interpreter.escape_scoped(prototype).as_object().unwrap();
        interpreter
            .create_data_property(&mut object, "method", Value::number_i32(317))
            .unwrap();
        let receiver = interpreter
            .scoped_object_with_proto(scope, prototype)
            .expect("receiver");
        let mut value = interpreter.escape_scoped(receiver);
        let mut object = value.as_object().expect("object receiver");
        interpreter.migrate_slow_to_fast(&mut object);
        value = Value::object(object);
        let property = context.property_atom_for_function(0, 0).unwrap();
        let prototype = interpreter.escape_scoped(prototype).as_object().unwrap();
        assert_eq!(
            crate::object::prototype(object, interpreter.gc_heap()),
            Some(prototype),
            "migration retains the exact inherited holder"
        );
        assert!(!crate::object::keyed_shape(object, interpreter.gc_heap()).is_null());
        assert!(crate::object::is_dictionary(
            prototype,
            interpreter.gc_heap()
        ));
        assert!(crate::object::keyed_shape(prototype, interpreter.gc_heap()).is_null());
        assert_eq!(
            crate::object::with_properties(prototype, interpreter.gc_heap(), |properties| {
                properties.keys().count()
            }),
            crate::object::MAX_FAST_PROPERTIES as usize + 1,
            "the real holder exceeds ordinary migration capacity"
        );
        let descriptor =
            crate::object::get_own_descriptor(prototype, interpreter.gc_heap(), "method")
                .expect("wide holder retains its actual own data descriptor");
        assert!(descriptor.writable() && descriptor.enumerable() && descriptor.configurable());
        assert!(
            matches!(descriptor.kind, crate::object::DescriptorKind::Data { value }
            if value == Value::number_i32(317))
        );
        assert!(
            crate::cache_ir::resolve_atom_data_slot(object, interpreter.gc_heap(), property,)
                .is_none(),
            "the central runtime CacheIR owner refuses dictionary holder proofs"
        );
        assert!(
            interpreter
                .method_site_for_receiver(&context, 0, 0, &mut value)
                .is_none()
        );
        assert_eq!(
            crate::object::get(value.as_object().unwrap(), interpreter.gc_heap(), "method"),
            Some(Value::number_i32(317))
        );
    });
}

#[test]
fn retraining_blocks_prewarm_and_inline_body_baking_before_preparation() {
    let mut interpreter = Interpreter::new().expect("fixture interpreter bootstrap");
    let context = interpreter
        .link_module(
            crate::test_support::minimal_bytecode_module("retraining-bake.js"),
            crate::source_registry::SourceRegistry::default(),
        )
        .unwrap();
    interpreter.optimizing_tier_policy.begin_retraining(0);
    assert!(interpreter.prewarm_literal_cells(&context, 0).is_none());
    let mut budget = super::InlineSnapshotBudget::new();
    assert!(
        super::Baker::new(
            &crate::interp::jit_bake_env::BakeEnv::capture(&interpreter),
            None
        )
        .bake_inline_body(
            &context,
            0,
            crate::jit_debug::JitDebugTier::Optimizing,
            &mut budget,
            None,
            None,
            None
        )
        .is_none()
    );
    assert!(interpreter.literal_cells().is_empty());
}

#[test]
fn repeated_inline_site_history_suppresses_the_source_snapshot_guard() {
    let mut interpreter = Interpreter::new().expect("fixture interpreter bootstrap");
    let context = interpreter
        .link_module(
            crate::test_support::minimal_bytecode_module("inline-source-history.js"),
            crate::source_registry::SourceRegistry::default(),
        )
        .unwrap();
    let exit = crate::native_abi::SideExit::new(
        37,
        crate::native_abi::ExitReason::IdentityGuard,
        crate::native_abi::ExitAction::Recompile,
    );
    interpreter.note_jit_optimized_bail_at(&context, 99, 99, exit, (0, 0), false);
    interpreter.note_jit_optimized_bail_at(&context, 100, 100, exit, (0, 0), false);
    let mut source = context.jit_compile_snapshot(0).unwrap();
    super::Baker::new(
        &crate::interp::jit_bake_env::BakeEnv::capture(&interpreter),
        None,
    )
    .bake_optimized_exit_profile(&mut source, 0);
    assert!(source.optimized_exit_reasons[&0].contains(&exit.reason()));
    let mut outer = context.jit_compile_snapshot(0).unwrap();
    super::Baker::new(
        &crate::interp::jit_bake_env::BakeEnv::capture(&interpreter),
        None,
    )
    .bake_optimized_exit_profile(&mut outer, 99);
    assert!(
        outer.optimized_exit_reasons.is_empty(),
        "the diagnostic outer resume PC is not the guarded source operation"
    );
}

#[test]
fn fresh_interpreter_has_no_executable_code_residency() {
    let interpreter = Interpreter::new().expect("fixture interpreter bootstrap");
    assert_eq!(interpreter.jit_code_residency().code_bytes, 0);
    assert_eq!(interpreter.jit_code_residency().unique_code_objects, 0);
}

#[test]
fn prewarm_publishes_one_shared_stable_cell_for_all_literal_sites() {
    let module = BytecodeModule {
        module: "lazy-string-cell.js".to_string(),
        template_sites: Vec::new(),
        source_kind: SourceKind::JavaScript,
        functions: vec![Function {
            id: 0,
            name: "literal".to_string(),
            locals: 2,
            code: vec![
                Instruction {
                    pc: 0,
                    op: Op::LoadString,
                    operands: vec![Operand::Register(0), Operand::ConstIndex(0)],
                },
                Instruction {
                    pc: 1,
                    op: Op::LoadString,
                    operands: vec![Operand::Register(1), Operand::ConstIndex(0)],
                },
                Instruction {
                    pc: 2,
                    op: Op::ReturnValue,
                    operands: vec![Operand::Register(1)],
                },
            ]
            .into(),
            ..Function::default()
        }],
        constants: vec![Constant::String {
            utf16: "cold literal".encode_utf16().collect(),
        }],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
        lazy_environments: Vec::new(),
        function_source: None,
    };
    let mut interpreter = Interpreter::new().expect("fixture interpreter bootstrap");
    let context = interpreter
        .link_module(module, crate::source_registry::SourceRegistry::default())
        .expect("valid bytecode fixture");
    assert!(
        interpreter.prewarm_literal_cells(&context, 0).is_none(),
        "a cold allocation without published frame roots must decline"
    );
    assert_eq!(interpreter.literal_cells().len(), 0);

    let mut stack = ActivationStack::new();
    interpreter.with_runtime_turn(&mut stack, |turn| {
        let (interpreter, _) = turn.into_parts();
        interpreter
            .prewarm_literal_cells(&context, 0)
            .expect("rooted prewarm");
    });
    assert!(
        interpreter.prewarm_literal_cells(&context, 0).is_some(),
        "an already-prepared function needs no active root provider"
    );

    let mut snapshot = context.jit_compile_snapshot(0).expect("snapshot");
    super::Baker::new(
        &crate::interp::jit_bake_env::BakeEnv::capture(&interpreter),
        None,
    )
    .bake_literal_cells(&mut snapshot, &context, 0, true)
    .expect("prepared bake");
    assert_eq!(snapshot.literal_cells.len(), 2);
    let cells = snapshot.literal_cells.values().copied().collect::<Vec<_>>();
    let first = cells[0];
    let second = cells[1];
    assert_eq!(first.cell_addr, second.cell_addr);
    assert!(unsafe { *(first.cell_addr as *const Value) }.is_string());

    interpreter.force_gc().expect("move the prepared literal");
    assert!(unsafe { *(first.cell_addr as *const Value) }.is_string());
    assert!(
        snapshot
            .literal_cells
            .values()
            .all(|cell| cell.cell_addr == first.cell_addr)
    );

    // No production path inserts a cell before successful allocation. If
    // corrupt state nevertheless exposes a hole, preparation and baking
    // both decline instead of publishing that state to generated code.
    **interpreter
        .literal_cells_mut()
        .get_mut(&context.constant_cache_key(0))
        .expect("prepared cell") = Value::hole();
    assert!(interpreter.prewarm_literal_cells(&context, 0).is_none());
    let mut invalid = context
        .jit_compile_snapshot(0)
        .expect("invalid snapshot input");
    assert!(
        super::Baker::new(
            &crate::interp::jit_bake_env::BakeEnv::capture(&interpreter),
            None
        )
        .bake_literal_cells(&mut invalid, &context, 0, true)
        .is_none()
    );
    assert!(invalid.literal_cells.is_empty());
}

#[test]
fn caller_snapshot_survives_forced_gc_then_nested_target_prewarm() {
    let literal_function = |id, constant, name: &str| Function {
        id,
        name: name.to_string(),
        locals: 1,
        code: vec![
            Instruction {
                pc: 0,
                op: Op::LoadString,
                operands: vec![Operand::Register(0), Operand::ConstIndex(constant)],
            },
            Instruction {
                pc: 1,
                op: Op::ReturnValue,
                operands: vec![Operand::Register(0)],
            },
        ]
        .into(),
        ..Function::default()
    };
    let module = BytecodeModule {
        module: "nested-prewarm-gc.js".to_string(),
        template_sites: Vec::new(),
        source_kind: SourceKind::JavaScript,
        functions: vec![
            literal_function(0, 0, "caller"),
            literal_function(1, 1, "nestedTarget"),
        ],
        constants: vec![
            Constant::String {
                utf16: "caller literal".encode_utf16().collect(),
            },
            Constant::String {
                utf16: "nested target literal".encode_utf16().collect(),
            },
        ],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
        lazy_environments: Vec::new(),
        function_source: None,
    };
    let mut interpreter = Interpreter::new().expect("fixture interpreter bootstrap");
    let context = interpreter
        .link_module(module, crate::source_registry::SourceRegistry::default())
        .expect("valid bytecode fixture");
    let mut stack = ActivationStack::new();
    let (caller, nested) = interpreter.with_runtime_turn(&mut stack, |turn| {
        let (interpreter, _) = turn.into_parts();
        interpreter
            .prewarm_literal_cells(&context, 0)
            .expect("caller prewarm");
        let mut caller = context.jit_compile_snapshot(0).expect("caller snapshot");
        super::Baker::new(
            &crate::interp::jit_bake_env::BakeEnv::capture(&interpreter),
            None,
        )
        .bake_literal_cells(&mut caller, &context, 0, true)
        .expect("caller bake");
        let caller_cell = caller.literal_cells[&0].cell_addr;

        // Direct-target baking owns this ordering: the caller snapshot is
        // already live when preparing a nested body may move the heap.
        // Snapshot state is GC-stable and its literal cell is traced and
        // rewritten in place.
        interpreter.collect_minor_tracing_runtime_roots();
        assert!(unsafe { *(caller_cell as *const Value) }.is_string());

        interpreter
            .prewarm_literal_cells(&context, 1)
            .expect("nested target prewarm after moving GC");
        let mut nested = context.jit_compile_snapshot(1).expect("nested snapshot");
        super::Baker::new(
            &crate::interp::jit_bake_env::BakeEnv::capture(&interpreter),
            None,
        )
        .bake_literal_cells(&mut nested, &context, 1, true)
        .expect("nested target bake");
        assert_eq!(caller.literal_cells[&0].cell_addr, caller_cell);
        assert!(unsafe { *(caller_cell as *const Value) }.is_string());
        (caller, nested)
    });

    assert_eq!(caller.literal_cells.len(), 1);
    assert_eq!(nested.literal_cells.len(), 1);
    let result = interpreter
        .run(&context)
        .expect("caller executes after snapshot/GC/nested prewarm");
    assert!(result.is_string());
}

#[test]
fn finalized_shape_baker_refuses_unsupported_state_without_retaining_a_proof() {
    use crate::object::{LookupFact, ShapeState, shape_body};
    let mut interpreter = Interpreter::new().expect("shape baker runtime");
    let context = interpreter
        .link_module(
            crate::test_support::minimal_bytecode_module("shape-baker-nonordinary-prototype.js"),
            crate::source_registry::SourceRegistry::default(),
        )
        .expect("actual linked function prototype");
    let prototype_function = Value::function(context.function_base());
    let mut stack = ActivationStack::new();
    interpreter.with_runtime_turn(&mut stack, |turn| {
        let (vm, _) = turn.into_parts();
        let ordinary = vm
            .object_root(None, 4, ShapeState::ORDINARY)
            .expect("ordinary root");
        let ordinary_id = shape_body::id_of(ordinary);
        assert_eq!(
            super::Baker::new(&crate::interp::jit_bake_env::BakeEnv::capture(vm), None)
                .bake_shape(ordinary),
            Some(ordinary.offset())
        );
        assert_eq!(
            super::Baker::new(&crate::interp::jit_bake_env::BakeEnv::capture(vm), None)
                .bake_shape_id(ordinary_id),
            Some(ordinary.offset())
        );
        // Reads can prove finalized non-extensible/prototype ordinary
        // state; write emitters retain their independent role guard.
        for state in [
            ShapeState::ORDINARY.with_extensible(false),
            ShapeState::ORDINARY.with_prototype_role(true),
        ] {
            let shape = vm
                .object_root(None, 4, state)
                .expect("eligible ordinary state root");
            assert_eq!(
                super::Baker::new(&crate::interp::jit_bake_env::BakeEnv::capture(vm), None)
                    .bake_shape(shape),
                Some(shape.offset())
            );
            assert_eq!(
                super::Baker::new(&crate::interp::jit_bake_env::BakeEnv::capture(vm), None)
                    .bake_shape_id(shape_body::id_of(shape)),
                Some(shape.offset())
            );
        }
        // A provisional constructor lineage is an immutable layout every
        // guard may name; only an allocation plan refuses its root.
        let provisional = vm
            .object_root(None, 4, ShapeState::ORDINARY.with_provisional(true))
            .expect("provisional state root");
        assert_eq!(
            super::Baker::new(&crate::interp::jit_bake_env::BakeEnv::capture(vm), None)
                .bake_shape(provisional),
            Some(provisional.offset())
        );
        assert!(crate::jit::JitObjectLiteralAllocationPlan::new(provisional, 0).is_none());
        let env = crate::interp::jit_bake_env::BakeEnv::capture(vm);
        let mut baker = super::Baker::new(&env, None);
        assert_eq!(baker.bake_shape(crate::object::ShapeHandle::null()), None);
        assert!(baker.roots.is_empty());
        drop(baker);
        for state in [
            ShapeState::ORDINARY.with_lookup(LookupFact::StringWrapper, true),
            ShapeState::ORDINARY.with_lookup(LookupFact::MappedArguments, true),
            ShapeState::ORDINARY.with_lookup(LookupFact::HostLookup, true),
            ShapeState::ORDINARY.with_lookup(LookupFact::NonOrdinaryPrototype, true),
        ] {
            // The sole root owner derives this fact from the actual
            // prototype. A null prototype cannot fabricate that bit.
            let shape = if state.bits() & ShapeState::OPAQUE_PROTOTYPE_MASK != 0 {
                vm.value_root(prototype_function, 4, ShapeState::ORDINARY)
            } else {
                vm.object_root(None, 4, state)
            }
            .expect("actual immutable state root");
            if state.bits() & ShapeState::OPAQUE_PROTOTYPE_MASK != 0 {
                assert!(matches!(
                    vm.gc_heap.read_payload(shape, shape_body::ShapeBody::prototype),
                    shape_body::ShapePrototype::Value(value) if value == prototype_function
                ));
            }
            assert_eq!(shape_body::state_of(shape), state);
            let env = crate::interp::jit_bake_env::BakeEnv::capture(vm);
            let mut baker = super::Baker::new(&env, None);
            assert_eq!(baker.bake_shape(shape), None);
            assert_eq!(baker.bake_shape_id(shape_body::id_of(shape)), None);
            assert!(baker.roots.is_empty());
            drop(baker);
            assert!(crate::jit::JitObjectLiteralAllocationPlan::new(shape, 0).is_none());
        }
        let dictionary = shape_body::dictionary_of(ordinary);
        assert!(shape_body::state_of(dictionary).is_dictionary());
        let env = crate::interp::jit_bake_env::BakeEnv::capture(vm);
        let mut baker = super::Baker::new(&env, None);
        assert_eq!(baker.bake_shape(dictionary), None);
        assert!(baker.roots.is_empty());
        drop(baker);
        assert!(crate::jit::JitObjectLiteralAllocationPlan::new(dictionary, 0).is_none());

        // Pinned primitive String holders are an explicit dictionary
        // proof domain with nonvirtual names, never an ordinary shape.
        for name in ["charCodeAt", "indexOf"] {
            let call = super::Baker::new(&crate::interp::jit_bake_env::BakeEnv::capture(vm), None)
                .jit_primitive_method_call(name)
                .expect("actual pinned String leaf proof");
            assert!(matches!(
                call.holder,
                crate::jit::JitMethodHolder::Dictionary(_)
            ));
            assert_eq!(call.safepoint_id, crate::native_abi::NO_SAFEPOINT);
        }
    });
}
