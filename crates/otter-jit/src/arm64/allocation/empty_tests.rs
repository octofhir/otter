//! Executable proof of the shared empty-literal fit and pre-effect miss.
//!
//! # Contents
//! - Poisoned private LAB, exact initialization and per-type accounting.
//! - Several declared register recipes and realm/limit miss boundaries.
//!
//! # Invariants
//! The private emitted ABI never invokes VM/GC or publishes its host storage as
//! a real cage object. It proves generated initialization and cursor ordering;
//! runtime moving-GC tests prove the actual heap/root integration separately.

use super::*;
use crate::CompiledCode;
use otter_gc::{lab::LinearAllocationArea, stats::TypeStats};
use otter_vm::jit::{JitEmptyArrayAllocationPlan, JitEmptyObjectAllocationPlan};
use otter_vm::native_abi::{CallRequest, JitCtx, NativeResultPair, VmThread};

const POISON: u64 = 0x9e37_79b9_7f4a_7c15;

fn executable(layout: EmptyLiteralLayout, realm_id: u32, regs: LabRegisters) -> CompiledCode {
    let mut ops = Assembler::new().unwrap();
    let entry = ops.offset();
    let slow = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops ; .arch aarch64 ; stp x20, x30, [sp, #-16]! ; mov x20, x0);
    emit_empty_literal(&mut ops, 20, realm_id, layout, regs, slow);
    dynasm!(ops
        ; .arch aarch64
        ; mov x0, X(regs.candidate)
        ; b =>done
        ; =>slow
        ; mov x0, xzr
        ; =>done
        ; ldp x20, x30, [sp], #16
        ; ret
    );
    let code = CompiledCode::new(ops.finalize().unwrap(), entry);
    if !matches!(layout, EmptyLiteralLayout::Array(_)) || realm_id == 0 {
        let words: Vec<u32> = code
            .bytes()
            .chunks_exact(4)
            .map(|word| u32::from_le_bytes(word.try_into().unwrap()))
            .collect();
        let publication = 0xf900_0000 | (u32::from(regs.buffer) << 5) | u32::from(regs.end);
        let publish_index = words
            .iter()
            .position(|&word| word == publication)
            .expect("LAB top publication");
        let initialized: Vec<usize> = words
            .iter()
            .enumerate()
            .filter_map(|(index, &word)| {
                let store = word & 0xffc0_0000;
                ((store == 0xf900_0000 || store == 0xb900_0000)
                    && ((word >> 5) & 31) == u32::from(regs.candidate))
                .then_some(index)
            })
            .collect();
        assert!(!initialized.is_empty());
        assert!(
            initialized.iter().all(|&index| index < publish_index),
            "every header/body store precedes cursor publication"
        );
    }
    code
}

fn probe(
    layout: EmptyLiteralLayout,
    expected_realm: u32,
    actual_realm: u32,
    fits: bool,
    regs: LabRegisters,
) {
    let (bytes, header) = match layout {
        EmptyLiteralLayout::Object(plan) => (plan.cell_bytes, plan.header_word),
        EmptyLiteralLayout::Array(plan) => (plan.cell_bytes, plan.header_word),
    };
    let mut words = vec![POISON; bytes as usize / 8 + 2];
    // SAFETY: the complete aligned payload lies between two initialized guards.
    let start = unsafe { words.as_mut_ptr().add(1) } as usize;
    let mut lab = LinearAllocationArea {
        top: start,
        limit: start + bytes as usize - usize::from(!fits),
    };
    let mut stats = [TypeStats::DEFAULT; 256];
    let mut thread = VmThread::empty();
    thread.active_realm_cell = std::ptr::addr_of!(actual_realm) as u64;
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
        completion: NativeResultPair::success(otter_vm::Value::UNDEFINED),
        completion_destination: u32::MAX,
        completion_generation: 0,
    };
    let code = executable(layout, expected_realm, regs);
    // SAFETY: the private machine ABI receives this complete JitCtx, preserves
    // callee-saved x20/LR/SP, and touches only its initialized LAB/statistics and
    // realm scalar. No VM entry or collector observes the private host storage.
    let call: extern "C" fn(*mut JitCtx) -> usize =
        unsafe { std::mem::transmute(code.entry_ptr()) };
    let result = call(&mut ctx);
    let accepted = fits
        && expected_realm == actual_realm
        && (!matches!(layout, EmptyLiteralLayout::Array(_)) || expected_realm == 0);
    assert_eq!(result, if accepted { start } else { 0 });
    assert_eq!(words[0], POISON);
    assert_eq!(*words.last().unwrap(), POISON);
    if accepted {
        assert_eq!(lab.top, start + bytes as usize);
        assert_eq!(words[1], header);
        let mut expected = vec![0u64; bytes as usize / 8];
        expected[0] = header;
        if let EmptyLiteralLayout::Object(plan) = layout {
            expected[plan.shape_byte as usize / 8] = u64::from(plan.shape);
            for byte in plan.initial_value_bytes {
                expected[byte as usize / 8] = VALUE_UNDEFINED;
            }
        }
        assert_eq!(&words[1..words.len() - 1], expected);
    } else {
        assert_eq!(lab.top, start, "miss has no cursor effect");
        assert!(
            words.iter().all(|&word| word == POISON),
            "miss has no initialization effect"
        );
    }
    for (tag, row) in stats.iter().enumerate() {
        let owns = accepted && tag == header as u8 as usize;
        assert_eq!(row.live_bytes, if owns { bytes as usize } else { 0 });
        assert_eq!(row.alloc_count_total, u64::from(owns));
        assert_eq!(
            row.alloc_bytes_total,
            if owns { u64::from(bytes) } else { 0 }
        );
        assert_eq!(row.free_count_total, 0);
    }
}

#[test]
fn empty_allocation_native_fit_and_miss_initialize_exact_cells_and_count_once() {
    let layouts = [
        EmptyLiteralLayout::Object(JitEmptyObjectAllocationPlan::new(4096)),
        EmptyLiteralLayout::Array(JitEmptyArrayAllocationPlan::default()),
    ];
    for regs in [
        LabRegisters {
            buffer: 1,
            candidate: 2,
            end: 3,
            scratch: 4,
            size: 17,
        },
        LabRegisters {
            buffer: 12,
            candidate: 9,
            end: 14,
            scratch: 15,
            size: 17,
        },
    ] {
        for layout in layouts {
            probe(layout, 0, 0, true, regs);
            probe(layout, 0, 0, false, regs);
            probe(layout, 0, 1, true, regs);
            probe(layout, 1, 1, true, regs);
        }
    }
}
