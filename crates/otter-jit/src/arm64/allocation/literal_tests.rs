//! Executable private-LAB proof of canonical-home literal initialization.
//!
//! # Contents
//! - Numeric/holey/tagged two-cell arrays with exact initialization and one
//!   cursor publication covering both cells.
//! - Shaped objects, bounded bulk inputs and pre-effect realm/limit misses.
//!
//! # Invariants
//! These host buffers never enter the GC cage or collector. Executed code
//! preserves its private ABI and tests exact bytes, not JavaScript reachability.
//! Real native/GC integration requires the runtime allocation fixtures.

use super::*;
use crate::CompiledCode;
use otter_gc::lab::LinearAllocationArea;
use otter_vm::jit::{JitAllocationSite, JitEmptyObjectAllocationPlan, JitSiteMode};
use otter_vm::{
    Value,
    native_abi::{CallRequest, JitCtx, NativeResultPair, VmThread},
};

const POISON: u64 = 0xcafe_1357_2468_abcd;

#[derive(Clone)]
enum Layout {
    Object(JitObjectLiteralAllocationPlan),
    Array(JitArrayLiteralAllocationPlan),
}

fn execute(
    layout: &Layout,
    site: JitAllocationSite,
    values: &[u64],
    fits: bool,
    actual_realm: u32,
    regs: LabRegisters,
) {
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
    // Both windows name one host buffer, so either placement carves it.
    let lab_ptr: *mut LinearAllocationArea = &mut lab;
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
            lab: lab_ptr,
            old_lab: lab_ptr,
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
        Layout::Object(plan) => emit_object_literal(
            &mut ops,
            &mut relocations,
            &otter_vm::JitCompileSnapshot::without_feedback(0, 0, 0, vec![]),
            20,
            0,
            plan,
            site,
            &vec![
                crate::field_store::FieldStoreOwner {
                    owner_shape: 0,
                    stored_shapes: smallvec::SmallVec::new(),
                    nullish_mask: 0,
                    int32: false,
                    int32_constant: None,
                };
                inputs.len()
            ],
            &inputs,
            regs,
            slow,
        ),
        Layout::Array(plan) => emit_array_literal(
            &mut ops,
            &mut relocations,
            20,
            0,
            start as u64 - 4096,
            *plan,
            site,
            &inputs,
            regs,
            slow,
        ),
    }
    dynasm!(ops ; .arch aarch64 ; mov x0, X(regs.candidate) ; b =>done ; =>slow ; mov x0, xzr ; =>done ; add sp, sp, stack ; ldp x20, x30, [sp], #16 ; ret);
    let code = CompiledCode::new(ops.finalize().unwrap(), entry);
    // SAFETY: this private ABI receives valid bounded host inputs and LAB,
    // preserves x20/LR/SP, and neither calls the VM nor exposes host cells to GC.
    let call: extern "C" fn(*mut JitCtx, *const u64) -> usize =
        unsafe { std::mem::transmute(code.entry_ptr()) };
    let result = call(&mut ctx, values.as_ptr());
    // A tenured array is carved only numeric.
    let accepted = fits
        && actual_realm == 0
        && !(site.mode == JitSiteMode::Old && matches!(layout, Layout::Array(_)) && !numeric);
    assert_eq!(result, if accepted { start } else { 0 });
    assert_eq!(words[0], POISON);
    assert_eq!(*words.last().unwrap(), POISON);
    if !accepted {
        assert_eq!(lab.top, start);
        assert!(words.iter().all(|word| *word == POISON));
        return;
    }
    assert_eq!(lab.top, start + bytes as usize);
    let body = &words[1..words.len() - 1];
    if matches!(layout, Layout::Array(_)) {
        let byte = otter_vm::jit::JitArrayStorage::current().exotic_byte as usize;
        assert_eq!(
            byte + 4,
            otter_vm::jit::JitArrayLayout::current().expando_byte as usize
        );
        assert!(byte.is_multiple_of(8));
        assert_eq!(
            body[byte / 8],
            0,
            "native shell publishes both rare and expando edges null"
        );
    }

    match layout {
        Layout::Object(plan) => {
            let mut expected = vec![0; body.len()];
            expected[0] = site.header_word(plan.header_word);
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
            expected[0] = site.header_word(p.shell.header_word);
            expected[slab] = if site.mode == JitSiteMode::Old {
                site.header_word(p.slab_header_word)
            } else {
                p.slab_header_word
            };
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
            (Layout::Object(object.clone()), object_values.as_slice()),
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
            for mode in [JitSiteMode::Young, JitSiteMode::Track, JitSiteMode::Old] {
                let site = JitAllocationSite {
                    id: 0x2a5,
                    record: 0,
                    mode,
                };
                execute(&layout, site, values, true, 0, regs);
                execute(&layout, site, values, false, 0, regs);
                execute(&layout, site, values, true, 1, regs);
            }
        }
    }
}

/// Selected records alone decide observation: an all-zero literal emits
/// neither the observer leaf nor the LAB buffer reload an observing one needs.
#[test]
fn zero_records_emit_no_observation_and_keep_the_buffer_live() {
    let fields = otter_vm::object::FieldLayout::current();
    let plan = JitObjectLiteralAllocationPlan {
        shape: 4096,
        shape_byte: 8,
        cell_bytes: fields.cell_bytes(4) as u32,
        header_word: JitEmptyObjectAllocationPlan::new(4096).header_word,
        inline_capacity: 4,
        value_count: 2,
        fields,
    };
    let regs = LabRegisters {
        buffer: 1,
        candidate: 2,
        end: 3,
        scratch: 4,
        size: 17,
    };
    let reload = {
        let mut ops = Assembler::new().unwrap();
        dynasm!(ops ; .arch aarch64 ; ldr X(regs.buffer), [x20, crate::entry::ALLOC_WINDOW_LAB_OFFSET]);
        ops.finalize().unwrap().to_vec()
    };
    let view = otter_vm::JitCompileSnapshot::without_feedback(0, 0, 0, vec![]);
    for observed in [false, true] {
        let records: Vec<_> = (0..2)
            .map(|index| crate::field_store::FieldStoreOwner {
                owner_shape: if observed && index == 1 { 8192 } else { 0 },
                stored_shapes: smallvec::smallvec![4096],
                nullish_mask: 0,
                int32: false,
                int32_constant: None,
            })
            .collect();
        let mut ops = Assembler::new().unwrap();
        let slow = ops.new_dynamic_label();
        let mut relocations = RelocationCapture::new(true);
        emit_object_literal(
            &mut ops,
            &mut relocations,
            &view,
            20,
            0,
            &plan,
            JitAllocationSite::default(),
            &records,
            &[
                AllocationValue::Constant(VALUE_UNDEFINED),
                AllocationValue::StackByte(0),
            ],
            regs,
            slow,
        );
        dynasm!(ops ; .arch aarch64 ; =>slow ; ret);
        let code = ops.finalize().unwrap();
        let reloads = code
            .chunks_exact(4)
            .filter(|word| *word == reload.as_slice())
            .count();
        assert_eq!(
            reloads,
            1 + usize::from(observed),
            "probe, then publication"
        );
        let json = relocations.render(&code).unwrap().json;
        assert_eq!(json.contains("field_type_observe"), observed);
    }
}
