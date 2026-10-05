//! Executable private-LAB proof of canonical-home literal initialization.
//!
//! # Contents
//! - Numeric/holey/tagged two-cell arrays with exact initialization/accounting.
//! - Shaped objects, bounded bulk inputs and pre-effect realm/limit misses.
//!
//! # Invariants
//! These host buffers never enter the GC cage or collector. Executed code
//! preserves its private ABI and tests exact bytes, not JavaScript reachability.
//! Real native/GC integration requires the runtime allocation fixtures.

use super::*;
use crate::CompiledCode;
use otter_gc::{lab::LinearAllocationArea, stats::TypeStats};
use otter_vm::jit::JitEmptyObjectAllocationPlan;
use otter_vm::{
    Value,
    native_abi::{CallRequest, JitCtx, NativeResultPair, VmThread},
};

const POISON: u64 = 0xcafe_1357_2468_abcd;

#[derive(Clone, Copy)]
enum Layout {
    Object(JitObjectLiteralAllocationPlan),
    Array(JitArrayLiteralAllocationPlan),
}

fn execute(layout: Layout, values: &[u64], fits: bool, actual_realm: u32, regs: LabRegisters) {
    let numeric = values
        .iter()
        .all(|bits| *bits == VALUE_HOLE || *bits & tag::NUMBER_TAG != 0);
    let dense = match layout {
        Layout::Array(p) => Some(if numeric { p.numeric } else { p.tagged }),
        _ => None,
    };
    let bytes = match layout {
        Layout::Object(p) => p.cell_bytes,
        Layout::Array(_) => {
            let p = dense.unwrap();
            p.shell.cell_bytes + p.slab_bytes
        }
    };
    let mut words = vec![POISON; bytes as usize / 8 + 2];
    // SAFETY: the complete payload is aligned and bounded by two guards.
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
        completion: NativeResultPair::success(Value::UNDEFINED),
        completion_destination: u32::MAX,
        completion_generation: 0,
    };
    let mut ops = Assembler::new().unwrap();
    let entry = ops.offset();
    let slow = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    let stack = (values.len() as u32 * 8).next_multiple_of(16);
    dynasm!(ops ; .arch aarch64 ; stp x20, x30, [sp, #-16]! ; mov x20, x0 ; sub sp, sp, stack);
    for index in 0..values.len() {
        let byte = index as u32 * 8;
        dynasm!(ops ; .arch aarch64 ; ldr x16, [x1, byte] ; str x16, [sp, byte]);
    }
    let inputs = values
        .iter()
        .enumerate()
        .map(|(index, bits)| {
            if index.is_multiple_of(2) {
                AllocationValue::StackByte(index as u32 * 8)
            } else {
                AllocationValue::Constant(*bits)
            }
        })
        .collect::<Vec<_>>();
    let mut relocations = RelocationCapture::default();
    match layout {
        Layout::Object(plan) => emit_object_literal(&mut ops, 20, 0, plan, &inputs, regs, slow),
        Layout::Array(plan) => emit_array_literal(
            &mut ops,
            &mut relocations,
            20,
            0,
            start as u64 - 4096,
            plan,
            &inputs,
            regs,
            slow,
        ),
    }
    dynasm!(ops ; .arch aarch64 ; mov x0, X(regs.candidate) ; b =>done ; =>slow ; mov x0, xzr ; =>done ; add sp, sp, stack ; ldp x20, x30, [sp], #16 ; ret);
    let code = CompiledCode::new(ops.finalize().unwrap(), entry);
    // SAFETY: this private ABI receives valid bounded host inputs/LAB/statistics,
    // preserves x20/LR/SP, and neither calls the VM nor exposes host cells to GC.
    let call: extern "C" fn(*mut JitCtx, *const u64) -> usize =
        unsafe { std::mem::transmute(code.entry_ptr()) };
    let result = call(&mut ctx, values.as_ptr());
    let accepted = fits && actual_realm == 0;
    assert_eq!(result, if accepted { start } else { 0 });
    assert_eq!(words[0], POISON);
    assert_eq!(*words.last().unwrap(), POISON);
    if !accepted {
        assert_eq!(lab.top, start);
        assert!(words.iter().all(|word| *word == POISON));
        assert!(
            stats
                .iter()
                .all(|row| row.alloc_count_total == 0 && row.live_bytes == 0)
        );
        return;
    }
    assert_eq!(lab.top, start + bytes as usize);
    let body = &words[1..words.len() - 1];
    match layout {
        Layout::Object(plan) => {
            let mut expected = vec![0; body.len()];
            expected[0] = plan.header_word;
            expected[plan.shape_byte as usize / 8] = u64::from(plan.shape);
            for index in 0..plan.inline_capacity {
                let byte = plan
                    .fields
                    .inline_byte(otter_vm::object::FieldLocation::inline(index));
                expected[byte as usize / 8] = values
                    .get(index as usize)
                    .copied()
                    .unwrap_or(VALUE_UNDEFINED);
            }
            assert_eq!(body, expected);
        }
        Layout::Array(_) => {
            let p = dense.unwrap();
            let slab = p.shell.cell_bytes as usize / 8;
            let holes = values.iter().filter(|bits| **bits == VALUE_HOLE).count();
            let kind = if !numeric {
                3
            } else if holes == 0 {
                1
            } else {
                2
            };
            let mut expected = vec![0; body.len()];
            expected[0] = p.shell.header_word;
            expected[slab] = p.slab_header_word;
            let put32 = |words: &mut [u64], byte: u32, value: u32| {
                words[byte as usize / 8] |= u64::from(value) << (byte % 8 * 8);
            };
            put32(&mut expected, p.shell_slab_byte, 4096 + p.shell.cell_bytes);
            expected[p.shell_length_byte as usize / 8] = values.len() as u64;
            expected[p.shell_data_byte as usize / 8] =
                start as u64 + u64::from(p.shell.cell_bytes + p.data_byte);
            put32(&mut expected, p.shell_len_byte, values.len() as u32);
            put32(&mut expected, p.shell_capacity_byte, values.len() as u32);
            put32(&mut expected, p.shell_kind_byte, kind);
            let tail = &mut expected[slab..];
            put32(tail, p.capacity_byte, values.len() as u32);
            put32(tail, p.len_byte, values.len() as u32);
            put32(
                tail,
                p.hole_count_byte,
                if numeric { holes as u32 } else { 0 },
            );
            put32(tail, p.kind_byte, kind);
            put32(tail, p.dirty_start_byte, u32::MAX);
            for (index, bits) in values.iter().copied().enumerate() {
                tail[p.data_byte as usize / 8 + index] = if !numeric {
                    bits
                } else if bits == VALUE_HOLE {
                    0
                } else {
                    Value::from_bits(bits)
                        .as_number()
                        .unwrap()
                        .as_f64()
                        .to_bits()
                };
                if numeric && bits == VALUE_HOLE {
                    tail[p.bitmap_byte as usize / 8 + index / 64] |= 1 << (index % 64);
                }
            }
            assert_eq!(body, expected);
        }
    }
    for (tag, row) in stats.iter().enumerate() {
        let owned = match layout {
            Layout::Object(p) if tag == p.header_word as u8 as usize => p.cell_bytes,
            Layout::Array(_) if tag == dense.unwrap().shell.header_word as u8 as usize => {
                dense.unwrap().shell.cell_bytes
            }
            Layout::Array(_) if tag == dense.unwrap().slab_header_word as u8 as usize => {
                dense.unwrap().slab_bytes
            }
            _ => 0,
        };
        assert_eq!(row.live_bytes, owned as usize);
        assert_eq!(row.alloc_count_total, u64::from(owned != 0));
        assert_eq!(row.alloc_bytes_total, u64::from(owned));
    }
}

#[test]
fn literal_native_fit_miss_bulk_homes_and_numeric_holes_initialize_exactly() {
    let fields = otter_vm::object::FieldLayout::current();
    let object = JitObjectLiteralAllocationPlan {
        shape: 4096,
        shape_byte: 8,
        cell_bytes: fields.cell_bytes(64) as u32,
        header_word: JitEmptyObjectAllocationPlan::new(4096).header_word & 0xffff_ffff
            | ((fields.cell_bytes(64) as u64) << 32),
        inline_capacity: 64,
        value_count: 64,
        fields,
    };
    let object_values = (0..64)
        .map(|index| Value::number_i32(index).to_bits())
        .collect::<Vec<_>>();
    let numeric = vec![
        Value::number_i32(-17).to_bits(),
        Value::number_f64(-0.0).to_bits(),
        Value::number_f64(f64::NAN).to_bits(),
        Value::number_f64(f64::INFINITY).to_bits(),
    ];
    let mut holes = (0..240)
        .map(|index| Value::number_i32(index).to_bits())
        .collect::<Vec<_>>();
    for index in [0, 63, 64, 127, 239] {
        holes[index] = VALUE_HOLE;
    }
    let tagged = vec![
        VALUE_UNDEFINED,
        Value::number_i32(4).to_bits(),
        VALUE_HOLE,
        0x1000,
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
        for (layout, values) in [
            (Layout::Object(object), object_values.as_slice()),
            (
                Layout::Array(JitArrayLiteralAllocationPlan::new(numeric.len()).unwrap()),
                numeric.as_slice(),
            ),
            (
                Layout::Array(JitArrayLiteralAllocationPlan::new(holes.len()).unwrap()),
                holes.as_slice(),
            ),
            (
                Layout::Array(JitArrayLiteralAllocationPlan::new(tagged.len()).unwrap()),
                tagged.as_slice(),
            ),
        ] {
            execute(layout, values, true, 0, regs);
            execute(layout, values, false, 0, regs);
            execute(layout, values, true, 1, regs);
        }
    }
}
