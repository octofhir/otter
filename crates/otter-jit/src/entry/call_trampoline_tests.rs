//! Shared-trampoline execution against both compiled tiers.
//!
//! # Contents
//! A resumable Rust entry calls real Template and Machine bodies in sequence.
//!
//! # Invariants
//! Compiled mappings outlive the call chain. Every entry consumes the same
//! VM-owned context, frame and argument ABI on the selected architecture.
//!
//! # See also
//! - `otter_vm::native_abi::call_trampoline`.

use otter_bytecode::{Op, Operand};
use otter_vm::{
    JitCompileSnapshot, JitFunctionCode, Value,
    jit::JitTestInstruction,
    native_abi::{
        CallRequest, JitCtx, NativeFrameKind, NativeResultPair, VmFrameHeader, VmThread,
        call_trampoline,
    },
};

struct Entries {
    template: u64,
    machine: u64,
    parent: usize,
    resumes: usize,
    in_place: bool,
    register_base: u64,
}

extern "C" fn parent_entry(ctx: *mut JitCtx) -> NativeResultPair {
    // SAFETY: the fixture and mappings remain live across all continuations.
    let ctx = unsafe { &mut *ctx };
    let entries = unsafe { &mut *((*ctx.thread).runtime_context as *mut Entries) };
    let frame = unsafe { &mut *ctx.native_frame };
    if entries.in_place {
        return transfer_entry(ctx, entries);
    }
    if entries.parent == 0 {
        entries.parent = ctx.native_frame.addr();
    }
    assert_eq!(entries.parent, ctx.native_frame.addr());
    entries.resumes += 1;
    if frame.header.pc != 0 {
        assert_eq!(
            ctx.completion,
            NativeResultPair::success(Value::number_i32(123))
        );
    }
    let (entry, kind, generation) = match frame.header.pc {
        0 => (entries.template, NativeFrameKind::Baseline, 1),
        1 => (entries.machine, NativeFrameKind::Optimizing, 2),
        _ => return ctx.completion,
    };
    frame.header.pc += 1;
    ctx.pending_call = CallRequest {
        entry,
        header: VmFrameHeader {
            function_id: 1,
            pc: 0,
            register_count: 1,
            kind,
            flags: Default::default(),
        },
        code_object_id: generation,
        arguments: frame.registers.as_mut_ptr(),
        argument_count: 1,
        parameter_count: 1,
        callee: Value::function(1),
        receiver: Value::UNDEFINED,
        new_target: Value::UNDEFINED,
        ..CallRequest::EMPTY
    };
    NativeResultPair::continue_execution()
}

fn transfer_entry(ctx: &mut JitCtx, entries: &mut Entries) -> NativeResultPair {
    let frame = unsafe { &mut *ctx.native_frame };
    if entries.parent == 0 {
        entries.parent = ctx.native_frame.addr();
        entries.register_base = frame.register_base();
    }
    assert_eq!(entries.parent, ctx.native_frame.addr());
    assert_eq!(entries.register_base, frame.register_base());
    assert_eq!(frame.depth, 1);
    if entries.resumes != 0 {
        assert_eq!(
            ctx.completion,
            NativeResultPair::success(Value::number_i32(123))
        );
        assert!(frame.enter_interpreter());
    }
    let (entry, kind, generation) = match entries.resumes {
        0 => (entries.template, NativeFrameKind::Baseline, 1),
        1 => (entries.machine, NativeFrameKind::Optimizing, 2),
        _ => {
            entries.resumes += 1;
            return ctx.completion;
        }
    };
    entries.resumes += 1;
    assert!(frame.enter_compiled(kind));
    frame.header.function_id = 1;
    frame.code_object_id = generation;
    ctx.pending_call = CallRequest::EMPTY;
    ctx.pending_call.entry = entry;
    ctx.pending_call.header.flags = otter_vm::native_abi::NativeFrameFlags::from_bits(
        otter_vm::native_abi::NativeFrameFlags::TIER_ENTRY,
    );
    NativeResultPair::continue_execution()
}

#[test]
fn native_call_trampoline_enters_template_and_machine_on_child_and_current_frames() {
    let snapshot = JitCompileSnapshot::without_feedback(
        1,
        1,
        1,
        vec![JitTestInstruction::new(
            Op::ReturnValue,
            0,
            0,
            vec![Operand::Register(0)],
        )],
    );
    let transitions = super::TransitionTable::resolve();
    let template = crate::template::compile(&snapshot, 1, &transitions).unwrap();
    #[cfg(target_arch = "aarch64")]
    let target = crate::machine::TargetSpec::aarch64();
    #[cfg(target_arch = "x86_64")]
    let target = crate::machine::TargetSpec::x86_64();
    let machine = crate::machine::numeric::try_compile(
        &target,
        &snapshot,
        2,
        &transitions,
        false,
        None,
        None,
    )
    .unwrap();
    for in_place in [false, true] {
        let mut entries = Entries {
            template: template.entry_addr().unwrap() as u64,
            machine: unsafe { machine.code.compiled_code().entry_ptr() } as u64,
            parent: 0,
            resumes: 0,
            in_place,
            register_base: 0,
        };
        let mut thread = VmThread::empty();
        let interrupt = 0_u8;
        let mut fuel = u64::MAX;
        thread.interrupt_cell = std::ptr::from_ref(&interrupt) as u64;
        thread.backedge_fuel_cell = std::ptr::from_mut(&mut fuel) as u64;
        thread.runtime_context = std::ptr::from_mut(&mut entries) as u64;
        let mut error = None;
        let arguments = [Value::number_i32(123)];
        let mut ctx = JitCtx {
            thread: &mut thread,
            native_frame: std::ptr::null_mut(),
            error: &mut error,
            generated_depth_limit: if in_place { 1 } else { 8 },
            global_this_offset: std::ptr::null(),
            native_stack_limit: 0,
            generated_feedback_clean: 1,
            completion_destination: u32::MAX,
            completion_generation: 0,
            alloc_window: otter_vm::jit::JitMachineAllocationWindow::disabled(),
            runtime_stats: std::ptr::null_mut(),
            pending_call: CallRequest {
                entry: parent_entry as *const () as u64,
                header: VmFrameHeader::interpreter(0, 1),
                arguments: arguments.as_ptr(),
                argument_count: 1,
                parameter_count: 1,
                callee: Value::function(0),
                ..CallRequest::EMPTY
            },
            completion: NativeResultPair::success(Value::UNDEFINED),
        };
        unsafe { (*ctx.thread).frame_cell = std::ptr::from_mut(&mut ctx.native_frame) as u64 };
        // SAFETY: all entries, context records and argument sources are live; these
        // non-allocating bodies need no heap or runtime activation.
        let result = unsafe { call_trampoline(&mut ctx) };
        assert_eq!(result, NativeResultPair::success(Value::number_i32(123)));
        assert_eq!(entries.resumes, 3);
        assert!(ctx.native_frame.is_null());
        assert!(error.is_none());
    }
}
