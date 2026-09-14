//! Native and inline activation visibility at observable reentry boundaries.
//!
//! # Contents
//! - Stack order, moving roots, source validation and lexical cleanup.
//!
//! # Invariants
//! - Tests use the same canonical native frame and rooted runtime turn as JIT.

use crate::{ActivationStack, ExecutionContext, Frame, Interpreter, JsString, Value};
use crate::{
    BytecodeModule, RuntimeCall,
    deopt::{DeoptFrame, DeoptFrameEntry},
    jit::VmRuntimeActivation,
    native_abi::{NativeFrame, NativeFrameKind, VmFrameHeader},
};
use otter_bytecode::{Function, Instruction, Op, SourceKind};
use std::ptr::NonNull;

fn context() -> ExecutionContext {
    ExecutionContext::from_module(BytecodeModule {
        module: "inline-activation.js".into(),
        source_kind: SourceKind::JavaScript,
        functions: (0..3)
            .map(|id| Function {
                id,
                name: ["outer", "helper", "getter"][id as usize].into(),
                param_count: 1,
                code: vec![Instruction {
                    pc: 0,
                    op: Op::ReturnUndefined,
                    operands: vec![],
                }]
                .into(),
                ..Function::default()
            })
            .collect(),
        constants: vec![],
        template_sites: vec![],
        module_resolutions: vec![],
        module_inits: vec![],
        function_source: None,
    })
    .unwrap()
}

fn native(function_id: u32, registers: &mut [Value]) -> NativeFrame {
    let mut frame = NativeFrame::new(
        VmFrameHeader {
            kind: NativeFrameKind::Optimizing,
            ..VmFrameHeader::interpreter(function_id, registers.len() as u16)
        },
        registers.as_mut_ptr() as u64,
        Value::function(function_id),
        Value::undefined(),
    );
    frame.set_stack_registers();
    frame
}

fn recipe(context: &ExecutionContext, function_id: u32, value: Value) -> DeoptFrame<Value> {
    DeoptFrame {
        function_id,
        byte_pc: context
            .exec_function(function_id)
            .unwrap()
            .instruction_byte_pc(0)
            .unwrap(),
        entry: Some(DeoptFrameEntry {
            new_target: Value::undefined(),
            return_register: 0,
            this: value,
            closure: Value::function(function_id),
        }),
        slots: vec![value; context.exec_function(function_id).unwrap().register_count as usize]
            .into(),
    }
}

#[test]
fn inline_activation_reentry_preserves_stack_and_moving_roots() {
    for stack_owned in [false, true] {
        let context = context();
        let mut vm = Interpreter::new();
        let mut stack = ActivationStack::new();
        let mut root_registers = [Value::number_i32(7)];
        let mut root = native(0, &mut root_registers);
        if !stack_owned {
            let mut window = vm.alloc_reg_window(1).unwrap();
            window[0] = root_registers[0];
            let mut frame = Frame::with_exec_return_upvalues_and_this(
                context.exec_function(0).unwrap(),
                None,
                Box::default(),
                Value::undefined(),
                window,
            );
            frame.self_value = Value::function(0);
            root.header.flags = Default::default();
            root.register_base = window.as_mut_ptr() as u64;
            stack.push(frame);
        }
        // SAFETY: root and its register window are stationary until the matching pop.
        unsafe {
            vm.jit_push_native_frame(&mut root).unwrap();
        }
        vm.with_runtime_turn(&mut stack, |turn| {
            let (vm, stack) = turn.into_parts();
            let value =
                Value::string(JsString::from_str("inline-live-value", &mut vm.gc_heap).unwrap());
            let closure = crate::closure::alloc_closure(
                &mut vm.gc_heap,
                1,
                vec![],
                Some(value),
                None,
                None,
                None,
            )
            .unwrap();
            // Closure allocation roots and may move its pending this value.
            let value = closure.bound_this(&vm.gc_heap).unwrap();
            let mut recipes = [recipe(&context, 1, value)];
            recipes[0].entry.as_mut().unwrap().closure = Value::closure(closure);
            let vm_ptr = std::ptr::from_mut(vm);
            let stack_ptr = std::ptr::from_mut(stack);
            let mut activation = VmRuntimeActivation::new(vm, stack, &context, 0);
            // SAFETY: the rooted turn, activation, and already-published root stay live.
            let mut call = unsafe {
                RuntimeCall::bind(NonNull::from(&mut activation), NonNull::from(&mut root))
            }
            .unwrap();
            let result = call
                .with_inline_activations(&mut recipes, |inner| {
                    assert_eq!(inner.function_id(), 1);
                    // SAFETY: use the bound services only inside this exclusive cold operation.
                    let vm = unsafe { &mut *vm_ptr };
                    let stack = unsafe { &mut *stack_ptr };
                    assert_eq!(vm.logical_call_depth(stack), 2);
                    let window = vm.alloc_reg_window(1).unwrap();
                    let mut getter = Frame::with_exec_return_upvalues_and_this(
                        context.exec_function(2).unwrap(),
                        None,
                        Box::default(),
                        Value::undefined(),
                        window,
                    );
                    getter.self_value = Value::function(2);
                    stack.push(getter);
                    let names: Vec<_> = vm
                        .snapshot_active_frames(&context, stack, 10)
                        .into_iter()
                        .map(|frame| frame.function_name)
                        .collect();
                    assert_eq!(names, ["getter", "helper", "outer"]);
                    vm.force_gc().unwrap();
                    let first = inner.read(0).unwrap();
                    assert_eq!(
                        first
                            .as_string(&vm.gc_heap)
                            .unwrap()
                            .to_lossy_string(&vm.gc_heap),
                        "inline-live-value"
                    );
                    // SAFETY: this native frame remains published for the cold scope.
                    let helper = unsafe { inner.frame.as_ref() };
                    assert_eq!(helper.this_value(), first);
                    let thrown = inner.take_js_throw(crate::VmError::TypeMismatch).unwrap();
                    let captured =
                        crate::object::error_stack_frames(thrown.as_object().unwrap(), &vm.gc_heap)
                            .unwrap();
                    assert_eq!(
                        captured
                            .iter()
                            .map(|frame| frame.function_name.as_str())
                            .collect::<Vec<_>>(),
                        ["getter", "helper", "outer"]
                    );
                    let getter = stack.pop().unwrap();
                    vm.free_reg_window(getter.registers.stack_base());
                    inner.read(0).unwrap()
                })
                .unwrap();
            assert_eq!(call.function_id(), 0);
            assert_eq!(recipes[0].slots[0], result);
            assert_eq!(recipes[0].entry.as_ref().unwrap().this, result);
            let vm = unsafe { &*vm_ptr };
            let exact_closure = recipes[0]
                .entry
                .as_ref()
                .unwrap()
                .closure
                .as_closure(&vm.gc_heap)
                .unwrap();
            assert_eq!(exact_closure.function_id(), 1);
            assert_eq!(exact_closure.bound_this(&vm.gc_heap), Some(result));
            assert_eq!(unsafe { &*vm_ptr }.jit_native_activation_top, 1);
        });
        vm.jit_pop_native_activation();
        assert_eq!(vm.jit_native_activation_top, 0);
    }
}

#[test]
fn inline_activation_validation_and_unwind_leave_caller_published() {
    let context = context();
    let mut vm = Interpreter::new();
    let mut stack = ActivationStack::new();
    let mut registers = [Value::undefined()];
    let mut root = native(0, &mut registers);
    unsafe {
        vm.jit_push_native_frame(&mut root).unwrap();
    }
    let vm_ptr = std::ptr::from_mut(&mut vm);
    let mut activation = VmRuntimeActivation::new(&mut vm, &mut stack, &context, 0);
    let mut call =
        unsafe { RuntimeCall::bind(NonNull::from(&mut activation), NonNull::from(&mut root)) }
            .unwrap();
    let mut invalid = [
        recipe(&context, 1, Value::undefined()),
        recipe(&context, 2, Value::undefined()),
    ];
    invalid[1].byte_pc = u32::MAX;
    let mut entered = false;
    assert!(
        call.with_inline_activations(&mut invalid, |_| entered = true)
            .is_err()
    );
    assert!(!entered);
    assert_eq!(unsafe { &*vm_ptr }.jit_native_activation_top, 1);
    invalid[1] = recipe(&context, 2, Value::undefined());
    invalid[1].entry.as_mut().unwrap().closure = Value::function(0);
    assert!(
        call.with_inline_activations(&mut invalid, |_| entered = true)
            .is_err()
    );
    assert!(!entered);
    assert_eq!(unsafe { &*vm_ptr }.jit_native_activation_top, 1);
    let mut valid = [recipe(&context, 1, Value::undefined())];
    let outcome: Result<(), crate::VmError> = call
        .with_inline_activations(&mut valid, |_| Err(crate::VmError::Uncaught))
        .unwrap();
    assert!(matches!(outcome, Err(crate::VmError::Uncaught)));
    assert_eq!(unsafe { &*vm_ptr }.jit_native_activation_top, 1);
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = call.with_inline_activations(&mut valid, |_| panic!("cold operation panic"));
    }));
    assert!(failure.is_err());
    assert_eq!(call.function_id(), 0);
    assert_eq!(unsafe { &*vm_ptr }.jit_native_activation_top, 1);
    vm.jit_pop_native_activation();
}

#[test]
fn inline_activation_nested_scopes_restore_the_same_parent() {
    let context = context();
    let mut vm = Interpreter::new();
    let mut stack = ActivationStack::new();
    let mut registers = [Value::undefined()];
    let mut root = native(0, &mut registers);
    // SAFETY: native frame and register storage remain stationary until pop.
    unsafe {
        vm.jit_push_native_frame(&mut root).unwrap();
    }
    let vm_ptr = std::ptr::from_mut(&mut vm);
    let stack_ptr = std::ptr::from_mut(&mut stack);
    let mut activation = VmRuntimeActivation::new(&mut vm, &mut stack, &context, 0);
    // SAFETY: all four owners outlive the exclusive runtime call.
    let mut call =
        unsafe { RuntimeCall::bind(NonNull::from(&mut activation), NonNull::from(&mut root)) }
            .unwrap();
    let mut outer = [recipe(&context, 1, Value::number_i32(11))];
    let mut inner = [recipe(&context, 2, Value::number_i32(22))];
    call.with_inline_activations(&mut outer, |parent| {
        parent
            .with_inline_activations(&mut inner, |callee| {
                assert_eq!(callee.read(0).unwrap(), Value::number_i32(22));
                // SAFETY: metadata-only reads during the exclusive cold operation.
                let vm = unsafe { &*vm_ptr };
                let stack = unsafe { &*stack_ptr };
                assert_eq!(vm.logical_call_depth(stack), 3);
                assert_eq!(
                    vm.snapshot_active_frames(&context, stack, 10)
                        .iter()
                        .map(|frame| frame.function_id)
                        .collect::<Vec<_>>(),
                    [2, 1, 0]
                );
            })
            .unwrap();
        assert_eq!(parent.function_id(), 1);
        assert_eq!(parent.read(0).unwrap(), Value::number_i32(11));
        assert_eq!(unsafe { &*vm_ptr }.jit_native_activation_top, 2);
    })
    .unwrap();
    assert_eq!(call.function_id(), 0);
    assert_eq!(unsafe { &*vm_ptr }.jit_native_activation_top, 1);
    vm.jit_pop_native_activation();
}

#[test]
fn inline_constructor_new_target_is_a_moving_root() {
    let context = context();
    let mut vm = Interpreter::new();
    let mut stack = ActivationStack::new();
    let mut registers = [Value::undefined()];
    let mut root = native(0, &mut registers);
    // SAFETY: the frame and window remain stationary until the matching pop.
    unsafe {
        vm.jit_push_native_frame(&mut root).unwrap();
    }
    vm.with_runtime_turn(&mut stack, |turn| {
        let (vm, stack) = turn.into_parts();
        let receiver = vm.alloc_runtime_rooted_object_with_roots(&[], &[]).unwrap();
        root.set_this_value(Value::object(receiver));
        let target =
            crate::closure::alloc_closure(&mut vm.gc_heap, 2, vec![], None, None, None, None)
                .unwrap();
        registers[0] = Value::closure(target);
        let mut recipes = [recipe(&context, 1, root.this_value())];
        recipes[0].entry.as_mut().unwrap().new_target = registers[0];
        let vm_ptr = std::ptr::from_mut(vm);
        let mut activation = VmRuntimeActivation::new(vm, stack, &context, 0);
        // SAFETY: rooted turn and native caller outlive this cold scope.
        let mut call =
            unsafe { RuntimeCall::bind(NonNull::from(&mut activation), NonNull::from(&mut root)) }
                .unwrap();
        call.with_inline_activations(&mut recipes, |inner| {
            // SAFETY: services are accessed only under this exclusive runtime operation.
            let vm = unsafe { &mut *vm_ptr };
            vm.force_gc().unwrap();
            // SAFETY: the callee remains published and root-rewritten through the scope.
            let target = unsafe { inner.frame.as_ref() }.new_target();
            assert_eq!(target.as_closure(&vm.gc_heap).unwrap().function_id(), 2);
            assert!(!unsafe { inner.frame.as_ref() }.is_derived_constructor());
        })
        .unwrap();
        assert_eq!(recipes[0].entry.as_ref().unwrap().new_target, registers[0]);
    });
    vm.jit_pop_native_activation();
}
