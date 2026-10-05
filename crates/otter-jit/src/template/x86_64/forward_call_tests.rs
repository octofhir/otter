//! Executable intrinsic-forwarding proofs using the production x86 encoders.
//!
//! # Contents
//! - Exact zero, odd, extra and multi-page actual spans, with mapped window and
//!   context reloads, untouched source/slack, native alignment and throw cleanup.
//! - Bootstrap identity, exposed arguments and stack admission misses reach the
//!   existing staged C boundary once and preserve its exact packet order.
//!
//! # Invariants
//! - Owned non-moving layout bytes, frames, actuals and executable mappings
//!   outlive each call. The fixture performs no managed allocation or collection.
//! - The private caller/JS target use System V explicitly on every host; staged
//!   callbacks use the platform C ABI through the production boundary encoder.
//! - Callback observations use fixed owned storage and never assert or unwind.
//! - Runtime forwarding regressions separately prove real moving roots,
//!   polymorphic/sibling generation linkage, overrides and exception identity.

use super::*;
use crate::entry::{
    NATIVE_FRAME_OFFSET, NATIVE_FRAME_REGISTER_BASE_OFFSET, THREAD_OFFSET, TransitionTable,
};
use otter_bytecode::{ArgumentBindingStorage, ArgumentsObjectKind, Op, Operand};
use otter_vm::jit::JitTestInstruction;
use otter_vm::{
    Value,
    native_abi::{
        CallRequest, Frame, JitCtx, NativeResultPair, NativeResultStatus, VmFrameHeader, VmThread,
    },
};

const APPLY: u32 = 42;
const NATIVE_RESULT: i32 = 731;
const STAGED_RESULT: i32 = 713;
const DESTINATION: usize = 0;
const METHOD: usize = 1;
const CALLEE: usize = 2;
const RECEIVER: usize = 3;
const REGISTER_BINDING: usize = 4;
const CONTEXT: usize = 5;
const ABSENT_BINDING: usize = 6;
const SLACK: u64 = 0x9e37_79b9_7f4a_7c15;
const ACTUAL_CAPACITY: usize = 600;

#[repr(C)]
struct Observation {
    native_calls: u64,
    stage_calls: u64,
    trampoline_calls: u64,
    argument_count: u64,
    callee: u64,
    receiver: u64,
    new_target: u64,
    entry_alignment: u64,
    payload: u64,
    status: u64,
    stack_restored: u64,
    packet_count: u64,
    invalid_packet: u64,
    packet: [u64; 8],
    actuals: [u64; ACTUAL_CAPACITY],
}

impl Default for Observation {
    fn default() -> Self {
        Self {
            native_calls: 0,
            stage_calls: 0,
            trampoline_calls: 0,
            argument_count: 0,
            callee: 0,
            receiver: 0,
            new_target: 0,
            entry_alignment: 0,
            payload: 0,
            status: 0,
            stack_restored: 0,
            packet_count: 0,
            invalid_packet: 0,
            packet: [0; 8],
            actuals: [0; ACTUAL_CAPACITY],
        }
    }
}

unsafe fn observation<'a>(ctx: *mut JitCtx) -> &'a mut Observation {
    // SAFETY: the private synchronous fixture owns all three records for the
    // dynamic extent of its call, and no other writer aliases the observation.
    unsafe { &mut *((*(*ctx).thread).runtime_context as *mut Observation) }
}

extern "C" fn stage(ctx: *mut JitCtx, packet: *const Value, count: u32) -> NativeResultPair {
    // SAFETY: the emitted caller supplies its live context and initialized
    // stack packet; this callback copies only the checked bounded words.
    let observed = unsafe { observation(ctx) };
    observed.stage_calls += 1;
    observed.packet_count = u64::from(count);
    if packet.is_null() || count as usize > observed.packet.len() {
        observed.invalid_packet = 1;
    } else {
        // SAFETY: the production packet encoder initialized count Value words.
        for (dst, value) in observed
            .packet
            .iter_mut()
            .zip(unsafe { std::slice::from_raw_parts(packet, count as usize) })
        {
            *dst = value.to_bits();
        }
    }
    NativeResultPair::success(Value::undefined())
}

extern "C" fn trampoline(ctx: *mut JitCtx) -> NativeResultPair {
    // SAFETY: the same private owned context remains live across staged entry.
    unsafe { observation(ctx) }.trampoline_calls += 1;
    NativeResultPair::throw_value(Value::number_i32(STAGED_RESULT))
}

fn snapshot() -> JitCompileSnapshot {
    let mut view = JitCompileSnapshot::without_feedback(
        7,
        6,
        8,
        vec![JitTestInstruction::new(
            Op::CallForwardArguments,
            0,
            0,
            [DESTINATION, METHOD, CALLEE, RECEIVER, CONTEXT]
                .into_iter()
                .map(|register| Operand::Register(register as u16))
                .collect(),
        )],
    );
    view.forward_apply_native_ref = Some(APPLY);
    view.native_call_layout = otter_vm::jit::JitNativeCallLayout::current();
    view.collection_layout.native_function_type_tag =
        otter_vm::native_function::NATIVE_FUNCTION_BODY_TYPE_TAG;
    view.seed_argument_bindings_for_test(
        ArgumentsObjectKind::Mapped,
        &[
            (
                0,
                ArgumentBindingStorage::Register {
                    reg: REGISTER_BINDING as u16,
                },
            ),
            (
                1,
                ArgumentBindingStorage::Context {
                    reg: CONTEXT as u16,
                    slot: 1,
                },
            ),
            (
                5,
                ArgumentBindingStorage::Register {
                    reg: ABSENT_BINDING as u16,
                },
            ),
        ],
    );
    view
}

/// Owned cell-layout bytes exercise the VM's actual type/identity offsets.
/// They are never handed to the collector or any semantic VM operation.
fn native_cell(view: &JitCompileSnapshot, identity: u32, tag: u8) -> Box<[u64]> {
    let bytes = view.native_call_layout.identity_byte as usize + 4;
    let mut cell = vec![0u64; bytes.div_ceil(8)].into_boxed_slice();
    let ptr = cell.as_mut_ptr().cast::<u8>();
    // SAFETY: the aligned allocation contains both initialized layout ranges.
    unsafe {
        ptr.write(tag);
        ptr.add(view.native_call_layout.identity_byte as usize)
            .cast::<u32>()
            .write_unaligned(identity);
    }
    cell
}

fn native_target(status: NativeResultStatus) -> dynasmrt::ExecutableBuffer {
    let mut ops = Assembler::new().unwrap();
    dynasm!(ops ; .arch x64
        ; mov r9, [rdi + THREAD_OFFSET as i32]
        ; mov r9, [r9 + std::mem::offset_of!(VmThread, runtime_context) as i32]
        ; inc QWORD [r9 + std::mem::offset_of!(Observation, native_calls) as i32]
        ; mov [r9 + std::mem::offset_of!(Observation, argument_count) as i32], r8
        ; mov [r9 + std::mem::offset_of!(Observation, callee) as i32], rsi
        ; mov [r9 + std::mem::offset_of!(Observation, receiver) as i32], rdx
        ; mov [r9 + std::mem::offset_of!(Observation, new_target) as i32], rcx
        ; mov rax, rsp
        ; and eax, 15
        ; mov [r9 + std::mem::offset_of!(Observation, entry_alignment) as i32], rax
        ; mov r10, r8
        ; copy:
        ; test r10, r10
        ; jz >copied
        ; dec r10
        ; mov rax, [rsp + r10 * 8 + 8]
        ; mov [r9 + r10 * 8 + std::mem::offset_of!(Observation, actuals) as i32], rax
        ; jmp <copy
        ; copied:
        ; mov rax, QWORD Value::number_i32(NATIVE_RESULT).to_bits() as i64
        ; mov edx, status as i32
        ; ret
    );
    ops.finalize().unwrap()
}

fn caller(view: &JitCompileSnapshot, table: &TransitionTable) -> dynasmrt::ExecutableBuffer {
    let mut ops = Assembler::new().unwrap();
    let mut relocations = RelocationCapture::default();
    let threw = ops.new_dynamic_label();
    let throw_value = ops.new_dynamic_label();
    let finished = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64
        ; push r12 ; push r13 ; push r14 ; push r15 ; sub rsp, 8
        ; mov r15, rdi
        ; mov r14, [r15 + NATIVE_FRAME_OFFSET as i32]
        ; mov r13, [r14 + NATIVE_FRAME_REGISTER_BASE_OFFSET as i32]
        ; mov r12, rsp
    );
    let mut return_sites = Vec::new();
    let mut call_source = crate::return_sites::ReturnSiteRecorder {
        entries: &mut return_sites,
        safepoint_id: 1,
        logical_pc: 0,
    };
    emit_forward_call(
        &mut ops,
        &mut relocations,
        table,
        &mut call_source,
        view,
        [
            DESTINATION as u16,
            METHOD as u16,
            CALLEE as u16,
            RECEIVER as u16,
        ],
        throw_value,
        threw,
        threw,
    )
    .unwrap();
    dynasm!(ops ; .arch x64
        ; xor edx, edx
        ; jmp =>finished
        ; =>throw_value
        ; mov edx, NativeResultStatus::Throw as i32
        ; jmp =>finished
        ; =>threw
        ; mov edx, NativeResultStatus::Fatal as i32
        ; =>finished
        ; mov r9, [r15 + THREAD_OFFSET as i32]
        ; mov r9, [r9 + std::mem::offset_of!(VmThread, runtime_context) as i32]
        ; mov [r9 + std::mem::offset_of!(Observation, payload) as i32], rax
        ; mov [r9 + std::mem::offset_of!(Observation, status) as i32], rdx
        ; xor r10d, r10d
        ; cmp rsp, r12
        ; sete r10b
        ; mov [r9 + std::mem::offset_of!(Observation, stack_restored) as i32], r10
        ; mov rsp, r12
        ; add rsp, 8 ; pop r15 ; pop r14 ; pop r13 ; pop r12 ; ret
    );
    ops.finalize().unwrap()
}

fn context(
    thread: &mut VmThread,
    error: &mut Option<otter_vm::VmError>,
    frame: &mut Frame,
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
        completion: NativeResultPair::success(Value::undefined()),
    }
}

unsafe fn enter(code: &dynasmrt::ExecutableBuffer, ctx: &mut JitCtx) {
    // SAFETY: this private generated System V caller accepts the owned context,
    // preserves its nonvolatile registers and touches only initialized fixture
    // ranges or its admitted/probed native stack reservation.
    let run: unsafe extern "sysv64" fn(*mut JitCtx) -> u64 =
        unsafe { std::mem::transmute(code.ptr(dynasmrt::AssemblyOffset(0))) };
    unsafe { run(ctx) };
}

#[test]
fn intrinsic_forward_copies_actuals_refreshes_mappings_and_releases_every_completion() {
    let view = snapshot();
    let cell = native_cell(
        &view,
        APPLY,
        view.collection_layout.native_function_type_tag,
    );
    let slot_index = view.context_layout.slots_byte as usize / 8 + 1;
    for count in [0usize, 1, 3, ACTUAL_CAPACITY] {
        for status in [NativeResultStatus::Success, NativeResultStatus::Throw] {
            let mut observed = Box::new(Observation::default());
            let mut thread = VmThread::empty();
            thread.runtime_context = std::ptr::from_mut(observed.as_mut()) as u64;
            let mut actuals = (0..count)
                .map(|i| Value::number_i32(i as i32))
                .collect::<Vec<_>>();
            // One poison word is outside the initialized actual span.
            actuals.push(Value::from_bits(SLACK));
            let original = actuals.clone();
            let mut parameter_context = vec![0u64; slot_index + 1].into_boxed_slice();
            parameter_context[slot_index] = Value::number_i32(912).to_bits();
            let mut window = [Value::undefined(); 8];
            window[METHOD] = Value::from_bits(cell.as_ptr() as u64);
            window[CALLEE] = Value::function(19);
            window[RECEIVER] = Value::null();
            window[REGISTER_BINDING] = Value::number_i32(901);
            window[CONTEXT] = if count >= 2 {
                Value::from_bits(parameter_context.as_ptr() as u64)
            } else {
                // No actual at index 1 means its context must not be read.
                Value::number_i32(-777)
            };
            window[ABSENT_BINDING] = Value::number_i32(955);
            let source_window = window;
            let mut frame = Frame::new(
                VmFrameHeader::interpreter(7, 8),
                window.as_mut_ptr() as u64,
                Value::function(7),
                Value::undefined(),
            );
            frame.argument_count = count as u32;
            frame.actuals = actuals.as_mut_ptr();
            let mut error = None;
            let mut ctx = context(&mut thread, &mut error, &mut frame);
            let target = native_target(status);
            let mut table = TransitionTable::resolve();
            table.replace_entry_for_test(
                abi::STUB_JIT_CALL_GENERIC,
                target.ptr(dynasmrt::AssemblyOffset(0)) as usize,
            );
            table.replace_entry_for_test(abi::STUB_JIT_STAGE_FORWARD, stage as *const () as usize);
            table.replace_entry_for_test(abi::STUB_JIT_CALL, trampoline as *const () as usize);
            let code = caller(&view, &table);
            // SAFETY: all owned ranges and both mappings remain live here.
            unsafe { enter(&code, &mut ctx) };
            assert_eq!(
                (
                    observed.native_calls,
                    observed.stage_calls,
                    observed.trampoline_calls
                ),
                (1, 0, 0)
            );
            assert_eq!(observed.argument_count, count as u64);
            assert_eq!(observed.entry_alignment, 8);
            assert_eq!(observed.stack_restored, 1);
            assert_eq!(observed.callee, window[CALLEE].to_bits());
            assert_eq!(observed.receiver, window[RECEIVER].to_bits());
            assert_eq!(observed.new_target, Value::undefined().to_bits());
            assert_eq!(observed.payload, Value::number_i32(NATIVE_RESULT).to_bits());
            assert_eq!(observed.status, status as u64);
            let mut expected = original[..count]
                .iter()
                .map(|v| v.to_bits())
                .collect::<Vec<_>>();
            for (index, value) in [(0, 901), (1, 912), (5, 955)] {
                if let Some(actual) = expected.get_mut(index) {
                    *actual = Value::number_i32(value).to_bits();
                }
            }
            assert_eq!(&observed.actuals[..count], expected);
            assert_eq!(
                actuals, original,
                "forwarding must not mutate the incoming span or read alignment slack"
            );
            assert_eq!(&window[1..], &source_window[1..]);
            assert_eq!(
                window[DESTINATION],
                if status == NativeResultStatus::Success {
                    Value::number_i32(NATIVE_RESULT)
                } else {
                    Value::undefined()
                }
            );
            assert!(error.is_none());
            // The same machine mapping reloads the current traced context slot.
            if count >= 2 {
                parameter_context[slot_index] = Value::number_i32(913).to_bits();
                unsafe { enter(&code, &mut ctx) };
                assert_eq!(observed.native_calls, 2);
                assert_eq!(observed.actuals[1], Value::number_i32(913).to_bits());
                assert_eq!(observed.stage_calls + observed.trampoline_calls, 0);
                assert_eq!(observed.stack_restored, 1);
            }
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Miss {
    MissingProof,
    WrongIdentity,
    WrongType,
    Primitive,
    Null,
    MaterializedArguments,
    StackLimit,
}

#[test]
fn failed_intrinsic_and_span_proofs_stage_one_exact_packet_without_native_effects() {
    for miss in [
        Miss::MissingProof,
        Miss::WrongIdentity,
        Miss::WrongType,
        Miss::Primitive,
        Miss::Null,
        Miss::MaterializedArguments,
        Miss::StackLimit,
    ] {
        let mut view = snapshot();
        let tag = if matches!(miss, Miss::WrongType) {
            view.collection_layout.native_function_type_tag ^ 1
        } else {
            view.collection_layout.native_function_type_tag
        };
        let identity = if matches!(miss, Miss::WrongIdentity) {
            APPLY | 0x0100_0000
        } else {
            APPLY
        };
        let cell = native_cell(&view, identity, tag);
        if matches!(miss, Miss::MissingProof) {
            view.forward_apply_native_ref = None;
        }
        let mut observed = Box::new(Observation::default());
        let mut thread = VmThread::empty();
        thread.runtime_context = std::ptr::from_mut(observed.as_mut()) as u64;
        let mut actuals = [
            Value::number_i32(1),
            Value::number_i32(2),
            Value::number_i32(3),
        ];
        let mut window = [Value::undefined(); 8];
        window[METHOD] = match miss {
            Miss::Primitive => Value::number_i32(42),
            Miss::Null => Value::from_bits(0),
            _ => Value::from_bits(cell.as_ptr() as u64),
        };
        window[CALLEE] = Value::function(19);
        window[RECEIVER] = Value::null();
        window[REGISTER_BINDING] = Value::number_i32(901);
        window[CONTEXT] = Value::number_i32(-777); // Staging transports, never dereferences, this word.
        window[ABSENT_BINDING] = Value::number_i32(955);
        let original_window = window;
        let original_actuals = actuals;
        let mut frame = Frame::new(
            VmFrameHeader::interpreter(7, 8),
            window.as_mut_ptr() as u64,
            Value::function(7),
            Value::undefined(),
        );
        frame.argument_count = actuals.len() as u32;
        frame.actuals = actuals.as_mut_ptr();
        if matches!(miss, Miss::MaterializedArguments) {
            // SAFETY: this engine-private geometry fixture writes the derived
            // compressed arguments-identity word in its owned Frame; no heap
            // consumer or collector observes this nonzero sentinel.
            unsafe {
                std::ptr::from_mut(&mut frame)
                    .cast::<u8>()
                    .add(abi::NATIVE_FRAME_ARGUMENTS_OBJECT_OFFSET as usize)
                    .cast::<u32>()
                    .write(37);
            }
        }
        let mut error = None;
        let mut ctx = context(&mut thread, &mut error, &mut frame);
        if matches!(miss, Miss::StackLimit) {
            ctx.native_stack_limit = usize::MAX;
        }
        let target = native_target(NativeResultStatus::Success);
        let mut table = TransitionTable::resolve();
        table.replace_entry_for_test(
            abi::STUB_JIT_CALL_GENERIC,
            target.ptr(dynasmrt::AssemblyOffset(0)) as usize,
        );
        table.replace_entry_for_test(abi::STUB_JIT_STAGE_FORWARD, stage as *const () as usize);
        table.replace_entry_for_test(abi::STUB_JIT_CALL, trampoline as *const () as usize);
        let code = caller(&view, &table);
        // SAFETY: every fixture record/mapping outlives the synchronous call.
        unsafe { enter(&code, &mut ctx) };
        assert_eq!(
            (
                observed.native_calls,
                observed.stage_calls,
                observed.trampoline_calls
            ),
            (0, 1, 1),
            "{miss:?}"
        );
        assert_eq!(observed.packet_count, 6, "{miss:?}");
        assert_eq!(
            &observed.packet[..6],
            &[
                window[METHOD].to_bits(),
                window[CALLEE].to_bits(),
                window[RECEIVER].to_bits(),
                window[REGISTER_BINDING].to_bits(),
                window[ABSENT_BINDING].to_bits(),
                window[CONTEXT].to_bits()
            ],
            "{miss:?}"
        );
        assert_eq!(observed.invalid_packet, 0, "{miss:?}");
        assert_eq!(
            observed.payload,
            Value::number_i32(STAGED_RESULT).to_bits(),
            "{miss:?}"
        );
        assert_eq!(
            observed.status,
            NativeResultStatus::Throw as u64,
            "{miss:?}"
        );
        assert_eq!(observed.stack_restored, 1, "{miss:?}");
        assert_eq!(window, original_window, "{miss:?}");
        assert_eq!(actuals, original_actuals, "{miss:?}");
        assert!(error.is_none(), "{miss:?}");
    }
}
