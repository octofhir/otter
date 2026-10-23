//! Execution tests of the graph tier on hand-built bytecode.
//!
//! # Contents
//! - A harness that compiles a test snapshot and runs its tier entry over a
//!   fake published interpreter frame.
//! - Straight-line, branch and loop programs over int32 and float64
//!   feedback, and exact deopt exits.
//! - Every node kind's declared slow-path stubs agree with what its
//!   properties say it may do.
//! - A cancelled request stops before bytecode analysis can reject its body.
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

#[test]
fn cancellation_precedes_bytecode_analysis() {
    // An empty body fails analysis unless cancellation stops the request first.
    let view = JitCompileSnapshot::without_feedback(90, 0, 0, Vec::new());
    let cancellation = otter_vm::JitCompileCancellation::default();
    cancellation.cancel();
    let result = super::compile_optimized(
        &view,
        7000,
        &TransitionTable::resolve(),
        None,
        None,
        false,
        &cancellation,
    );
    assert!(matches!(result, Err(crate::CompileFailure::Cancelled)));
}

/// Run the tier entry over a frame holding `args`; returns the result pair
/// and the frame window afterwards.
fn run(view: &JitCompileSnapshot, args: &[u64]) -> (NativeResultPair, Vec<u64>) {
    let transitions = TransitionTable::resolve();
    let compiled = super::compile(view, 7001, &transitions, None, false, &Default::default())
        .expect("graph compile");
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
fn captured_splices_report_exact_nested_source_owners_and_budget_bytes() {
    use otter_vm::jit::{JitDirectCallPlan, JitDirectCallThisMode, JitDirectCallee};
    use otter_vm::{
        JitCompilerDiagnostic, JitFunctionCode, JitInlineCallee, JitInlineLoweringOutcome,
    };
    use std::sync::Arc;

    fn give_id(view: &mut JitCompileSnapshot, fid: u32) {
        Arc::get_mut(&mut view.code_block).unwrap().id = fid;
    }

    fn offer(caller: &mut JitCompileSnapshot, body: Arc<JitCompileSnapshot>) {
        caller.instructions[0].call_attempted = true;
        caller.direct_callees.insert(
            0,
            vec![JitDirectCallee {
                plan: JitDirectCallPlan {
                    function_id: body.code_block.id,
                    code_object_id: 1,
                    entry_cell: 0,
                    tier: NativeFrameKind::Baseline,
                    this_mode: JitDirectCallThisMode::StrictOrLexical,
                    is_derived_constructor: false,
                    call_flags: 0,
                    callee_cell: 0,
                },
                receiver_allocation: None,
            }],
        );
        caller.inline_callees.insert(0, JitInlineCallee { body });
    }

    // The innermost body has no exits of its own: source attribution must
    // come from accepted graph origins rather than deopt frame recipes.
    let mut leaf = snapshot(0, 1, vec![(Op::ReturnUndefined, vec![])]);
    give_id(&mut leaf, 91);
    let leaf = Arc::new(leaf);
    let mut middle = snapshot(
        1,
        2,
        vec![
            (
                Op::Call,
                vec![
                    Operand::Register(1),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(1)]),
        ],
    );
    give_id(&mut middle, 92);
    offer(&mut middle, leaf.clone());
    let middle = Arc::new(middle);
    let mut root = snapshot(
        2,
        3,
        vec![
            (
                Op::Call,
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
    offer(&mut root, middle.clone());
    let transitions = TransitionTable::resolve();
    let disabled = super::compile_optimized(
        &root,
        7002,
        &transitions,
        None,
        None,
        false,
        &Default::default(),
    )
    .expect("nested graph without events");
    assert!(disabled.diagnostics.is_empty());
    assert_eq!(disabled.code.spliced_functions(), &[91, 92]);

    let captured = super::compile_optimized(
        &root,
        7003,
        &transitions,
        None,
        None,
        true,
        &Default::default(),
    )
    .expect("nested graph with events");
    assert_eq!(captured.code.spliced_functions(), &[91, 92]);
    assert_eq!(
        captured.diagnostics.as_ref(),
        &[
            JitCompilerDiagnostic::InlineLowered {
                parent_function_id: 90,
                instruction_pc: 0,
                byte_pc: 0,
                callee_function_id: 92,
                depth: 1,
                cost: middle.code_block.instruction_count(),
                outcome: JitInlineLoweringOutcome::Inlined,
            },
            JitCompilerDiagnostic::InlineLowered {
                parent_function_id: 92,
                instruction_pc: 0,
                byte_pc: 0,
                callee_function_id: 91,
                depth: 2,
                cost: leaf.code_block.instruction_count(),
                outcome: JitInlineLoweringOutcome::Inlined,
            },
        ]
    );
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
    let (result, _) = run(
        &view,
        &[Value::double(40.0).to_bits(), Value::double(2.0).to_bits()],
    );
    assert_eq!(returned(result), int(42));
    let original = [Value::double(-0.0).to_bits(), int(0)];
    let (result, window) = run(&view, &original);
    assert_eq!(
        result.validate(NativeResultDomain::Compiled),
        Some(NativeResultStatus::SideExit)
    );
    assert_eq!(&window[..2], &original);
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
    // The builder's diamond, edge blocks and all, picks with one select.
    let transitions = TransitionTable::resolve();
    let compiled = super::compile(&view, 7002, &transitions, None, false, &Default::default())
        .expect("graph compile");
    let graph = &compiled.built.graph;
    let selects = compiled
        .built
        .layout
        .iter()
        .flat_map(|&block| graph.block(block).body.iter())
        .filter(|&&node| matches!(graph.node(node).kind, super::ir::Kind::Select(_)))
        .count();
    assert_eq!(selects, 1, "{}", graph.dump(&compiled.built.layout));
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
    let compiled = super::compile(&view, 7001, &transitions, None, false, &Default::default())
        .expect("graph compile");
    let dump = compiled.built.graph.dump(&compiled.built.layout);
    let (result, _) = run(&view, &[int(10), int(1)]);
    assert_eq!(
        returned(result),
        int(40),
        "{dump}\n{:#?}",
        compiled.allocation.edges
    );
}

#[path = "osr_tests.rs"]
mod osr_tests;

/// One node of every kind, with each payload that selects other slow paths.
fn every_kind() -> Vec<super::ir::Kind> {
    use super::ir::{BlockId, BranchKind, Condition, DeoptReason, Kind, MathOp};
    use otter_vm::jit::{
        JitArrayLayout, JitBodySlot, JitElementRepr, JitHoleBitmap, JitReceiverAllocationPlan,
    };
    use otter_vm::object::FieldLocation;
    let plan = JitReceiverAllocationPlan {
        new_target_function_id: 1,
        new_target_is_class: false,
        family_id: 1,
        family_cell: 8,
        receiver_shape: 0,
        initial_field_count: 0,
        inline_capacity: 4,
        prototype_validity: None,
        prototype_root: 16,
    };
    let holes = JitHoleBitmap {
        capacity_byte: 0,
        hole_count_byte: 4,
        storage_kind_byte: 8,
        kind_byte: 12,
        packed_kind: 1,
        holey_kind: 2,
    };
    let field = FieldLocation::inline(0);
    let condition = Condition::Less;
    let mut kinds = vec![
        Kind::ConstTagged(0),
        Kind::ConstInt32(0),
        Kind::ConstFloat64(0),
        Kind::InitialRegister(0),
        Kind::LoadThis,
        Kind::LoadNewTarget,
        Kind::LoadClosure,
        Kind::LoadLiteral(0),
        Kind::LoadGlobalBinding(0),
        Kind::LoadGlobalThis,
        Kind::LoadWindow(0),
        Kind::Phi,
        Kind::Select(BranchKind::Truthy),
        Kind::AllocationGroup(0),
        Kind::AllocationProjection(0),
        Kind::NewObject,
        Kind::NewArrayEmpty,
        Kind::NewObjectLiteral { fields: None },
        Kind::NewArrayLiteral,
        Kind::NativeNewContext(0),
        Kind::CopyContext,
        Kind::NewClosure,
        Kind::NewReceiver(plan),
        Kind::CheckReceiverPlan(plan),
        Kind::VirtualObject { shape: 0 },
        Kind::NewArrayWithLength,
        Kind::PrimitiveAdd,
        Kind::BigIntBinary(otter_vm::bigint::ops::Operator::Add),
        Kind::PrimitiveCompare(condition),
        Kind::Int32Add,
        Kind::Int32Sub,
        Kind::Int32AddWrapping,
        Kind::Int32SubWrapping,
        Kind::Int32Mul,
        Kind::Int32MulExact,
        Kind::Int32MulIdentifyZeros,
        Kind::Int32Div,
        Kind::Int32Mod,
        Kind::Int32Negate,
        Kind::Int32BitAnd,
        Kind::Int32BitOr,
        Kind::Int32BitXor,
        Kind::Int32BitNot,
        Kind::Int32ShiftLeft,
        Kind::Int32ShiftRight,
        Kind::Int32ShiftRightLogical,
        Kind::Uint32ShiftRightToFloat64,
        Kind::Int32Compare(condition),
        Kind::Float64Add,
        Kind::Float64Sub,
        Kind::Float64Mul,
        Kind::Float64Div,
        Kind::Float64Negate,
        Kind::Float64Mod,
        Kind::Float64Math(MathOp::Sqrt),
        Kind::Float64Math(MathOp::Pow),
        Kind::Float64Compare(condition),
        Kind::CheckedNumberToInt32,
        Kind::CheckedTaggedToFloat64,
        Kind::Int32ToTagged,
        Kind::Float64ToTagged,
        Kind::Int32ToFloat64,
        Kind::TruncateFloat64ToInt32,
        Kind::CheckedFloat64ToInt32,
        Kind::CheckedTaggedToIndex,
        Kind::CheckedFloat64ToIndex,
        Kind::StrictEqual { negate: false },
        Kind::LooseEqual { negate: false },
        Kind::NullishEqual {
            negate: false,
            object_or_nullish: false,
        },
        Kind::ToBoolean,
        Kind::LogicalNot,
        Kind::TestTypeOf { test: 0 },
        Kind::CheckNumber,
        Kind::CheckNotNullish { mask: 1 },
        Kind::CheckFieldTypes(Default::default()),
        Kind::CheckShapes {
            shapes: Default::default(),
            writable: true,
            cell_proved: false,
        },
        Kind::CheckBounds,
        Kind::CheckElements {
            type_tag: 0,
            guards: [None, None],
            holes: Some(holes),
            cached_base: None,
        },
        Kind::CheckElementPresent,
        Kind::LoadPresentElement,
        Kind::CheckHoleyElementPresent(holes),
        Kind::LoadOwnField { field, atom: 0 },
        Kind::LoadNumberOwnField {
            double_only: false,
            field,
            atom: 0,
        },
        Kind::LoadArrayExpando(JitArrayLayout::default()),
        Kind::StringCharCodeAt,
        Kind::LoadFunctionBag,
        Kind::StoreOwnField {
            field,
            atom: 0,
            owners: Default::default(),
        },
        Kind::StoreDoubleOwnField {
            field,
            atom: 0,
            owners: Default::default(),
        },
        Kind::LoadShapedProperty { atom: 0 },
        Kind::LoadNamedProperty {
            byte_pc: 0,
            programs: Default::default(),
            receiver_shape: None,
        },
        Kind::StoreNamedProperty {
            plan: super::ir::NamedStorePlan::Programs,
            byte_pc: 0,
            owners: Default::default(),
            receiver_shape: None,
            fresh: false,
        },
        Kind::StoreNamedProperty {
            plan: super::ir::NamedStorePlan::TransitionTagged(0),
            byte_pc: 0,
            owners: Default::default(),
            receiver_shape: None,
            fresh: false,
        },
        Kind::StoreNamedProperty {
            plan: super::ir::NamedStorePlan::TransitionDouble(0),
            byte_pc: 0,
            owners: Default::default(),
            receiver_shape: None,
            fresh: false,
        },
        Kind::StoreBodySlot(JitBodySlot {
            type_tag: 0,
            guard: None,
            byte: 8,
        }),
        Kind::LoadPropertyCached {
            pc: 0,
            atom: None,
            length: true,
        },
        Kind::StorePropertyCached {
            pc: 0,
            atom: None,
            barrier: true,
        },
        Kind::StoreKeyedCached { pc: 0 },
        Kind::LoadKeyedCached { pc: 0 },
        Kind::RegExpMethod { test: true },
        Kind::RegExpMethod { test: false },
        Kind::LoadTaggedField(8),
        Kind::StoreTaggedField(8),
        Kind::WriteBarrier,
        Kind::ElementWriteBarrier,
        Kind::LoadContextParent,
        Kind::LoadClosureContext,
        Kind::LoadArrayLength {
            array_proved: false,
        },
        Kind::LoadTypedArrayLength,
        Kind::LoadElementsLength {
            byte: 8,
            width: Default::default(),
        },
        Kind::LoadElementsBase {
            byte: 8,
            off_heap: false,
        },
        Kind::LoadElement(JitElementRepr::default()),
        Kind::LoadElementUint32ToFloat64,
        Kind::LoadHoleyFloat64Element(holes),
        Kind::StoreElement(JitElementRepr::default()),
        Kind::CallJs {
            pc: 0,
            plan: crate::call_linkage::CallPlan::Generic,
            construct: true,
            receiver: false,
            receiver_object: false,
            allocation: Some(plan),
        },
        Kind::CallForward {
            pc: 0,
            plan: crate::call_linkage::CallPlan::Generic,
            bindings: Box::new([]),
        },
        Kind::CheckFunctionPrototypeCall(0),
        Kind::CheckNative(0),
        Kind::CheckArgumentsElided,
        Kind::LoadArgumentCount,
        Kind::LoadActualArgument,
        Kind::SelectArgument { count: 1 },
        Kind::CheckFunction {
            function_id: 1,
            cell: 0,
        },
        Kind::CheckCellValue(0),
        Kind::LoadCellValue(0),
        Kind::CheckClosureContext(0),
        Kind::CheckContextSlot { watch: 0, slot: 0 },
        Kind::NoteContextWrite(0),
        Kind::CheckNotHole,
        Kind::Instanceof,
        Kind::ArrayPush(0),
        Kind::ArrayPop(0),
        Kind::LoadGuardedMethod {
            byte_pc: 0,
            target: 0,
            receiver_proved: false,
            cell_read: false,
        },
        Kind::LoadReceiverShape,
        Kind::BooleanToInt32,
        Kind::Generic {
            pc: 0,
            registers: Box::new([]),
        },
        Kind::LoadBackedgeFuel,
        Kind::DecrementBackedgeFuel,
        Kind::StoreBackedgeFuel,
        Kind::Jump(BlockId(0)),
        Kind::JumpLoop(BlockId(0)),
        Kind::Branch {
            kind: BranchKind::Int32(condition),
            if_true: BlockId(0),
            if_false: BlockId(1),
        },
        Kind::Return,
        Kind::Deopt(DeoptReason::WrongType),
        Kind::CheckStableShapes(Default::default()),
        Kind::CheckDependencies,
    ];
    for kind in [
        BranchKind::Truthy,
        BranchKind::Float64(condition),
        BranchKind::Float64Truthy,
        BranchKind::TaggedEqual,
        BranchKind::Nullish,
        BranchKind::WordEqual(0),
        BranchKind::Native(0),
    ] {
        kinds.push(Kind::Branch {
            kind,
            if_true: BlockId(0),
            if_false: BlockId(1),
        });
    }
    for stub in [
        otter_vm::native_abi::STUB_MATH_ABS_LEAF,
        otter_vm::native_abi::STUB_STRING_CHAR_CODE_AT_LEAF,
        otter_vm::native_abi::STUB_COLLECTION_MAP_SET_MUTATING,
    ] {
        kinds.push(Kind::NativeLeaf(stub.id));
    }
    for kind in &kinds {
        listed(kind);
    }
    kinds
}

/// Fails to compile once a kind exists that [`every_kind`] does not list.
fn listed(kind: &super::ir::Kind) {
    use super::ir::Kind as K;
    match kind {
        K::ConstTagged(_)
        | K::ConstInt32(_)
        | K::ConstFloat64(_)
        | K::InitialRegister(_)
        | K::LoadThis
        | K::LoadNewTarget
        | K::LoadClosure
        | K::LoadLiteral(_)
        | K::LoadGlobalBinding(_)
        | K::LoadGlobalThis
        | K::LoadWindow(_)
        | K::Phi
        | K::Select(_)
        | K::AllocationGroup(_)
        | K::AllocationProjection(_)
        | K::NewObject
        | K::NewArrayEmpty
        | K::NewObjectLiteral { .. }
        | K::NewArrayLiteral
        | K::NativeNewContext(_)
        | K::CopyContext
        | K::NewClosure
        | K::NewReceiver(_)
        | K::CheckReceiverPlan(_)
        | K::VirtualObject { .. }
        | K::NewArrayWithLength
        | K::PrimitiveAdd
        | K::BigIntBinary(_)
        | K::PrimitiveCompare(_)
        | K::Int32Add
        | K::Int32Sub
        | K::Int32AddWrapping
        | K::Int32SubWrapping
        | K::Int32Mul
        | K::Int32MulExact
        | K::Int32MulIdentifyZeros
        | K::Int32Div
        | K::Int32Mod
        | K::Int32Negate
        | K::Int32BitAnd
        | K::Int32BitOr
        | K::Int32BitXor
        | K::Int32BitNot
        | K::Int32ShiftLeft
        | K::Int32ShiftRight
        | K::Int32ShiftRightLogical
        | K::Uint32ShiftRightToFloat64
        | K::Int32Compare(_)
        | K::Float64Add
        | K::Float64Sub
        | K::Float64Mul
        | K::Float64Div
        | K::Float64Negate
        | K::Float64Mod
        | K::Float64Math(_)
        | K::Float64Compare(_)
        | K::CheckedNumberToInt32
        | K::CheckedTaggedToFloat64
        | K::Int32ToTagged
        | K::Float64ToTagged
        | K::Int32ToFloat64
        | K::TruncateFloat64ToInt32
        | K::CheckedFloat64ToInt32
        | K::CheckedTaggedToIndex
        | K::CheckedFloat64ToIndex
        | K::StrictEqual { .. }
        | K::LooseEqual { .. }
        | K::NullishEqual { .. }
        | K::ToBoolean
        | K::LogicalNot
        | K::TestTypeOf { .. }
        | K::CheckNumber
        | K::CheckNotNullish { .. }
        | K::CheckFieldTypes(_)
        | K::CheckShapes { .. }
        | K::CheckBounds
        | K::CheckElements { .. }
        | K::CheckElementPresent
        | K::LoadPresentElement
        | K::CheckHoleyElementPresent(_)
        | K::LoadOwnField { .. }
        | K::LoadNumberOwnField { .. }
        | K::LoadArrayExpando(_)
        | K::StringCharCodeAt
        | K::LoadFunctionBag
        | K::StoreOwnField { .. }
        | K::StoreDoubleOwnField { .. }
        | K::LoadShapedProperty { .. }
        | K::LoadNamedProperty { .. }
        | K::StoreNamedProperty { .. }
        | K::StoreBodySlot(_)
        | K::LoadPropertyCached { .. }
        | K::StorePropertyCached { .. }
        | K::StoreKeyedCached { .. }
        | K::LoadKeyedCached { .. }
        | K::RegExpMethod { .. }
        | K::LoadTaggedField(_)
        | K::StoreTaggedField(_)
        | K::WriteBarrier
        | K::ElementWriteBarrier
        | K::LoadContextParent
        | K::LoadClosureContext
        | K::LoadArrayLength { .. }
        | K::LoadTypedArrayLength
        | K::LoadElementsLength { .. }
        | K::LoadElementsBase { .. }
        | K::LoadElement(_)
        | K::LoadElementUint32ToFloat64
        | K::LoadHoleyFloat64Element(_)
        | K::StoreElement(_)
        | K::NativeLeaf(_)
        | K::CallJs { .. }
        | K::CallForward { .. }
        | K::CheckFunctionPrototypeCall(_)
        | K::CheckNative(_)
        | K::CheckArgumentsElided
        | K::LoadArgumentCount
        | K::LoadActualArgument
        | K::SelectArgument { .. }
        | K::CheckFunction { .. }
        | K::CheckCellValue(_)
        | K::LoadCellValue(_)
        | K::CheckClosureContext(_)
        | K::CheckContextSlot { .. }
        | K::NoteContextWrite(_)
        | K::CheckNotHole
        | K::Instanceof
        | K::ArrayPush(_)
        | K::ArrayPop(_)
        | K::LoadGuardedMethod { .. }
        | K::LoadReceiverShape
        | K::BooleanToInt32
        | K::Generic { .. }
        | K::LoadBackedgeFuel
        | K::DecrementBackedgeFuel
        | K::StoreBackedgeFuel
        | K::Jump(_)
        | K::JumpLoop(_)
        | K::Branch { .. }
        | K::Return
        | K::Deopt(_)
        | K::CheckStableShapes(_)
        | K::CheckDependencies => {}
    }
}

#[test]
fn slow_path_stubs_bound_what_a_node_may_do() {
    use otter_vm::native_abi::{RUNTIME_STUB_DESCRIPTORS, RuntimeStubEffects as E};
    for kind in every_kind() {
        let properties = kind.properties();
        let stubs = kind.slow_path_stubs();
        for stub in stubs {
            assert_eq!(
                RUNTIME_STUB_DESCRIPTORS.get(stub.id as usize - 1),
                Some(stub),
                "{kind:?} names a declared stub"
            );
        }
        let any = |mask: u16| stubs.iter().any(|stub| stub.effects.bits() & mask != 0);
        let reenters = any(E::MAY_REENTER_JS);
        if !properties.call {
            // Only a call or a JavaScript-running node reenters, and only a
            // call or a collecting node allocates.
            assert!(!reenters || properties.runs_js, "{kind:?} reenters JS");
            assert!(
                !any(E::MAY_ALLOCATE | E::MAY_TRIGGER_GC) || properties.may_collect,
                "{kind:?} allocates"
            );
            assert!(
                !properties.runs_js || properties.may_collect,
                "{kind:?} runs JS without collecting"
            );
        }
        assert!(
            !properties.runs_js || properties.call || reenters,
            "{kind:?} runs JS through no reentrant stub"
        );
    }
}
