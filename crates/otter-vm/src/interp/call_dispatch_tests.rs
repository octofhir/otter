//! Completion ownership while reconstructed inline children enter and return.
//!
//! # Contents
//! - Real trampoline resumption of a three-frame restored chain.
//! - Suspended source PCs, moved argument roots and child success/throw delivery.
//!
//! # Invariants
//! - Native callbacks record observations without assertions across the C ABI.
//! - A waiting parent stays on its call until the actual child completion.
//! - Queued inputs and the published physical window remain moving-GC roots.
//!
//! # See also
//! - [`super::interpreter_turn`] for completion consumption.
//! - [`Interpreter::prepare_inline_deopt_frames`] for inline reconstruction.

use std::sync::{Arc, Mutex};

use otter_bytecode::{Function, Instruction, Op, Operand};

use crate::deopt::{DeoptFrame, DeoptFrameEntry};
use crate::native_abi::{ExitAction, ExitReason, NativeFrameFlags, SideExit, VmFrameHeader};
use crate::native_stack_snapshot::FrameSite;
use crate::{ExecutionContext, Frame, Interpreter, NativeFunction, Value};

#[derive(Default)]
struct Observation {
    calls: usize,
    before: Vec<FrameSite>,
    after: Vec<FrameSite>,
    moved: bool,
    marker: i32,
}

fn context(catch_parent: Option<u32>) -> ExecutionContext {
    let functions = (0..3)
        .map(|id| {
            let mut function = Function {
                id,
                name: format!("restored_{id}"),
                scratch: 3,
                code: if id == 2 {
                    vec![
                        Instruction {
                            pc: 0,
                            op: Op::Call,
                            operands: vec![
                                Operand::Register(2),
                                Operand::Register(0),
                                Operand::ConstIndex(1),
                                Operand::Register(1),
                            ],
                        },
                        Instruction {
                            pc: 1,
                            op: Op::ReturnValue,
                            operands: vec![Operand::Register(2)],
                        },
                    ]
                    .into()
                } else {
                    vec![
                        Instruction {
                            pc: 0,
                            op: Op::Call,
                            operands: vec![
                                Operand::Register(0),
                                Operand::Register(1),
                                Operand::ConstIndex(0),
                            ],
                        },
                        Instruction {
                            pc: 1,
                            op: Op::ReturnValue,
                            operands: vec![Operand::Register(0)],
                        },
                        Instruction {
                            pc: 2,
                            op: Op::ReturnValue,
                            operands: vec![Operand::Register(1)],
                        },
                    ]
                    .into()
                },
                ..Function::default()
            };
            if catch_parent == Some(id) {
                function.handlers.push(otter_bytecode::ExceptionHandler {
                    start: 0,
                    end: 1,
                    target: 2,
                    exception: 1,
                });
            }
            function
        })
        .collect();
    let mut module = crate::test_support::minimal_bytecode_module("restored-completion.js");
    module.functions = functions;
    ExecutionContext::from_module(module, crate::source_registry::SourceRegistry::default())
        .expect("verified restored chain")
}

#[test]
fn queued_inline_child_owns_source_pc_completion_and_moving_roots() {
    for catch_parent in [None, Some(0), Some(1)] {
        let context = context(catch_parent);
        let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
        let mut stack = crate::test_support::FrameChainFixture::new();
        let mut registers = [Value::undefined(); 3];
        let mut physical = Frame::new(
            VmFrameHeader::interpreter(0, 3),
            registers.as_mut_ptr() as u64,
            Value::function(0),
            Value::undefined(),
        );
        let observed = Arc::new(Mutex::new(Observation::default()));
        // SAFETY: the physical record and initialized window stay stationary
        // throughout the rooted resumption and are unlinked before dropping.
        unsafe { vm.jit_push_native_frame(&mut physical).unwrap() };
        let result = vm.with_runtime_turn(&mut stack, |turn| {
            let (vm, stack) = turn.into_parts();
            vm.with_handle_scope(|vm, scope| {
                let observed = Arc::clone(&observed);
                let probe =
                    NativeFunction::new(vm.gc_heap_mut(), "restored-child", move |ctx, args, _| {
                        let (value, before, after, moved, marker) = ctx.scope(|mut scope| {
                            let value = scope.argument(args, 0);
                            let old = scope.raw(value).to_bits();
                            let before = scope.with_turn_parts(|vm, _| vm.capture_active_sites(3));
                            scope.with_turn_parts(|vm, _| vm.force_gc())?;
                            let after = scope.with_turn_parts(|vm, _| vm.capture_active_sites(3));
                            let moved = scope.raw(value).to_bits() != old;
                            let marker = scope.get(value, "marker")?;
                            let marker = scope
                                .raw(marker)
                                .as_number()
                                .map_or(-1, |number| number.as_f64() as i32);
                            Ok::<_, crate::NativeError>((
                                scope.finish(value),
                                before,
                                after,
                                moved,
                                marker,
                            ))
                        })?;
                        if let Ok(mut observation) = observed.lock() {
                            observation.calls += 1;
                            observation.before = before;
                            observation.after = after;
                            observation.moved = moved;
                            observation.marker = marker;
                        }
                        if catch_parent.is_some() {
                            Err(ctx.throw_value("restored-child", value))
                        } else {
                            Ok(value)
                        }
                    })
                    .expect("native child observer");
                let probe = vm.scoped_value(scope, Value::native_function(probe));
                let value = vm.scoped_object(scope).expect("young rooted input");
                let marker = vm.scoped_value(scope, Value::number_i32(719));
                vm.scoped_set(scope, value, "marker", marker).unwrap();
                let value = vm.escape_scoped(value);
                let entry = |function_id| {
                    Some(DeoptFrameEntry {
                        return_register: 0,
                        this: Value::undefined(),
                        closure: Value::function(function_id),
                        new_target: Value::undefined(),
                    })
                };
                let resume_byte_pc = |function_id, pc| {
                    context
                        .exec_function(function_id)
                        .unwrap()
                        .instruction_byte_pc(pc)
                        .unwrap()
                };
                let frames = [
                    DeoptFrame::with_window(
                        0,
                        resume_byte_pc(0, 1),
                        None,
                        [Value::undefined(), Value::function(1), value],
                    ),
                    DeoptFrame::with_window(
                        1,
                        resume_byte_pc(1, 1),
                        entry(1),
                        [Value::undefined(), Value::function(2), value],
                    ),
                    DeoptFrame::with_window(
                        2,
                        resume_byte_pc(2, 0),
                        entry(2),
                        [vm.escape_scoped(probe), value, Value::undefined()],
                    ),
                ];
                vm.prepare_inline_deopt_frames(
                    &context,
                    stack,
                    &mut physical,
                    &frames,
                    SideExit::new(1, ExitReason::InsufficientFeedback, ExitAction::Recompile),
                )
                .unwrap();
                crate::test_support::resume_prepared_frame(vm, stack, &context, &mut physical)
                    .expect("restored child completion reaches its exact parent")
            })
        });
        vm.jit_pop_native_frame();
        let observed = observed.lock().expect("completed observer");
        let expected = (0..3)
            .rev()
            .map(|function_id| FrameSite::Instruction { function_id, pc: 0 })
            .collect::<Vec<_>>();
        assert_eq!(observed.before, expected, "parent source remains its call");
        assert_eq!(
            observed.after, expected,
            "collection preserves source ownership"
        );
        assert_eq!(observed.calls, 1, "child executes once");
        assert!(
            observed.moved,
            "actual argument crossed a moving collection"
        );
        assert_eq!(observed.marker, 719);
        assert_eq!(
            result, registers[2],
            "child payload and parent root moved together"
        );
        assert!(
            !physical
                .header
                .flags
                .contains(NativeFrameFlags::ADVANCE_ON_RESUME)
        );
        assert!(stack.is_empty());
    }
}
