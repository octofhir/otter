//! No-call x86-64 proof for `value instanceof target` over an ordinary closure.
//!
//! # Contents
//! - The same target proof, `target.prototype` read and bounded
//!   `[[Prototype]]` walk as the AArch64 probe.
//!
//! # Invariants
//! - No allocation, call, deopt or user code occurs inside the probe; every
//!   uncertain case misses into the committed operation.
//! - `r11` is the target-reserved scratch register, so the target is copied
//!   there before the value is copied into `r10`; the remaining scratch is
//!   the declared property-load clobber set. Outputs are written last.
//!
//! # See also
//! - `super::super::arm64::instanceof` — the AArch64 peer and its contract.

use super::*;

/// Prototype links the probe follows before handing the walk to the VM.
const MAX_CHAIN: i32 = 32;

pub(super) fn emit(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    inputs: [u8; 2],
    outputs: [u8; 2],
) {
    let [value, target] = inputs;
    let [result, hit] = outputs;
    let yes = ops.new_dynamic_label();
    let no = ops.new_dynamic_label();
    let miss = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    let walk = ops.new_dynamic_label();
    let step = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64 ; mov r11, Rq(target) ; mov r10, Rq(value));
    if view.cage_base == 0 || view.closure_call_layout.prototype_shape_byte == 0 {
        dynasm!(ops ; .arch x64 ; jmp =>miss);
    } else {
        let layout = view.closure_call_layout;
        let named_lookup = otter_vm::closure::CLOSURE_NAMED_LOOKUP_BYTE as i32;
        symbolic(
            ops,
            relocations,
            9,
            view.cage_base as u64,
            RelocationTarget::GcCageBase,
        );
        load64(ops, 0, NOT_CELL_MASK);
        dynasm!(ops
            ; .arch x64
            // Target: an ordinary closure; a bag may exist but no override.
            ; test r11, rax
            ; jnz =>miss
            ; test r11, r11
            ; jz =>miss
            ; mov r11d, r11d
            ; add r11, r9
            ; cmp BYTE [r11], otter_vm::closure::JS_CLOSURE_BODY_TYPE_TAG as i8
            ; jne =>miss
            ; movzx eax, BYTE [r11 + named_lookup]
            ; and eax, !i32::from(otter_vm::closure::CLOSURE_LOOKUP_OWN_PROPS)
            ; cmp eax, i32::from(otter_vm::closure::CLOSURE_LOOKUP_ORDINARY)
            ; jne =>miss
            // `prototype` lives in the bag once observed. The bag and its
            // slot proof live in the rare record (`r11` from here).
            ; mov r11d, [r11 + layout.rare_byte as i32]
            ; test r11d, r11d
            ; jz =>miss
            ; add r11, r9
            ; mov r8d, [r11 + layout.own_props_byte as i32]
            ; test r8d, r8d
            ; jz =>miss
            ; add r8, r9
            // No symbol-keyed own property, so no own `@@hasInstance`.
            ; mov eax, [r8 + view.object_exotic_handle_byte as i32]
            ; test eax, eax
            ; jz >symbols_absent
            ; add rax, r9
            ; cmp DWORD [rax + otter_vm::object::EXOTIC_SLOTS_SYMBOL_PROPS_BYTE as i32], 0
            ; jne =>miss
            ; symbols_absent:
            ; mov eax, [r11 + layout.prototype_shape_byte as i32]
            ; test eax, eax
            ; jz =>miss
            ; cmp eax, [r8 + view.object_shape_byte as i32]
            ; jne =>miss
            ; mov eax, [r11 + layout.prototype_slot_byte as i32]
            ; movzx ecx, WORD [r8 + view.object_slab_len_byte as i32]
            ; cmp eax, ecx
            ; jae =>miss
            // The bag's slot base: in-object until it spills, then its
            // slab's words (`r9` holds the cage base).
            ; mov ecx, [r8 + view.object_slab_handle_byte as i32]
            ; test ecx, ecx
            ; jnz >bag_spilled
            ; lea rcx, [r8 + view.object_inline_values_byte as i32]
            ; jmp >bag_base
            ; bag_spilled:
            ; add rcx, r9
            ; add rcx, view.object_slab_words_byte as i32
            ; bag_base:
            ; mov rdx, [rcx + rax * 8]
        );
        // The prototype must be an ordinary object; anything else throws.
        load64(ops, 0, NOT_CELL_MASK);
        dynasm!(ops
            ; .arch x64
            ; test rdx, rax
            ; jnz =>miss
            ; test rdx, rdx
            ; jz =>miss
            ; mov edx, edx
            ; add rdx, r9
            ; cmp BYTE [rdx], OBJECT_BODY_TYPE_TAG as i8
            ; jne =>miss
            // Value: a non-cell answers false; primitive cells answer false; an
            // ordinary object is walked; every other cell misses.
            ; test r10, rax
            ; jnz =>no
            ; test r10, r10
            ; jz =>miss
            ; mov r10d, r10d
            ; add r10, r9
            ; movzx eax, BYTE [r10]
            ; cmp eax, OBJECT_BODY_TYPE_TAG as i32
            ; je =>walk
        );
        for primitive_tag in view.primitive_cell_type_tags {
            dynasm!(ops ; .arch x64 ; cmp eax, i32::from(primitive_tag) ; je =>no);
        }
        dynasm!(ops
            ; .arch x64
            ; jmp =>miss
            ; =>walk
            ; mov ecx, MAX_CHAIN
            ; =>step
            ; test BYTE [r10 + view.object_flags_byte as i32], otter_vm::jit::JIT_OBJECT_FLAG_CHAIN_LINK_OPAQUE as i8
            ; jnz =>miss
            ; mov eax, [r10 + view.jit_proto_byte as i32]
            ; test eax, eax
            ; jz =>no
            ; add rax, r9
            ; mov r10, rax
            ; cmp r10, rdx
            ; je =>yes
            ; cmp BYTE [r10], OBJECT_BODY_TYPE_TAG as i8
            ; jne =>miss
            ; dec ecx
            ; jnz =>step
            ; jmp =>miss
        );
    }
    dynasm!(ops ; .arch x64 ; =>yes);
    load64(ops, result, Value::boolean(true).to_bits());
    dynasm!(ops ; .arch x64 ; mov Rd(hit), 1 ; jmp =>done ; =>no);
    load64(ops, result, Value::boolean(false).to_bits());
    dynasm!(ops ; .arch x64 ; mov Rd(hit), 1 ; jmp =>done ; =>miss);
    load64(ops, result, Value::boolean(false).to_bits());
    dynasm!(ops ; .arch x64 ; xor Rd(hit), Rd(hit) ; =>done);
}
