//! Executable x86 private-LAB proofs of the production literal encoders.
//!
//! # Contents
//! - Exact empty/static shells, numeric/tagged slabs and multiword hole maps.
//! - Source-realm and limit misses, complete type accounting and late results.
//!
//! # Invariants
//! - These poisoned host buffers never enter the GC cage or collector.
//! - The emitted System V fixture preserves all nonvolatile registers it uses.
//! - Tests execute the production fit encoder and inspect its complete bytes;
//!   real moving-GC and native-generation proofs belong to runtime fixtures.

use super::*;
use crate::CompiledCode;
use crate::allocation::{AllocationValue, EmptyLiteralLayout};
use crate::artifact::relocation::RelocationCapture;
use crate::entry::{VALUE_HOLE, VALUE_UNDEFINED};
use otter_gc::{lab::LinearAllocationArea, stats::TypeStats};
use otter_vm::jit::{
    JitArrayLiteralAllocationPlan, JitEmptyArrayAllocationPlan, JitEmptyObjectAllocationPlan,
    JitObjectLiteralAllocationPlan,
};
use otter_vm::{
    Value,
    native_abi::{CallRequest, JitCtx, NativeResultPair, VmThread},
    value::tag,
};

const POISON: u64 = 0xcafe_1357_2468_abcd;
const OLD_RESULT: usize = 0x1357;

#[derive(Clone, Copy)]
enum Layout {
    Empty(EmptyLiteralLayout),
    Object(JitObjectLiteralAllocationPlan),
    Array(JitArrayLiteralAllocationPlan),
}

fn put32(words: &mut [u64], byte: u32, value: u32) {
    words[byte as usize / 8] |= u64::from(value) << (byte % 8 * 8);
}

fn execute(
    layout: Layout,
    values: &[u64],
    fits: bool,
    expected_realm: u32,
    actual_realm: u32,
    regs: LabRegisters,
) {
    // The committed result register is excluded from all initialization temps.
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
    let numeric = values
        .iter()
        .all(|bits| *bits == VALUE_HOLE || *bits & tag::NUMBER_TAG != 0);
    let dense = match layout {
        Layout::Array(plan) => Some(if numeric { plan.numeric } else { plan.tagged }),
        _ => None,
    };
    let bytes = match layout {
        Layout::Empty(EmptyLiteralLayout::Object(plan)) => plan.cell_bytes,
        Layout::Empty(EmptyLiteralLayout::Array(plan)) => plan.cell_bytes,
        Layout::Object(plan) => plan.cell_bytes,
        Layout::Array(_) => {
            let plan = dense.unwrap();
            plan.shell.cell_bytes + plan.slab_bytes
        }
    };
    let mut words = vec![POISON; bytes as usize / 8 + 2];
    // SAFETY: the aligned payload is complete and has initialized guards.
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
    // Preserve the full nonvolatile bank because alternate allocator recipes
    // exercise RBX/R12 as well as volatile GP temporaries.
    dynasm!(ops ; .arch x64
        ; push rbx ; push rbp ; push r12 ; push r13 ; push r14 ; push r15
        ; mov r15, rdi ; sub rsp, stack as i32
    );
    for index in 0..values.len() {
        let byte = index as i32 * 8;
        dynasm!(ops ; .arch x64 ; mov r10, [rsi + byte] ; mov [rsp + byte], r10);
    }
    crate::x86_64::values::emit_load_u64(&mut ops, 0, OLD_RESULT as u64);
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
        Layout::Empty(plan) => emit_empty_literal(&mut ops, 15, expected_realm, plan, regs, slow),
        Layout::Object(plan) => {
            emit_object_literal(&mut ops, 15, expected_realm, plan, &inputs, regs, slow)
        }
        Layout::Array(plan) => emit_array_literal(
            &mut ops,
            &mut relocations,
            15,
            expected_realm,
            start as u64 - 4096,
            plan,
            &inputs,
            regs,
            slow,
        ),
    }
    dynasm!(ops ; .arch x64
        ; mov rax, Rq(regs.candidate) ; jmp =>done ; =>slow ; =>done
        ; add rsp, stack as i32
        ; pop r15 ; pop r14 ; pop r13 ; pop r12 ; pop rbp ; pop rbx ; ret
    );
    let code = CompiledCode::new(ops.finalize().unwrap(), entry);
    // SAFETY: the private System V function receives valid bounded host
    // inputs/LAB/stats, preserves every nonvolatile GP and exact stack, and
    // neither calls the VM nor exposes the fabricated host cells to GC.
    let call: extern "sysv64" fn(*mut JitCtx, *const u64) -> usize =
        unsafe { std::mem::transmute(code.entry_ptr()) };
    let result = call(&mut ctx, values.as_ptr());
    let array = matches!(
        layout,
        Layout::Array(_) | Layout::Empty(EmptyLiteralLayout::Array(_))
    );
    let accepted = fits && actual_realm == expected_realm && (!array || expected_realm == 0);
    assert_eq!(result, if accepted { start } else { OLD_RESULT });
    assert_eq!(words[0], POISON);
    assert_eq!(*words.last().unwrap(), POISON);
    if !accepted {
        assert_eq!(lab.top, start, "miss has no publication effect");
        assert!(
            words.iter().all(|word| *word == POISON),
            "miss has no initialization effect"
        );
    } else {
        assert_eq!(lab.top, start + bytes as usize);
        let body = &words[1..words.len() - 1];
        let mut expected = vec![0; body.len()];
        match layout {
            Layout::Empty(EmptyLiteralLayout::Object(plan)) => {
                expected[0] = plan.header_word;
                put32(&mut expected, plan.shape_byte, plan.shape);
                for byte in plan.initial_value_bytes {
                    expected[byte as usize / 8] = VALUE_UNDEFINED;
                }
            }
            Layout::Empty(EmptyLiteralLayout::Array(plan)) => {
                expected[0] = plan.header_word;
            }
            Layout::Object(plan) => {
                expected[0] = plan.header_word;
                put32(&mut expected, plan.shape_byte, plan.shape);
                for index in 0..plan.inline_capacity {
                    let byte = plan
                        .fields
                        .inline_byte(otter_vm::object::FieldLocation::inline(index));
                    expected[byte as usize / 8] = values
                        .get(index as usize)
                        .copied()
                        .unwrap_or(VALUE_UNDEFINED);
                }
            }
            Layout::Array(_) => {
                let plan = dense.unwrap();
                let slab = plan.shell.cell_bytes as usize / 8;
                let holes = values.iter().filter(|bits| **bits == VALUE_HOLE).count();
                let kind = if !numeric {
                    3
                } else if holes == 0 {
                    1
                } else {
                    2
                };
                expected[0] = plan.shell.header_word;
                expected[slab] = plan.slab_header_word;
                put32(
                    &mut expected,
                    plan.shell_slab_byte,
                    4096 + plan.shell.cell_bytes,
                );
                expected[plan.shell_length_byte as usize / 8] = values.len() as u64;
                expected[plan.shell_data_byte as usize / 8] =
                    start as u64 + u64::from(plan.shell.cell_bytes + plan.data_byte);
                put32(&mut expected, plan.shell_len_byte, values.len() as u32);
                put32(&mut expected, plan.shell_capacity_byte, values.len() as u32);
                put32(&mut expected, plan.shell_kind_byte, kind);
                let tail = &mut expected[slab..];
                put32(tail, plan.capacity_byte, values.len() as u32);
                put32(tail, plan.len_byte, values.len() as u32);
                put32(
                    tail,
                    plan.hole_count_byte,
                    if numeric { holes as u32 } else { 0 },
                );
                put32(tail, plan.kind_byte, kind);
                put32(tail, plan.dirty_start_byte, u32::MAX);
                for (index, bits) in values.iter().copied().enumerate() {
                    tail[plan.data_byte as usize / 8 + index] = if !numeric {
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
                        tail[plan.bitmap_byte as usize / 8 + index / 64] |= 1 << (index % 64);
                    }
                }
            }
        }
        assert_eq!(
            body, expected,
            "entire shell/slab payload, including padding"
        );
    }
    for (tag, row) in stats.iter().enumerate() {
        let owned = if !accepted {
            0
        } else {
            match layout {
                Layout::Empty(EmptyLiteralLayout::Object(plan))
                    if tag == plan.header_word as u8 as usize =>
                {
                    plan.cell_bytes
                }
                Layout::Empty(EmptyLiteralLayout::Array(plan))
                    if tag == plan.header_word as u8 as usize =>
                {
                    plan.cell_bytes
                }
                Layout::Object(plan) if tag == plan.header_word as u8 as usize => plan.cell_bytes,
                Layout::Array(_) if tag == dense.unwrap().shell.header_word as u8 as usize => {
                    dense.unwrap().shell.cell_bytes
                }
                Layout::Array(_) if tag == dense.unwrap().slab_header_word as u8 as usize => {
                    dense.unwrap().slab_bytes
                }
                _ => 0,
            }
        };
        assert_eq!(row.live_bytes, owned as usize);
        assert_eq!(row.alloc_count_total, u64::from(owned != 0));
        assert_eq!(row.alloc_bytes_total, u64::from(owned));
        assert_eq!(row.free_count_total, 0);
    }
}

const RECIPES: [LabRegisters; 2] = [
    LabRegisters {
        buffer: 1,
        candidate: 2,
        end: 3,
        scratch: 6,
        size: 11,
    },
    LabRegisters {
        buffer: 12,
        candidate: 9,
        end: 7,
        scratch: 8,
        size: 11,
    },
];

#[test]
fn native_empty_fit_and_pre_effect_realm_limit_misses_keep_late_result() {
    let layouts = [
        Layout::Empty(EmptyLiteralLayout::Object(
            JitEmptyObjectAllocationPlan::new(4096),
        )),
        Layout::Empty(EmptyLiteralLayout::Array(
            JitEmptyArrayAllocationPlan::default(),
        )),
    ];
    for regs in RECIPES {
        for layout in layouts {
            execute(layout, &[], true, 0, 0, regs);
            execute(layout, &[], false, 0, 0, regs);
            execute(layout, &[], true, 0, 1, regs);
            execute(layout, &[], true, 1, 1, regs);
        }
    }
}

#[test]
fn native_literal_bulk_homes_and_numeric_holes_initialize_and_count_exact_cells() {
    let fields = otter_vm::object::FieldLayout::current();
    let object = JitObjectLiteralAllocationPlan {
        shape: 4096,
        shape_byte: 8,
        cell_bytes: fields.cell_bytes(64) as u32,
        header_word: JitEmptyObjectAllocationPlan::new(4096).header_word & 0xffff_ffff
            | ((fields.cell_bytes(64) as u64) << 32),
        inline_capacity: 64,
        value_count: 63,
        fields,
    };
    let object_values = (0..63)
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
    for regs in RECIPES {
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
            execute(layout, values, true, 0, 0, regs);
            execute(layout, values, false, 0, 0, regs);
            execute(layout, values, true, 0, 1, regs);
            execute(layout, values, true, 1, 1, regs);
        }
    }
}
