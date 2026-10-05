//! Executable dynamic receiver writer and untouched pre-effect miss proofs.
//!
//! # Contents
//! - Actual VM snapshot geometry at zero, small and maximum final capacity.
//! - Full initialized bytes, ticket stores, cursor and exact type accounting.
//! - Call-prefix promotion/nonvolatile/FP preservation on both target encoders.
//!
//! # Invariants
//! Host storage never enters the collector. This fixture executes the sole
//! production writer; real family/root/prototype/GC admission is proved by VM
//! and runtime fixtures. A limit miss changes no memory, ticket or counters.

use super::*;
use crate::CompiledCode;
use otter_bytecode::{BytecodeModule, Function, FunctionCodeBuilder, Op, SourceKind};
use otter_gc::{lab::LinearAllocationArea, stats::TypeStats};
use otter_vm::{
    ExecutionContext, JitRuntimeStats, Value,
    native_abi::{CallRequest, Frame, JitCtx, NativeResultPair, VmFrameHeader, VmThread},
};

const POISON: u64 = 0xcafe_1357_2468_abcd;
const CANARY: u64 = 0x51a7_6d9b_2468_1357;

fn view() -> JitCompileSnapshot {
    let mut code = FunctionCodeBuilder::new();
    code.push(Op::ReturnUndefined, &[]);
    ExecutionContext::from_module(
        BytecodeModule {
            module: "dynamic-receiver-layout.js".into(),
            template_sites: vec![],
            source_kind: SourceKind::JavaScript,
            functions: vec![Function {
                id: 0,
                name: "<main>".into(),
                locals: 1,
                code: code.finish(),
                ..Function::default()
            }],
            constants: vec![],
            module_resolutions: vec![],
            module_inits: vec![],
            function_source: None,
        },
        Default::default(),
    )
    .expect("verified layout owner")
    .jit_compile_snapshot(0)
    .expect("actual VM allocation layout")
}

#[cfg(target_arch = "aarch64")]
fn executable(view: &JitCompileSnapshot) -> CompiledCode {
    let mut ops = Assembler::new().unwrap();
    let entry = ops.offset();
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64
        ; stp x19, x30, [sp, #-16]! ; stp x20, x21, [sp, #-16]!
        ; stp x22, x23, [sp, #-16]! ; stp x24, x25, [sp, #-16]! ; stp d8, d9, [sp, #-16]!
        ; mov x20, x0 ; mov x21, x1 ; mov w9, w2 ; mov x19, x3);
    for reg in [22, 23, 24] {
        emit_load_u64(&mut ops, reg, CANARY);
    }
    emit_load_u64(&mut ops, 25, 0x5a5a_5a5a_5a5a_5a5a);
    dynasm!(ops ; .arch aarch64 ; fmov d0, x25 ; fmov d8, x25 ; fmov d15, x25);
    let mut relocations = RelocationCapture::default();
    emit_fit(&mut ops, &mut relocations, view, miss);
    dynasm!(ops ; .arch aarch64 ; b =>done ; =>miss ; mov x0, xzr ; =>done
        ; stp x22, x23, [x19] ; str x24, [x19, #16]
        ; str d0, [x19, #32] ; str d8, [x19, #40] ; str d15, [x19, #48]
        ; ldp d8, d9, [sp], #16
        ; ldp x24, x25, [sp], #16 ; ldp x22, x23, [sp], #16
        ; ldp x20, x21, [sp], #16 ; ldp x19, x30, [sp], #16 ; ret);
    CompiledCode::new(ops.finalize().unwrap(), entry)
}

#[cfg(target_arch = "x86_64")]
fn executable(view: &JitCompileSnapshot) -> CompiledCode {
    let mut ops = Assembler::new().unwrap();
    let entry = ops.offset();
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64 ; push rbx ; push rbp ; push r12 ; push r13 ; push r14 ; push r15
        ; mov r15, rdi ; mov r14, rsi ; mov r8d, edx ; mov rbx, rcx);
    for reg in [5, 12, 13] {
        emit_load_u64(&mut ops, reg, CANARY);
    }
    emit_load_u64(&mut ops, 10, 0x5a5a_5a5a_5a5a_5a5a);
    dynasm!(ops ; .arch x64 ; movq xmm0, r10 ; punpcklqdq xmm0, xmm0
        ; movapd xmm8, xmm0 ; movapd xmm15, xmm0);
    let mut relocations = RelocationCapture::default();
    emit_fit(&mut ops, &mut relocations, view, miss);
    dynasm!(ops ; .arch x64 ; jmp =>done ; =>miss ; xor eax, eax ; =>done
        ; mov [rbx], rbp ; mov [rbx + 8], r12 ; mov [rbx + 16], r13
        ; movq [rbx + 32], xmm0 ; movq [rbx + 40], xmm8 ; movq [rbx + 48], xmm15
        ; pop r15 ; pop r14 ; pop r13 ; pop r12 ; pop rbp ; pop rbx ; ret);
    CompiledCode::new(ops.finalize().unwrap(), entry)
}

fn store32(words: &mut [u64], byte: usize, value: u32) {
    let shift = byte % 8 * 8;
    words[byte / 8] =
        (words[byte / 8] & !(u64::from(u32::MAX) << shift)) | (u64::from(value) << shift);
}

#[test]
fn dynamic_receiver_writer_initializes_and_publishes_once_or_has_no_effect() {
    for capacity in [0usize, 4, 64] {
        for fits in [true, false] {
            let mut view = view();
            let bytes = view.field_layout.cell_bytes(capacity);
            let mut memory = vec![POISON; 320];
            let base = memory.as_mut_ptr() as usize;
            let cage = base & !(u32::MAX as usize);
            let offset = |byte: usize| u32::try_from(base + byte - cage).unwrap();
            view.cage_base = cage;
            let root_byte = 128usize;
            let family_byte = 512usize;
            let start_byte = 1024usize;
            let root = offset(root_byte);
            let family = offset(family_byte);
            let start = base + start_byte;
            store32(
                &mut memory,
                family_byte + view.constructor_layout.root_byte as usize,
                root,
            );
            let cap = root_byte + view.shape_inline_capacity_byte as usize;
            let pointer = memory.as_mut_ptr().cast::<u8>();
            // SAFETY: the initialized host array covers this actual scalar layout.
            unsafe {
                pointer.add(cap).write(capacity as u8);
            }
            let before = memory.clone();
            let mut lab = LinearAllocationArea {
                top: start,
                limit: start + bytes - usize::from(!fits),
            };
            let mut stats = [TypeStats::DEFAULT; 256];
            let mut runtime = JitRuntimeStats::default();
            let mut thread = VmThread::empty();
            let mut frame = Frame::new(
                VmFrameHeader::interpreter(0, 0),
                0,
                Value::function(0),
                Value::UNDEFINED,
            );
            frame.set_construct();
            // Preserve the neighbouring compressed identity word exactly; fabricated
            // host frames are never traced or dereferenced by the collector.
            unsafe {
                (std::ptr::from_mut(&mut frame)
                    .cast::<u8>()
                    .add(abi::NATIVE_FRAME_DERIVED_THIS_CONTEXT_OFFSET as usize)
                    .cast::<u32>())
                .write(0xa1b2_c3d4);
            }

            let mut ctx = JitCtx {
                thread: &mut thread,
                native_frame: &mut frame,
                error: std::ptr::null_mut(),
                generated_depth_limit: u64::MAX,
                global_this_offset: std::ptr::null(),
                native_stack_limit: 0,
                generated_feedback_clean: 1,
                alloc_window: otter_vm::jit::JitMachineAllocationWindow {
                    lab: &mut lab,
                    type_stats: stats.as_mut_ptr(),
                },
                runtime_stats: &mut runtime,
                pending_call: CallRequest::EMPTY,
                completion: NativeResultPair::success(Value::UNDEFINED),
                completion_destination: u32::MAX,
                completion_generation: 0,
            };
            let code = executable(&view);
            let mut canaries = [0u64; 10];
            #[cfg(target_arch = "aarch64")]
            let call: extern "C" fn(*mut JitCtx, *mut Frame, u32, *mut u64) -> usize =
                unsafe { std::mem::transmute(code.entry_ptr()) };
            #[cfg(target_arch = "x86_64")]
            let call: extern "sysv64" fn(
                *mut JitCtx,
                *mut Frame,
                u32,
                *mut u64,
            ) -> usize = unsafe { std::mem::transmute(code.entry_ptr()) };
            // SAFETY: wrapper preserves its host ABI; writer sees private initialized
            // LAB, scalar metadata and statistic rows, and cannot call or collect.
            let result = call(&mut ctx, &mut frame, family, canaries.as_mut_ptr());
            assert_eq!(&canaries[..3], &[CANARY; 3]);
            assert_eq!(&canaries[4..7], &[0x5a5a_5a5a_5a5a_5a5a; 3]);
            assert_eq!(
                result,
                if fits { start } else { 0 },
                "capacity={capacity}, fit={fits}"
            );
            if fits {
                let mut expected = vec![0u64; bytes / 8];
                expected[0] = otter_vm::jit::ordinary_object_header_word(bytes as u32);
                store32(&mut expected, view.object_shape_byte as usize, root);
                for word in &mut expected[view.field_layout.inline_values_byte as usize / 8..] {
                    *word = VALUE_UNDEFINED;
                }
                assert_eq!(
                    &memory[start_byte / 8..(start_byte + bytes) / 8],
                    expected,
                    "complete initialized payload"
                );
                assert_eq!(memory[start_byte / 8 - 1], POISON);
                assert_eq!(memory[(start_byte + bytes) / 8], POISON);
                assert_eq!(lab.top, start + bytes);
                assert_eq!(frame.this_value.to_bits(), start as u64);
                let frame_bytes = std::ptr::from_ref(&frame).cast::<u8>();
                assert_eq!(
                    unsafe {
                        frame_bytes
                            .add(abi::NATIVE_FRAME_CONSTRUCT_RECEIVER_OFFSET as usize)
                            .cast::<u64>()
                            .read()
                    },
                    start as u64
                );
                assert_eq!(
                    unsafe {
                        frame_bytes
                            .add(abi::NATIVE_FRAME_CONSTRUCT_LAYOUT_OFFSET as usize)
                            .cast::<u32>()
                            .read()
                    },
                    family
                );
                let preserved = unsafe {
                    std::ptr::from_ref(&frame)
                        .cast::<u8>()
                        .add(abi::NATIVE_FRAME_DERIVED_THIS_CONTEXT_OFFSET as usize)
                        .cast::<u32>()
                        .read()
                };
                assert_eq!(
                    preserved, 0xa1b2_c3d4,
                    "ticket u32 store preserves the adjacent identity root"
                );
            } else {
                assert_eq!(memory, before);
                assert_eq!(lab.top, start);
                let frame_bytes = std::ptr::from_ref(&frame).cast::<u8>();
                assert!(frame.this_value.is_undefined());
                assert_eq!(
                    unsafe {
                        frame_bytes
                            .add(abi::NATIVE_FRAME_CONSTRUCT_RECEIVER_OFFSET as usize)
                            .cast::<u64>()
                            .read()
                    },
                    Value::UNDEFINED.to_bits()
                );
                assert_eq!(
                    unsafe {
                        frame_bytes
                            .add(abi::NATIVE_FRAME_CONSTRUCT_LAYOUT_OFFSET as usize)
                            .cast::<u32>()
                            .read()
                    },
                    0
                );
                assert_eq!(
                    unsafe {
                        frame_bytes
                            .add(abi::NATIVE_FRAME_DERIVED_THIS_CONTEXT_OFFSET as usize)
                            .cast::<u32>()
                            .read()
                    },
                    0xa1b2_c3d4
                );
            }
            assert_eq!(runtime.receiver_alloc_generated, u64::from(fits));
            assert_eq!(runtime.receiver_alloc_attempts, 0);
            assert_eq!(runtime.receiver_alloc_guard_misses, 0);
            assert_eq!(runtime.receiver_alloc_space_misses, 0);
            for (tag, row) in stats.iter().enumerate() {
                let owns = fits && tag == OBJECT_BODY_TYPE_TAG as usize;
                assert_eq!(row.alloc_count_total, u64::from(owns));
                assert_eq!(row.alloc_bytes_total, if owns { bytes as u64 } else { 0 });
                assert_eq!(row.live_bytes, if owns { bytes } else { 0 });
                assert_eq!(row.free_count_total, 0);
            }
        }
    }
}
