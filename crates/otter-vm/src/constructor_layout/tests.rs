//! Physical first-seven cells and canonical completion ownership proofs.
//!
//! # Contents
//! - Seven bounded samples, static minimum and future-only root publication.
//! - Actual new.target identity and reentrant prototype-family replacement.
//! - Super transfer retains the original receiver through object replacement.
//!
//! # Invariants
//! All moving values are built/rooted in production handle scopes. These
//! tests exercise the production family/terminal kernels and inspect real GC
//! cells and immutable shapes; they do not replay constructor observations.
//!
//! # See also
//! - `otter-runtime/tests/jit_stack_owned_array_construct` owns native call fits.

use super::*;
use crate::native_abi::{Frame, VmFrameHeader};
use crate::{Interpreter, Value, object};

fn receiver(vm: &mut Interpreter, root: ShapeHandle, fields: usize) -> Value {
    let mut shape = root;
    for index in 0..fields {
        shape = vm
            .shape_runtime
            .child_with_roots(
                &mut vm.gc_heap,
                shape,
                &format!("f{index}"),
                object::PropertyFlags::data_default(),
                false,
                &mut |_| {},
            )
            .expect("receiver's source fields");
    }
    let object = vm
        .alloc_runtime_rooted_object_with_shape(root, &[], &[])
        .expect("physical receiver cell");
    let values = vec![Value::UNDEFINED; fields];
    object::install_fresh_shape_with_slots(object, &mut vm.gc_heap, shape, &values, fields);
    Value::object(object)
}
fn ticket(layout: ConstructorLayout, receiver: Value) -> Frame {
    let mut frame = Frame::new(
        VmFrameHeader::interpreter(1, 0),
        0,
        Value::function(1),
        receiver,
    );
    frame.set_construct();
    frame.construct_layout = layout;
    frame.construct_receiver = receiver;
    frame
}

#[test]
fn seventh_terminal_sample_changes_only_future_physical_cells() {
    let mut vm = Interpreter::new().expect("constructor fixture interpreter");
    vm.with_handle_scope(|vm, scope| {
        let prototype = vm.scoped_object(scope).unwrap();
        let target = Value::function(1);
        let layout = vm
            .constructor_layout_for_receiver(1, target, vm.escape_scoped(prototype), |_, _| 4)
            .unwrap();
        let provisional = vm.gc_heap.read_payload(layout, ConstructorLayoutBody::root);
        assert_eq!(object::shape_body::inline_capacity_of(provisional), 64);
        assert!(object::shape_body::state_of(provisional).is_provisional());
        let mut retained = Vec::new();
        for (index, count) in [1, 2, 3, 4, 5, 6, 3].into_iter().enumerate() {
            let value = receiver(vm, provisional, count);
            retained.push(vm.scoped_value(scope, value));
            let mut frame = ticket(layout, value);
            vm.complete_constructor_layout(&mut frame);
            // Terminal callbacks are also invoked on unwind/replacement. The
            // family kernel consumes only the physical receiver's own fields.
            vm.complete_constructor_layout(&mut frame);
            assert_eq!(
                vm.gc_heap
                    .read_payload(layout, ConstructorLayoutBody::samples_remaining),
                6 - index as u8
            );
            assert!(frame.construct_layout.is_null());
            assert!(frame.construct_receiver.is_undefined());
            assert_eq!(
                vm.gc_heap.read_payload(layout, ConstructorLayoutBody::root),
                provisional,
                "completion cannot allocate, shrink or publish a final root"
            );
        }
        assert_eq!(
            vm.gc_heap
                .read_payload(layout, ConstructorLayoutBody::final_capacity),
            6
        );
        let again = vm
            .constructor_layout_for_receiver(1, target, vm.escape_scoped(prototype), |_, _| 4)
            .unwrap();
        assert_eq!(
            again, layout,
            "finalization preserves exact family identity"
        );
        let final_root = vm.gc_heap.read_payload(layout, ConstructorLayoutBody::root);
        assert_ne!(final_root, provisional);
        assert!(
            vm.gc_heap
                .read_payload(layout, ConstructorLayoutBody::finalized)
        );
        assert_eq!(object::shape_body::inline_capacity_of(final_root), 6);
        let future = receiver(vm, final_root, 4);
        let future = vm.scoped_value(scope, future);
        for handle in retained {
            let object = vm.escape_scoped(handle).as_object().unwrap();
            let shape = object::shape(object, &vm.gc_heap);
            assert_eq!(object::shape_body::inline_capacity_of(shape), 64);
            assert!(object::shape_body::state_of(shape).is_provisional());
            // SAFETY: the handle scope retains this live receiver cell.
            assert_eq!(
                unsafe { (*object.as_header_ptr()).size_bytes() } as usize,
                otter_gc::header::HEADER_SIZE + std::mem::size_of::<object::ObjectBody>() + 64 * 8
            );
        }
        let object = vm.escape_scoped(future).as_object().unwrap();
        // SAFETY: as above; this is the newly allocated future receiver.
        assert_eq!(
            unsafe { (*object.as_header_ptr()).size_bytes() } as usize,
            otter_gc::header::HEADER_SIZE + std::mem::size_of::<object::ObjectBody>() + 6 * 8
        );
        let mut later = ticket(layout, vm.escape_scoped(future));
        vm.complete_constructor_layout(&mut later);
        assert_eq!(
            vm.gc_heap
                .read_payload(layout, ConstructorLayoutBody::final_capacity),
            6,
            "an eighth receiver never restarts or enlarges the finalized sampling contract"
        );
    });
}

#[test]
fn changed_prototype_and_sibling_closures_select_distinct_families() {
    let mut vm = Interpreter::new().expect("constructor fixture interpreter");
    vm.with_handle_scope(|vm, scope| {
        let first_proto = vm.scoped_object(scope).unwrap();
        let second_proto = vm.scoped_object(scope).unwrap();
        let mut closures = Vec::new();
        for _ in 0..2 {
            let closure = crate::closure::alloc_closure_with_roots(
                &mut vm.gc_heap,
                9,
                Value::UNDEFINED,
                None,
                None,
                &mut |_| {},
            )
            .unwrap();
            closures.push(vm.scoped_value(scope, Value::closure(closure)));
        }
        let first = vm
            .constructor_layout_for_receiver(
                3,
                vm.escape_scoped(closures[0]),
                vm.escape_scoped(first_proto),
                |_, _| 2,
            )
            .unwrap();
        let sibling = vm
            .constructor_layout_for_receiver(
                3,
                vm.escape_scoped(closures[1]),
                vm.escape_scoped(first_proto),
                |_, _| 2,
            )
            .unwrap();
        assert_ne!(
            first, sibling,
            "actual function objects, not their shared template, own samples"
        );
        let initial_root = vm.gc_heap.read_payload(first, ConstructorLayoutBody::root);
        let old_receiver = receiver(vm, initial_root, 1);
        let old_receiver = vm.scoped_value(scope, old_receiver);
        let mut frame = ticket(first, vm.escape_scoped(old_receiver));
        // SAFETY: this local frame remains initialized and at a stable address
        // through the matching pop. It owns the exact original family ticket.
        unsafe {
            vm.jit_push_native_frame(&mut frame).unwrap();
        }
        let changed = vm
            .constructor_layout_for_receiver(
                3,
                vm.escape_scoped(closures[0]),
                vm.escape_scoped(second_proto),
                |_, _| 2,
            )
            .unwrap();
        assert_ne!(
            vm.gc_heap
                .read_payload(first, ConstructorLayoutBody::family_id),
            vm.gc_heap
                .read_payload(changed, ConstructorLayoutBody::family_id)
        );
        vm.complete_constructor_layout(&mut frame);
        vm.jit_pop_native_frame();
        assert_eq!(
            vm.gc_heap
                .read_payload(first, ConstructorLayoutBody::samples_remaining),
            6
        );
        assert_eq!(
            vm.gc_heap
                .read_payload(changed, ConstructorLayoutBody::samples_remaining),
            7,
            "reentry cannot redirect an in-flight terminal sample to the replacement family"
        );
        assert_eq!(
            vm.gc_heap
                .read_payload(sibling, ConstructorLayoutBody::samples_remaining),
            7
        );
    });
}

#[test]
fn super_ticket_samples_original_receiver_even_when_this_becomes_replacement() {
    let mut vm = Interpreter::new().expect("constructor fixture interpreter");
    vm.with_handle_scope(|vm, scope| {
        let proto = vm.scoped_object(scope).unwrap();
        let layout = vm
            .constructor_layout_for_receiver(
                1,
                Value::function(1),
                vm.escape_scoped(proto),
                |_, _| 2,
            )
            .unwrap();
        let root = vm.gc_heap.read_payload(layout, ConstructorLayoutBody::root);
        let original = receiver(vm, root, 5);
        let original = vm.scoped_value(scope, original);
        let replacement = vm.scoped_object(scope).unwrap();
        let mut base = ticket(layout, vm.escape_scoped(original));
        let mut outer = Frame::new(
            VmFrameHeader::interpreter(2, 0),
            0,
            Value::function(2),
            Value::hole(),
        );
        outer.set_derived_constructor();
        vm.transfer_super_constructor_layout(&mut base, &mut outer);
        outer.this_value = vm.escape_scoped(replacement);
        vm.complete_constructor_layout(&mut base);
        assert_eq!(
            vm.gc_heap
                .read_payload(layout, ConstructorLayoutBody::samples_remaining),
            7,
            "inner super completion does not consume the outer construction's sample"
        );
        vm.complete_constructor_layout(&mut outer);
        assert_eq!(
            vm.gc_heap
                .read_payload(layout, ConstructorLayoutBody::samples_remaining),
            6
        );
        assert_eq!(
            vm.gc_heap
                .read_payload(layout, ConstructorLayoutBody::final_capacity),
            5,
            "the allocated receiver, not replacement this, teaches physical usage"
        );
    });
}

#[test]
fn abrupt_super_retry_and_second_success_sample_every_allocated_receiver_once() {
    let mut vm = Interpreter::new().expect("constructor fixture interpreter");
    vm.with_handle_scope(|vm, scope| {
        let proto = vm.scoped_object(scope).unwrap();
        let layout = vm
            .constructor_layout_for_receiver(
                1,
                Value::function(1),
                vm.escape_scoped(proto),
                |_, _| 0,
            )
            .unwrap();
        let root = vm.gc_heap.read_payload(layout, ConstructorLayoutBody::root);
        let mut receivers = Vec::new();
        for fields in [2, 5, 6] {
            let value = receiver(vm, root, fields);
            receivers.push(vm.scoped_value(scope, value));
        }
        let mut outer = Frame::new(
            VmFrameHeader::interpreter(2, 0),
            0,
            Value::function(2),
            Value::hole(),
        );
        outer.set_derived_constructor();
        // A base constructor throws before installing its result in outer.
        // Catch/retry must not silently lose that allocated physical receiver.
        let mut abrupt = ticket(layout, vm.escape_scoped(receivers[0]));
        vm.complete_constructor_layout(&mut abrupt);
        vm.complete_constructor_layout(&mut abrupt);
        assert_eq!(
            vm.gc_heap
                .read_payload(layout, ConstructorLayoutBody::samples_remaining),
            6
        );
        assert!(outer.construct_layout.is_null());
        // Retried successful super transfers exactly one ticket to outer.
        let mut retry = ticket(layout, vm.escape_scoped(receivers[1]));
        vm.transfer_super_constructor_layout(&mut retry, &mut outer);
        vm.complete_constructor_layout(&mut retry);
        assert_eq!(
            vm.gc_heap
                .read_payload(layout, ConstructorLayoutBody::samples_remaining),
            6
        );
        let original_outer_receiver = outer.construct_receiver;
        // A second super can allocate successfully before InitializeThisBinding
        // throws. Its own terminal is still one sample; the first receiver
        // stays rooted on outer and later teaches its own terminal once.
        let mut repeated = ticket(layout, vm.escape_scoped(receivers[2]));
        vm.transfer_super_constructor_layout(&mut repeated, &mut outer);
        vm.complete_constructor_layout(&mut repeated);
        assert_eq!(
            vm.gc_heap
                .read_payload(layout, ConstructorLayoutBody::samples_remaining),
            5
        );
        assert_eq!(outer.construct_receiver, original_outer_receiver);
        assert_eq!(outer.construct_layout, layout);
        vm.complete_constructor_layout(&mut outer);
        vm.complete_constructor_layout(&mut outer);
        assert_eq!(
            vm.gc_heap
                .read_payload(layout, ConstructorLayoutBody::samples_remaining),
            4
        );
        assert_eq!(
            vm.gc_heap
                .read_payload(layout, ConstructorLayoutBody::final_capacity),
            6
        );
        assert!(outer.construct_layout.is_null());
        assert!(outer.construct_receiver.is_undefined());
    });
}

#[test]
fn original_receiver_is_rewritten_through_published_frame_ticket() {
    let mut vm = Interpreter::new().expect("constructor fixture interpreter");
    vm.with_handle_scope(|vm, scope| {
        let proto = vm.scoped_object(scope).unwrap();
        let layout = vm
            .constructor_layout_for_receiver(
                1,
                Value::function(1),
                vm.escape_scoped(proto),
                |_, _| 0,
            )
            .unwrap();
        let root = vm.gc_heap.read_payload(layout, ConstructorLayoutBody::root);
        let original = receiver(vm, root, 3);
        // The helper installs the receiver's three own-field transitions. GC
        // must retain that exact descendant, rather than reset to its empty
        // family root or repoint the first-seven physical cell.
        let original_shape = object::shape(original.as_object().unwrap(), &vm.gc_heap);
        assert_ne!(original_shape, root);
        assert_eq!(object::shape_body::inline_capacity_of(original_shape), 64);
        assert!(object::shape_body::state_of(original_shape).is_provisional());
        let before = original.to_abi_bits();
        let mut frame = ticket(layout, original);
        // The arena does not own this receiver: only the published Frame's
        // canonical original receiver slot keeps it live across collection.
        frame.this_value = Value::UNDEFINED;
        // SAFETY: initialized local Frame remains at this address through pop.
        unsafe {
            vm.jit_push_native_frame(&mut frame).unwrap();
        }
        vm.collect_minor_tracing_runtime_roots();
        assert_ne!(
            frame.construct_receiver.to_abi_bits(),
            before,
            "the tested canonical original-receiver root must actually relocate"
        );
        assert_eq!(
            object::shape(frame.construct_receiver.as_object().unwrap(), &vm.gc_heap),
            original_shape,
            "actual relocation preserves the exact provisional own-field lineage"
        );
        assert_eq!(
            vm.gc_heap.read_payload(
                frame.construct_receiver.as_object().unwrap(),
                object::ObjectBody::slot_count
            ),
            3
        );
        vm.complete_constructor_layout(&mut frame);
        vm.jit_pop_native_frame();
        assert_eq!(
            vm.gc_heap
                .read_payload(layout, ConstructorLayoutBody::samples_remaining),
            6
        );
        assert_eq!(
            vm.gc_heap
                .read_payload(layout, ConstructorLayoutBody::final_capacity),
            3
        );
        assert!(frame.construct_layout.is_null());
        assert!(frame.construct_receiver.is_undefined());
    });
}

#[test]
fn proxy_bound_and_native_actual_targets_trace_distinct_family_heads() {
    fn native_constructor(
        _: &mut crate::NativeCtx<'_>,
        _: &[Value],
    ) -> Result<Value, crate::native_function::NativeError> {
        Ok(Value::UNDEFINED)
    }
    let mut vm = Interpreter::new().expect("constructor fixture interpreter");
    vm.with_handle_scope(|vm, scope| {
        let prototype = vm.scoped_object(scope).unwrap();
        let target = vm.scoped_value(scope, Value::function(4));
        let handler = vm.scoped_object(scope).unwrap();
        let proxy = vm.scoped_proxy(scope, target, handler).unwrap();
        let bound = crate::test_support::alloc_bound_function(
            vm,
            vm.escape_scoped(target),
            Value::UNDEFINED,
            &[],
        )
        .unwrap();
        let bound = vm.scoped_value(scope, Value::bound_function(bound));
        let native = crate::native_function::NativeFunction::new_constructor_static_with_roots(
            &mut vm.gc_heap,
            "newTargetOwner",
            0,
            native_constructor,
            &mut |_| {},
        )
        .unwrap();
        let native = vm.scoped_value(scope, Value::native_function(native));
        let mut identities = Vec::new();
        for target in [proxy, bound, native] {
            let layout = vm
                .constructor_layout_for_receiver(
                    1,
                    vm.escape_scoped(target),
                    vm.escape_scoped(prototype),
                    |_, _| 0,
                )
                .unwrap();
            identities.push(
                vm.gc_heap
                    .read_payload(layout, ConstructorLayoutBody::family_id),
            );
        }
        assert_ne!(identities[0], identities[1]);
        assert_ne!(identities[0], identities[2]);
        assert_ne!(
            identities[1], identities[2],
            "actual wrapper/native objects own heads, never underlying template aliases"
        );
        vm.force_gc()
            .expect("full GC traces the existing GC-owned family heads");
        for (target, identity) in [proxy, bound, native].into_iter().zip(identities) {
            let value = vm.escape_scoped(target);
            let head = if let Some(proxy) = value.as_proxy() {
                proxy.constructor_layouts(&vm.gc_heap)
            } else if let Some(bound) = value.as_bound_function() {
                bound.constructor_layouts(&vm.gc_heap)
            } else {
                value
                    .as_native_function()
                    .unwrap()
                    .constructor_layouts(&vm.gc_heap)
            };
            assert!(!head.is_null());
            assert_eq!(
                vm.gc_heap
                    .read_payload(head, ConstructorLayoutBody::family_id),
                identity
            );
            assert_eq!(
                vm.gc_heap.read_payload(head, |body| body.prototype),
                vm.escape_scoped(prototype)
            );
            assert_eq!(
                vm.gc_heap
                    .read_payload(head, ConstructorLayoutBody::samples_remaining),
                7
            );
        }
        // The code-census probe is a valid family payload: its shape comes
        // from the actual retained native owner. A null root is not an admitted
        // constructor layout and would fail before testing base-ID ownership.
        let native_head = vm
            .escape_scoped(native)
            .as_native_function()
            .unwrap()
            .constructor_layouts(&vm.gc_heap);
        let provisional_root = vm
            .gc_heap
            .read_payload(native_head, ConstructorLayoutBody::root);
        assert_eq!(object::shape_body::inline_capacity_of(provisional_root), 64);
        assert!(object::shape_body::state_of(provisional_root).is_provisional());
        let body = ConstructorLayoutBody::new(
            vm.constructor_families.allocate_id(),
            12345,
            super::ConstructorFamilyOwner::Other,
            0,
            0,
            Value::UNDEFINED,
            provisional_root,
            ConstructorLayout::null(),
        );
        let mut ids = Vec::new();
        body.visit_function_ids(&mut |id| ids.push(id));
        assert!(
            ids.is_empty(),
            "a layout selection base ID must not retain otherwise dead code"
        );
    });
}

fn derived_context_source() -> crate::ExecutionContext {
    use otter_bytecode::{
        ContextCoord, Function, FunctionCodeBuilder, Op, Operand, ScopeDescriptor, ScopeFlags,
        ScopeKind, SlotDescriptor, SlotKind,
    };
    let mut module = crate::test_support::minimal_bytecode_module("derived-context-owner.js");
    module.functions[0].is_derived_constructor = true;
    module.functions[0].scopes = vec![ScopeDescriptor {
        kind: ScopeKind::Body,
        flags: ScopeFlags::default(),
        slots: vec![SlotDescriptor {
            name: "this".into(),
            kind: SlotKind::DerivedThis,
            exported: false,
        }],
    }];
    // A constructor with an own DerivedThis cell must complete through the
    // canonical derived-return instruction. The test publishes this cell through
    // the real CreateContext owner below; this source supplies its verified scope
    // identity without executing the constructor body.
    let mut constructor_code = FunctionCodeBuilder::new();
    constructor_code.push(
        Op::ReturnDerived,
        &[
            Operand::Register(0),
            Operand::Register(1),
            Operand::Imm32(
                ContextCoord::new(0, 0)
                    .expect("own derived-this coordinate")
                    .to_imm32(),
            ),
        ],
    );
    module.functions[0].code = constructor_code.finish();
    let mut code = FunctionCodeBuilder::new();
    code.push(Op::ReturnUndefined, &[]);
    module.functions.push(Function {
        id: 1,
        name: "lexicalArrow".into(),
        is_arrow: true,
        code: code.finish(),
        ..Function::default()
    });
    crate::ExecutionContext::from_module(module, crate::source_registry::SourceRegistry::default())
        .expect("verified descriptor identity fixture")
}

#[test]
fn own_derived_context_root_moves_and_survives_park_tier_and_exact_recursion_matching() {
    let source = derived_context_source();
    let mut vm = Interpreter::new().expect("derived-context interpreter");
    vm.gc_heap.set_gc_stress(0, false);
    let mut slots = [Value::UNDEFINED; 2];
    let mut outer = Frame::new(
        VmFrameHeader::interpreter(0, 2),
        slots.as_mut_ptr() as u64,
        Value::function(0),
        Value::hole(),
    );
    outer.set_derived_constructor();
    // SAFETY: initialized frame and slots retain these addresses through pop.
    unsafe {
        vm.jit_push_native_frame(&mut outer).unwrap();
    }
    vm.frame_create_context(
        &source,
        &mut crate::ActiveFrameMut::from_frame(&mut outer),
        0,
        1,
        0,
    )
    .expect("actual CreateContext semantic owner");
    assert_eq!(outer.derived_this_context, slots[0].as_context().unwrap());
    let before = outer.derived_this_context;
    slots[0] = Value::UNDEFINED;
    assert!(
        slots[0].is_undefined(),
        "ordinary slot cannot keep the context alive"
    );
    // Only the new canonical field roots this cell at the tested collection.
    vm.collect_minor_tracing_runtime_roots();
    assert_ne!(
        outer.derived_this_context, before,
        "actual minor relocation, not a root count"
    );
    assert_eq!(
        crate::context::scope_identity(&vm.gc_heap, outer.derived_this_context),
        (0, 0)
    );
    assert!(super::lexical::this_is_unbound(&vm, &source, &outer).unwrap());
    let owned = outer.derived_this_context;
    outer.enter_compiled(crate::native_abi::NativeFrameKind::Optimizing);
    assert_eq!(outer.derived_this_context, owned);
    assert!(
        outer.enter_interpreter(),
        "same-record deopt/tier transition retains identity"
    );
    let restored = crate::frame_state::ParkedFrameState::copy_from_active(&outer).into_prepared();
    assert_eq!(restored.derived_this_context, owned);
    assert_eq!(restored.packet().derived_this_context, owned);

    // A recursive constructor with the same source descriptor has its OWN
    // context. Its parameter may hold the older context without owning it.
    let mut recursive_slots = [Value::context(owned), Value::UNDEFINED];
    let mut recursive = Frame::new(
        VmFrameHeader::interpreter(0, 2),
        recursive_slots.as_mut_ptr() as u64,
        Value::function(0),
        Value::hole(),
    );
    recursive.set_derived_constructor();
    unsafe {
        vm.jit_push_native_frame(&mut recursive).unwrap();
    }
    vm.frame_create_context(
        &source,
        &mut crate::ActiveFrameMut::from_frame(&mut recursive),
        1,
        1,
        0,
    )
    .unwrap();
    assert_ne!(recursive.derived_this_context, outer.derived_this_context);
    let closure = crate::closure::alloc_closure_with_roots(
        &mut vm.gc_heap,
        1,
        Value::context(outer.derived_this_context),
        Some(Value::hole()),
        None,
        &mut |_| {},
    )
    .expect("actual lexical SELF context");
    let mut arrow = Frame::new(
        VmFrameHeader::interpreter(1, 0),
        0,
        Value::closure(closure),
        Value::hole(),
    );
    unsafe {
        vm.jit_push_native_frame(&mut arrow).unwrap();
    }
    vm.collect_minor_tracing_runtime_roots();
    let selected = super::lexical::owner(&vm, &source, &mut arrow)
        .unwrap()
        .unwrap();
    assert_eq!(
        selected, &mut outer as *mut Frame,
        "same-FID recursive parameter/SELF roots cannot claim the older binding"
    );
    let captured = arrow
        .self_value
        .as_closure(&vm.gc_heap)
        .unwrap()
        .context(&vm.gc_heap)
        .as_context()
        .unwrap();
    assert_eq!(
        captured, outer.derived_this_context,
        "collector rewrites both exact identity aliases to the same current cell"
    );
    assert_ne!(captured, recursive.derived_this_context);
    assert_eq!(
        recursive_slots[0].as_context().unwrap(),
        captured,
        "an unrelated recursive parameter is a relocated alias, never the owner"
    );
    assert!(crate::context::write_slot(
        &mut vm.gc_heap,
        captured,
        0,
        Value::number_i32(41)
    ));
    assert!(
        outer.this_value.is_hole(),
        "Access::Outer does not mirror Frame.this"
    );
    assert!(
        !super::lexical::this_is_unbound(&vm, &source, &outer).unwrap(),
        "the actual descriptor slot owns repeated lexical super rejection"
    );
    // An escaped caller with no published ancestor is local even if its SELF
    // keeps exactly the same context alive: no function/new.target fallback.
    let mut escaped = Frame::new(
        VmFrameHeader::interpreter(1, 0),
        0,
        arrow.self_value,
        Value::hole(),
    );
    assert!(
        super::lexical::owner(&vm, &source, &mut escaped)
            .unwrap()
            .is_none()
    );
    vm.jit_pop_native_frame();
    vm.jit_pop_native_frame();
    vm.jit_pop_native_frame();
}
