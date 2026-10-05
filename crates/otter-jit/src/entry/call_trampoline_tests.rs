//! Shared-trampoline execution against every compiled tier available on the
//! target.
//!
//! # Contents
//! A resumable Rust entry calls the Template body, followed by the graph
//! body on a target with optimizing code generation, using child frames and
//! transfers of the current frame.
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
    compiled: Vec<(u64, NativeFrameKind, u32)>,
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
    let Some(&(entry, kind, generation)) = entries.compiled.get(frame.header.pc as usize) else {
        return ctx.completion;
    };
    frame.header.pc += 1;
    let mut request = CallRequest::EMPTY;
    request.entry = entry;
    request.header = VmFrameHeader {
        function_id: 1,
        pc: 0,
        register_count: 1,
        kind,
        flags: Default::default(),
    };
    request.code_object_id = generation;
    request.arguments = frame.registers.as_mut_ptr();
    request.argument_count = 1;
    request.parameter_count = 1;
    request.callee = Value::function(1);
    ctx.pending_call = request;
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
    let Some(&(entry, kind, generation)) = entries.compiled.get(entries.resumes) else {
        entries.resumes += 1;
        return ctx.completion;
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
fn native_call_trampoline_enters_available_compiled_tiers_on_child_and_current_frames() {
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
    let template_entry = (
        template.entry_addr().unwrap() as u64,
        NativeFrameKind::Baseline,
        1,
    );
    #[cfg(target_arch = "aarch64")]
    let optimized = crate::optimizing::compile_optimized(&snapshot, 2, None).unwrap();
    #[cfg(target_arch = "aarch64")]
    let compiled = vec![
        template_entry,
        (
            optimized.entry_addr().unwrap() as u64,
            NativeFrameKind::Optimizing,
            2,
        ),
    ];
    #[cfg(target_arch = "x86_64")]
    let compiled = vec![template_entry];
    for in_place in [false, true] {
        let mut entries = Entries {
            compiled: compiled.clone(),
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
            pending_call: {
                let mut request = CallRequest::EMPTY;
                request.entry = parent_entry as *const () as u64;
                request.header = VmFrameHeader::interpreter(0, 1);
                request.arguments = arguments.as_ptr();
                request.argument_count = 1;
                request.parameter_count = 1;
                request.callee = Value::function(0);
                request
            },
            completion: NativeResultPair::success(Value::UNDEFINED),
        };
        unsafe { (*ctx.thread).frame_cell = std::ptr::from_mut(&mut ctx.native_frame) as u64 };
        // SAFETY: all entries, context records and argument sources are live; these
        // non-allocating bodies need no heap or runtime activation.
        let result = unsafe { call_trampoline(&mut ctx) };
        assert_eq!(result, NativeResultPair::success(Value::number_i32(123)));
        assert_eq!(entries.resumes, entries.compiled.len() + 1);
        assert!(ctx.native_frame.is_null());
        assert!(error.is_none());
    }
}

#[derive(Default)]
struct StagedAnchorObservation {
    caller: u64,
    anchor: u64,
}

extern "C" fn staged_anchor_child(ctx: *mut JitCtx) -> NativeResultPair {
    // SAFETY: the noncollecting fixture retains its context and observation.
    let ctx = unsafe { &mut *ctx };
    let observation =
        unsafe { &mut *((*ctx.thread).runtime_context as *mut StagedAnchorObservation) };
    let frame = unsafe { &*ctx.native_frame };
    observation.caller = frame.caller;
    observation.anchor = frame.caller_return_pc;
    NativeResultPair::success(Value::number_i32(917))
}

#[test]
fn generated_staged_anchor_is_the_exact_return_table_coordinate() {
    use dynasmrt::{DynasmApi, dynasm};
    let table = super::TransitionTable::resolve();
    let mut relocations = crate::artifact::relocation::RelocationCapture::default();
    #[cfg(target_arch = "aarch64")]
    let (mapping, entry, returned) = {
        let mut ops = dynasmrt::aarch64::Assembler::new().unwrap();
        let entry = ops.offset();
        dynasm!(ops ; .arch aarch64
            ; stp x29, x30, [sp, #-32]!
            ; stp x19, x20, [sp, #16]
            ; mov x29, sp
            ; mov x20, x0
        );
        let returned =
            crate::arm64::js_call::emit_enter_staged(&mut ops, &mut relocations, &table, 20);
        dynasm!(ops ; .arch aarch64
            ; ldp x19, x20, [sp, #16]
            ; ldp x29, x30, [sp], #32
            ; ret
        );
        (ops.finalize().unwrap(), entry, returned)
    };
    #[cfg(target_arch = "x86_64")]
    let (mapping, entry, returned) = {
        let mut ops = dynasmrt::x64::Assembler::new().unwrap();
        let entry = ops.offset();
        crate::x86_64::call_abi::emit_c_entry(&mut ops);
        dynasm!(ops ; .arch x64
            ; push rbp
            ; mov rbp, rsp
            ; push r15
            ; sub rsp, 8
            ; mov r15, rdi
        );
        let returned =
            crate::x86_64::js_call::emit_enter_staged(&mut ops, &mut relocations, &table, 15);
        dynasm!(ops ; .arch x64 ; add rsp, 8 ; pop r15 ; pop rbp ; ret);
        (ops.finalize().unwrap(), entry, returned)
    };
    let mut parent = otter_vm::native_abi::Frame::new(
        VmFrameHeader::interpreter(9, 0),
        0,
        Value::function(9),
        Value::UNDEFINED,
    );
    parent.header.kind = NativeFrameKind::Baseline;
    parent.code_object_id = 17;
    let parent_address = std::ptr::from_mut(&mut parent) as u64;
    let code = crate::CompiledCode::new(mapping, entry);
    // SAFETY: only the address is read; `code` outlives every use of it.
    let entry_address = unsafe { code.entry_ptr() } as usize as u64;
    let actual_return = entry_address + (returned.0 - entry.0) as u64;
    let mut sites = Vec::new();
    crate::return_sites::ReturnSiteRecorder {
        entries: &mut sites,
        safepoint_id: 0,
        logical_pc: 5,
    }
    .record(returned)
    .unwrap();
    for existing_anchor in [0, actual_return + 8] {
        let mut observed = StagedAnchorObservation::default();
        let mut thread = VmThread::empty();
        thread.runtime_context = std::ptr::from_mut(&mut observed) as u64;
        let mut error = None;
        let mut ctx = JitCtx {
            thread: &mut thread,
            native_frame: std::ptr::from_mut(&mut parent),
            error: &mut error,
            generated_depth_limit: 512,
            global_this_offset: std::ptr::null(),
            native_stack_limit: 0,
            generated_feedback_clean: 1,
            alloc_window: otter_vm::jit::JitMachineAllocationWindow::disabled(),
            runtime_stats: std::ptr::null_mut(),
            pending_call: CallRequest::EMPTY,
            completion: NativeResultPair::success(Value::UNDEFINED),
            completion_destination: u32::MAX,
            completion_generation: 0,
        };
        ctx.pending_call.entry = staged_anchor_child as *const () as u64;
        ctx.pending_call.header = VmFrameHeader::interpreter(7, 0);
        ctx.pending_call.callee = Value::function(7);
        if existing_anchor != 0 {
            ctx.pending_call.caller = parent_address;
            ctx.pending_call.caller_return_pc = existing_anchor;
        }
        // SAFETY: the wrapper obeys platform C and the request owns empty live spans.
        let run: otter_vm::native_abi::JitEntry = unsafe { std::mem::transmute(code.entry_ptr()) };
        assert_eq!(
            run(&mut ctx),
            NativeResultPair::success(Value::number_i32(917))
        );
        assert_eq!(observed.caller, parent_address);
        if existing_anchor == 0 {
            assert_eq!(observed.anchor, actual_return);
            // The same coordinate recorded for machine return lookup resolves
            // the source recipe; no post-Windows-cleanup coordinate is substituted.
            let base = entry_address - entry.0 as u64;
            assert_eq!(sites.len(), 1);
            assert_eq!(
                u64::from(sites[0].native_return_offset),
                observed.anchor - base
            );
            assert_eq!(sites[0].safepoint_id, 0);
        } else {
            assert_eq!(observed.anchor, existing_anchor);
        }
        assert_eq!(ctx.native_frame, std::ptr::from_mut(&mut parent));
        assert!(error.is_none());
    }
}
