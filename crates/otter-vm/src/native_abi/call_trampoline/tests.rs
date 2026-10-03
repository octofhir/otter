//! Execution tests for native-stack calls, continuation resumption and roots.
//!
//! # Contents
//! Arity, nested continuations, abrupt completion, stack limits and moving GC.
//!
//! # Invariants
//! Fixtures use the actual architecture trampoline and collector frame walk.
//! Entry-local Rust guards must be dropped before another JS entry starts.
//!
//! # See also
//! - [`super::call_trampoline`] for the common stack owner.

use super::*;
use crate::native_abi::{Frame, NativeFrameKind, NativeResultDomain, NativeResultStatus, VmThread};

pub(super) fn context(thread: &mut VmThread, error: &mut Option<VmError>) -> JitCtx {
    JitCtx {
        thread,
        native_frame: std::ptr::null_mut(),
        error,
        generated_depth_limit: 512,
        global_this_offset: std::ptr::null(),
        native_stack_limit: 0,
        generated_feedback_clean: 1,
        completion_destination: u32::MAX,
        completion_generation: 0,
        alloc_window: crate::jit::JitMachineAllocationWindow::disabled(),
        runtime_stats: std::ptr::null_mut(),
        pending_call: CallRequest::EMPTY,
        completion: NativeResultPair::success(Value::UNDEFINED),
    }
}

fn request(entry: JitEntry, arguments: &[Value], registers: u16, parameters: u32) -> CallRequest {
    CallRequest {
        entry: entry as usize as u64,
        header: VmFrameHeader::interpreter(7, registers),
        code_object_id: 0,
        arguments: arguments.as_ptr(),
        argument_count: arguments.len() as u32,
        parameter_count: parameters,
        callee: Value::function(7),
        receiver: Value::boolean(true),
        new_target: Value::UNDEFINED,
        ..CallRequest::EMPTY
    }
}

extern "C" fn arity_entry(ctx: *mut JitCtx) -> NativeResultPair {
    // SAFETY: the trampoline publishes a fully initialized frame and windows.
    let ctx = unsafe { &mut *ctx };
    let frame = unsafe { &*ctx.native_frame };
    assert_eq!(ctx.native_frame.addr() & 15, 0);
    assert_eq!(
        frame.registers.as_mut_ptr().addr(),
        ctx.native_frame.addr() + std::mem::size_of::<Frame>()
    );
    assert_eq!(frame.self_value(), Value::function(7));
    assert_eq!(frame.this_value(), Value::boolean(true));
    assert_eq!(frame.new_target(), Value::UNDEFINED);
    assert!(frame.cold.is_none());
    assert_eq!(frame.code_object_id, 0);
    assert_eq!(frame.call_site, super::super::NO_SAFEPOINT);
    assert_eq!(frame.return_register(), None);
    let actual = frame.incoming_argument_count();
    let parameter_count = unsafe { (*ctx.thread).runtime_context } as usize;
    let window = unsafe {
        std::slice::from_raw_parts(
            frame.registers.as_mut_ptr().add(frame.registers.len()),
            actual as usize,
        )
    };
    for (index, value) in frame.registers.iter().enumerate() {
        let expected = if index < parameter_count && index < actual as usize {
            Value::number_i32(index as i32 + 11)
        } else {
            Value::UNDEFINED
        };
        assert_eq!(*value, expected);
    }
    for (index, value) in window.iter().enumerate() {
        assert_eq!(*value, Value::number_i32(index as i32 + 11));
    }
    NativeResultPair::success(Value::number_i32(actual as i32))
}

#[test]
fn initializes_formals_and_retains_every_actual_argument() {
    for (registers, parameters, actual) in [
        (0, 0, 0),
        (9, 4, 0),
        (9, 4, 2),
        (9, 4, 12),
        (4096, 4096, 8192),
    ] {
        let arguments = (0..actual)
            .map(|i| Value::number_i32(i + 11))
            .collect::<Vec<_>>();
        let mut thread = VmThread::empty();
        thread.runtime_context = parameters as u64;
        let mut error = None;
        let mut ctx = context(&mut thread, &mut error);
        ctx.pending_call = request(arity_entry, &arguments, registers, parameters);
        // SAFETY: arguments and context stay live and this entry never allocates.
        let result = unsafe { call_trampoline(&mut ctx) };
        assert_eq!(result, NativeResultPair::success(Value::number_i32(actual)));
        assert!(ctx.native_frame.is_null());
        assert!(error.is_none());
    }
}

#[derive(Default)]
struct Harness {
    vm: *mut crate::Interpreter,
    active_rust: u32,
    max_rust: u32,
    max_js: u32,
    entries: u32,
    throws: bool,
    collections: u32,
    tail_arguments: [Value; 2],
}

struct EntryGuard(*mut Harness);
impl Drop for EntryGuard {
    fn drop(&mut self) {
        // SAFETY: fixture harness outlives every entry and its local guard.
        unsafe { (*self.0).active_rust -= 1 };
    }
}

extern "C" fn recursive_entry(ctx: *mut JitCtx) -> NativeResultPair {
    // SAFETY: the fixture context points at its live harness and current frame.
    let ctx = unsafe { &mut *ctx };
    let harness = unsafe { (*ctx.thread).runtime_context as *mut Harness };
    unsafe {
        (*harness).active_rust += 1;
        (*harness).max_rust = (*harness).max_rust.max((*harness).active_rust);
        (*harness).max_js = (*harness).max_js.max((*ctx.native_frame).depth);
        (*harness).entries += 1;
    }
    let _guard = EntryGuard(harness);
    let frame = ctx.native_frame;
    let pc = unsafe { (*frame).header.pc };
    if pc == 1 {
        assert_eq!(ctx.completion_destination, 2);
        // Home the child's payload before collection; no frame borrow spans it.
        unsafe { (&mut (*frame).registers)[2] = ctx.completion.payload_value() };
        if !unsafe { (*harness).vm }.is_null() {
            collect_and_check_roots(harness, frame);
        }
        let value = unsafe { (&(*frame).registers)[2] };
        return match ctx.completion.validate(NativeResultDomain::Execution) {
            Some(NativeResultStatus::Success) => NativeResultPair::success(value),
            Some(NativeResultStatus::Throw) => NativeResultPair::throw_value(value),
            _ => NativeResultPair::fatal_internal(),
        };
    }
    let remaining = unsafe { (&(*frame).registers)[0].as_i32().unwrap() };
    if remaining == 0 {
        if !unsafe { (*harness).vm }.is_null() {
            collect_and_check_roots(harness, frame);
        }
        let value = unsafe { (&(*frame).registers)[1] };
        return if unsafe { (*harness).throws } {
            NativeResultPair::throw_value(value)
        } else {
            NativeResultPair::success(value)
        };
    }
    unsafe {
        (*frame).header.pc = 1;
        (&mut (*frame).registers)[0] = Value::number_i32(remaining - 1);
    }
    let args = unsafe { std::slice::from_raw_parts((*frame).registers.as_mut_ptr(), 2) };
    ctx.pending_call = request(recursive_entry, args, 3, 2);
    ctx.pending_call.return_destination = 2;
    ctx.pending_call.receiver = args[1];
    ctx.pending_call.new_target = args[1];
    // Different tier tags share the identical continuation and stack contract.
    ctx.pending_call.header.kind = match remaining % 3 {
        0 => NativeFrameKind::Interpreter,
        1 => NativeFrameKind::Baseline,
        _ => NativeFrameKind::Optimizing,
    };
    NativeResultPair::continue_execution()
}

fn collect_and_check_roots(harness: *mut Harness, frame: *mut Frame) {
    // SAFETY: the enclosing runtime turn roots the native frame cell. Raw frame
    // descriptors retain no Rust slot references while the collector rewrites.
    unsafe { (*(*harness).vm).force_gc().unwrap() };
    unsafe { (*harness).collections += 1 };
    let mut cursor = frame;
    while !cursor.is_null() {
        let frame = unsafe { &*cursor };
        if frame.registers.len() == 3 {
            let actuals =
                unsafe { std::slice::from_raw_parts(frame.registers.as_mut_ptr().add(3), 2) };
            assert_eq!(frame.registers[1], actuals[1]);
            assert_eq!(frame.this_value(), actuals[1]);
            assert_eq!(frame.new_target(), actuals[1]);
            assert!(frame.registers[1].as_object().is_some());
        }
        cursor = frame.caller_frame();
    }
}

#[test]
fn continuation_calls_release_rust_entries_and_propagate_completions() {
    for throws in [false, true] {
        let mut harness = Harness {
            throws,
            ..Harness::default()
        };
        let mut thread = VmThread::empty();
        thread.runtime_context = std::ptr::from_mut(&mut harness) as u64;
        let mut error = None;
        let mut ctx = context(&mut thread, &mut error);
        let args = [Value::number_i32(128), Value::number_i32(917)];
        ctx.pending_call = request(recursive_entry, &args, 3, 2);
        let result = unsafe { call_trampoline(&mut ctx) };
        assert_eq!(
            result.validate(NativeResultDomain::Execution),
            Some(if throws {
                NativeResultStatus::Throw
            } else {
                NativeResultStatus::Success
            })
        );
        assert_eq!(result.payload_value(), args[1]);
        assert_eq!(harness.max_js, 129);
        assert_eq!(harness.max_rust, 1);
        assert_eq!(harness.active_rust, 0);
        assert_eq!(harness.entries, 257);
        assert!(ctx.native_frame.is_null());
        assert!(error.is_none());
    }
}

#[test]
fn stack_limits_reject_before_publishing_a_child() {
    for native_limit in [false, true] {
        let mut thread = VmThread::empty();
        let mut error = None;
        let mut ctx = context(&mut thread, &mut error);
        ctx.pending_call = request(arity_entry, &[], 0, 0);
        if native_limit {
            ctx.native_stack_limit = usize::MAX;
        } else {
            ctx.generated_depth_limit = 0;
        }
        let result = unsafe { call_trampoline(&mut ctx) };
        assert_eq!(
            result.validate(NativeResultDomain::Execution),
            Some(NativeResultStatus::Fatal)
        );
        assert!(ctx.native_frame.is_null());
        assert!(matches!(error, Some(VmError::StackOverflow { .. })));
    }
}

#[test]
fn moving_collection_rewrites_the_entire_native_call_chain() {
    let mut vm = crate::Interpreter::new();
    let mut stack = crate::ActivationStack::new();
    vm.with_runtime_turn(&mut stack, |turn| {
        let (vm, _) = turn.into_parts();
        let object = vm.alloc_host_object_with_roots(&[], &[]).unwrap();
        let args = [Value::number_i32(16), Value::object(object)];
        let mut harness = Harness {
            vm,
            ..Harness::default()
        };
        let mut thread = VmThread::empty();
        thread.runtime_context = std::ptr::from_mut(&mut harness) as u64;
        let mut error = None;
        let mut ctx = context(&mut thread, &mut error);
        let mut empty_window: [Value; 0] = [];
        let mut outer = Frame::new(
            VmFrameHeader::interpreter(0, 0),
            empty_window.as_mut_ptr() as u64,
            Value::UNDEFINED,
            Value::UNDEFINED,
        );
        ctx.native_frame = &mut outer;
        ctx.pending_call = request(recursive_entry, &args, 3, 2);
        ctx.pending_call.receiver = args[1];
        ctx.pending_call.new_target = args[1];
        let enclosing = unsafe {
            (*harness.vm)
                .jit_enter_native_frames(std::ptr::NonNull::from(&mut ctx.native_frame).cast())
                .unwrap()
        };
        let result = unsafe { call_trampoline(&mut ctx) };
        unsafe { (*harness.vm).jit_leave_native_frames(enclosing) };
        assert_eq!(
            result.validate(NativeResultDomain::Execution),
            Some(NativeResultStatus::Success)
        );
        assert!(result.payload_value().as_object().is_some());
        assert_eq!(harness.max_js, 17);
        assert_eq!(harness.collections, 17);
        assert_eq!(harness.max_rust, 1);
        assert_eq!(ctx.native_frame, std::ptr::from_mut(&mut outer));
        assert!(error.is_none());
    });
}

extern "C" fn invalid_domain_entry(_ctx: *mut JitCtx) -> NativeResultPair {
    NativeResultPair::out_of_memory()
}

#[test]
fn rejects_invalid_requests_and_non_execution_statuses() {
    for invalid in 0..4 {
        let mut thread = VmThread::empty();
        let mut error = None;
        let mut ctx = context(&mut thread, &mut error);
        ctx.pending_call = request(invalid_domain_entry, &[], 0, 0);
        match invalid {
            0 => ctx.pending_call.entry = 0,
            1 => ctx.pending_call.parameter_count = 1,
            2 => {
                ctx.pending_call.argument_count = 1;
                ctx.pending_call.arguments = std::ptr::null();
            }
            _ => {}
        }
        let result = unsafe { call_trampoline(&mut ctx) };
        assert_eq!(result, NativeResultPair::fatal_internal());
        assert!(ctx.native_frame.is_null());
        if invalid < 3 {
            assert!(matches!(error, Some(VmError::InvalidOperand)));
        }
    }
}

extern "C" fn restored_entry(ctx: *mut JitCtx) -> NativeResultPair {
    let ctx = unsafe { &mut *ctx };
    let frame = unsafe { &*ctx.native_frame };
    assert_eq!(
        &*frame.registers,
        &[
            Value::number_i32(31),
            Value::UNDEFINED,
            Value::number_i32(44),
            Value::UNDEFINED
        ]
    );
    let view = crate::ActiveFrameRef::from_frame(frame);
    assert_eq!(view.incoming_argument_count(), 3);
    assert_eq!(view.incoming_argument(1).unwrap(), Value::number_i32(12));
    assert_eq!(frame.return_register(), Some(0));
    NativeResultPair::success(Value::number_i32(44))
}

#[test]
fn restored_registers_overlay_formals_without_changing_actuals() {
    let mut thread = VmThread::empty();
    let mut error = None;
    let mut ctx = context(&mut thread, &mut error);
    let actuals = [
        Value::number_i32(11),
        Value::number_i32(12),
        Value::number_i32(13),
    ];
    let seeds = [
        Value::number_i32(31),
        Value::UNDEFINED,
        Value::number_i32(44),
    ];
    ctx.pending_call = request(restored_entry, &actuals, 4, 2);
    ctx.pending_call.initial_registers = seeds.as_ptr();
    ctx.pending_call.initial_register_count = 3;
    ctx.pending_call.return_destination = 0;
    let result = unsafe { call_trampoline(&mut ctx) };
    assert_eq!(result, NativeResultPair::success(Value::number_i32(44)));
    assert!(ctx.native_frame.is_null());
    assert!(error.is_none());
}

extern "C" fn tail_entry(ctx: *mut JitCtx) -> NativeResultPair {
    let ctx = unsafe { &mut *ctx };
    let harness = unsafe { (*ctx.thread).runtime_context as *mut Harness };
    unsafe {
        (*harness).active_rust += 1;
        (*harness).max_rust = (*harness).max_rust.max((*harness).active_rust);
        (*harness).max_js = (*harness).max_js.max((*ctx.native_frame).depth);
        (*harness).entries += 1;
    }
    let _guard = EntryGuard(harness);
    let frame = unsafe { &*ctx.native_frame };
    assert!(
        !frame
            .header
            .flags
            .contains(super::super::NativeFrameFlags::TAIL_CALL)
    );
    let remaining = frame.registers[0].as_i32().unwrap();
    if remaining == 0 {
        return NativeResultPair::success(frame.registers[1]);
    }
    unsafe {
        (*harness).tail_arguments[0] = Value::number_i32(remaining - 1);
    }
    ctx.pending_call = request(tail_entry, unsafe { &(*harness).tail_arguments }, 2, 2);
    ctx.pending_call.header.flags =
        super::super::NativeFrameFlags::from_bits(super::super::NativeFrameFlags::TAIL_CALL);
    NativeResultPair::continue_execution()
}

#[test]
fn tail_replacement_keeps_one_physical_activation_and_rust_entry() {
    let args = [Value::number_i32(10000), Value::number_i32(917)];
    let mut harness = Harness {
        tail_arguments: args,
        ..Default::default()
    };
    let mut thread = VmThread::empty();
    thread.runtime_context = std::ptr::from_mut(&mut harness) as u64;
    let mut error = None;
    let mut ctx = context(&mut thread, &mut error);
    ctx.generated_depth_limit = 1;
    ctx.pending_call = request(tail_entry, &args, 2, 2);
    let result = unsafe { call_trampoline(&mut ctx) };
    assert_eq!(result, NativeResultPair::success(args[1]));
    assert_eq!(harness.entries, 10001);
    assert_eq!(harness.max_js, 1);
    assert_eq!(harness.max_rust, 1);
    assert!(ctx.native_frame.is_null());
    assert!(error.is_none());
}

struct TierHarness {
    active: u32,
    entries: u32,
    frame: *mut Frame,
    result: NativeResultPair,
}
struct TierGuard(*mut TierHarness);
impl Drop for TierGuard {
    fn drop(&mut self) {
        unsafe { (*self.0).active -= 1 };
    }
}
fn tier_guard(ctx: &JitCtx) -> (*mut TierHarness, TierGuard) {
    let harness = unsafe { (*ctx.thread).runtime_context as *mut TierHarness };
    unsafe {
        assert_eq!(
            (*harness).active,
            0,
            "Rust dispatch ends before tier transfer"
        );
        (*harness).active += 1;
        (*harness).entries += 1;
        assert_eq!((*harness).frame, ctx.native_frame);
    }
    (harness, TierGuard(harness))
}
extern "C" fn selected_tier(ctx: *mut JitCtx) -> NativeResultPair {
    let ctx = unsafe { &mut *ctx };
    let (harness, _guard) = tier_guard(ctx);
    assert_eq!(unsafe { (*ctx.native_frame).depth }, 1);
    unsafe { (*harness).result }
}
extern "C" fn tier_dispatch(ctx: *mut JitCtx) -> NativeResultPair {
    let ctx = unsafe { &mut *ctx };
    let harness = unsafe { (*ctx.thread).runtime_context as *mut TierHarness };
    if unsafe { (*harness).frame.is_null() } {
        unsafe { (*harness).frame = ctx.native_frame };
    }
    let (harness, _guard) = tier_guard(ctx);
    if unsafe { (*harness).entries } == 1 {
        unsafe { (*ctx.native_frame).code_object_id = 0xf012_3456 };
        ctx.pending_call = CallRequest::EMPTY;
        ctx.pending_call.set_entry(selected_tier);
        ctx.pending_call.header.flags =
            super::super::NativeFrameFlags::from_bits(super::super::NativeFrameFlags::TIER_ENTRY);
        NativeResultPair::continue_execution()
    } else {
        assert_eq!(ctx.completion, unsafe { (*harness).result });
        assert_eq!(ctx.completion_destination, TIER_COMPLETION_DESTINATION);
        assert_eq!(ctx.completion_generation, 0xf012_3456);
        NativeResultPair::success(Value::number_i32(91))
    }
}
#[test]
fn tier_transfer_retains_the_frame_and_releases_rust_before_every_entry() {
    use super::super::{ExitAction, ExitReason, SideExit};
    for result in [
        NativeResultPair::success(Value::number_i32(72)),
        NativeResultPair::throw_value(Value::number_i32(73)),
        NativeResultPair::side_exit(SideExit::new(
            3,
            ExitReason::TypeMismatch,
            ExitAction::Recompile,
        )),
        NativeResultPair::fatal_internal(),
    ] {
        let mut harness = TierHarness {
            active: 0,
            entries: 0,
            frame: std::ptr::null_mut(),
            result,
        };
        let mut thread = VmThread::empty();
        thread.runtime_context = std::ptr::from_mut(&mut harness) as u64;
        let mut error = None;
        let mut ctx = context(&mut thread, &mut error);
        ctx.generated_depth_limit = 1;
        ctx.pending_call = request(tier_dispatch, &[], 0, 0);
        assert_eq!(
            unsafe { call_trampoline(&mut ctx) },
            NativeResultPair::success(Value::number_i32(91))
        );
        assert_eq!(harness.entries, 3);
        assert_eq!(harness.active, 0);
        assert!(ctx.native_frame.is_null());
        assert!(error.is_none());
    }
}

extern "C" fn generation_entry(ctx: *mut JitCtx) -> NativeResultPair {
    let ctx = unsafe { &mut *ctx };
    let frame = unsafe { &*ctx.native_frame };
    use super::super::NativeFrameFlags as Flags;
    assert_eq!(frame.code_object_id, 0xf012_3456);
    assert_eq!(frame.header.kind, NativeFrameKind::Baseline);
    assert_eq!(
        frame.header.flags.bits(),
        Flags::HAS_SAFEPOINTS | Flags::DERIVED_CONSTRUCTOR | Flags::CONSTRUCT
    );
    // An object result completes the construct without consulting `this`.
    NativeResultPair::success(Value::function(17))
}
#[test]
fn request_control_bits_leave_the_generation_and_persistent_flags_intact() {
    use super::super::NativeFrameFlags as Flags;
    let mut thread = VmThread::empty();
    let mut error = None;
    let mut ctx = context(&mut thread, &mut error);
    ctx.pending_call = request(generation_entry, &[], 0, 0);
    ctx.pending_call.code_object_id = 0xf012_3456;
    ctx.pending_call.header.kind = NativeFrameKind::Baseline;
    ctx.pending_call.header.flags = Flags::from_bits(
        Flags::HAS_SAFEPOINTS | Flags::DERIVED_CONSTRUCTOR | Flags::CONSTRUCT | Flags::TAIL_CALL,
    );
    assert_eq!(
        unsafe { call_trampoline(&mut ctx) },
        NativeResultPair::success(Value::function(17))
    );
    assert!(ctx.native_frame.is_null());
    assert!(error.is_none());
}

#[test]
fn resumption_uses_the_published_frame_at_the_depth_limit() {
    let mut registers = [Value::number_i32(17)];
    let mut frame = Frame::new(
        VmFrameHeader::interpreter(7, 1),
        registers.as_mut_ptr() as u64,
        Value::function(7),
        Value::UNDEFINED,
    );
    frame.depth = 1;
    let pointer = std::ptr::from_mut(&mut frame);
    let mut harness = TierHarness {
        active: 0,
        entries: 0,
        frame: pointer,
        result: NativeResultPair::success(Value::number_i32(72)),
    };
    let mut thread = VmThread::empty();
    thread.runtime_context = std::ptr::from_mut(&mut harness) as u64;
    let mut error = None;
    let mut ctx = context(&mut thread, &mut error);
    ctx.native_frame = pointer;
    ctx.generated_depth_limit = 1;
    ctx.pending_call = CallRequest::EMPTY;
    ctx.pending_call.set_entry(tier_dispatch);
    ctx.pending_call.header.flags =
        super::super::NativeFrameFlags::from_bits(super::super::NativeFrameFlags::TIER_ENTRY);
    assert_eq!(
        unsafe { call_trampoline(&mut ctx) },
        NativeResultPair::success(Value::number_i32(91))
    );
    assert_eq!(ctx.native_frame, pointer);
    assert_eq!(frame.register_base(), registers.as_mut_ptr() as u64);
    assert_eq!(registers[0], Value::number_i32(17));
    assert_eq!(frame.depth, 1);
    assert_eq!(harness.entries, 3);
    assert_eq!(harness.active, 0);
    assert!(error.is_none());
}

#[test]
fn tail_call_from_a_resumed_record_returns_the_request_to_its_owner() {
    // A tier request resumes a record whose native frame belongs to a
    // generated caller; the resumed code then tail-calls. The trampoline must
    // not replace a record it does not own: it hands the staged request back
    // as `Continue`, so the owner retires the record and its caller enters
    // the request.
    let mut registers = [Value::number_i32(10_000), Value::number_i32(41)];
    let mut frame = Frame::new(
        VmFrameHeader::interpreter(7, 2),
        registers.as_mut_ptr() as u64,
        Value::function(7),
        Value::UNDEFINED,
    );
    frame.depth = 1;
    let pointer = std::ptr::from_mut(&mut frame);
    let mut harness = Harness {
        tail_arguments: registers,
        ..Harness::default()
    };
    let mut thread = VmThread::empty();
    thread.runtime_context = std::ptr::from_mut(&mut harness) as u64;
    let mut error = None;
    let mut ctx = context(&mut thread, &mut error);
    ctx.native_frame = pointer;
    ctx.generated_depth_limit = 1;
    ctx.pending_call = CallRequest::EMPTY;
    ctx.pending_call.set_entry(tail_entry);
    ctx.pending_call.header.flags =
        super::super::NativeFrameFlags::from_bits(super::super::NativeFrameFlags::TIER_ENTRY);
    assert_eq!(
        unsafe { call_trampoline(&mut ctx) },
        NativeResultPair::continue_execution()
    );
    assert_eq!(ctx.native_frame, pointer);
    assert!(
        ctx.pending_call
            .header
            .flags
            .contains(super::super::NativeFrameFlags::TAIL_CALL)
    );
    assert_eq!(harness.entries, 1);
    assert_eq!(harness.max_rust, 1);
    assert!(error.is_none());
}
