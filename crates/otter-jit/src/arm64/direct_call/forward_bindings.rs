//! Generated register-alias stores for live argument forwarding.
//!
//! # Contents
//! - Patching mapped parameters after the incoming/captured window copy.
//! - Stable caller-base recovery across dynamic private-frame reservations.
//!
//! # Invariants
//! - All capture allocation and moving GC precede these stores. The loader reads
//!   current rooted homes, never stale copies from before that allocation.
//! - Missing actuals remain absent; only declared formals and existing actual
//!   slots are patched. Captured bindings were read by the shared leaf copy.
//! - No call, collection or stack-pointer change occurs in this program.
//! - Entry: x0 actual count, w2 callee parameter count, w3 full register count.
//!   The loader writes x14, may clobber x15/x16/x17, and preserves x2/x3/x8.
//!
//! # See also
//! - [`super::layout`] — the sole owner of linkage size and control offsets.
//! - `otter_vm::forward_arguments` — incoming/captured copy and binding metadata.

use super::*;

pub(super) fn emit(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    layout: StackLayout,
    mut load: impl FnMut(&mut Assembler, u16, u8) -> Result<(), Unsupported>,
) -> Result<(), Unsupported> {
    dynasm!(ops ; .arch aarch64 ; mov x8, x0);
    for (index, storage) in view.code_block.forwarded_argument_bindings() {
        let otter_bytecode::ArgumentBindingStorage::Register { reg } = storage else {
            continue;
        };
        let next = ops.new_dynamic_label();
        let actual = ops.new_dynamic_label();
        emit_load_u64(ops, 9, u64::from(index));
        dynasm!(ops ; .arch aarch64 ; cmp x8, x9 ; b.ls =>next);
        if let Some(size) = layout.allocation_size {
            dynasm!(ops ; .arch aarch64 ; ldr w17, [sp, size] ; add x17, sp, x17);
        } else {
            dynasm!(ops ; .arch aarch64 ; add x17, sp, layout.frame_bytes);
        }
        // x17 is the caller SP before reservation. An allocating helper can
        // rewrite its root homes while the private callee stays below that base.
        load(ops, reg, 17)?;
        emit_load_u64(ops, 9, u64::from(index));
        dynasm!(ops
            ; .arch aarch64
            ; ldr x10, [sp, NATIVE_FRAME_REGISTER_BASE_OFFSET]
            ; cmp x9, x2
            ; b.hs =>actual
            ; str x14, [x10, x9, lsl #3]
            ; =>actual
            ; ldr w11, [sp, abi::NATIVE_FRAME_ARGUMENT_COUNT_OFFSET]
            ; cbz w11, =>next
            ; add x10, x10, x3, lsl #3
            ; str x14, [x10, x9, lsl #3]
            ; =>next
        );
    }
    Ok(())
}

#[cfg(all(test, target_arch = "aarch64"))]
mod tests {
    use super::*;
    use dynasmrt::AssemblyOffset;
    use otter_bytecode::{ArgumentBindingStorage, ArgumentsObjectKind};

    #[test]
    fn current_caller_home_is_read_above_the_private_callee() {
        let mut view = JitCompileSnapshot::without_feedback(0, 1, 1, vec![]);
        view.seed_argument_bindings_for_test(
            ArgumentsObjectKind::Mapped,
            &[(1, ArgumentBindingStorage::Register { reg: 0 })],
        );
        for dynamic in [false, true] {
            let mut ops = Assembler::new().unwrap();
            let mut layout = StackLayout::dynamic_prefix();
            layout.frame_bytes = 256;
            if !dynamic {
                layout.allocation_size = None;
            }
            // x0 is a live rooted word in the caller's original frame; x1 is
            // the actual count. Leave a different word in the callee's homes
            // so a wrong SP-relative load or an invented missing argument fails.
            dynasm!(ops
                ; .arch aarch64
                ; sub sp, sp, #16
                ; str x0, [sp]
                ; sub sp, sp, #256
                ; mov w9, #256
                ; str w9, [sp, 120]
                ; add x9, sp, #128
                ; str x9, [sp, NATIVE_FRAME_REGISTER_BASE_OFFSET]
                ; str w1, [sp, abi::NATIVE_FRAME_ARGUMENT_COUNT_OFFSET]
            );
            let initial = otter_vm::Value::undefined().to_bits();
            emit_load_u64(&mut ops, 9, initial);
            dynasm!(ops
                ; .arch aarch64
                ; str x9, [sp, 136]
                ; str x9, [sp, 152]
                ; mov x0, x1
                ; mov w2, #2
                ; mov w3, #2
            );
            emit(&mut ops, &view, layout, |ops, register, base| {
                assert_eq!(register, 0);
                dynasm!(ops ; .arch aarch64 ; ldr x14, [X(base)]);
                Ok(())
            })
            .unwrap();
            dynasm!(ops
                ; .arch aarch64
                ; ldr x0, [sp, 136]
                ; ldr x1, [sp, 152]
                ; eor x0, x0, x1, ror #1
                ; add sp, sp, #256
                ; add sp, sp, #16
                ; ret
            );
            let code = ops.finalize().unwrap();
            // SAFETY: this complete leaf obeys the C argument/return ABI,
            // touches only its initialized private stack, restores SP and
            // never calls Rust or retains the executable pointer.
            let call: extern "C" fn(u64, u64) -> u64 =
                unsafe { std::mem::transmute(code.ptr(AssemblyOffset(0))) };
            let value = otter_vm::Value::number_i32(32).to_bits();
            assert_eq!(call(value, 2), value ^ value.rotate_right(1));
            assert_eq!(call(value, 0), initial ^ initial.rotate_right(1));
        }
    }
}
