//! Backend tests for the optimizing AArch64 emitter.

use std::sync::Arc;

use otter_vm::{
    JitDirectCallThisMode, JitDirectCallee, JitFunctionCode,
    jit::{JitDirectCallPlan, JitTestInstruction},
    jit_feedback::{ARITH_FLOAT64, ARITH_INT32, ArithFeedback},
    native_abi::{NativeFrame, NativeFrameFlags, NativeFrameKind, VmFrameHeader, VmThread},
};

use super::*;
use crate::entry::{JitCtx, JitEntry, JitRet, STATUS_BAILED, STATUS_RETURNED, STATUS_THREW};

const STRIDE: u32 = 8;

fn construct_target(
    parameter_count: u16,
    register_count: u16,
    generated_stack_frame_bytes: Option<u32>,
    derived: bool,
    upvalues: (u16, u16),
) -> JitDirectCallee {
    JitDirectCallee {
        plan: JitDirectCallPlan {
            function_id: 73,
            code_object_id: 74,
            entry_cell: 0x1000,
            tier: NativeFrameKind::Optimizing,
            this_mode: JitDirectCallThisMode::ConstructReceiver,
            is_derived_constructor: derived,
            generated_stack_frame_bytes,
            param_count: parameter_count,
            register_count,
            own_upvalue_count: upvalues.0,
            inherited_upvalue_count: upvalues.1,
        },
        receiver_allocation: None,
    }
}

#[test]
fn tiny_zero_argument_construct_uses_the_canonical_cost_model_path() {
    use otter_vm::JitDirectCallLoweringRejectionReason::{LayoutUnsupported, Unprofitable};

    let tiny = construct_target(
        0,
        TINY_BASE_CONSTRUCT_REGISTER_LIMIT,
        Some(16),
        false,
        (0, 0),
    );
    assert!(direct_call_target_is_supported(&tiny));
    assert_eq!(
        optimizing_direct_construct_rejection(&tiny, 0),
        Some(Unprofitable)
    );

    assert_eq!(
        optimizing_direct_construct_rejection(&construct_target(0, 4, Some(16), false, (0, 0)), 0,),
        None,
        "a larger body can amortize generated receiver and frame setup"
    );
    assert_eq!(
        optimizing_direct_construct_rejection(&construct_target(1, 3, Some(16), false, (0, 0)), 1,),
        None,
        "a parameterized body retains stack-owned argument entry"
    );
    assert_eq!(
        optimizing_direct_construct_rejection(&construct_target(0, 3, Some(16), false, (0, 0)), 1,),
        None,
        "extra arguments are not covered by the measured tiny-body gate"
    );
    assert_eq!(
        optimizing_direct_construct_rejection(&construct_target(0, 3, Some(16), true, (0, 0)), 0,),
        None,
        "derived receiver and result semantics retain generated linkage"
    );
    for upvalues in [(1, 0), (0, 1)] {
        assert_eq!(
            optimizing_direct_construct_rejection(
                &construct_target(0, 3, Some(16), false, upvalues),
                0,
            ),
            None,
            "capture setup is not covered by the measured tiny-body gate"
        );
    }
    assert_eq!(
        optimizing_direct_construct_rejection(&construct_target(0, 3, None, false, (0, 0)), 0,),
        Some(LayoutUnsupported),
        "layout rejection remains distinct from the cost-model decision"
    );
}

/// A spliced unit compiles, and its callee-identity guard deoptimizes when
/// the runtime callee is not the body the tree spliced. Wrong-callee is the
/// one splice path executable without a VM: the guard fails before any
/// callee code runs, so the exit owes only the caller's own frame, and the
/// interpreter re-runs the call generically from the call PC.
#[test]
fn a_spliced_unit_compiles_and_guards_the_callee_identity() {
    use crate::ir::inline::InlineTree;

    // Three params so the harness-supplied callee and argument values are
    // real inputs rather than compiler-seeded undefined.
    let mut view = JitCompileSnapshot::without_feedback(
        7,
        3,
        8,
        vec![
            JitTestInstruction::new(
                Op::Call,
                0,
                0,
                vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::ConstIndex(1),
                    Operand::Register(2),
                ],
            ),
            JitTestInstruction::new(Op::ReturnValue, 1, 8, vec![Operand::Register(0)]),
        ],
    );
    let callee = JitCompileSnapshot::without_feedback(
        9,
        1,
        4,
        vec![JitTestInstruction::new(
            Op::ReturnValue,
            0,
            0,
            vec![Operand::Register(0)],
        )],
    );
    let call_byte_pc = view.instructions[0].byte_pc;
    view.inline_callees.insert(
        call_byte_pc,
        otter_vm::JitInlineCallee {
            body: Arc::new(callee),
        },
    );

    let tree = InlineTree::build(&view);
    assert_eq!(tree.frames.len(), 2, "the fixture must splice");
    let code = compile(&view, 91).expect("a spliced unit compiles");

    // A non-cell callee value fails the guard's very first check.
    let (ret, frame, pc) = execute_with_frame(&code, &[box_i32(1), box_i32(5), box_i32(3)]);
    assert_eq!(ret.status, STATUS_BAILED);
    assert_eq!(pc, 0, "the interpreter re-runs the call itself");
    // The caller's registers were written back intact for that re-run.
    assert_eq!(frame[1], box_i32(5));
    assert_eq!(frame[2], box_i32(3));
}

/// Boxing recorded at an inlined call boundary is dead: the callee consumes
/// the caller's SSA value directly and any real tagged use performs its own
/// conversion. The final eligibility sweep must therefore accept the removed
/// call site's conversion instead of rejecting the whole spliced unit.
#[test]
fn a_spliced_call_accepts_a_numeric_argument() {
    use crate::ir::inline::InlineTree;

    let mut view = JitCompileSnapshot::without_feedback(
        7,
        1,
        5,
        vec![
            JitTestInstruction::new(
                Op::LoadInt32,
                0,
                0,
                vec![Operand::Register(1), Operand::Imm32(7)],
            ),
            JitTestInstruction::new(
                Op::Call,
                1,
                8,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::ConstIndex(1),
                    Operand::Register(1),
                ],
            ),
            JitTestInstruction::new(
                Op::Add,
                2,
                16,
                vec![
                    Operand::Register(3),
                    Operand::Register(1),
                    Operand::Register(1),
                ],
            ),
            JitTestInstruction::new(Op::ReturnValue, 3, 24, vec![Operand::Register(3)]),
        ],
    );
    view.seed_arith_feedback_for_test(2, ArithFeedback::from_bits(ARITH_INT32));
    let callee = JitCompileSnapshot::without_feedback(
        9,
        1,
        2,
        vec![JitTestInstruction::new(
            Op::ReturnValue,
            0,
            0,
            vec![Operand::Register(0)],
        )],
    );
    let call_byte_pc = view.instructions[1].byte_pc;
    view.inline_callees.insert(
        call_byte_pc,
        otter_vm::JitInlineCallee {
            body: Arc::new(callee),
        },
    );

    let tree = InlineTree::build(&view);
    assert_eq!(tree.frames.len(), 2, "the fixture must splice");
    let transitions = TransitionTable::resolve();
    let output = compile_with_artifacts(&view, 92, &transitions, None, true)
        .expect("the root unit remains compilable");
    assert!(
        output.diagnostics.iter().any(|diagnostic| matches!(
            diagnostic,
            otter_vm::JitCompilerDiagnostic::InlineLowered {
                outcome: otter_vm::JitInlineLoweringOutcome::Inlined,
                ..
            }
        )),
        "a numeric argument must not reject the spliced body: {:?}",
        output.diagnostics
    );
}

fn box_i32(value: i32) -> u64 {
    (0xfffe_u64 << 48) | u64::from(value as u32)
}

fn unbox_i32(value: u64) -> i32 {
    value as u32 as i32
}

fn box_f64(value: f64) -> u64 {
    otter_vm::Value::number_f64(value).to_bits()
}

fn view(
    param_count: u16,
    register_count: u16,
    instructions: Vec<(Op, Vec<Operand>)>,
) -> JitCompileSnapshot {
    let mut view = JitCompileSnapshot::without_feedback(
        41,
        param_count,
        register_count,
        instructions
            .into_iter()
            .enumerate()
            .map(|(pc, (op, operands))| {
                JitTestInstruction::new(op, pc as u32, pc as u32 * STRIDE + 3, operands)
            })
            .collect(),
    );
    for pc in 0..view.instructions.len() {
        if matches!(
            view.instructions[pc].op(&view.code_block),
            Op::Add
                | Op::Sub
                | Op::Mul
                | Op::Neg
                | Op::LessThan
                | Op::LessEq
                | Op::GreaterThan
                | Op::GreaterEq
                | Op::Equal
                | Op::NotEqual
        ) {
            view.seed_arith_feedback_for_test(pc as u32, ArithFeedback::from_bits(ARITH_INT32));
        }
    }
    view
}

fn float_view(
    param_count: u16,
    register_count: u16,
    instructions: Vec<(Op, Vec<Operand>)>,
    float_feedback_pcs: &[u32],
    numbers: &[(u32, f64)],
) -> JitCompileSnapshot {
    let mut view = view(param_count, register_count, instructions);
    for &pc in float_feedback_pcs {
        view.seed_arith_feedback_for_test(pc, ArithFeedback::from_bits(ARITH_FLOAT64));
    }
    for &(pc, number) in numbers {
        view.instructions[pc as usize].load_number = Some(number);
    }
    view
}

fn execute(code: &OptimizedCode, args: &[u64]) -> JitRet {
    execute_with_frame(code, args).0
}

fn execute_with_frame(code: &OptimizedCode, args: &[u64]) -> (JitRet, Vec<u64>, u32) {
    let interrupt = 0_u8;
    let mut fuel = i64::MAX as u64;
    execute_with_poll_cells(code, args, std::ptr::addr_of!(interrupt), &mut fuel)
}

fn execute_with_poll_cells(
    code: &OptimizedCode,
    args: &[u64],
    interrupt: *const u8,
    fuel: &mut u64,
) -> (JitRet, Vec<u64>, u32) {
    // SAFETY: the compiler emitted `JitEntry`, `code` owns the mapping
    // through the call, and the context plus poll cells remain valid until
    // the entry returns.
    let entry = unsafe { code.compiled_code().entry_ptr() };
    let mut frame =
        vec![otter_vm::Value::undefined().to_bits(); code.metadata().register_count as usize];
    frame[..args.len()].copy_from_slice(args);
    execute_at(code, entry, frame, interrupt, fuel)
}

fn execute_osr_with_frame(
    code: &OptimizedCode,
    logical_pc: u32,
    frame: Vec<u64>,
) -> (JitRet, Vec<u64>, u32) {
    let interrupt = 0_u8;
    let mut fuel = i64::MAX as u64;
    // SAFETY: the code object owns the recorded trampoline while this call
    // executes.
    let entry = unsafe {
        code.osr_entry_ptr_for_test(logical_pc)
            .expect("optimized OSR entry")
    };
    execute_at(code, entry, frame, std::ptr::addr_of!(interrupt), &mut fuel)
}

fn execute_at(
    code: &OptimizedCode,
    entry: *const u8,
    mut frame: Vec<u64>,
    interrupt: *const u8,
    fuel: &mut u64,
) -> (JitRet, Vec<u64>, u32) {
    assert_eq!(frame.len(), code.metadata().register_count as usize);
    // SAFETY: `entry` is a main entry or OSR trampoline in `code`, whose
    // executable mapping outlives this call.
    let entry: JitEntry = unsafe { std::mem::transmute(entry) };
    let metadata = code.metadata();
    let mut native_frame = NativeFrame::new(
        VmFrameHeader {
            function_id: metadata.function_id,
            code_block_id: metadata.function_id,
            pc: 0,
            register_count: metadata.register_count,
            kind: NativeFrameKind::Baseline,
            flags: NativeFrameFlags::empty(),
        },
        frame.as_mut_ptr() as u64,
        otter_vm::Value::undefined(),
        otter_vm::Value::undefined(),
    );
    native_frame.set_materialized_activation(0);
    let mut thread = VmThread::empty();
    thread.current_frame = std::ptr::addr_of_mut!(native_frame) as u64;
    thread.current_code_object_id = metadata.code_object_id;
    thread.interrupt_cell = interrupt as u64;
    thread.backedge_fuel_cell = std::ptr::from_mut(fuel) as u64;
    let mut error = None;
    let mut ctx = JitCtx {
        thread: std::ptr::addr_of_mut!(thread),
        native_frame: std::ptr::addr_of_mut!(native_frame),
        error: &mut error,
        activation_base: std::ptr::null_mut(),
        activation_top_ptr: std::ptr::null_mut(),
        activation_limit: 0,
        machine_roots_ptr: std::ptr::null_mut(),
        receiver_alloc: otter_vm::jit::JitMachineAllocationWindow::disabled(),
        runtime_stats: std::ptr::null_mut(),
        global_this_offset: std::ptr::null(),
        native_stack_limit: 0,
        generated_feedback_clean: 1,
    };
    let result = entry(&mut ctx);
    (result, frame, native_frame.header.pc)
}

fn summation_view() -> JitCompileSnapshot {
    view(
        1,
        5,
        vec![
            (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
            (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(0)]),
            (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(1)]),
            (
                Op::LessThan,
                vec![
                    Operand::Register(4),
                    Operand::Register(1),
                    Operand::Register(0),
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
                Op::Add,
                vec![
                    Operand::Register(1),
                    Operand::Register(1),
                    Operand::Register(3),
                ],
            ),
            (Op::Jump, vec![Operand::Imm32(-5)]),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ],
    )
}

unsafe fn fixture_registers(ctx: *mut JitCtx) -> *mut u64 {
    // SAFETY: every execution fixture publishes a live canonical frame and
    // register window for the complete transition call.
    unsafe { (*(*ctx).native_frame).register_base as *mut u64 }
}

extern "C" fn relocating_element_load(
    ctx: *mut JitCtx,
    dst: u64,
    receiver: u64,
    _index: u64,
) -> u64 {
    // SAFETY: the execution fixture supplies a live three-or-more-slot
    // register window for the duration of this transition call.
    let regs = unsafe { fixture_registers(ctx) };
    unsafe {
        *regs.add(receiver as usize) = box_i32(5);
        *regs.add(dst as usize) = box_i32(37);
    }
    0
}

extern "C" fn relocating_precise_element_load(
    ctx: *mut JitCtx,
    dst: u64,
    receiver: u64,
    index: u64,
) -> u64 {
    // SAFETY: this fixture compiles a twenty-six-slot frame and keeps it live
    // for the transition. Tagged slots model moving-GC rewrites; numeric
    // slots are deliberately poisoned to prove they are never reloaded.
    let regs = unsafe { fixture_registers(ctx) };
    unsafe {
        assert_eq!(*regs.add(receiver as usize), box_i32(99));
        assert_eq!(*regs.add(index as usize), box_i32(0));
        assert_eq!(*regs.add(1), box_i32(20));
        assert_eq!(*regs.add(2), box_i32(30));
        assert_eq!(*regs.add(11), otter_vm::Value::undefined().to_bits());
        assert_eq!(*regs.add(12), otter_vm::Value::undefined().to_bits());
        for (register, value) in [5, 7, 11, 13, 17, 19, 23, 29, 31, 37]
            .into_iter()
            .enumerate()
        {
            *regs.add(register) = box_i32(value);
        }
        *regs.add(10) = box_i32(1_000);
        *regs.add(11) = box_i32(1_000);
        *regs.add(12) = box_f64(1_000.0);
        *regs.add(dst as usize) = box_i32(37);
    }
    0
}

extern "C" fn throwing_element_load(
    ctx: *mut JitCtx,
    _dst: u64,
    _receiver: u64,
    _index: u64,
) -> u64 {
    // SAFETY: the execution fixture owns the live error slot for the
    // complete entry call, matching the production transition contract.
    unsafe {
        *(*ctx).error = Some(otter_vm::VmError::InvalidOperand);
    }
    1
}

extern "C" fn relocating_precise_element_store(
    ctx: *mut JitCtx,
    receiver: u64,
    index: u64,
    value: u64,
) -> u64 {
    // SAFETY: this fixture owns a seven-slot interpreter window for the
    // transition. Slots 0..=2 model moving-GC rewrites; the numeric index
    // is poisoned to prove optimized code ignores its window contents
    // after the call.
    let regs = unsafe { fixture_registers(ctx) };
    unsafe {
        assert_eq!(*regs.add(receiver as usize), box_i32(99));
        assert_eq!(*regs.add(index as usize), box_i32(0));
        assert_eq!(*regs.add(value as usize), box_i32(20));
        *regs.add(0) = box_i32(5);
        *regs.add(1) = box_i32(7);
        *regs.add(2) = box_i32(11);
        *regs.add(index as usize) = box_i32(1_000);
    }
    0
}

extern "C" fn throwing_element_store(
    ctx: *mut JitCtx,
    _receiver: u64,
    _index: u64,
    _value: u64,
) -> u64 {
    // SAFETY: the execution fixture owns the live error slot for the
    // complete entry call, matching the production transition contract.
    unsafe {
        *(*ctx).error = Some(otter_vm::VmError::InvalidOperand);
    }
    1
}

extern "C" fn successful_construct(
    ctx: *mut JitCtx,
    dst: u64,
    callee: u64,
    argc: u64,
    packed_args: u64,
) -> u64 {
    // SAFETY: the fixture supplies a three-slot frame window for the
    // duration of this transition and the emitted ABI passes slot ids.
    let regs = unsafe { fixture_registers(ctx) };
    unsafe {
        assert_eq!(*regs.add(callee as usize), box_i32(99));
        assert_eq!(argc, 1);
        assert_eq!(packed_args & 0xffff, 1);
        assert_eq!(*regs.add(1), box_i32(7));
        *regs.add(dst as usize) = box_i32(37);
    }
    0
}

fn element_load_transitions(entry: usize) -> TransitionTable {
    let mut transitions = TransitionTable::resolve();
    transitions.replace_variadic_entry_for_test(STUB_JIT_LOAD_ELEMENT, entry);
    transitions
}

fn element_store_transitions(entry: usize) -> TransitionTable {
    let mut transitions = TransitionTable::resolve();
    transitions.replace_variadic_entry_for_test(STUB_JIT_STORE_ELEMENT, entry);
    transitions
}

#[test]
fn method_call_accepts_boxed_numeric_arguments() {
    let view = view(
        1,
        3,
        vec![
            (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(7)]),
            (
                Op::CallMethodValue,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::ConstIndex(1),
                    Operand::ConstIndex(1),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ],
    );
    let code =
        compile(&view, 142).expect("numeric method arguments are boxed into the transition frame");

    assert_eq!(code.safepoint_count(), 1);
    let record = code.safepoint_record(0).expect("method-call safepoint");
    assert_eq!(
        record.tagged_locations,
        vec![otter_vm::native_abi::TaggedLocation::frame_slot(0)]
    );
    let frame_map = code.frame_map(0).expect("method-call frame map");
    assert_eq!(frame_map.slot_count, 3);
    assert_eq!(frame_map.bitmap_word_count, 1);
    assert_eq!(
        code.frame_map_bitmap_words(),
        &[0b1],
        "the receiver is rooted while the boxed primitive argument is not traced"
    );
}

#[test]
fn method_call_accepts_boxed_float_argument() {
    let view = float_view(
        1,
        3,
        vec![
            (
                Op::LoadNumber,
                vec![Operand::Register(1), Operand::ConstIndex(0)],
            ),
            (
                Op::CallMethodValue,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::ConstIndex(1),
                    Operand::ConstIndex(1),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ],
        &[],
        &[(0, 7.5)],
    );
    let code =
        compile(&view, 143).expect("float method arguments are boxed into the transition frame");

    assert_eq!(code.safepoint_count(), 1);
    let record = code.safepoint_record(0).expect("method-call safepoint");
    assert_eq!(
        record.tagged_locations,
        vec![otter_vm::native_abi::TaggedLocation::frame_slot(0)]
    );
    assert_eq!(
        code.frame_map_bitmap_words(),
        &[0b1],
        "the receiver is rooted while the boxed float argument is not traced"
    );
}

fn construct_transitions(entry: usize) -> TransitionTable {
    let mut transitions = TransitionTable::resolve();
    transitions.replace_variadic_entry_for_test(STUB_JIT_CONSTRUCT, entry);
    transitions
}

#[test]
fn executes_element_load_and_reloads_relocated_tagged_values() {
    let view = view(
        2,
        4,
        vec![
            (
                Op::LoadElement,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(3),
                    Operand::Register(2),
                    Operand::Register(0),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(3)]),
        ],
    );
    let transitions = element_load_transitions(relocating_element_load as *const () as usize);
    let code = compile_with_transitions(&view, 109, &transitions)
        .expect("element load is optimizing-eligible");
    let result = execute(&code, &[box_i32(99), box_i32(0)]);
    assert_eq!(result.status, STATUS_RETURNED);
    assert_eq!(unbox_i32(result.value), 42);

    assert_eq!(code.safepoint_count(), 1);
    assert_eq!(JitFunctionCode::metadata(&code).safepoint_count, 1);
    let record = code.safepoint_record(0).expect("element-load safepoint");
    assert_eq!(
        record.tagged_locations,
        vec![
            otter_vm::native_abi::TaggedLocation::frame_slot(0),
            otter_vm::native_abi::TaggedLocation::frame_slot(1),
        ]
    );
    let frame_map = code.frame_map(0).expect("precise element-load frame map");
    assert_eq!(frame_map.slot_count, 4);
    assert_eq!(frame_map.bitmap_word_count, 1);
    assert_eq!(code.frame_map_bitmap_words(), &[0b11]);
}

#[test]
fn precise_element_load_reloads_tagged_spills_but_not_numeric_values() {
    let view = float_view(
        10,
        26,
        vec![
            (
                Op::LoadInt32,
                vec![Operand::Register(10), Operand::Imm32(0)],
            ),
            (
                Op::LoadInt32,
                vec![Operand::Register(11), Operand::Imm32(4)],
            ),
            (
                Op::LoadNumber,
                vec![Operand::Register(12), Operand::ConstIndex(0)],
            ),
            (
                Op::LoadElement,
                vec![
                    Operand::Register(13),
                    Operand::Register(0),
                    Operand::Register(10),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(14),
                    Operand::Register(13),
                    Operand::Register(0),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(15),
                    Operand::Register(14),
                    Operand::Register(1),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(16),
                    Operand::Register(15),
                    Operand::Register(2),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(17),
                    Operand::Register(16),
                    Operand::Register(3),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(18),
                    Operand::Register(17),
                    Operand::Register(4),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(19),
                    Operand::Register(18),
                    Operand::Register(5),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(20),
                    Operand::Register(19),
                    Operand::Register(6),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(21),
                    Operand::Register(20),
                    Operand::Register(7),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(22),
                    Operand::Register(21),
                    Operand::Register(8),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(23),
                    Operand::Register(22),
                    Operand::Register(9),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(24),
                    Operand::Register(23),
                    Operand::Register(11),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(25),
                    Operand::Register(24),
                    Operand::Register(12),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(25)]),
        ],
        &[15],
        &[(2, 2.5)],
    );
    let transitions =
        element_load_transitions(relocating_precise_element_load as *const () as usize);
    let code = compile_with_transitions(&view, 111, &transitions)
        .expect("precise element load is optimizing-eligible");
    assert!(
        code.metadata().linear_scan_spill_slot_count > 0,
        "tagged live-across fixture must exercise optimizing spills"
    );

    let result = execute(
        &code,
        &[
            box_i32(99),
            box_i32(20),
            box_i32(30),
            box_i32(40),
            box_i32(50),
            box_i32(60),
            box_i32(70),
            box_i32(80),
            box_i32(90),
            box_i32(100),
        ],
    );
    assert_eq!(result.status, STATUS_RETURNED);
    assert_eq!(result.value, box_f64(235.5));

    let record = code.safepoint_record(0).expect("precise safepoint");
    assert_eq!(
        record.tagged_locations,
        (0..10)
            .map(otter_vm::native_abi::TaggedLocation::frame_slot)
            .collect::<Vec<_>>()
    );
    assert_eq!(code.frame_map_bitmap_words(), &[0b11_1111_1111]);
}

#[test]
fn element_load_nonzero_status_uses_shared_throw_exit() {
    let view = view(
        2,
        3,
        vec![
            (
                Op::LoadElement,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ],
    );
    let transitions = element_load_transitions(throwing_element_load as *const () as usize);
    let code = compile_with_transitions(&view, 110, &transitions)
        .expect("throwing element load is optimizing-eligible");
    let result = execute(&code, &[box_i32(99), box_i32(0)]);
    assert_eq!(result.status, STATUS_THREW);
    assert_eq!(result.value, 0);
}

#[test]
fn element_store_reloads_tagged_roots() {
    let view = view(
        3,
        7,
        vec![
            (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(0)]),
            (
                Op::StoreElement,
                vec![
                    Operand::Register(0),
                    Operand::Register(3),
                    Operand::Register(1),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(5),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(6),
                    Operand::Register(5),
                    Operand::Register(2),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(6)]),
        ],
    );
    let transitions =
        element_store_transitions(relocating_precise_element_store as *const () as usize);
    let code = compile_with_transitions(&view, 112, &transitions)
        .expect("precise element store is optimizing-eligible");
    let result = execute(&code, &[box_i32(99), box_i32(20), box_i32(30)]);
    assert_eq!(result.status, STATUS_RETURNED);
    assert_eq!(unbox_i32(result.value), 23);

    let record = code.safepoint_record(0).expect("element-store safepoint");
    assert_eq!(
        record.tagged_locations,
        vec![
            otter_vm::native_abi::TaggedLocation::frame_slot(0),
            otter_vm::native_abi::TaggedLocation::frame_slot(1),
            otter_vm::native_abi::TaggedLocation::frame_slot(2),
        ]
    );
    assert_eq!(code.frame_map_bitmap_words(), &[0b111]);
}

#[test]
fn element_store_nonzero_status_uses_shared_throw_exit() {
    let view = view(
        1,
        4,
        vec![
            (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
            (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(7)]),
            (
                Op::StoreElement,
                vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::Register(2),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ],
    );
    let transitions = element_store_transitions(throwing_element_store as *const () as usize);
    let code = compile_with_transitions(&view, 113, &transitions)
        .expect("throwing element store is optimizing-eligible");
    let result = execute(&code, &[box_i32(99)]);
    assert_eq!(result.status, STATUS_THREW);
    assert_eq!(result.value, 0);
}

#[test]
fn property_store_roots_tagged_value_dead_after_transition() {
    let view = view(
        2,
        3,
        vec![
            (
                Op::StoreProperty,
                vec![
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                    Operand::Register(1),
                    Operand::Register(2),
                ],
            ),
            (Op::ReturnUndefined, vec![]),
        ],
    );
    let code = compile(&view, 114).expect("property store is optimizing-eligible");

    let record = code.safepoint_record(0).expect("property-store safepoint");
    assert_eq!(
        record.tagged_locations,
        vec![
            otter_vm::native_abi::TaggedLocation::frame_slot(0),
            otter_vm::native_abi::TaggedLocation::frame_slot(1),
        ]
    );
    assert_eq!(code.frame_map_bitmap_words(), &[0b11]);
}

#[test]
fn construct_transition_materializes_args_and_reloads_result() {
    let view = view(
        2,
        3,
        vec![
            (
                Op::New,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::ConstIndex(1),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ],
    );
    let transitions = construct_transitions(successful_construct as *const () as usize);
    let code = compile_with_transitions(&view, 117, &transitions)
        .expect("construct is optimizing-eligible");

    let result = execute(&code, &[box_i32(99), box_i32(7)]);
    assert_eq!(result.status, STATUS_RETURNED);
    assert_eq!(result.value, box_i32(37));

    let record = code.safepoint_record(0).expect("construct safepoint");
    assert_eq!(code.frame_map_bitmap_words(), &[0b11]);
    assert_eq!(record.tagged_locations.len(), 2);
}

#[test]
fn executes_add() {
    let view = view(
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
    let code = compile(&view, 1).expect("add is eligible");
    let result = execute(&code, &[box_i32(17), box_i32(-5)]);
    assert_eq!(result.status, STATUS_RETURNED);
    assert_eq!(unbox_i32(result.value), 12);
}

#[test]
fn executes_three_operation_expression() {
    let view = view(
        3,
        7,
        vec![
            (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(7)]),
            (
                Op::Add,
                vec![
                    Operand::Register(4),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (
                Op::Mul,
                vec![
                    Operand::Register(5),
                    Operand::Register(4),
                    Operand::Register(2),
                ],
            ),
            (
                Op::Sub,
                vec![
                    Operand::Register(6),
                    Operand::Register(5),
                    Operand::Register(3),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(6)]),
        ],
    );
    let code = compile(&view, 2).expect("three-op expression is eligible");
    let result = execute(&code, &[box_i32(6), box_i32(4), box_i32(3)]);
    assert_eq!(result.status, STATUS_RETURNED);
    assert_eq!(unbox_i32(result.value), 23);
}

#[test]
fn executes_float_add_mul_chain() {
    let view = float_view(
        3,
        5,
        vec![
            (
                Op::Add,
                vec![
                    Operand::Register(3),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (
                Op::Mul,
                vec![
                    Operand::Register(4),
                    Operand::Register(3),
                    Operand::Register(2),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(4)]),
        ],
        &[0, 1],
        &[],
    );
    let code = compile(&view, 101).expect("float add/mul chain is eligible");
    let result = execute(&code, &[box_f64(1.5), box_f64(2.0), box_f64(4.0)]);
    assert_eq!(result.status, STATUS_RETURNED);
    assert_eq!(result.value, box_f64(14.0));
}

#[test]
fn executes_float_division_with_int32_widening() {
    let view = float_view(
        0,
        3,
        vec![
            (Op::LoadInt32, vec![Operand::Register(0), Operand::Imm32(7)]),
            (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(2)]),
            (
                Op::Div,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ],
        &[2],
        &[],
    );
    let code = compile(&view, 102).expect("float division is eligible");
    let result = execute(&code, &[]);
    assert_eq!(result.status, STATUS_RETURNED);
    assert_eq!(result.value, box_f64(3.5));
}

#[test]
fn executes_float_remainder_with_exact_edge_semantics() {
    let view = float_view(
        2,
        3,
        vec![
            (
                Op::Rem,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ],
        &[0],
        &[],
    );
    let code = compile(&view, 116).expect("float remainder is eligible");

    let fractional = execute(&code, &[box_f64(7.5), box_f64(2.0)]);
    assert_eq!(fractional.status, STATUS_RETURNED);
    assert_eq!(fractional.value, box_f64(1.5));

    let negative_zero = execute(&code, &[box_f64(-6.0), box_f64(3.0)]);
    assert_eq!(negative_zero.status, STATUS_RETURNED);
    assert_eq!(negative_zero.value, box_f64(-0.0));

    let zero_divisor = execute(&code, &[box_f64(7.5), box_f64(0.0)]);
    assert_eq!(zero_divisor.status, STATUS_RETURNED);
    assert_eq!(zero_divisor.value, box_f64(f64::NAN));
}

#[test]
fn executes_numeric_negate_and_deopts_int32_zero() {
    let int_view = view(
        1,
        2,
        vec![
            (Op::Neg, vec![Operand::Register(1), Operand::Register(0)]),
            (Op::ReturnValue, vec![Operand::Register(1)]),
        ],
    );
    let int_code = compile(&int_view, 118).expect("int32 negate is eligible");
    assert_eq!(execute(&int_code, &[box_i32(7)]).value, box_i32(-7));
    let zero = execute(&int_code, &[box_i32(0)]);
    assert_eq!(zero.status, STATUS_BAILED);

    let float_view = float_view(
        1,
        2,
        vec![
            (Op::Neg, vec![Operand::Register(1), Operand::Register(0)]),
            (Op::ReturnValue, vec![Operand::Register(1)]),
        ],
        &[0],
        &[],
    );
    let float_code = compile(&float_view, 119).expect("float negate is eligible");
    assert_eq!(execute(&float_code, &[box_f64(-0.0)]).value, box_f64(0.0));
}

#[test]
fn logical_not_inverts_full_inline_truthiness() {
    let view = view(
        1,
        2,
        vec![
            (
                Op::LogicalNot,
                vec![Operand::Register(1), Operand::Register(0)],
            ),
            (Op::ReturnValue, vec![Operand::Register(1)]),
        ],
    );
    let code = compile(&view, 120).expect("logical-not is eligible");

    assert_eq!(execute(&code, &[VALUE_TRUE]).value, VALUE_FALSE);
    assert_eq!(execute(&code, &[box_i32(0)]).value, VALUE_TRUE);
    assert_eq!(execute(&code, &[box_f64(f64::NAN)]).value, VALUE_TRUE);
    assert_eq!(execute(&code, &[box_f64(1.5)]).value, VALUE_FALSE);
}

#[test]
fn executes_mixed_tagged_int_and_double_division() {
    let view = float_view(
        2,
        3,
        vec![
            (
                Op::Div,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ],
        &[0],
        &[],
    );
    let code = compile(&view, 103).expect("mixed division is eligible");
    let result = execute(&code, &[box_i32(7), box_f64(2.0)]);
    assert_eq!(result.status, STATUS_RETURNED);
    assert_eq!(result.value, box_f64(3.5));
    assert_eq!(
        execute(&code, &[box_f64(1.0), box_f64(0.0)]).value,
        box_f64(f64::INFINITY)
    );
    assert_eq!(
        execute(&code, &[box_f64(-1.0), box_f64(f64::INFINITY)]).value,
        box_f64(-0.0)
    );
    assert_eq!(
        execute(&code, &[box_f64(0.0), box_f64(0.0)]).value,
        box_f64(f64::NAN)
    );
}

#[test]
fn executes_float_compare_branch() {
    let view = float_view(
        2,
        4,
        vec![
            (
                Op::LessThan,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(2), Operand::Register(2)],
            ),
            (
                Op::LoadNumber,
                vec![Operand::Register(3), Operand::ConstIndex(0)],
            ),
            (Op::ReturnValue, vec![Operand::Register(3)]),
            (
                Op::LoadNumber,
                vec![Operand::Register(3), Operand::ConstIndex(1)],
            ),
            (Op::ReturnValue, vec![Operand::Register(3)]),
        ],
        &[0],
        &[(2, 11.5), (4, 22.5)],
    );
    let code = compile(&view, 104).expect("float comparison branch is eligible");
    assert_eq!(
        execute(&code, &[box_f64(1.5), box_f64(2.5)]).value,
        box_f64(11.5)
    );
    assert_eq!(
        execute(&code, &[box_f64(3.5), box_f64(2.5)]).value,
        box_f64(22.5)
    );
}

#[test]
fn executes_float_accumulation_loop_with_fp_phi_moves() {
    let view = float_view(
        1,
        7,
        vec![
            (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
            (
                Op::LoadNumber,
                vec![Operand::Register(2), Operand::ConstIndex(0)],
            ),
            (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(1)]),
            (
                Op::LoadNumber,
                vec![Operand::Register(4), Operand::ConstIndex(1)],
            ),
            (
                Op::LoadNumber,
                vec![Operand::Register(6), Operand::ConstIndex(2)],
            ),
            (
                Op::LessThan,
                vec![
                    Operand::Register(5),
                    Operand::Register(1),
                    Operand::Register(0),
                ],
            ),
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(4), Operand::Register(5)],
            ),
            (
                Op::Mul,
                vec![
                    Operand::Register(6),
                    Operand::Register(1),
                    Operand::Register(4),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(2),
                    Operand::Register(2),
                    Operand::Register(6),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(1),
                    Operand::Register(1),
                    Operand::Register(3),
                ],
            ),
            (Op::Jump, vec![Operand::Imm32(-6)]),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ],
        &[7, 8],
        &[(1, -0.0), (3, 1.5), (4, -0.0)],
    );
    let code = compile(&view, 105).expect("float accumulation loop is eligible");
    let result = execute(&code, &[box_i32(5)]);
    assert_eq!(result.status, STATUS_RETURNED);
    assert_eq!(result.value, box_f64(15.0));
}

#[test]
fn float_nan_relational_and_equality_compares_are_false() {
    let view = float_view(
        0,
        4,
        vec![
            (
                Op::LoadNumber,
                vec![Operand::Register(0), Operand::ConstIndex(0)],
            ),
            (
                Op::LoadNumber,
                vec![Operand::Register(1), Operand::ConstIndex(1)],
            ),
            (
                Op::LessThan,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (
                Op::JumpIfTrue,
                vec![Operand::Imm32(2), Operand::Register(2)],
            ),
            (
                Op::Equal,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(2), Operand::Register(2)],
            ),
            (
                Op::LoadNumber,
                vec![Operand::Register(3), Operand::ConstIndex(2)],
            ),
            (Op::ReturnValue, vec![Operand::Register(3)]),
            (
                Op::LoadNumber,
                vec![Operand::Register(3), Operand::ConstIndex(3)],
            ),
            (Op::ReturnValue, vec![Operand::Register(3)]),
        ],
        &[2, 4],
        &[(0, f64::NAN), (1, 1.0), (6, 11.5), (8, 22.5)],
    );
    let code = compile(&view, 106).expect("NaN comparison is eligible");
    let result = execute(&code, &[]);
    assert_eq!(result.status, STATUS_RETURNED);
    assert_eq!(result.value, box_f64(22.5));
}

#[test]
fn non_number_float_guard_deopts_and_boxes_prior_fp_value() {
    let view = float_view(
        2,
        4,
        vec![
            (
                Op::LoadNumber,
                vec![Operand::Register(2), Operand::ConstIndex(0)],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(2),
                ],
            ),
            (
                Op::Div,
                vec![
                    Operand::Register(3),
                    Operand::Register(2),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(3)]),
        ],
        &[1, 2],
        &[(0, 1.5)],
    );
    let code = compile(&view, 107).expect("float deopt fixture is eligible");
    let undefined = otter_vm::Value::undefined().to_bits();
    let (result, frame, resume_pc) = execute_with_frame(&code, &[box_f64(2.0), undefined]);
    assert_eq!(result.status, STATUS_BAILED);
    assert_eq!(result.value, 0);
    assert_eq!(resume_pc, 2);
    assert_eq!(frame[0], box_f64(2.0));
    assert_eq!(frame[1], undefined);
    assert_eq!(frame[2], box_f64(3.5));
}

#[test]
fn executes_if_else_with_distinct_values() {
    let view = view(
        2,
        4,
        vec![
            (
                Op::LessThan,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(2), Operand::Register(2)],
            ),
            (
                Op::LoadInt32,
                vec![Operand::Register(3), Operand::Imm32(11)],
            ),
            (Op::ReturnValue, vec![Operand::Register(3)]),
            (
                Op::LoadInt32,
                vec![Operand::Register(3), Operand::Imm32(22)],
            ),
            (Op::ReturnValue, vec![Operand::Register(3)]),
        ],
    );
    let code = compile(&view, 7).expect("if/else is eligible");

    let taken = execute(&code, &[box_i32(3), box_i32(8)]);
    assert_eq!(taken.status, STATUS_RETURNED);
    assert_eq!(unbox_i32(taken.value), 11);

    let fallthrough = execute(&code, &[box_i32(9), box_i32(4)]);
    assert_eq!(fallthrough.status, STATUS_RETURNED);
    assert_eq!(unbox_i32(fallthrough.value), 22);
}

#[test]
fn executes_tagged_phi_with_boxed_int32_edge() {
    let view = view(
        2,
        3,
        vec![
            (
                Op::StoreLocal,
                vec![Operand::Register(1), Operand::Imm32(2)],
            ),
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(2), Operand::Register(0)],
            ),
            (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(0)]),
            (Op::Jump, vec![Operand::Imm32(0)]),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ],
    );
    let code = compile(&view, 115).expect("tagged phi is eligible");

    let truthy = execute(&code, &[VALUE_TRUE, box_f64(1.5)]);
    assert_eq!(truthy.status, STATUS_RETURNED);
    assert_eq!(truthy.value, box_i32(0));

    let falsy = execute(&code, &[VALUE_FALSE, box_f64(1.5)]);
    assert_eq!(falsy.status, STATUS_RETURNED);
    assert_eq!(falsy.value, box_f64(1.5));
}

#[test]
fn executes_max_diamond_phi_in_both_orders() {
    let view = view(
        2,
        5,
        vec![
            (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(0)]),
            (
                Op::GreaterThan,
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
            (
                Op::Add,
                vec![
                    Operand::Register(4),
                    Operand::Register(0),
                    Operand::Register(2),
                ],
            ),
            (Op::Jump, vec![Operand::Imm32(1)]),
            (
                Op::Add,
                vec![
                    Operand::Register(4),
                    Operand::Register(1),
                    Operand::Register(2),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(4)]),
        ],
    );
    let cfg = ControlFlowGraph::build(&view).expect("diamond CFG");
    let ssa = SsaFunction::build(&view, &cfg).expect("diamond SSA");
    let phi_block = ssa
        .blocks
        .iter()
        .find(|block| {
            block
                .phis
                .iter()
                .any(|value| matches!(ssa.values[value.0 as usize].def, ValueDef::Phi { .. }))
        })
        .expect("diamond must contain a join phi")
        .id;
    let liveness = Liveness::compute(&ssa, &cfg);
    let tree = InlineTree::trivial(&view);
    let reprs = ReprMap::compute(&tree, &ssa);
    let allocation = Allocation::compute(&ssa, &cfg, &liveness, &reprs, REGISTER_BUDGET)
        .expect("diamond allocation");
    let incoming: Vec<_> = allocation
        .edge_moves
        .iter()
        .filter(|edge| edge.block == phi_block)
        .collect();
    assert_eq!(incoming.len(), 2);
    assert!(
        incoming.iter().any(|edge| !edge.moves.is_empty()),
        "fixture must execute a concrete phi edge move"
    );
    let code = compile(&view, 8).expect("max diamond is eligible");

    let left = execute(&code, &[box_i32(19), box_i32(7)]);
    assert_eq!(left.status, STATUS_RETURNED);
    assert_eq!(unbox_i32(left.value), 19);

    let right = execute(&code, &[box_i32(-4), box_i32(12)]);
    assert_eq!(right.status, STATUS_RETURNED);
    assert_eq!(unbox_i32(right.value), 12);
}

#[test]
fn executes_nested_if() {
    let view = view(
        3,
        6,
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
                vec![Operand::Imm32(6), Operand::Register(3)],
            ),
            (
                Op::LessThan,
                vec![
                    Operand::Register(4),
                    Operand::Register(1),
                    Operand::Register(2),
                ],
            ),
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(2), Operand::Register(4)],
            ),
            (
                Op::LoadInt32,
                vec![Operand::Register(5), Operand::Imm32(11)],
            ),
            (Op::ReturnValue, vec![Operand::Register(5)]),
            (
                Op::LoadInt32,
                vec![Operand::Register(5), Operand::Imm32(22)],
            ),
            (Op::ReturnValue, vec![Operand::Register(5)]),
            (
                Op::LoadInt32,
                vec![Operand::Register(5), Operand::Imm32(33)],
            ),
            (Op::ReturnValue, vec![Operand::Register(5)]),
        ],
    );
    let code = compile(&view, 9).expect("nested if is eligible");

    assert_eq!(
        unbox_i32(execute(&code, &[box_i32(1), box_i32(2), box_i32(3)]).value),
        11
    );
    assert_eq!(
        unbox_i32(execute(&code, &[box_i32(1), box_i32(4), box_i32(3)]).value),
        22
    );
    assert_eq!(
        unbox_i32(execute(&code, &[box_i32(5), box_i32(2), box_i32(3)]).value),
        33
    );
}

#[test]
fn non_entry_block_overflow_deopts() {
    let view = view(
        1,
        5,
        vec![
            (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
            (Op::LoadTrue, vec![Operand::Register(2)]),
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(3), Operand::Register(2)],
            ),
            (
                Op::LoadInt32,
                vec![Operand::Register(3), Operand::Imm32(i32::MAX)],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(4),
                    Operand::Register(3),
                    Operand::Register(0),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(4)]),
            (Op::LoadInt32, vec![Operand::Register(4), Operand::Imm32(7)]),
            (Op::ReturnValue, vec![Operand::Register(4)]),
        ],
    );
    let code = compile(&view, 10).expect("branch-local overflow is eligible");
    let (result, frame, resume_pc) = execute_with_frame(&code, &[box_i32(1)]);
    assert_eq!(result.status, STATUS_BAILED);
    assert_eq!(result.value, 0);
    assert_eq!(resume_pc, 4);
    assert_eq!(frame[0], box_i32(1));
    assert_eq!(frame[1], box_i32(0));
    assert_eq!(frame[2], VALUE_TRUE);
    assert_eq!(frame[3], box_i32(i32::MAX));
}

#[test]
fn executes_strict_int32_equality_branch() {
    let view = view(
        2,
        4,
        vec![
            (
                Op::Equal,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(2), Operand::Register(2)],
            ),
            (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(1)]),
            (Op::ReturnValue, vec![Operand::Register(3)]),
            (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(0)]),
            (Op::ReturnValue, vec![Operand::Register(3)]),
        ],
    );
    let code = compile(&view, 11).expect("strict equality branch is eligible");
    assert_eq!(
        unbox_i32(execute(&code, &[box_i32(6), box_i32(6)]).value),
        1
    );
    assert_eq!(
        unbox_i32(execute(&code, &[box_i32(6), box_i32(7)]).value),
        0
    );
}

#[test]
fn executes_forced_spills() {
    let mut instructions = vec![
        (Op::LoadInt32, vec![Operand::Register(4), Operand::Imm32(1)]),
        (Op::LoadInt32, vec![Operand::Register(5), Operand::Imm32(2)]),
        (Op::LoadInt32, vec![Operand::Register(6), Operand::Imm32(3)]),
        (Op::LoadInt32, vec![Operand::Register(7), Operand::Imm32(4)]),
        (Op::LoadInt32, vec![Operand::Register(8), Operand::Imm32(5)]),
        (Op::LoadInt32, vec![Operand::Register(9), Operand::Imm32(6)]),
        (
            Op::LoadInt32,
            vec![Operand::Register(10), Operand::Imm32(7)],
        ),
        (
            Op::LoadInt32,
            vec![Operand::Register(11), Operand::Imm32(8)],
        ),
    ];
    for (dst, left, right) in [
        (12, 0, 1),
        (13, 2, 3),
        (14, 4, 5),
        (15, 6, 7),
        (16, 8, 9),
        (17, 10, 11),
        (18, 12, 13),
        (19, 14, 15),
        (20, 16, 17),
        (21, 18, 19),
        (22, 21, 20),
    ] {
        instructions.push((
            Op::Add,
            vec![
                Operand::Register(dst),
                Operand::Register(left),
                Operand::Register(right),
            ],
        ));
    }
    instructions.push((Op::ReturnValue, vec![Operand::Register(22)]));
    let view = view(4, 23, instructions);
    let code = compile(&view, 3).expect("spill expression is eligible");
    assert!(code.metadata().linear_scan_spill_slot_count > 0);
    assert!(code.metadata().spill_slot_count > 0);
    let result = execute(&code, &[box_i32(10), box_i32(20), box_i32(30), box_i32(40)]);
    assert_eq!(result.status, STATUS_RETURNED);
    assert_eq!(unbox_i32(result.value), 136);
}

#[test]
fn executes_forced_fp_spills() {
    let mut instructions = Vec::new();
    let mut numbers = Vec::new();
    for register in 0_u16..9 {
        instructions.push((
            Op::LoadNumber,
            vec![
                Operand::Register(register),
                Operand::ConstIndex(register as u32),
            ],
        ));
        numbers.push((u32::from(register), f64::from(register) + 0.5));
    }
    let mut float_pcs = Vec::new();
    let mut accumulator = 0_u16;
    for right in 1_u16..9 {
        let destination = 8 + right;
        let pc = instructions.len() as u32;
        float_pcs.push(pc);
        instructions.push((
            Op::Add,
            vec![
                Operand::Register(destination),
                Operand::Register(accumulator),
                Operand::Register(right),
            ],
        ));
        accumulator = destination;
    }
    instructions.push((Op::ReturnValue, vec![Operand::Register(accumulator)]));

    let view = float_view(0, 17, instructions, &float_pcs, &numbers);
    let code = compile(&view, 108).expect("FP spill expression is eligible");
    assert!(code.metadata().linear_scan_spill_slot_count > 0);
    assert!(code.metadata().spill_slot_count > 0);
    let result = execute(&code, &[]);
    assert_eq!(result.status, STATUS_RETURNED);
    assert_eq!(result.value, box_f64(40.5));
}

#[test]
fn parameter_guard_bails_at_first_use_logical_pc() {
    let view = view(
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
    let code = compile(&view, 4).expect("guarded add is eligible");
    let (result, frame, resume_pc) = execute_with_frame(&code, &[0, box_i32(9)]);
    assert_eq!(result.status, STATUS_BAILED);
    assert_eq!(result.value, 0);
    assert_eq!(resume_pc, 0);
    assert_eq!(
        frame,
        vec![0, box_i32(9), otter_vm::Value::undefined().to_bits()]
    );
}

#[test]
fn int32_overflow_bails_at_arithmetic_logical_pc() {
    let view = view(
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
    let code = compile(&view, 5).expect("overflow-checked add is eligible");
    let (result, frame, resume_pc) = execute_with_frame(&code, &[box_i32(i32::MAX), box_i32(1)]);
    assert_eq!(result.status, STATUS_BAILED);
    assert_eq!(result.value, 0);
    assert_eq!(resume_pc, 0);
    assert_eq!(
        frame,
        vec![
            box_i32(i32::MAX),
            box_i32(1),
            otter_vm::Value::undefined().to_bits()
        ]
    );
}

#[test]
fn later_parameter_guard_materializes_prior_intermediates() {
    let view = view(
        2,
        5,
        vec![
            (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(7)]),
            (
                Op::Add,
                vec![
                    Operand::Register(3),
                    Operand::Register(0),
                    Operand::Register(2),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(4),
                    Operand::Register(3),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(4)]),
        ],
    );
    let code = compile(&view, 6).expect("later guard leaf is eligible");
    let undefined = otter_vm::Value::undefined().to_bits();
    let (result, frame, resume_pc) = execute_with_frame(&code, &[box_i32(5), undefined]);
    assert_eq!(result.status, STATUS_BAILED);
    assert_eq!(result.value, 0);
    assert_eq!(resume_pc, 2);
    assert_eq!(
        frame,
        vec![box_i32(5), undefined, box_i32(7), box_i32(12), undefined]
    );
}

#[test]
fn refuses_unsupported_operation() {
    let view = view(
        1,
        2,
        vec![
            (Op::TypeOf, vec![Operand::Register(1), Operand::Register(0)]),
            (Op::ReturnValue, vec![Operand::Register(1)]),
        ],
    );
    assert!(compile(&view, 6).is_err());
}

#[test]
fn executes_summation_loop_with_header_phis() {
    let view = summation_view();
    let cfg = ControlFlowGraph::build(&view).expect("summation CFG");
    let header = cfg
        .blocks
        .iter()
        .find(|block| block.is_loop_header)
        .expect("summation loop header")
        .id;
    let ssa = SsaFunction::build(&view, &cfg).expect("summation SSA");
    assert!(ssa.blocks[header.0 as usize].phis.len() >= 2);
    let liveness = Liveness::compute(&ssa, &cfg);
    let tree = InlineTree::trivial(&view);
    let reprs = ReprMap::compute(&tree, &ssa);
    let allocation = Allocation::compute(&ssa, &cfg, &liveness, &reprs, REGISTER_BUDGET)
        .expect("summation allocation");
    assert!(
        allocation
            .edge_moves
            .iter()
            .any(|edge| edge.block == header && !edge.moves.is_empty()),
        "fixture must require concrete loop-header phi moves"
    );

    let code = compile(&view, 12).expect("summation loop is eligible");
    for (n, expected) in [(0, 0), (1, 0), (5, 10), (10, 45), (100, 4_950)] {
        let result = execute(&code, &[box_i32(n)]);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(unbox_i32(result.value), expected, "n={n}");
    }
}

#[test]
fn osr_materializes_live_header_phis_from_interpreter_window() {
    let code = compile(&summation_view(), 120).expect("summation loop is eligible");
    let frame = vec![box_i32(10), box_i32(4), box_i32(6), box_i32(1), VALUE_TRUE];
    let (result, _frame, _resume_pc) = execute_osr_with_frame(&code, 3, frame);
    assert_eq!(result.status, STATUS_RETURNED);
    assert_eq!(unbox_i32(result.value), 45);
}

#[test]
fn osr_representation_mismatch_bails_with_window_untouched() {
    let code = compile(&summation_view(), 121).expect("summation loop is eligible");
    let frame = vec![
        box_i32(10),
        box_f64(4.5),
        box_i32(6),
        box_i32(1),
        VALUE_TRUE,
    ];
    let (result, after, resume_pc) = execute_osr_with_frame(&code, 3, frame.clone());
    assert_eq!(result.status, STATUS_BAILED);
    assert_eq!(resume_pc, 3);
    assert_eq!(after, frame);
}

#[test]
fn loop_overflow_deopts_with_reconstructed_mid_loop_frame() {
    let view = view(
        1,
        5,
        vec![
            (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
            (
                Op::LoadInt32,
                vec![Operand::Register(2), Operand::Imm32(i32::MAX - 2)],
            ),
            (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(1)]),
            (
                Op::LessThan,
                vec![
                    Operand::Register(4),
                    Operand::Register(1),
                    Operand::Register(0),
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
                    Operand::Register(3),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(1),
                    Operand::Register(1),
                    Operand::Register(3),
                ],
            ),
            (Op::Jump, vec![Operand::Imm32(-5)]),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ],
    );
    let code = compile(&view, 13).expect("overflowing loop is eligible");
    let (result, frame, resume_pc) = execute_with_frame(&code, &[box_i32(5)]);
    assert_eq!(result.status, STATUS_BAILED);
    assert_eq!(result.value, 0);
    assert_eq!(resume_pc, 5);
    assert_eq!(frame[0], box_i32(5));
    assert_eq!(frame[1], box_i32(2));
    assert_eq!(frame[2], box_i32(i32::MAX));
    assert_eq!(frame[3], box_i32(1));
    assert_eq!(frame[4], VALUE_TRUE);
}

#[test]
fn overflow_after_osr_reconstructs_current_loop_frame() {
    let view = view(
        1,
        5,
        vec![
            (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
            (
                Op::LoadInt32,
                vec![Operand::Register(2), Operand::Imm32(i32::MAX - 2)],
            ),
            (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(1)]),
            (
                Op::LessThan,
                vec![
                    Operand::Register(4),
                    Operand::Register(1),
                    Operand::Register(0),
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
                    Operand::Register(3),
                ],
            ),
            (
                Op::Add,
                vec![
                    Operand::Register(1),
                    Operand::Register(1),
                    Operand::Register(3),
                ],
            ),
            (Op::Jump, vec![Operand::Imm32(-5)]),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ],
    );
    let code = compile(&view, 122).expect("overflowing loop is eligible");
    let frame = vec![
        box_i32(5),
        box_i32(1),
        box_i32(i32::MAX - 1),
        box_i32(1),
        VALUE_TRUE,
    ];
    let (result, frame, resume_pc) = execute_osr_with_frame(&code, 3, frame);
    assert_eq!(result.status, STATUS_BAILED);
    assert_eq!(resume_pc, 5);
    assert_eq!(frame[0], box_i32(5));
    assert_eq!(frame[1], box_i32(2));
    assert_eq!(frame[2], box_i32(i32::MAX));
    assert_eq!(frame[3], box_i32(1));
    assert_eq!(frame[4], VALUE_TRUE);
}

#[test]
fn executes_nested_loops() {
    let view = view(
        1,
        4,
        vec![
            (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
            (
                Op::LessThan,
                vec![
                    Operand::Register(3),
                    Operand::Register(1),
                    Operand::Register(0),
                ],
            ),
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(9), Operand::Register(3)],
            ),
            (Op::LoadInt32, vec![Operand::Register(2), Operand::Imm32(0)]),
            (
                Op::LessThan,
                vec![
                    Operand::Register(3),
                    Operand::Register(2),
                    Operand::Register(1),
                ],
            ),
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(3), Operand::Register(3)],
            ),
            (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(1)]),
            (
                Op::Add,
                vec![
                    Operand::Register(2),
                    Operand::Register(2),
                    Operand::Register(3),
                ],
            ),
            (Op::Jump, vec![Operand::Imm32(-5)]),
            (Op::LoadInt32, vec![Operand::Register(3), Operand::Imm32(1)]),
            (
                Op::Add,
                vec![
                    Operand::Register(1),
                    Operand::Register(1),
                    Operand::Register(3),
                ],
            ),
            (Op::Jump, vec![Operand::Imm32(-11)]),
            (Op::ReturnValue, vec![Operand::Register(1)]),
        ],
    );
    let code = compile(&view, 14).expect("nested loops are eligible");
    for n in [0, 1, 5, 20] {
        let result = execute(&code, &[box_i32(n)]);
        assert_eq!(result.status, STATUS_RETURNED);
        assert_eq!(unbox_i32(result.value), n, "n={n}");
    }
}

#[test]
fn backedge_interrupt_raises_through_the_poll_stub() {
    // The poll matches the template tier: an interrupt reaches the leaf
    // poll stub, which raises; the loop never deoptimizes for it.
    extern "C" fn raising_poll(_ctx: *mut JitCtx) -> u64 {
        1
    }
    let view = view(
        0,
        2,
        vec![
            (Op::LoadInt32, vec![Operand::Register(0), Operand::Imm32(0)]),
            (Op::LoadInt32, vec![Operand::Register(1), Operand::Imm32(0)]),
            (
                Op::Add,
                vec![
                    Operand::Register(0),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::Jump, vec![Operand::Imm32(-2)]),
        ],
    );
    let mut transitions = TransitionTable::resolve();
    transitions.replace_entry_for_test(
        otter_vm::native_abi::STUB_JIT_BACKEDGE_POLL,
        raising_poll as *const () as usize,
    );
    let code = compile_with_transitions(&view, 15, &transitions)
        .expect("reducible infinite loop is eligible");
    let interrupt = 1_u8;
    let mut fuel = i64::MAX as u64;
    let (result, _, _) =
        execute_with_poll_cells(&code, &[], std::ptr::addr_of!(interrupt), &mut fuel);
    assert_eq!(result.status, STATUS_THREW);
}

#[test]
fn exhausted_backedge_fuel_refills_through_the_poll_stub() {
    // An exhausted budget refills through the stub and the compiled loop
    // keeps running to its own return — no deopt, no interpreter resume.
    extern "C" fn refilling_poll(ctx: *mut JitCtx) -> u64 {
        // SAFETY: the fixture's thread cell outlives the call.
        unsafe {
            let thread = &*(*ctx).thread;
            let fuel = thread.backedge_fuel_cell as *mut u64;
            *fuel = 1_000_000;
        }
        0
    }
    let view = summation_view();
    let mut transitions = TransitionTable::resolve();
    transitions.replace_entry_for_test(
        otter_vm::native_abi::STUB_JIT_BACKEDGE_POLL,
        refilling_poll as *const () as usize,
    );
    let code =
        compile_with_transitions(&view, 16, &transitions).expect("summation loop is eligible");
    let interrupt = 0_u8;
    let mut fuel = 1_u64;
    let (result, _, _) = execute_with_poll_cells(
        &code,
        &[box_i32(5)],
        std::ptr::addr_of!(interrupt),
        &mut fuel,
    );

    assert_eq!(result.status, STATUS_RETURNED);
    assert_eq!(unbox_i32(result.value), 10);
}

#[test]
fn refuses_irreducible_loop() {
    let view = view(
        0,
        1,
        vec![
            (Op::LoadTrue, vec![Operand::Register(0)]),
            (
                Op::JumpIfTrue,
                vec![Operand::Imm32(1), Operand::Register(0)],
            ),
            (Op::Jump, vec![Operand::Imm32(0)]),
            (Op::Jump, vec![Operand::Imm32(-2)]),
        ],
    );
    assert!(compile(&view, 17).is_err());
}
