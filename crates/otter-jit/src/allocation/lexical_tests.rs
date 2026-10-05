//! Executable both-target context/closure LAB geometry proofs.
//!
//! # Contents
//! - Prepared scope/arrow payloads and dynamic copies with alternate GP recipes.
//! - Complete poisoned bytes/accounting, pre-effect misses and live canaries.
//!
//! # Invariants
//! - Host cells never enter the moving heap; runtime tests own GC acceptance.
//! - Physical offsets come from a real VM compile snapshot of verified code.
//! - Both fixtures execute the production target encoder and preserve their ABI.
//! - Every non-temporary GP/FP canary survives both fit and miss paths.
//!
//! # See also
//! - `crate::graph` owns canonical roots at allocating cold calls.

use super::*;
use crate::CompiledCode;
#[cfg(target_arch = "aarch64")]
use dynasmrt::aarch64::Assembler;
#[cfg(target_arch = "x86_64")]
use dynasmrt::x64::Assembler;
use dynasmrt::{DynasmApi, DynasmLabelApi, dynasm};
use otter_bytecode::{BytecodeModule, Function, FunctionCodeBuilder, Op, SourceKind};
use otter_gc::{lab::LinearAllocationArea, stats::TypeStats};
use otter_vm::jit::{
    JIT_CLOSURE_CELL_BYTES, JIT_YOUNG_CLOSURE_HEADER_WORD, JIT_YOUNG_CONTEXT_HEADER_WORD,
    JitClosureAllocationPlan, JitContextAllocationPlan,
};
use otter_vm::{
    ExecutionContext, JitCompileSnapshot, Value,
    native_abi::{CallRequest, JitCtx, NativeResultPair, VmThread},
};

const POISON: u64 = 0xcafe_1357_2468_abcd;
const OLD_RESULT: u64 = 0x1357;
const CANARY: u64 = 0x51a7_6d9b_2468_1357;

#[derive(Clone, Copy, Debug)]
enum Construction {
    Create {
        extension: bool,
    },
    Copy {
        extension: bool,
        slots: usize,
    },
    Closure {
        arrow: bool,
        new_target: bool,
        invalid_context: bool,
    },
}

fn fixture_view() -> JitCompileSnapshot {
    let mut code = FunctionCodeBuilder::new();
    code.push(Op::ReturnUndefined, &[]);
    let context = ExecutionContext::from_module(
        BytecodeModule {
            module: "native-lexical-layout.js".into(),
            template_sites: Vec::new(),
            source_kind: SourceKind::JavaScript,
            functions: vec![Function {
                id: 0,
                name: "<main>".into(),
                locals: 1,
                code: code.finish(),
                ..Function::default()
            }],
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
            function_source: None,
        },
        otter_vm::source_registry::SourceRegistry::default(),
    )
    .expect("verifier-valid source for the VM-owned lexical layouts");
    let view = context
        .jit_compile_snapshot(0)
        .expect("snapshot of the actual linked function");
    let layout = view.closure_call_layout;
    // The plain closure initializes exactly four disjoint physical words;
    // bound words follow them. The flags share the call-header word only.
    let ranges = [
        (0, otter_gc::header::HEADER_SIZE as u32),
        (layout.function_id_byte, 8),
        (layout.context_byte, 8),
        (layout.rare_byte, 8),
    ];
    for pair in ranges.windows(2) {
        assert_eq!(pair[0].0 + pair[0].1, pair[1].0);
    }
    assert_eq!(layout.flags_byte, layout.function_id_byte + 4);
    assert_eq!(layout.rare_byte + 8, JIT_CLOSURE_CELL_BYTES);
    assert_eq!(layout.bound_this_byte, JIT_CLOSURE_CELL_BYTES);
    assert_eq!(layout.bound_new_target_byte, layout.bound_this_byte + 8);
    view
}

fn execute(view: &JitCompileSnapshot, case: Construction, regs: LabRegisters, fits: bool) {
    let case_label = format!("{case:?}, {regs:?}, fit={fits}");
    let layout = view.context_layout;
    let mut source = vec![0u64; layout.slots_byte as usize / 8 + 3];
    source[0] = JIT_YOUNG_CONTEXT_HEADER_WORD | ((source.len() as u64 * 8) << 32);
    source[layout.scope_function_id_byte as usize / 8] = 55 | (2u64 << 32) | (3u64 << 48);
    source[layout.parent_byte as usize / 8] = Value::undefined().to_bits();
    source[layout.slots_byte as usize / 8..].copy_from_slice(&[
        Value::hole().to_bits(),
        Value::number_i32(17).to_bits(),
        Value::undefined().to_bits(),
    ]);
    let mut expected;
    let mut allowed = true;
    let context_plan;
    let closure_plan;
    let values;
    let tag;
    match case {
        Construction::Create { extension } => {
            let initial = vec![
                Value::hole().to_bits(),
                Value::undefined().to_bits(),
                Value::hole().to_bits(),
            ];
            let initial = if extension {
                [initial, vec![Value::undefined().to_bits()]].concat()
            } else {
                initial
            };
            let bytes = layout.slots_byte + initial.len() as u32 * 8;
            context_plan = Some(JitContextAllocationPlan {
                cell_bytes: bytes,
                header_word: JIT_YOUNG_CONTEXT_HEADER_WORD | (u64::from(bytes) << 32),
                body_word: 77 | ((4u64 | if extension { 1 << 15 } else { 0 }) << 32) | (3u64 << 48),
                derived_this_slot: None,
                initial_words: initial.clone().into_boxed_slice(),
            });
            closure_plan = None;
            expected = vec![0; bytes as usize / 8];
            expected[0] = context_plan.as_ref().unwrap().header_word;
            expected[layout.scope_function_id_byte as usize / 8] =
                context_plan.as_ref().unwrap().body_word;
            expected[layout.parent_byte as usize / 8] = source.as_ptr() as u64;
            expected[layout.slots_byte as usize / 8..].copy_from_slice(&initial);
            values = [
                source.as_ptr() as u64,
                Value::undefined().to_bits(),
                Value::undefined().to_bits(),
            ];
            tag = layout.type_tag;
        }
        Construction::Copy { extension, slots } => {
            source.resize(
                layout.slots_byte as usize / 8 + slots,
                Value::undefined().to_bits(),
            );
            source[0] = JIT_YOUNG_CONTEXT_HEADER_WORD | ((source.len() as u64 * 8) << 32);
            source[layout.scope_function_id_byte as usize / 8] =
                55 | ((2u64 | if extension { 1 << 15 } else { 0 }) << 32) | ((slots as u64) << 48);
            expected = source.clone();
            context_plan = None;
            closure_plan = None;
            values = [
                source.as_ptr() as u64,
                Value::undefined().to_bits(),
                Value::undefined().to_bits(),
            ];
            allowed = !extension && slots <= otter_vm::jit::JIT_INLINE_CONTEXT_MAX_WORDS;
            tag = layout.type_tag;
        }
        Construction::Closure {
            arrow,
            new_target,
            invalid_context,
        } => {
            // Synthetic expected header: the public named-lookup summary
            // occupies the flags word's top byte. The real VM producer owns
            // the production call-word recipe.
            let flags = (u32::from(otter_vm::closure::CLOSURE_LOOKUP_ORDINARY) << 24)
                | if arrow {
                    otter_vm::closure::CLOSURE_CALL_FLAG_BOUND_THIS
                } else {
                    0
                };
            let plan = JitClosureAllocationPlan {
                call_word: 78 | (u64::from(flags) << 32),
                arrow,
            };
            let bytes = JIT_CLOSURE_CELL_BYTES
                + if arrow {
                    8 + if new_target { 8 } else { 0 }
                } else {
                    0
                };
            values = [
                if invalid_context {
                    Value::number_i32(7).to_bits()
                } else {
                    source.as_ptr() as u64
                },
                Value::number_i32(23).to_bits(),
                if new_target {
                    Value::function(91).to_bits()
                } else {
                    Value::undefined().to_bits()
                },
            ];
            expected = vec![0; bytes as usize / 8];
            expected[0] = JIT_YOUNG_CLOSURE_HEADER_WORD | (u64::from(bytes) << 32);
            let layout = view.closure_call_layout;
            expected[layout.function_id_byte as usize / 8] = plan.call_word
                | if arrow && new_target {
                    JitClosureAllocationPlan::bound_new_target_word()
                } else {
                    0
                };
            expected[layout.context_byte as usize / 8] = values[0];
            if arrow {
                expected[layout.bound_this_byte as usize / 8] = values[1];
                if new_target {
                    expected[layout.bound_new_target_byte as usize / 8] = values[2];
                }
            }
            context_plan = None;
            closure_plan = Some(plan);
            allowed = !invalid_context;
            tag = otter_vm::closure::JS_CLOSURE_BODY_TYPE_TAG;
        }
    }
    let bytes = expected.len() * 8;
    let mut words = vec![POISON; expected.len() + 2];
    // SAFETY: complete aligned private host payload and guard words.
    let start = unsafe { words.as_mut_ptr().add(1) } as usize;
    let mut lab = LinearAllocationArea {
        top: start,
        limit: start + bytes - usize::from(!fits),
    };
    let mut stats = [TypeStats::DEFAULT; 256];
    let mut thread = VmThread::empty();
    let mut ctx = JitCtx {
        thread: &mut thread,
        native_frame: std::ptr::null_mut(),
        error: std::ptr::null_mut(),
        generated_depth_limit: u64::MAX,
        global_this_offset: std::ptr::null(),
        native_stack_limit: 0,
        generated_feedback_clean: 1,
        alloc_window: otter_vm::jit::JitMachineAllocationWindow {
            lab: &mut lab,
            type_stats: stats.as_mut_ptr(),
        },
        runtime_stats: std::ptr::null_mut(),
        pending_call: CallRequest::EMPTY,
        completion: NativeResultPair::success(Value::UNDEFINED),
        completion_destination: u32::MAX,
        completion_generation: 0,
    };
    let mut canaries = [0u64; 16];
    let mut ops = Assembler::new().unwrap();
    let entry = ops.offset();
    let slow = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    #[cfg(target_arch = "aarch64")]
    let gp = [1u8, 2, 3, 4, 5, 14, 15];
    #[cfg(target_arch = "x86_64")]
    let gp = [1u8, 5, 6, 7, 13, 14];
    assert!(
        ![
            regs.buffer,
            regs.candidate,
            regs.end,
            regs.scratch,
            regs.size
        ]
        .contains(&0)
    );
    assert!(gp.iter().all(|register| {
        ![
            regs.buffer,
            regs.candidate,
            regs.end,
            regs.scratch,
            regs.size,
        ]
        .contains(register)
    }));
    #[cfg(target_arch = "aarch64")]
    {
        dynasm!(ops ; .arch aarch64 ; stp x20, x30, [sp, #-16]! ; mov x20, x0 ; sub sp, sp, 32 ; str x2, [sp, #24]);
        for index in 0..3u32 {
            let byte = index * 8;
            dynasm!(ops ; .arch aarch64 ; ldr x16, [x1, byte] ; str x16, [sp, byte]);
        }
        for &register in &gp {
            crate::template::arm64::values::emit_load_u64(
                &mut ops,
                register,
                CANARY + u64::from(register),
            );
        }
        for register in 0..8u8 {
            crate::template::arm64::values::emit_load_u64(
                &mut ops,
                16,
                CANARY + u64::from(register),
            );
            dynasm!(ops ; .arch aarch64 ; fmov D(register), x16);
        }
        crate::template::arm64::values::emit_load_u64(&mut ops, 0, OLD_RESULT);
        let input = [
            AllocationValue::StackByte(0),
            AllocationValue::StackByte(8),
            AllocationValue::StackByte(16),
        ];
        if let Some(ref plan) = context_plan {
            crate::arm64::allocation::emit_create_context(
                &mut ops, view, plan, 20, input[0], regs, slow,
            );
        } else if let Some(plan) = closure_plan {
            crate::arm64::allocation::emit_closure(&mut ops, view, plan, 20, input, regs, slow);
        } else {
            crate::arm64::allocation::emit_copy_context(&mut ops, view, 20, input[0], regs, slow);
        }
        dynasm!(ops ; .arch aarch64 ; mov x0, X(regs.candidate) ; b =>done ; =>slow ; =>done ; ldr x16, [sp, #24]);
        for (index, &register) in gp.iter().enumerate() {
            let byte = index as u32 * 8;
            dynasm!(ops ; .arch aarch64 ; str X(register), [x16, byte]);
        }
        for register in 0..8u8 {
            let byte = (gp.len() as u32 + u32::from(register)) * 8;
            dynasm!(ops ; .arch aarch64 ; str D(register), [x16, byte]);
        }
        dynasm!(ops ; .arch aarch64 ; add sp, sp, 32 ; ldp x20, x30, [sp], #16 ; ret);
    }
    #[cfg(target_arch = "x86_64")]
    {
        dynasm!(ops ; .arch x64 ; push rbx ; push rbp ; push r12 ; push r13 ; push r14 ; push r15 ; mov r15, rdi ; sub rsp, 32 ; mov [rsp + 24], rdx);
        for index in 0..3i32 {
            let byte = index * 8;
            dynasm!(ops ; .arch x64 ; mov r10, [rsi + byte] ; mov [rsp + byte], r10);
        }
        for &register in &gp {
            crate::x86_64::values::emit_load_u64(&mut ops, register, CANARY + u64::from(register));
        }
        for register in 0..8u8 {
            crate::x86_64::values::emit_load_u64(&mut ops, 10, CANARY + u64::from(register));
            dynasm!(ops ; .arch x64 ; movq Rx(register), r10);
        }
        crate::x86_64::values::emit_load_u64(&mut ops, 0, OLD_RESULT);
        let input = [
            AllocationValue::StackByte(0),
            AllocationValue::StackByte(8),
            AllocationValue::StackByte(16),
        ];
        if let Some(ref plan) = context_plan {
            crate::x86_64::allocation::emit_create_context(
                &mut ops, view, plan, 15, input[0], regs, slow,
            );
        } else if let Some(plan) = closure_plan {
            crate::x86_64::allocation::emit_closure(&mut ops, view, plan, 15, input, regs, slow);
        } else {
            crate::x86_64::allocation::emit_copy_context(&mut ops, view, 15, input[0], regs, slow);
        }
        dynasm!(ops ; .arch x64 ; mov rax, Rq(regs.candidate) ; jmp =>done ; =>slow ; =>done ; mov r10, [rsp + 24]);
        for (index, &register) in gp.iter().enumerate() {
            let byte = index as i32 * 8;
            dynasm!(ops ; .arch x64 ; mov [r10 + byte], Rq(register));
        }
        for register in 0..8u8 {
            let byte = (gp.len() as i32 + i32::from(register)) * 8;
            dynasm!(ops ; .arch x64 ; movq [r10 + byte], Rx(register));
        }
        dynasm!(ops ; .arch x64 ; add rsp, 32 ; pop r15 ; pop r14 ; pop r13 ; pop r12 ; pop rbp ; pop rbx ; ret);
    }
    let code = CompiledCode::new(ops.finalize().unwrap(), entry);
    // SAFETY: the finalized fixture has this exact preserved native ABI.
    #[cfg(target_arch = "aarch64")]
    let call: extern "C" fn(*mut JitCtx, *const u64, *mut u64) -> usize =
        unsafe { std::mem::transmute(code.entry_ptr()) };
    // SAFETY: the finalized fixture has this exact private SysV ABI.
    #[cfg(target_arch = "x86_64")]
    let call: extern "sysv64" fn(*mut JitCtx, *const u64, *mut u64) -> usize =
        unsafe { std::mem::transmute(code.entry_ptr()) };
    // SAFETY: the fixture preserves its private ABI and uses complete bounded
    // host input/LAB/stat spans; no fabricated cell reaches the collector.
    let result = call(&mut ctx, values.as_ptr(), canaries.as_mut_ptr());
    let accepted = fits && allowed;
    assert_eq!(
        result,
        if accepted { start } else { OLD_RESULT as usize },
        "{case_label} result"
    );
    assert_eq!(words[0], POISON, "{case_label} preceding guard");
    assert_eq!(
        *words.last().unwrap(),
        POISON,
        "{case_label} trailing guard"
    );
    for (index, &register) in gp.iter().enumerate() {
        assert_eq!(
            canaries[index],
            CANARY + u64::from(register),
            "{case_label} GP{register}"
        );
    }
    for register in 0..8usize {
        assert_eq!(
            canaries[gp.len() + register],
            CANARY + register as u64,
            "{case_label} FP{register}"
        );
    }
    if accepted {
        assert_eq!(lab.top, start + bytes, "{case_label} publication");
        assert_eq!(
            &words[1..words.len() - 1],
            expected,
            "{case_label} complete payload"
        );
    } else {
        assert_eq!(lab.top, start, "{case_label} no publication");
        assert!(
            words.iter().all(|word| *word == POISON),
            "{case_label} no partial initialization"
        );
    }
    for (index, row) in stats.iter().enumerate() {
        let owned = accepted && index == tag as usize;
        assert_eq!(
            row.alloc_count_total,
            u64::from(owned),
            "{case_label} tag {index} allocation count"
        );
        assert_eq!(
            row.alloc_bytes_total,
            if owned { bytes as u64 } else { 0 },
            "{case_label} tag {index} allocated bytes"
        );
        assert_eq!(
            row.live_bytes,
            if owned { bytes } else { 0 },
            "{case_label} tag {index} live bytes"
        );
    }
}

#[test]
fn lexical_native_fits_and_pre_effect_misses_keep_complete_payloads_and_canaries() {
    let view = fixture_view();
    #[cfg(target_arch = "aarch64")]
    let recipes = [
        LabRegisters {
            buffer: 6,
            candidate: 7,
            end: 8,
            scratch: 9,
            size: 17,
        },
        LabRegisters {
            buffer: 10,
            candidate: 11,
            end: 12,
            scratch: 13,
            size: 17,
        },
    ];
    #[cfg(target_arch = "x86_64")]
    let recipes = [
        LabRegisters {
            buffer: 2,
            candidate: 3,
            end: 8,
            scratch: 9,
            size: 11,
        },
        LabRegisters {
            buffer: 12,
            candidate: 3,
            end: 8,
            scratch: 9,
            size: 11,
        },
    ];
    for regs in recipes {
        for fits in [false, true] {
            for extension in [false, true] {
                execute(&view, Construction::Create { extension }, regs, fits);
            }
            for slots in [
                0,
                3,
                otter_vm::jit::JIT_INLINE_CONTEXT_MAX_WORDS,
                otter_vm::jit::JIT_INLINE_CONTEXT_MAX_WORDS + 1,
            ] {
                execute(
                    &view,
                    Construction::Copy {
                        extension: false,
                        slots,
                    },
                    regs,
                    fits,
                );
            }
            execute(
                &view,
                Construction::Copy {
                    extension: true,
                    slots: 3,
                },
                regs,
                fits,
            );
            for arrow in [false, true] {
                for new_target in [false, true] {
                    for invalid_context in [false, true] {
                        execute(
                            &view,
                            Construction::Closure {
                                arrow,
                                new_target,
                                invalid_context,
                            },
                            regs,
                            fits,
                        );
                    }
                }
            }
        }
    }
}
