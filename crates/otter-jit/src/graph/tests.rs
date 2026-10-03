//! Execution tests of the graph tier on hand-built bytecode.
//!
//! # Contents
//! - A harness that compiles a test snapshot and runs its tier entry over a
//!   fake published interpreter frame.
//! - Straight-line, branch and loop programs over int32 and float64
//!   feedback, and exact deopt exits.
//!
//! # See also
//! - [`super::compile`] — the pipeline under test.

use otter_bytecode::{Op, Operand};
use otter_vm::{
    JitCompileSnapshot, Value,
    jit::JitTestInstruction,
    jit_feedback::{ARITH_FLOAT64, ARITH_INT32, ArithFeedback},
    native_abi::{
        Frame, JitCtx, JitEntry, NativeFrameFlags, NativeFrameKind, NativeResultDomain,
        NativeResultPair, NativeResultStatus, VmFrameHeader, VmThread,
    },
    value::tag,
};

use crate::entry::TransitionTable;

fn snapshot(params: u16, registers: u16, code: Vec<(Op, Vec<Operand>)>) -> JitCompileSnapshot {
    let instructions = code
        .into_iter()
        .enumerate()
        .map(|(pc, (op, operands))| JitTestInstruction::new(op, pc as u32, pc as u32 * 4, operands))
        .collect();
    let mut view = JitCompileSnapshot::without_feedback(90, params, registers, instructions);
    view.object_shape_byte = 8;
    view
}

fn seed(view: &mut JitCompileSnapshot, pcs: &[u32], bits: u8) {
    for &pc in pcs {
        view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(bits));
    }
}

/// Run the tier entry over a frame holding `args`; returns the result pair
/// and the frame window afterwards.
fn run(view: &JitCompileSnapshot, args: &[u64]) -> (NativeResultPair, Vec<u64>) {
    let transitions = TransitionTable::resolve();
    let compiled = super::compile(view, 7001, &transitions, None).expect("graph compile");
    let entry: JitEntry = unsafe {
        std::mem::transmute(
            compiled
                .emission
                .buffer
                .ptr(dynasmrt::AssemblyOffset(compiled.emission.tier_entry)),
        )
    };
    let register_count = view.code_block.register_count;
    let mut window = vec![Value::undefined().to_bits(); usize::from(register_count)];
    window[..args.len()].copy_from_slice(args);
    let heap = otter_gc::GcHeap::new().expect("test heap");
    let mut frame = Frame::new(
        VmFrameHeader {
            function_id: view.code_block.id,
            pc: 0,
            register_count,
            kind: NativeFrameKind::Optimizing,
            flags: NativeFrameFlags::empty(),
        },
        window.as_mut_ptr() as u64,
        Value::undefined(),
        Value::undefined(),
    );
    let interrupt = 0_u8;
    let mut fuel = i64::MAX as u64;
    let mut thread = VmThread::empty();
    thread.interrupt_cell = std::ptr::addr_of!(interrupt) as u64;
    thread.gc_heap = std::ptr::from_ref(&heap) as u64;
    thread.backedge_fuel_cell = std::ptr::addr_of_mut!(fuel) as u64;
    let mut error = None;
    let mut ctx = JitCtx {
        thread: std::ptr::addr_of_mut!(thread),
        native_frame: std::ptr::addr_of_mut!(frame),
        error: &mut error,
        generated_depth_limit: u64::MAX,
        alloc_window: otter_vm::jit::JitMachineAllocationWindow::disabled(),
        runtime_stats: std::ptr::null_mut(),
        global_this_offset: std::ptr::null(),
        native_stack_limit: 0,
        generated_feedback_clean: 1,
        completion_destination: u32::MAX,
        completion_generation: 0,
        pending_call: otter_vm::native_abi::CallRequest::EMPTY,
        completion: NativeResultPair::success(Value::UNDEFINED),
    };
    unsafe { (*ctx.thread).frame_cell = std::ptr::addr_of_mut!(ctx.native_frame) as u64 };
    let result = entry(&mut ctx);
    (result, window)
}

fn int(value: i32) -> u64 {
    tag::box_int32(value)
}

fn returned(result: NativeResultPair) -> u64 {
    assert_eq!(
        result.validate(NativeResultDomain::Compiled),
        Some(NativeResultStatus::Success),
        "expected a normal return, got {result:?}"
    );
    result.payload_bits()
}

#[test]
fn int32_add_returns_boxed_sum() {
    let mut view = snapshot(
        2,
        3,
        vec![
            (
                Op::Add,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ],
    );
    seed(&mut view, &[0], ARITH_INT32);
    let (result, _) = run(&view, &[int(40), int(2)]);
    assert_eq!(returned(result), int(42));
}

#[test]
fn float64_mul_boxes_canonically() {
    let mut view = snapshot(
        2,
        3,
        vec![
            (
                Op::Mul,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ],
    );
    seed(&mut view, &[0], ARITH_INT32 | ARITH_FLOAT64);
    let (result, _) = run(
        &view,
        &[
            Value::number_f64(1.5).to_bits(),
            Value::number_f64(3.0).to_bits(),
        ],
    );
    assert_eq!(returned(result), Value::number_f64(4.5).to_bits());
    let (result, _) = run(
        &view,
        &[
            Value::number_f64(2.0).to_bits(),
            Value::number_f64(3.0).to_bits(),
        ],
    );
    assert_eq!(returned(result), int(6), "integral results box as int32");
}

/// `s = 0; i = 0; while (i < n) { s = s + i; i = i + 1 } return s`.
fn sum_loop() -> JitCompileSnapshot {
    let mut view = snapshot(
        1,
        4,
        vec![
            (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
            (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(0)]),
            (
                Op::LessThan,
                vec![
                    Operand::Register(3),
                    Operand::Register(2),
                    Operand::Register(0),
                ],
            ),
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(3), Operand::Register(3)],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(1),
                    Operand::Register(1),
                    Operand::Register(2),
                ],
            ),
            (
                Op::AddImm,
                vec![
                    Operand::Register(2),
                    Operand::Register(2),
                    Operand::Imm32(1),
                ],
            ),
            (Op::Jump, vec![Operand::Imm32(-5)]),
            (Op::ReturnValue, vec![Operand::Register(1)]),
        ],
    );
    seed(&mut view, &[2, 4, 5], ARITH_INT32);
    view
}

#[test]
fn int32_loop_sums() {
    let view = sum_loop();
    let (result, _) = run(&view, &[int(10)]);
    assert_eq!(returned(result), int(45));
    let (result, _) = run(&view, &[int(0)]);
    assert_eq!(returned(result), int(0));
}

#[test]
fn branches_merge_through_phis() {
    // r1 = a < b ? a : b ; return r1 + 1
    let mut view = snapshot(
        2,
        4,
        vec![
            (
                Op::LessThan,
                vec![
                    Operand::Register(3),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(2), Operand::Register(3)],
            ),
            (Op::LoadLocal, vec![Operand::Register(2), Operand::Imm32(0)]),
            (Op::Jump, vec![Operand::Imm32(1)]),
            (Op::LoadLocal, vec![Operand::Register(2), Operand::Imm32(1)]),
            (
                Op::AddImm,
                vec![
                    Operand::Register(2),
                    Operand::Register(2),
                    Operand::Imm32(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ],
    );
    seed(&mut view, &[0, 5], ARITH_INT32);
    let (result, _) = run(&view, &[int(3), int(7)]);
    assert_eq!(returned(result), int(4));
    let (result, _) = run(&view, &[int(9), int(7)]);
    assert_eq!(returned(result), int(8));
}

#[test]
fn int32_overflow_deopts_before_the_add() {
    let mut view = snapshot(
        2,
        3,
        vec![
            (
                Op::Add,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ],
    );
    seed(&mut view, &[0], ARITH_INT32);
    let args = [int(i32::MAX), int(1)];
    let (result, window) = run(&view, &args);
    assert_eq!(
        result.validate(NativeResultDomain::Compiled),
        Some(NativeResultStatus::SideExit),
        "overflow leaves through the eager deopt, got {result:?}"
    );
    assert_eq!(
        &window[..2],
        &args,
        "the operands are rebuilt in the window"
    );
}

#[test]
fn wrong_type_deopts_with_loop_state() {
    // The loop bound turns out not to be an int32: the check exits before
    // the comparison with the loop's registers rebuilt.
    let view = sum_loop();
    let bound = Value::undefined().to_bits();
    let (result, window) = run(&view, &[bound]);
    assert_eq!(
        result.validate(NativeResultDomain::Compiled),
        Some(NativeResultStatus::SideExit)
    );
    assert_eq!(window[0], bound);
    assert_eq!(window[1], int(0));
    assert_eq!(window[2], int(0));
}

/// `count = 0; for (round < n) for (i < 4) count = count + v; return count`.
fn nested_loops() -> JitCompileSnapshot {
    let mut view = snapshot(
        2,
        6,
        vec![
            (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(0)]),
            (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(0)]),
            (
                Op::LessThan,
                vec![
                    Operand::Register(4),
                    Operand::Register(3),
                    Operand::Register(0),
                ],
            ),
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(8), Operand::Register(4)],
            ),
            (Op::LoadInt32, vec![Operand::Register(5), Operand::Imm32(0)]),
            (
                Op::LessThanImm,
                vec![
                    Operand::Register(4),
                    Operand::Register(5),
                    Operand::Imm32(4),
                ],
            ),
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(3), Operand::Register(4)],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(2),
                    Operand::Register(2),
                    Operand::Register(1),
                ],
            ),
            (
                Op::AddImm,
                vec![
                    Operand::Register(5),
                    Operand::Register(5),
                    Operand::Imm32(1),
                ],
            ),
            (Op::Jump, vec![Operand::Imm32(-5)]),
            (
                Op::AddImm,
                vec![
                    Operand::Register(3),
                    Operand::Register(3),
                    Operand::Imm32(1),
                ],
            ),
            (Op::Jump, vec![Operand::Imm32(-10)]),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ],
    );
    seed(&mut view, &[2, 5, 7, 8, 10], ARITH_INT32);
    view
}

#[test]
fn nested_loops_carry_values_through_both_headers() {
    let view = nested_loops();
    let transitions = TransitionTable::resolve();
    let compiled = super::compile(&view, 7001, &transitions, None).expect("graph compile");
    let dump = compiled.built.graph.dump(&compiled.built.layout);
    let (result, _) = run(&view, &[int(10), int(1)]);
    assert_eq!(
        returned(result),
        int(40),
        "{dump}\n{:#?}",
        compiled.allocation.edges
    );
}
