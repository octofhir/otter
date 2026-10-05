//! Executable proofs of the shared x86-64 frame protocol.
//!
//! # Contents
//! - Tier-root restoration on normal and side-exit returns.
//! - Actual-only entry, underarity, called deopt and overflow publication.
//! - Graph RBX preservation on return and physical tail replacement.
//!
//! # Invariants
//! - Owned VM records, actual spans, cells and mappings outlive each call.
//! - Test callbacks copy live stack observations before the frame is retired.
//! - No fixture allocates VM values or invokes the collector; runtime tests
//!   prove moving roots through the same production frame protocol.
//! - The private test caller explicitly uses System V on every host, while
//!   runtime callbacks and external tier entries use the platform C ABI.

use super::*;
use crate::x86_64::js_call::{self, CallTarget};
use otter_bytecode::{Op, Operand};
use otter_vm::{
    Value,
    jit::JitTestInstruction,
    native_abi::{
        CallRequest, CodeEntryCell, FunctionEntryCell, JitCtx, NativeResultPair, VmThread,
    },
};

const CANARY: u64 = 0x3479_1592_6535_8979;
const SAFEPOINT: abi::SafepointId = 37;
const SPILL: SpillArea = SpillArea {
    bytes: 48,
    scratch_slot: Some(1),
    safepoint: SAFEPOINT,
};

#[derive(Default, Debug)]
struct Observation {
    calls: usize,
    frame: usize,
    caller: u64,
    caller_return_pc: u64,
    depth: u32,
    call_site: abi::SafepointId,
    machine_roots: u64,
    register_count: u16,
    register_base: u64,
    actuals: Vec<Value>,
    registers: Vec<Value>,
    roots: Vec<u64>,
    rbx_after: u64,
    incoming_count: u64,
}

unsafe fn observe(ctx: *mut JitCtx) {
    // SAFETY: every executable fixture keeps its context, observation and
    // published record alive until its generated call returns.
    let ctx = unsafe { &*ctx };
    let observation = unsafe { &mut *((*ctx.thread).runtime_context as *mut Observation) };
    let frame = unsafe { &*ctx.native_frame };
    observation.calls += 1;
    observation.frame = ctx.native_frame as usize;
    observation.caller = frame.caller;
    observation.caller_return_pc = frame.caller_return_pc;
    observation.depth = frame.depth;
    observation.call_site = frame.call_site;
    observation.machine_roots = frame.machine_roots;
    observation.register_count = frame.header.register_count;
    observation.register_base = frame.register_base();
    observation.actuals = if frame.argument_count == 0 {
        Vec::new()
    } else {
        // SAFETY: the caller owns this initialized actual span.
        unsafe { std::slice::from_raw_parts(frame.actuals, frame.argument_count as usize) }.to_vec()
    };
    observation.registers = if frame.header.register_count == 0 {
        Vec::new()
    } else {
        // SAFETY: publication follows initialization of the entire window.
        unsafe {
            std::slice::from_raw_parts(
                frame.registers.as_mut_ptr(),
                usize::from(frame.header.register_count),
            )
        }
        .to_vec()
    };
    observation.roots = if frame.machine_roots == 0 {
        Vec::new()
    } else {
        // SAFETY: both canonical homes are live during this callback; the
        // previous tier roots in the owned fixture also contain two words.
        unsafe { std::slice::from_raw_parts(frame.machine_roots as *const u64, 2) }.to_vec()
    };
}

unsafe extern "C" fn inspect(ctx: *mut JitCtx) -> NativeResultPair {
    unsafe { observe(ctx) };
    NativeResultPair::success(Value::number_i32(731))
}

unsafe extern "C" fn inspect_deopt(ctx: *mut JitCtx, _exit: u64) -> NativeResultPair {
    unsafe { inspect(ctx) }
}

unsafe extern "C" fn inspect_overflow(ctx: *mut JitCtx) -> NativeResultPair {
    unsafe { observe(ctx) };
    NativeResultPair::throw_value(Value::number_i32(919))
}

fn transitions() -> TransitionTable {
    let mut transitions = TransitionTable::resolve();
    transitions
        .replace_entry_for_test(abi::STUB_JIT_PROMOTE_ENTERED, inspect as *const () as usize);
    transitions.replace_entry_for_test(
        abi::STUB_JIT_DEOPT_CALL,
        inspect_deopt as *const () as usize,
    );
    transitions.replace_entry_for_test(
        abi::STUB_JIT_CALL_OVERFLOW,
        inspect_overflow as *const () as usize,
    );
    transitions
}

fn snapshot() -> JitCompileSnapshot {
    let mut view = JitCompileSnapshot::without_feedback(
        1,
        3,
        4,
        vec![JitTestInstruction::new(
            Op::ReturnValue,
            0,
            0,
            vec![Operand::Register(2)],
        )],
    );
    std::sync::Arc::get_mut(&mut view.code_block)
        .unwrap()
        .is_strict = true;
    view
}

fn shape(view: &JitCompileSnapshot, lazy: bool) -> EntryShape {
    let mut shape = EntryShape::of(view, 11, abi::NativeFrameKind::Optimizing, true)
        .unwrap()
        .with_lazy_window(lazy);
    shape.constructible = false;
    shape
}

fn exits(ops: &mut Assembler) -> ActivationExits {
    ActivationExits {
        construct: ops.new_dynamic_label(),
        side_exit: ops.new_dynamic_label(),
    }
}

fn context<'a>(
    thread: &'a mut VmThread,
    error: &'a mut Option<otter_vm::VmError>,
    frame: *mut abi::Frame,
) -> JitCtx {
    JitCtx {
        thread,
        native_frame: frame,
        error,
        generated_depth_limit: 8,
        global_this_offset: std::ptr::null(),
        native_stack_limit: 0,
        generated_feedback_clean: 1,
        completion_destination: u32::MAX,
        completion_generation: 0,
        alloc_window: otter_vm::jit::JitMachineAllocationWindow::disabled(),
        runtime_stats: std::ptr::null_mut(),
        pending_call: CallRequest::EMPTY,
        completion: NativeResultPair::success(Value::UNDEFINED),
    }
}

fn owned_frame(registers: &mut [Value]) -> abi::Frame {
    let mut frame = abi::Frame::new(
        abi::VmFrameHeader::interpreter(9, registers.len() as u16),
        registers.as_mut_ptr() as u64,
        Value::function(9),
        Value::UNDEFINED,
    );
    frame.depth = 7;
    frame
}

fn emit_inspect(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    transitions: &TransitionTable,
) {
    dynasm!(ops ; .arch x64 ; mov rdi, r15);
    emit_stub(ops, relocations, transitions, abi::STUB_JIT_PROMOTE_ENTERED);
}

/// The caller owns its original RBX and records the canary after the JIT call.
fn caller(
    transitions: &TransitionTable,
    cell: &FunctionEntryCell,
    actuals: &[Value],
) -> dynasmrt::ExecutableBuffer {
    let mut ops = Assembler::new().unwrap();
    let mut relocations = RelocationCapture::default();
    dynasm!(ops ; .arch x64
        ; push rbx
        ; push r15
        ; sub rsp, 8
        ; mov r15, rdi
        ; mov rbx, QWORD CANARY as i64
    );
    let bytes = js_call::emit_push_arguments(&mut ops, actuals.len(), 11, |ops, i, scratch, _| {
        dynasm!(ops ; .arch x64 ; mov Rq(scratch), QWORD NativeResultPair::success(actuals[i]).payload_bits() as i64);
        Ok(())
    }).unwrap();
    if actuals.len() % 2 == 1 {
        dynasm!(ops ; .arch x64
            ; mov r11, QWORD NativeResultPair::success(Value::number_i32(999)).payload_bits() as i64
            ; mov [rsp + (actuals.len() * 8) as i32], r11
        );
    }
    dynasm!(ops ; .arch x64 ; mov rsi, QWORD NativeResultPair::success(Value::function(1)).payload_bits() as i64);
    js_call::emit_call(
        &mut ops,
        &mut relocations,
        transitions,
        15,
        false,
        false,
        actuals.len() as u32,
        CallTarget::Known {
            entry_cell: std::ptr::from_ref(cell) as u64,
            function_id: 1,
        },
    );
    js_call::emit_pop_arguments(&mut ops, bytes);
    dynasm!(ops ; .arch x64
        ; mov r11, [r15 + THREAD_OFFSET as i32]
        ; mov r11, [r11 + std::mem::offset_of!(VmThread, runtime_context) as i32]
        ; mov [r11 + std::mem::offset_of!(Observation, rbx_after) as i32], rbx
        ; add rsp, 8
        ; pop r15
        ; pop rbx
        ; ret
    );
    ops.finalize().unwrap()
}

#[test]
fn baseline_save_geometry_has_no_graph_rbx_instructions() {
    let bytes = |kind| {
        let mut ops = Assembler::new().unwrap();
        emit_save(&mut ops, kind);
        emit_restore(&mut ops, kind);
        ops.finalize().unwrap()
    };
    let baseline = bytes(abi::NativeFrameKind::Baseline);
    let graph = bytes(abi::NativeFrameKind::Optimizing);
    assert_eq!(
        baseline.len(),
        30,
        "baseline's fixed save/restore stays byte-identical"
    );
    assert_eq!(graph.len(), baseline.len() + 8);
}

#[test]
fn tier_returns_restore_both_interpreter_root_words() {
    let view = snapshot();
    let shape = shape(&view, false);
    let transitions = transitions();
    let exit = abi::SideExit::new(
        13,
        abi::ExitReason::RuntimeTransition,
        abi::ExitAction::Resume,
    );
    for side_exit in [false, true] {
        let mut ops = Assembler::new().unwrap();
        let mut relocations = RelocationCapture::default();
        let exits = exits(&mut ops);
        emit_tier_prologue(&mut ops, shape.kind, SPILL);
        emit_inspect(&mut ops, &mut relocations, &transitions);
        dynasm!(ops ; .arch x64 ; mov rbx, QWORD 0x1155);
        if side_exit {
            dynasm!(ops ; .arch x64 ; mov rax, QWORD exit.to_bits() as i64 ; jmp =>exits.side_exit);
        } else {
            emit_epilogue(&mut ops, exits, shape.kind, SPILL);
        }
        emit_exits(
            &mut ops,
            &mut relocations,
            &transitions,
            &view,
            shape,
            exits,
            SPILL,
        );
        let mapping = ops.finalize().unwrap();
        let mut registers = [Value::number_i32(17); 4];
        let mut frame = owned_frame(&mut registers);
        let previous_roots = [0x3322_u64, 0x7744];
        frame.caller_return_pc = CANARY;
        frame.call_site = 93;
        frame.machine_roots = previous_roots.as_ptr() as u64;
        let previous_words = (frame.depth, frame.call_site, frame.machine_roots);
        let frame_pointer = std::ptr::from_mut(&mut frame);
        let mut observation = Observation::default();
        let mut thread = VmThread::empty();
        thread.runtime_context = std::ptr::from_mut(&mut observation) as u64;
        let mut error = None;
        let mut ctx = context(&mut thread, &mut error, frame_pointer);
        thread.frame_cell = std::ptr::from_mut(&mut ctx.native_frame) as u64;
        assert_eq!(
            thread.frame_cell,
            std::ptr::addr_of_mut!(ctx.native_frame) as u64
        );
        // SAFETY: this is the production platform C tier entry; every record
        // and canonical home remains live throughout the non-allocating call.
        let entry: unsafe extern "C" fn(*mut JitCtx) -> NativeResultPair =
            unsafe { std::mem::transmute(mapping.ptr(AssemblyOffset(0))) };
        let result = unsafe { entry(&mut ctx) };
        assert_eq!(
            result,
            if side_exit {
                NativeResultPair::side_exit(exit)
            } else {
                NativeResultPair::success(Value::number_i32(731))
            }
        );
        assert_eq!(ctx.native_frame, frame_pointer);
        assert_eq!(
            frame.caller_return_pc, CANARY,
            "tier return/SideExit keeps the existing suspended anchor"
        );
        assert_eq!(
            observation.caller_return_pc, CANARY,
            "body sees the same canonical anchor"
        );
        assert_eq!(
            (frame.depth, frame.call_site, frame.machine_roots),
            previous_words
        );
        assert_eq!(observation.calls, 1);
        assert_eq!(observation.frame, frame_pointer as usize);
        assert_eq!(observation.depth, 7);
        assert_eq!(observation.call_site, SAFEPOINT);
        // Only the exception scratch is zeroed at entry; every other home is
        // rooted only by a boundary that wrote it.
        assert_eq!(observation.roots[1], 0);
        assert_eq!(observation.registers, registers);
        assert_ne!(observation.machine_roots, frame.machine_roots);
        assert!(error.is_none());
    }
}

#[test]
fn lazy_called_entries_publish_only_initialized_roots_and_preserve_rbx() {
    let view = snapshot();
    let shape = shape(&view, true);
    let transitions = transitions();
    for deopt in [false, true] {
        let mut ops = Assembler::new().unwrap();
        let mut relocations = RelocationCapture::default();
        let exits = exits(&mut ops);
        let cold = CallEntryCold::new(&mut ops, shape);
        let start = emit_call_entry(&mut ops, &mut relocations, &view, shape, SPILL, cold);
        dynasm!(ops ; .arch x64 ; mov rbx, QWORD 0x2288);
        if deopt {
            let exit = abi::SideExit::new(
                13,
                abi::ExitReason::RuntimeTransition,
                abi::ExitAction::Resume,
            );
            dynasm!(ops ; .arch x64 ; mov rax, QWORD exit.to_bits() as i64 ; jmp =>exits.side_exit);
        } else {
            emit_inspect(&mut ops, &mut relocations, &transitions);
            dynasm!(ops ; .arch x64 ; mov rax, [r13 + 16] ; xor edx, edx);
            emit_epilogue(&mut ops, exits, shape.kind, SPILL);
        }
        emit_exits(
            &mut ops,
            &mut relocations,
            &transitions,
            &view,
            shape,
            exits,
            SPILL,
        );
        emit_call_entry_cold(
            &mut ops,
            &mut relocations,
            &transitions,
            &view,
            shape,
            exits,
            cold,
        );
        let mapping = ops.finalize().unwrap();
        let code = CodeEntryCell::new(
            mapping.ptr(start) as usize,
            11,
            1,
            4,
            abi::CODE_ENTRY_HAS_SAFEPOINTS | abi::CODE_ENTRY_OPTIMIZING_TIER,
            None,
        );
        let function = FunctionEntryCell::new(1, 3, 4, view.code_block.call_flags(), 0);
        function.publish(std::ptr::from_ref(&code) as u64);
        for argc in 0..=4 {
            let actuals: Vec<_> = (0..argc).map(|i| Value::number_i32(100 + i)).collect();
            let caller = caller(&transitions, &function, &actuals);
            let mut registers = [Value::number_i32(17); 4];
            let mut frame = owned_frame(&mut registers);
            let frame_pointer = std::ptr::from_mut(&mut frame);
            let mut observation = Observation::default();
            let mut thread = VmThread::empty();
            thread.runtime_context = std::ptr::from_mut(&mut observation) as u64;
            let mut error = None;
            let mut ctx = context(&mut thread, &mut error, frame_pointer);
            thread.frame_cell = std::ptr::from_mut(&mut ctx.native_frame) as u64;
            let entry: unsafe extern "sysv64" fn(*mut JitCtx) -> NativeResultPair =
                unsafe { std::mem::transmute(caller.ptr(AssemblyOffset(0))) };
            // SAFETY: permanent cells, mappings and every actual span are
            // owned for this call; no VM operation reads absent services.
            let result = unsafe { entry(&mut ctx) };
            let expected = if deopt {
                Value::number_i32(731)
            } else {
                actuals.get(2).copied().unwrap_or(Value::UNDEFINED)
            };
            assert_eq!(
                result,
                NativeResultPair::success(expected),
                "argc={argc} deopt={deopt}"
            );
            assert_eq!(ctx.native_frame, frame_pointer);
            assert_eq!(observation.calls, 1);
            assert_ne!(observation.frame, frame_pointer as usize);
            assert_eq!(observation.caller, frame_pointer as u64);
            assert_eq!(observation.depth, 8);
            assert_eq!(observation.actuals, actuals);
            assert_eq!(observation.rbx_after, CANARY);
            if deopt {
                assert_eq!(observation.call_site, abi::NO_SAFEPOINT);
                assert_eq!(observation.machine_roots, 0);
            } else {
                assert_eq!(observation.call_site, SAFEPOINT);
                // Only the exception scratch is zeroed at entry.
                assert_eq!(observation.roots[1], 0);
                assert_ne!(observation.machine_roots, 0);
            }
            if argc < 3 || deopt {
                let mut initialized = if argc < 3 {
                    actuals.clone()
                } else {
                    Vec::new()
                };
                initialized.resize(4, Value::UNDEFINED);
                assert_eq!(observation.register_count, 4);
                assert_ne!(observation.register_base, 0);
                assert_eq!(observation.registers, initialized);
            } else {
                assert_eq!(observation.register_count, 0);
                assert_eq!(observation.register_base, 0);
                assert!(observation.registers.is_empty());
            }
            assert_eq!(registers, [Value::number_i32(17); 4]);
            assert!(error.is_none());
        }
    }
}

#[test]
fn overflow_never_publishes_reserved_record_or_spill_roots() {
    let view = snapshot();
    let shape = shape(&view, true);
    let transitions = transitions();
    let mut ops = Assembler::new().unwrap();
    let mut relocations = RelocationCapture::default();
    let exits = exits(&mut ops);
    let cold = CallEntryCold::new(&mut ops, shape);
    let start = emit_call_entry(&mut ops, &mut relocations, &view, shape, SPILL, cold);
    dynasm!(ops ; .arch x64 ; mov eax, VALUE_UNDEFINED as i32 ; xor edx, edx);
    emit_epilogue(&mut ops, exits, shape.kind, SPILL);
    emit_exits(
        &mut ops,
        &mut relocations,
        &transitions,
        &view,
        shape,
        exits,
        SPILL,
    );
    emit_call_entry_cold(
        &mut ops,
        &mut relocations,
        &transitions,
        &view,
        shape,
        exits,
        cold,
    );
    let mapping = ops.finalize().unwrap();
    let code = CodeEntryCell::new(mapping.ptr(start) as usize, 11, 1, 4, 0, None);
    let function = FunctionEntryCell::new(1, 3, 4, view.code_block.call_flags(), 0);
    function.publish(std::ptr::from_ref(&code) as u64);
    let caller = caller(&transitions, &function, &[]);
    let mut registers = [Value::number_i32(17); 4];
    let mut frame = owned_frame(&mut registers);
    let frame_pointer = std::ptr::from_mut(&mut frame);
    let mut observation = Observation::default();
    let mut thread = VmThread::empty();
    thread.runtime_context = std::ptr::from_mut(&mut observation) as u64;
    let mut error = None;
    let mut ctx = context(&mut thread, &mut error, frame_pointer);
    ctx.native_stack_limit = usize::MAX;
    thread.frame_cell = std::ptr::from_mut(&mut ctx.native_frame) as u64;
    assert_eq!(
        thread.frame_cell,
        std::ptr::from_mut(&mut ctx.native_frame) as u64
    );
    let entry: unsafe extern "sysv64" fn(*mut JitCtx) -> NativeResultPair =
        unsafe { std::mem::transmute(caller.ptr(AssemblyOffset(0))) };
    let result = unsafe { entry(&mut ctx) };
    assert_eq!(
        thread.frame_cell,
        std::ptr::from_mut(&mut ctx.native_frame) as u64
    );
    assert_eq!(
        result,
        NativeResultPair::throw_value(Value::number_i32(919))
    );
    assert_eq!(ctx.native_frame, frame_pointer);
    assert_eq!(observation.frame, frame_pointer as usize);
    assert_eq!(observation.call_site, abi::NO_SAFEPOINT);
    assert_eq!(observation.machine_roots, 0);
    assert_eq!(observation.registers, registers);
    assert_eq!(observation.rbx_after, CANARY);
    assert!(error.is_none());
}

#[test]
fn physical_tail_replacement_restores_graph_rbx_and_reuses_the_actual_span() {
    let view = snapshot();
    let shape = shape(&view, true);
    let transitions = transitions();
    // This private target checks the caller published by physical replacement
    // and returns its first incoming actual. It needs no JavaScript frame or GC.
    let mut target = Assembler::new().unwrap();
    let mut target_relocations = RelocationCapture::default();
    dynasm!(target ; .arch x64
        ; push rbp
        ; mov rbp, rsp
        ; mov r11, [rdi + THREAD_OFFSET as i32]
        ; mov r11, [r11 + std::mem::offset_of!(VmThread, runtime_context) as i32]
        ; mov [r11 + std::mem::offset_of!(Observation, incoming_count) as i32], r8
    );
    emit_inspect(&mut target, &mut target_relocations, &transitions);
    dynasm!(target ; .arch x64
        ; mov rax, [rbp + 16]
        ; xor edx, edx
        ; pop rbp
        ; ret
    );
    let target = target.finalize().unwrap();
    let target_code = CodeEntryCell::new(target.ptr(AssemblyOffset(0)) as usize, 12, 2, 1, 0, None);
    let target_function = FunctionEntryCell::new(2, 1, 1, view.code_block.call_flags(), 0);
    target_function.publish(std::ptr::from_ref(&target_code) as u64);

    let mut ops = Assembler::new().unwrap();
    let mut relocations = RelocationCapture::default();
    let exits = exits(&mut ops);
    let cold = CallEntryCold::new(&mut ops, shape);
    let ordinary = ops.new_dynamic_label();
    let start = emit_call_entry(&mut ops, &mut relocations, &view, shape, SPILL, cold);
    emit_tail_admission(&mut ops, exits.side_exit, ordinary);
    emit_tail_span_check(&mut ops, 2, exits.side_exit);
    dynasm!(ops ; .arch x64 ; mov rbx, QWORD 0x2299);
    let replacement = Value::number_i32(731);
    let bytes = js_call::emit_push_arguments(&mut ops, 1, 11, |ops, _, scratch, _| {
        dynasm!(ops ; .arch x64 ; mov Rq(scratch), QWORD NativeResultPair::success(replacement).payload_bits() as i64);
        Ok(())
    }).unwrap();
    dynasm!(ops ; .arch x64
        ; mov r11, QWORD NativeResultPair::success(Value::number_i32(999)).payload_bits() as i64
        ; mov [rsp + 8], r11
        ; mov rsi, QWORD NativeResultPair::success(Value::function(2)).payload_bits() as i64
    );
    emit_tail_transfer(
        &mut ops,
        &mut relocations,
        &transitions,
        bytes,
        1,
        CallTarget::Known {
            entry_cell: std::ptr::from_ref(target_function.as_ref()) as u64,
            function_id: 2,
        },
        shape.kind,
    );
    dynasm!(ops ; .arch x64
        ; =>ordinary
        ; mov eax, VALUE_UNDEFINED as i32
        ; xor edx, edx
    );
    emit_epilogue(&mut ops, exits, shape.kind, SPILL);
    emit_exits(
        &mut ops,
        &mut relocations,
        &transitions,
        &view,
        shape,
        exits,
        SPILL,
    );
    emit_call_entry_cold(
        &mut ops,
        &mut relocations,
        &transitions,
        &view,
        shape,
        exits,
        cold,
    );
    let mapping = ops.finalize().unwrap();
    let code = CodeEntryCell::new(
        mapping.ptr(start) as usize,
        11,
        1,
        4,
        abi::CODE_ENTRY_HAS_SAFEPOINTS | abi::CODE_ENTRY_OPTIMIZING_TIER,
        None,
    );
    let function = FunctionEntryCell::new(1, 3, 4, view.code_block.call_flags(), 0);
    function.publish(std::ptr::from_ref(&code) as u64);
    for argc in [3, 4] {
        let actuals: Vec<_> = (0..argc).map(|i| Value::number_i32(100 + i)).collect();
        let caller = caller(&transitions, &function, &actuals);
        let mut registers = [Value::number_i32(17); 4];
        let mut frame = owned_frame(&mut registers);
        let frame_pointer = std::ptr::from_mut(&mut frame);
        let mut observation = Observation::default();
        let interrupt = 0_u8;
        let mut thread = VmThread::empty();
        thread.runtime_context = std::ptr::from_mut(&mut observation) as u64;
        thread.interrupt_cell = std::ptr::from_ref(&interrupt) as u64;
        let mut error = None;
        let mut ctx = context(&mut thread, &mut error, frame_pointer);
        thread.frame_cell = std::ptr::from_mut(&mut ctx.native_frame) as u64;
        let entry: unsafe extern "sysv64" fn(*mut JitCtx) -> NativeResultPair =
            unsafe { std::mem::transmute(caller.ptr(AssemblyOffset(0))) };
        // SAFETY: both permanent targets and all stack storage outlive this
        // non-allocating transfer, including its private leaf target.
        let result = unsafe { entry(&mut ctx) };
        assert_eq!(result, NativeResultPair::success(replacement));
        assert_eq!(ctx.native_frame, frame_pointer);
        assert_eq!(observation.calls, 1);
        assert_eq!(observation.frame, frame_pointer as usize);
        assert_eq!(observation.incoming_count, 1);
        assert_eq!(observation.rbx_after, CANARY);
        assert_eq!(observation.registers, registers);
        assert!(error.is_none());
    }
}
