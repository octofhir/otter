//! Guarded primitive-string intrinsic completion for the optimizing tier.
//!
//! # Contents
//! - Selection of `charCodeAt(Int32)` and one-code-unit `indexOf(String)`.
//! - Direct reads from inline and sequential Latin-1 / UTF-16 bodies.
//! - A bounded generated search loop with an exact cold semantic fallback.
//!
//! # Invariants
//! - The caller has already proved the exact realm prototype method identity
//!   and leaves the receiver's guarded `JsStringBody` header in `x13`.
//! - Generated code reads immutable contiguous content only. Rope, slice,
//!   coercive, out-of-range, and long-search cases branch to the ordinary
//!   pre-effect method transition.
//! - `indexOf` scans at most [`MAX_GENERATED_INDEX_OF_LEN`] code units, so the
//!   generated loop does not weaken interrupt latency.
//!
//! # See also
//! - `otter_vm::jit::JitStringLayout` — stable tags and body offsets.
//! - `crate::template::arm64::ic_probe::emit_guarded_method_guard_preserving_receiver_from_tagged_register`.

use super::*;
use crate::template::arm64::values::{emit_box_int32, emit_load_symbol_u64};

const MAX_GENERATED_INDEX_OF_LEN: u32 = 256;

/// Whether one guarded string method can complete from the current SSA
/// representations without entering the Rust leaf ABI.
pub(super) fn guarded_string_intrinsic_is_supported(
    stub_id: otter_vm::native_abi::RuntimeStubId,
    reprs: &ReprMap,
    instruction: &SsaInstr,
) -> bool {
    let Some(arguments) = instruction.inputs.get(1..) else {
        return false;
    };
    if stub_id == otter_vm::native_abi::STUB_STRING_CHAR_CODE_AT_LEAF.id {
        return arguments.len() == 1 && reprs.representation(arguments[0]) == Representation::Int32;
    }
    if stub_id == otter_vm::native_abi::STUB_STRING_INDEX_OF_LEAF.id {
        return arguments.len() == 1
            && reprs.representation(arguments[0]) == Representation::Tagged;
    }
    false
}

/// Emit a selected string intrinsic. The tagged result is left in `x0`.
pub(super) fn emit_guarded_string_intrinsic_body(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    stub_id: otter_vm::native_abi::RuntimeStubId,
    reprs: &ReprMap,
    allocation: &Allocation,
    instruction: &SsaInstr,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    if stub_id == otter_vm::native_abi::STUB_STRING_CHAR_CODE_AT_LEAF.id {
        return emit_char_code_at(ops, view, allocation, instruction, miss);
    }
    if stub_id == otter_vm::native_abi::STUB_STRING_INDEX_OF_LEAF.id {
        return emit_single_unit_index_of(
            ops,
            relocations,
            view,
            reprs,
            allocation,
            instruction,
            miss,
        );
    }
    Err(Unsupported::OperandShape("guarded string intrinsic"))
}

fn emit_char_code_at(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    allocation: &Allocation,
    instruction: &SsaInstr,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    let argument = instruction.inputs[1];
    emit_load_location(ops, allocation.location(argument), 9)?;
    let layout = view.string_layout;
    let inline_latin1_tag = u32::from(layout.inline_latin1_tag);
    let seq_latin1_tag = u32::from(layout.seq_latin1_tag);
    let inline_flat_tag = u32::from(layout.inline_flat_tag);
    let seq_flat_tag = u32::from(layout.seq_flat_tag);
    let inline_latin1 = ops.new_dynamic_label();
    let seq_latin1 = ops.new_dynamic_label();
    let inline_flat = ops.new_dynamic_label();
    let seq_flat = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; tbnz w9, #31, =>miss
        ; ldr w14, [x13, layout.string_len_byte]
        ; cmp w9, w14
        ; b.hs =>miss
        ; ldrb w14, [x13, layout.string_repr_byte]
        ; cmp w14, inline_latin1_tag
        ; b.eq =>inline_latin1
        ; cmp w14, seq_latin1_tag
        ; b.eq =>seq_latin1
        ; cmp w14, inline_flat_tag
        ; b.eq =>inline_flat
        ; cmp w14, seq_flat_tag
        ; b.eq =>seq_flat
        ; b =>miss
        ; =>inline_latin1
        ; add x11, x13, layout.string_repr_payload_byte
        ; ldrb w0, [x11, x9]
        ; b =>done
        ; =>seq_latin1
        ; add x11, x13, layout.string_body_size
        ; ldrb w0, [x11, x9]
        ; b =>done
        ; =>inline_flat
        ; add x11, x13, layout.string_repr_payload_byte
        ; ldrh w0, [x11, x9, lsl #1]
        ; b =>done
        ; =>seq_flat
        ; add x11, x13, layout.string_body_size
        ; ldrh w0, [x11, x9, lsl #1]
        ; =>done
    );
    emit_box_int32(ops, 0, 9);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_single_unit_index_of(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    reprs: &ReprMap,
    allocation: &Allocation,
    instruction: &SsaInstr,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    let argument = instruction.inputs[1];
    emit_load_boxed_value(ops, reprs, allocation, argument, 9)?;
    emit_cell_test(ops, 9, 10, CellTest::IsNotCell, miss);
    dynasm!(ops ; .arch aarch64 ; mov w12, w9);
    emit_load_symbol_u64(
        ops,
        relocations,
        11,
        view.cage_base as u64,
        RelocationTarget::GcCageBase,
    );
    let layout = view.string_layout;
    let string_type_tag = u32::from(layout.string_type_tag);
    let inline_latin1_tag = u32::from(layout.inline_latin1_tag);
    let seq_latin1_tag = u32::from(layout.seq_latin1_tag);
    let inline_flat_tag = u32::from(layout.inline_flat_tag);
    let seq_flat_tag = u32::from(layout.seq_flat_tag);
    let needle_inline_latin1 = ops.new_dynamic_label();
    let needle_seq_latin1 = ops.new_dynamic_label();
    let needle_inline_flat = ops.new_dynamic_label();
    let needle_seq_flat = ops.new_dynamic_label();
    let needle_done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; add x12, x11, x12
        ; ldrb w14, [x12]
        ; cmp w14, string_type_tag
        ; b.ne =>miss
        ; ldr w14, [x12, layout.string_len_byte]
        ; cmp w14, #1
        ; b.ne =>miss
        ; ldrb w14, [x12, layout.string_repr_byte]
        ; cmp w14, inline_latin1_tag
        ; b.eq =>needle_inline_latin1
        ; cmp w14, seq_latin1_tag
        ; b.eq =>needle_seq_latin1
        ; cmp w14, inline_flat_tag
        ; b.eq =>needle_inline_flat
        ; cmp w14, seq_flat_tag
        ; b.eq =>needle_seq_flat
        ; b =>miss
        ; =>needle_inline_latin1
        ; ldrb w10, [x12, layout.string_repr_payload_byte]
        ; b =>needle_done
        ; =>needle_seq_latin1
        ; ldrb w10, [x12, layout.string_body_size]
        ; b =>needle_done
        ; =>needle_inline_flat
        ; ldrh w10, [x12, layout.string_repr_payload_byte]
        ; b =>needle_done
        ; =>needle_seq_flat
        ; ldrh w10, [x12, layout.string_body_size]
        ; =>needle_done
        ; ldr w14, [x13, layout.string_len_byte]
        ; cmp w14, MAX_GENERATED_INDEX_OF_LEN
        ; b.hi =>miss
    );

    let hay_inline_latin1 = ops.new_dynamic_label();
    let hay_seq_latin1 = ops.new_dynamic_label();
    let hay_inline_flat = ops.new_dynamic_label();
    let hay_seq_flat = ops.new_dynamic_label();
    let scan_latin1 = ops.new_dynamic_label();
    let scan_flat = ops.new_dynamic_label();
    let latin1_loop = ops.new_dynamic_label();
    let flat_loop = ops.new_dynamic_label();
    let found = ops.new_dynamic_label();
    let not_found = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    dynasm!(ops
        ; .arch aarch64
        ; ldrb w15, [x13, layout.string_repr_byte]
        ; cmp w15, inline_latin1_tag
        ; b.eq =>hay_inline_latin1
        ; cmp w15, seq_latin1_tag
        ; b.eq =>hay_seq_latin1
        ; cmp w15, inline_flat_tag
        ; b.eq =>hay_inline_flat
        ; cmp w15, seq_flat_tag
        ; b.eq =>hay_seq_flat
        ; b =>miss
        ; =>hay_inline_latin1
        ; add x11, x13, layout.string_repr_payload_byte
        ; b =>scan_latin1
        ; =>hay_seq_latin1
        ; add x11, x13, layout.string_body_size
        ; b =>scan_latin1
        ; =>hay_inline_flat
        ; add x11, x13, layout.string_repr_payload_byte
        ; b =>scan_flat
        ; =>hay_seq_flat
        ; add x11, x13, layout.string_body_size
        ; b =>scan_flat
        ; =>scan_latin1
        ; cmp w10, #255
        ; b.hi =>not_found
        ; mov w0, wzr
        ; =>latin1_loop
        ; cmp w0, w14
        ; b.hs =>not_found
        ; ldrb w15, [x11, x0]
        ; cmp w15, w10
        ; b.eq =>found
        ; add w0, w0, #1
        ; b =>latin1_loop
        ; =>scan_flat
        ; mov w0, wzr
        ; =>flat_loop
        ; cmp w0, w14
        ; b.hs =>not_found
        ; ldrh w15, [x11, x0, lsl #1]
        ; cmp w15, w10
        ; b.eq =>found
        ; add w0, w0, #1
        ; b =>flat_loop
        ; =>not_found
        ; movn w0, #0
        ; b =>done
        ; =>found
        ; =>done
    );
    emit_box_int32(ops, 0, 9);
    Ok(())
}
