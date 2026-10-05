//! Executable depth admission through both current compiled call protocols.
//!
//! # Contents
//! - Known-cell and generic compiled entry over Template and Graph mappings.
//! - Exact-limit acceptance, prepublication refusal and 32-bit depth carry.
//! - Typed body observations after the real native entry publishes its frame.
//!
//! # Invariants
//! - Real mappings, cells, directory and initialized windows outlive each call.
//! - A refused entry changes only the error slot through one overflow stub.
//! - Observation callbacks never allocate JS values, collect or reenter.
//! - The private x86 fixture explicitly uses System V on every host; typed
//!   observation callbacks use the platform C ABI.
//!
//! # See also
//! - `crate::arm64::frame` and `crate::x86_64::frame` own entry admission.
//! - `actual_arguments_tests` proves missing-formal and poisoned-slack values.

use dynasmrt::{AssemblyOffset, DynasmApi, dynasm};
use otter_bytecode::{Op, Operand};
use otter_vm::{
    JitCompileSnapshot, JitFunctionCode, Value, VmError,
    jit::JitTestInstruction,
    native_abi::{
        self as abi, CodeEntryCell, FunctionEntryCell, JitCtx, NativeResultPair, VmThread,
    },
};

use super::TransitionTable;
use crate::artifact::relocation::RelocationCapture;

const BODY_VALUE: Value = Value::number_i32(731);
const REGISTER_COUNT: u16 = 5;

#[derive(Default, Debug)]
struct Observation {
    overflow_calls: usize,
    overflow_frame: usize,
    preparation_calls: usize,
    promotion_calls: usize,
    body_calls: usize,
    body_frame: usize,
    body_caller: u64,
    body_return_pc: u64,
    body_pending_caller: u64,
    body_pending_return_pc: u64,
    body_depth: u32,
    body_registers: Vec<Value>,
    body_actuals: Vec<Value>,
}

unsafe fn observation(ctx: *mut JitCtx) -> *mut Observation {
    // SAFETY: the caller retains both the context and its observation record.
    unsafe { (*(*ctx).thread).runtime_context as *mut Observation }
}

unsafe extern "C" fn overflow(ctx: *mut JitCtx) -> NativeResultPair {
    // Read the publication before the production error entry changes its slot.
    unsafe {
        let observation = &mut *observation(ctx);
        observation.overflow_calls += 1;
        observation.overflow_frame = (*ctx).native_frame as usize;
        abi::call_stack_overflow(ctx)
    }
}

unsafe extern "C" fn prepare(ctx: *mut JitCtx) -> NativeResultPair {
    unsafe { (*observation(ctx)).preparation_calls += 1 };
    NativeResultPair::fatal_internal()
}

unsafe extern "C" fn promote(ctx: *mut JitCtx) -> NativeResultPair {
    unsafe { (*observation(ctx)).promotion_calls += 1 };
    NativeResultPair::success(Value::UNDEFINED)
}

extern "C" fn body(ctx: *mut JitCtx, _opcode: u64, dst: u64, _arg1: u64, _arg2: u64) -> u64 {
    // This typed construction-op test entry observes the published frame and
    // supplies an immediate in its destination. It performs no rest allocation;
    // the runtime rest regressions exercise that operation's JS semantics.
    unsafe {
        let observation = &mut *observation(ctx);
        let frame = &mut *(*ctx).native_frame;
        observation.body_calls += 1;
        observation.body_frame = std::ptr::from_ref(frame) as usize;
        observation.body_caller = frame.caller;
        observation.body_return_pc = frame.caller_return_pc;
        observation.body_pending_caller = (*ctx).pending_call.caller;
        observation.body_pending_return_pc = (*ctx).pending_call.caller_return_pc;
        observation.body_depth = frame.depth;
        observation.body_registers = frame.registers.iter().copied().collect();
        observation.body_actuals =
            std::slice::from_raw_parts(frame.actuals, frame.argument_count as usize).to_vec();
        let Some(slot) = usize::try_from(dst)
            .ok()
            .and_then(|dst| frame.registers.get_mut(dst))
        else {
            return abi::NativeResultStatus::Fatal as u64;
        };
        *slot = BODY_VALUE;
    }
    abi::NativeResultStatus::Success as u64
}

fn transitions() -> TransitionTable {
    let mut table = TransitionTable::resolve();
    table.replace_entry_for_test(abi::STUB_JIT_CALL_OVERFLOW, overflow as *const () as usize);
    table.replace_entry_for_test(
        abi::STUB_JIT_PREPARE_ACTIVATION,
        prepare as *const () as usize,
    );
    table.replace_entry_for_test(abi::STUB_JIT_PROMOTE_ENTERED, promote as *const () as usize);
    table.replace_entry_for_test(abi::STUB_JIT_CONSTRUCT_OP, body as *const () as usize);
    table
}

#[derive(Clone, Copy, Debug)]
enum Semantics {
    Strict,
    Sloppy,
    Construct,
}

fn snapshot(semantics: Semantics) -> JitCompileSnapshot {
    let mut snapshot = JitCompileSnapshot::without_feedback(
        1,
        3,
        REGISTER_COUNT,
        vec![
            JitTestInstruction::new(Op::CollectRest, 0, 0, vec![Operand::Register(3)]),
            JitTestInstruction::new(Op::ReturnValue, 1, 4, vec![Operand::Register(3)]),
        ],
    );
    let function = std::sync::Arc::get_mut(&mut snapshot.code_block).unwrap();
    function.is_strict = !matches!(semantics, Semantics::Sloppy);
    // The feedback-free fixture retains an observable receiver. Only strict
    // mode changes here; the typed body probe needs no rest-parameter metadata.
    assert!(function.observes_this());
    snapshot
}

fn compile(view: &JitCompileSnapshot, table: &TransitionTable) -> Vec<Box<dyn JitFunctionCode>> {
    vec![
        Box::new(crate::template::compile(view, 11, table).unwrap()),
        Box::new(
            crate::graph::compile_optimized(view, 12, table, None, None, false)
                .unwrap()
                .code,
        ),
    ]
}

#[cfg(target_arch = "aarch64")]
fn caller(
    table: &TransitionTable,
    cell: &FunctionEntryCell,
    actuals: &[Value],
    generic: bool,
    semantics: Semantics,
) -> (dynasmrt::ExecutableBuffer, AssemblyOffset) {
    use crate::arm64::js_call::{self, CallTarget};
    use crate::template::arm64::values::emit_load_u64;
    let mut ops = dynasmrt::aarch64::Assembler::new().unwrap();
    let mut relocations = RelocationCapture::default();
    dynasm!(ops ; .arch aarch64
        ; stp x29, x30, [sp, #-32]! ; stp x19, x20, [sp, #16] ; mov x20, x0
    );
    let bytes = js_call::emit_push_arguments(&mut ops, actuals.len(), |ops, i, scratch, _| {
        emit_load_u64(ops, scratch, actuals[i].to_bits());
        Ok(scratch)
    })
    .unwrap();
    if actuals.len() % 2 == 1 {
        emit_load_u64(&mut ops, 9, Value::number_i32(999).to_bits());
        dynasm!(ops ; .arch aarch64 ; str x9, [sp, (actuals.len() * 8) as u32]);
    }
    emit_load_u64(&mut ops, 1, Value::function(1).to_bits());
    emit_load_u64(&mut ops, 2, Value::number_i32(3).to_bits());
    let construct = matches!(semantics, Semantics::Construct);
    if construct {
        emit_load_u64(&mut ops, 2, Value::UNDEFINED.to_bits());
        emit_load_u64(&mut ops, 3, Value::function(1).to_bits());
    }
    let return_pc = js_call::emit_call(
        &mut ops,
        &mut relocations,
        table,
        20,
        1,
        Some(2),
        construct.then_some(3),
        Some(actuals.len() as u32),
        if generic {
            CallTarget::Generic
        } else {
            CallTarget::Known {
                entry_cell: std::ptr::from_ref(cell) as u64,
                function_id: 1,
            }
        },
    );
    js_call::emit_pop_arguments(&mut ops, bytes);
    dynasm!(ops ; .arch aarch64
        ; ldp x19, x20, [sp, #16] ; ldp x29, x30, [sp], #32 ; ret
    );
    (ops.finalize().unwrap(), return_pc)
}

#[cfg(target_arch = "x86_64")]
fn caller(
    table: &TransitionTable,
    cell: &FunctionEntryCell,
    actuals: &[Value],
    generic: bool,
    semantics: Semantics,
) -> (dynasmrt::ExecutableBuffer, AssemblyOffset) {
    use crate::x86_64::js_call::{self, CallTarget};
    let mut ops = dynasmrt::x64::Assembler::new().unwrap();
    let mut relocations = RelocationCapture::default();
    dynasm!(ops ; .arch x64 ; push r15 ; mov r15, rdi);
    let bytes = js_call::emit_push_arguments(&mut ops, actuals.len(), 11, |ops, i, scratch, _| {
        dynasm!(ops ; .arch x64 ; mov Rq(scratch), QWORD actuals[i].to_bits() as i64);
        Ok(())
    })
    .unwrap();
    if actuals.len() % 2 == 1 {
        dynasm!(ops ; .arch x64
            ; mov r11, QWORD Value::number_i32(999).to_bits() as i64
            ; mov [rsp + (actuals.len() * 8) as i32], r11
        );
    }
    dynasm!(ops ; .arch x64
        ; mov rsi, QWORD Value::function(1).to_bits() as i64
        ; mov rdx, QWORD Value::number_i32(3).to_bits() as i64
    );
    let construct = matches!(semantics, Semantics::Construct);
    if construct {
        dynasm!(ops ; .arch x64
            ; mov edx, Value::UNDEFINED.to_bits() as i32
            ; mov rcx, QWORD Value::function(1).to_bits() as i64
        );
    }
    let return_pc = js_call::emit_call(
        &mut ops,
        &mut relocations,
        table,
        15,
        true,
        construct,
        actuals.len() as u32,
        if generic {
            CallTarget::Generic
        } else {
            CallTarget::Known {
                entry_cell: std::ptr::from_ref(cell) as u64,
                function_id: 1,
            }
        },
    );
    js_call::emit_pop_arguments(&mut ops, bytes);
    dynasm!(ops ; .arch x64 ; pop r15 ; ret);
    (ops.finalize().unwrap(), return_pc)
}

#[derive(Clone, Copy, Debug)]
struct Case {
    caller_depth: Option<u32>,
    limit: u64,
    accepted: bool,
    no_native_room: bool,
    native_caller: bool,
}

fn run_case(
    view: &JitCompileSnapshot,
    code: &dyn JitFunctionCode,
    table: &TransitionTable,
    semantics: Semantics,
    generic: bool,
    count: usize,
    case: Case,
) {
    let optimizing = code.native_frame_kind() == abi::NativeFrameKind::Optimizing;
    let flags = if optimizing {
        abi::CODE_ENTRY_OPTIMIZING_TIER
    } else {
        0
    } | if code.safepoint_count() != 0 {
        abi::CODE_ENTRY_HAS_SAFEPOINTS
    } else {
        0
    };
    let generation = CodeEntryCell::new(
        code.call_entry_addr().unwrap(),
        code.metadata().id,
        1,
        REGISTER_COUNT,
        flags,
        // Refusal precedes promotion eligibility and every entry counter.
        Some(if case.accepted { u64::MAX } else { 0 }),
    );
    generation.generated_entries.set(41);
    generation.generated_deopts.set(7);
    let function = FunctionEntryCell::new(1, 3, REGISTER_COUNT, view.code_block.call_flags(), 0);
    let generation_address = std::ptr::from_ref(&generation) as u64;
    function.publish(generation_address);
    let directory = [0, std::ptr::from_ref(function.as_ref()) as u64];
    let registry = abi::CodeRegistryView {
        context: 0,
        resolve_safepoint: 0,
        function_entries: directory.as_ptr() as u64,
        function_entry_count: directory.len() as u64,
        resolve_return_pc: 0,
    };
    let actuals: Vec<_> = (0..count)
        .map(|i| Value::number_i32(101 + i as i32))
        .collect();
    let (buffer, return_pc) = caller(table, &function, &actuals, generic, semantics);
    let mut observation = Observation::default();
    let mut thread = VmThread::empty();
    let interrupt = 0_u8;
    let mut fuel = u64::MAX;
    thread.code_registry = std::ptr::from_ref(&registry) as u64;
    thread.runtime_context = std::ptr::from_mut(&mut observation) as u64;
    thread.interrupt_cell = std::ptr::from_ref(&interrupt) as u64;
    thread.backedge_fuel_cell = std::ptr::from_mut(&mut fuel) as u64;
    let mut parent_registers = [Value::number_i32(47), Value::UNDEFINED, Value::NULL];
    let mut parent_actuals = [Value::number_i32(43)];
    let mut parent = abi::Frame::new(
        abi::VmFrameHeader::interpreter(9, parent_registers.len() as u16),
        parent_registers.as_mut_ptr() as u64,
        Value::function(9),
        Value::number_i32(17),
    );
    parent.header.pc = 37;
    if case.native_caller {
        assert!(case.caller_depth.is_some());
        parent.header.kind = abi::NativeFrameKind::Baseline;
        // This genuine fixture mapping is never submitted to VM GC or source
        // resolution; the body observes the exact hardware entry contract.
        parent.code_object_id = 73;
    }
    parent.depth = case.caller_depth.unwrap_or(0);
    parent.set_incoming_arguments(parent_actuals.as_mut_ptr(), parent_actuals.len() as u32);
    parent.set_return_register(Some(2));
    let frame = if case.caller_depth.is_some() {
        std::ptr::from_mut(&mut parent)
    } else {
        std::ptr::null_mut()
    };
    let parent_before = format!("{parent:?}");
    let registers_before = parent_registers;
    let actuals_before = parent_actuals;
    let mut error = None;
    let mut ctx = JitCtx {
        thread: &mut thread,
        native_frame: frame,
        error: &mut error,
        generated_depth_limit: case.limit,
        global_this_offset: std::ptr::null(),
        native_stack_limit: if case.no_native_room { usize::MAX } else { 0 },
        generated_feedback_clean: 1,
        alloc_window: otter_vm::jit::JitMachineAllocationWindow::disabled(),
        runtime_stats: std::ptr::null_mut(),
        pending_call: abi::CallRequest::EMPTY,
        completion: NativeResultPair::success(Value::number_i32(73)),
        completion_destination: 17,
        completion_generation: 31,
    };
    let frame_cell = std::ptr::from_mut(&mut ctx.native_frame) as u64;
    thread.frame_cell = frame_cell;
    assert_eq!(thread.frame_cell, frame_cell);
    let request_before = format!("{:?}", ctx.pending_call);
    let work_before = view.code_block.source_work().total();
    #[cfg(target_arch = "aarch64")]
    let run: unsafe extern "C" fn(*mut JitCtx) -> NativeResultPair =
        unsafe { std::mem::transmute(buffer.ptr(AssemblyOffset(0))) };
    #[cfg(target_arch = "x86_64")]
    let run: unsafe extern "sysv64" fn(*mut JitCtx) -> NativeResultPair =
        unsafe { std::mem::transmute(buffer.ptr(AssemblyOffset(0))) };
    // SAFETY: all mappings/cells/windows are owned above. Callbacks touch only
    // these initialized carriers and never enter the VM, collect or reenter.
    let result = unsafe { run(&mut ctx) };
    let label = format!(
        "{:?} {semantics:?} generic={generic} argc={count} {case:?}",
        code.native_frame_kind()
    );
    assert_eq!(
        ctx.native_frame, frame,
        "{label}: restore caller publication"
    );
    assert_eq!(
        thread.frame_cell, frame_cell,
        "{label}: retain frame publisher"
    );
    assert_eq!(
        format!("{parent:?}"),
        parent_before,
        "{label}: caller record unchanged"
    );
    assert_eq!(
        parent_registers, registers_before,
        "{label}: caller window unchanged"
    );
    assert_eq!(
        parent_actuals, actuals_before,
        "{label}: caller actuals unchanged"
    );
    assert_eq!(
        format!("{:?}", ctx.pending_call),
        request_before,
        "{label}: no trampoline request"
    );
    assert_eq!(
        ctx.completion,
        NativeResultPair::success(Value::number_i32(73)),
        "{label}"
    );
    assert_eq!(
        (ctx.completion_destination, ctx.completion_generation),
        (17, 31),
        "{label}"
    );
    assert_eq!(function.current_generation(), generation_address, "{label}");
    assert_eq!(generation.generated_deopts.get(), 7, "{label}");
    assert_eq!(
        observation.preparation_calls, 0,
        "{label}: no receiver or constructor helper"
    );
    assert_eq!(
        observation.promotion_calls, 0,
        "{label}: no promotion helper"
    );
    if case.accepted {
        assert_eq!(result, NativeResultPair::success(BODY_VALUE), "{label}");
        assert!(error.is_none(), "{label}");
        assert_eq!(observation.overflow_calls, 0, "{label}");
        assert_eq!(observation.body_calls, 1, "{label}: body reached once");
        assert_ne!(
            observation.body_frame, frame as usize,
            "{label}: own native frame"
        );
        assert_eq!(observation.body_caller, frame as u64, "{label}");
        assert_eq!(
            observation.body_return_pc,
            if case.native_caller {
                buffer.ptr(return_pc) as u64
            } else {
                0
            },
            "{label}: genuine machine CALL/BLR return, no interpreter anchor"
        );
        assert_eq!(
            (
                observation.body_pending_caller,
                observation.body_pending_return_pc
            ),
            (0, 0),
            "{label}: published child owns the anchor exclusively"
        );
        assert_eq!(
            observation.body_depth,
            case.caller_depth.map_or(1, |depth| depth + 1),
            "{label}"
        );
        assert_eq!(observation.body_actuals, actuals, "{label}");
        let mut expected = actuals.iter().take(3).copied().collect::<Vec<_>>();
        expected.resize(usize::from(REGISTER_COUNT), Value::UNDEFINED);
        assert_eq!(
            observation.body_registers, expected,
            "{label}: published initialized window"
        );
        assert_eq!(
            generation.generated_entries.get(),
            if optimizing { 41 } else { 42 },
            "{label}"
        );
    } else {
        assert_eq!(result, NativeResultPair::fatal_internal(), "{label}");
        assert_eq!(
            error,
            Some(VmError::StackOverflow {
                limit: case.limit.min(u64::from(u32::MAX)) as u32
            }),
            "{label}"
        );
        assert_eq!(
            observation.overflow_calls, 1,
            "{label}: production overflow once"
        );
        assert_eq!(
            observation.overflow_frame, frame as usize,
            "{label}: refused frame never published"
        );
        assert_eq!(observation.body_calls, 0, "{label}: body never entered");
        assert_eq!(
            generation.generated_entries.get(),
            41,
            "{label}: no entry accounting"
        );
        assert_eq!(
            ctx.generated_feedback_clean, 1,
            "{label}: no feedback mutation"
        );
        assert_eq!(
            view.code_block.source_work().total(),
            work_before,
            "{label}: no source attempt"
        );
    }
}

#[test]
fn known_and_generic_depth_refusal_precedes_publication_preparation_and_accounting() {
    let table = transitions();
    for semantics in [Semantics::Strict, Semantics::Sloppy, Semantics::Construct] {
        let view = snapshot(semantics);
        for code in compile(&view, &table) {
            for generic in [false, true] {
                for count in [0, 1, 3, 5] {
                    for (caller_depth, limit) in [
                        (None, 0),
                        (Some(0), 0),
                        (Some(7), 7),
                        (Some(u32::MAX), u64::from(u32::MAX)),
                        (Some(u32::MAX), u64::MAX),
                    ] {
                        run_case(
                            &view,
                            code.as_ref(),
                            &table,
                            semantics,
                            generic,
                            count,
                            Case {
                                caller_depth,
                                limit,
                                accepted: false,
                                no_native_room: false,
                                native_caller: false,
                            },
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn exact_depth_limit_acceptance_publishes_checked_child_depth_and_real_windows() {
    let table = transitions();
    let view = snapshot(Semantics::Strict);
    for code in compile(&view, &table) {
        for generic in [false, true] {
            for count in [0, 1, 3, 5] {
                for (caller_depth, limit) in [
                    (None, 1),
                    (None, u64::MAX),
                    (Some(0), 1),
                    (Some(7), 8),
                    (Some(u32::MAX - 1), u64::from(u32::MAX)),
                ] {
                    run_case(
                        &view,
                        code.as_ref(),
                        &table,
                        Semantics::Strict,
                        generic,
                        count,
                        Case {
                            caller_depth,
                            limit,
                            accepted: true,
                            no_native_room: false,
                            native_caller: false,
                        },
                    );
                }
            }
        }
    }
}

#[test]
fn native_stack_refusal_keeps_the_same_unpublished_overflow_return() {
    let table = transitions();
    let view = snapshot(Semantics::Strict);
    for code in compile(&view, &table) {
        for generic in [false, true] {
            run_case(
                &view,
                code.as_ref(),
                &table,
                Semantics::Strict,
                generic,
                1,
                Case {
                    caller_depth: Some(7),
                    limit: 8,
                    accepted: false,
                    no_native_room: true,
                    native_caller: false,
                },
            );
        }
    }
}

#[test]
fn published_known_and_generic_children_own_the_exact_emitted_hardware_return() {
    let table = transitions();
    let view = snapshot(Semantics::Strict);
    for code in compile(&view, &table) {
        for generic in [false, true] {
            for count in [0, 1, 3, 5] {
                run_case(
                    &view,
                    code.as_ref(),
                    &table,
                    Semantics::Strict,
                    generic,
                    count,
                    Case {
                        caller_depth: Some(7),
                        limit: 8,
                        accepted: true,
                        no_native_room: false,
                        native_caller: true,
                    },
                );
            }
        }
    }
}

#[test]
fn tier_entries_keep_the_existing_suspended_anchor_without_creating_a_js_return() {
    let table = transitions();
    let view = snapshot(Semantics::Strict);
    for code in compile(&view, &table) {
        let mut observation = Observation::default();
        let mut thread = VmThread::empty();
        thread.runtime_context = std::ptr::from_mut(&mut observation) as u64;
        let mut registers = [Value::UNDEFINED; REGISTER_COUNT as usize];
        let mut actuals = [
            Value::number_i32(41),
            Value::number_i32(42),
            Value::number_i32(43),
        ];
        let mut parent = abi::Frame::new(
            abi::VmFrameHeader::interpreter(9, 0),
            0,
            Value::function(9),
            Value::UNDEFINED,
        );
        let mut frame = abi::Frame::new(
            abi::VmFrameHeader::interpreter(1, REGISTER_COUNT),
            registers.as_mut_ptr() as u64,
            Value::function(1),
            Value::UNDEFINED,
        );
        frame.set_incoming_arguments(actuals.as_mut_ptr(), actuals.len() as u32);
        frame.caller = std::ptr::from_mut(&mut parent) as u64;
        // This scalar sentinel is never resolved as executable metadata: the
        // emitted tier entry must preserve an already-owned caller anchor.
        frame.caller_return_pc = 0x3141_5926_5358_9793;
        let before = (frame.caller, frame.caller_return_pc);
        let frame_pointer = std::ptr::from_mut(&mut frame);
        let mut error = None;
        let mut ctx = JitCtx {
            thread: &mut thread,
            native_frame: frame_pointer,
            error: &mut error,
            generated_depth_limit: 8,
            global_this_offset: std::ptr::null(),
            native_stack_limit: 0,
            generated_feedback_clean: 1,
            alloc_window: otter_vm::jit::JitMachineAllocationWindow::disabled(),
            runtime_stats: std::ptr::null_mut(),
            pending_call: abi::CallRequest::EMPTY,
            completion: NativeResultPair::success(Value::UNDEFINED),
            completion_destination: u32::MAX,
            completion_generation: 0,
        };
        thread.frame_cell = std::ptr::from_mut(&mut ctx.native_frame) as u64;
        let entry: unsafe extern "C" fn(*mut JitCtx) -> NativeResultPair =
            unsafe { std::mem::transmute(code.entry_addr().unwrap()) };
        // SAFETY: genuine platform C tier mappings/records outlive this call;
        // the typed body observer does not collect, resolve a source or reenter.
        let result = unsafe { entry(&mut ctx) };
        assert_eq!(result, NativeResultPair::success(BODY_VALUE));
        assert_eq!(ctx.native_frame, frame_pointer);
        assert_eq!((frame.caller, frame.caller_return_pc), before);
        assert_eq!(observation.body_calls, 1);
        assert_eq!(
            (observation.body_caller, observation.body_return_pc),
            before
        );
        assert_eq!(
            (
                observation.body_pending_caller,
                observation.body_pending_return_pc
            ),
            (0, 0)
        );
        assert!(error.is_none());
    }
}
